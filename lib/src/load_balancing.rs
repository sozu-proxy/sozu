use std::{cell::RefCell, fmt::Debug, hash::Hasher, net::SocketAddr, rc::Rc};

use rand::{
    RngExt,
    distr::{Distribution, weighted::WeightedIndex},
    prelude::IndexedRandom,
    rng,
};

use crate::{backends::Backend, sozu_command::proto::command::LoadMetric};

/// Default weight applied when a backend declares no explicit
/// `load_balancing_parameters.weight`. Mirrors the value used by the
/// `Random` algorithm so weighting stays consistent across policies.
const DEFAULT_WEIGHT: i32 = 100;

/// Fixed seed used by the affinity hashers (HRW / Maglev). It must NOT be a
/// `RandomState`: the selection has to be reproducible across workers and
/// across process restarts so that every Sōzu instance routing the same flow
/// key lands on the same backend. The constant is arbitrary but stable.
pub const DEFAULT_HASH_SEED: u64 = 0x9E37_79B9_7F4A_7C15;

/// Read a backend's weight, defaulting to [`DEFAULT_WEIGHT`] when unset. Weight
/// is clamped to at least `1` so a `0`/negative configured weight never zeroes
/// out a backend's share (which would make weighted hashing degenerate).
fn backend_weight(backend: &Backend) -> u32 {
    backend
        .load_balancing_parameters
        .as_ref()
        .map(|p| p.weight)
        .unwrap_or(DEFAULT_WEIGHT)
        .max(1) as u32
}

/// Deterministic, seedable 64-bit hash over the backend's STABLE identifier.
///
/// We hash the backend **socket address** (`SocketAddr`) rather than the
/// `backend_id` string: the address is the routing-stable identity (it is the
/// key used by `BackendList::remove_backend` / `has_backend` and survives a
/// reconfiguration that merely re-emits the same backend), whereas `backend_id`
/// is a human label that the control plane may rename without changing where
/// traffic actually goes. Hashing the address keeps HRW/Maglev placement stable
/// across such cosmetic reconfigurations.
///
/// Uses `std::hash::SipHasher13` indirectly via a tiny FNV-1a construction with
/// an injected seed — fully reproducible, never `RandomState`.
fn hash_backend(seed: u64, key: u64, addr: &SocketAddr) -> u64 {
    let mut h = FnvHasher::with_seed(seed);
    h.write_u64(key);
    match addr {
        SocketAddr::V4(v4) => {
            h.write_u8(4);
            h.write(&v4.ip().octets());
            h.write_u16(v4.port());
        }
        SocketAddr::V6(v6) => {
            h.write_u8(6);
            h.write(&v6.ip().octets());
            h.write_u16(v6.port());
        }
    }
    // FNV-1a alone has weak avalanche on structured input (the low bits barely
    // mix), which skews rendezvous/Maglev distribution. Apply a splitmix64
    // finalizer to scatter the bits before the consumers reduce mod M or feed
    // it into the score function. Deterministic and seed-independent.
    splitmix64_finalize(h.finish())
}

/// splitmix64 mixing step — a strong, reversible 64-bit avalanche finalizer.
/// Used to scatter the FNV output so downstream `% M` / `ln(h)` reductions see
/// well-distributed bits.
fn splitmix64_finalize(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Smallest prime `>= n`. Maglev requires a prime table size `M`: it keeps the
/// permutation stride coprime with `M` so the population loop visits every
/// slot, and `M >= 2` so `skip = h2 % (M - 1) + 1` never divides by zero.
/// Runs once at construction (off the datapath); a trial-division check is more
/// than fast enough for the small sizes Sōzu uses (default 65537).
fn next_prime(n: usize) -> usize {
    let mut candidate = n.max(2);
    while !is_prime(candidate) {
        candidate += 1;
    }
    candidate
}

/// Trial-division primality test. Module-level (not nested in `next_prime`) so
/// the Maglev `rebuild` post-condition can re-assert that the table size stayed
/// the prime it was constructed with.
fn is_prime(x: usize) -> bool {
    if x < 2 {
        return false;
    }
    if x % 2 == 0 {
        return x == 2;
    }
    let mut d = 3;
    while d * d <= x {
        if x % d == 0 {
            return false;
        }
        d += 2;
    }
    true
}

/// Minimal FNV-1a 64-bit hasher with a seeded offset basis. Deterministic and
/// dependency-free — used for the affinity algorithms so results are
/// reproducible across runs (unlike `std::collections::hash_map::RandomState`).
struct FnvHasher {
    state: u64,
}

impl FnvHasher {
    const PRIME: u64 = 0x0000_0100_0000_01B3;
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

    fn with_seed(seed: u64) -> Self {
        Self {
            state: Self::OFFSET ^ seed,
        }
    }
}

impl Hasher for FnvHasher {
    fn finish(&self) -> u64 {
        self.state
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.state ^= u64::from(b);
            self.state = self.state.wrapping_mul(Self::PRIME);
        }
    }
}

pub trait LoadBalancingAlgorithm: Debug {
    /// Select the next backend.
    ///
    /// `key` carries an optional affinity hash (e.g. a UDP flow key). The
    /// stateless/round-robin policies ignore it; the consistent-hashing
    /// policies ([`Rendezvous`], [`Maglev`]) use it to pin a key to a backend.
    /// Passing `None` preserves the historical, key-agnostic behavior.
    fn next_available_backend(
        &mut self,
        key: Option<u64>,
        backends: &mut Vec<Rc<RefCell<Backend>>>,
    ) -> Option<Rc<RefCell<Backend>>>;

    /// Called by the control plane when the live backend set for a cluster
    /// changes, so table-based policies (Maglev) can recompute their lookup
    /// table off the datapath. The default is a no-op; only [`Maglev`]
    /// overrides it.
    fn rebuild(&mut self, _backends: &[Rc<RefCell<Backend>>]) {}
}

#[derive(Debug)]
pub struct RoundRobin {
    pub next_backend: u32,
}

impl LoadBalancingAlgorithm for RoundRobin {
    fn next_available_backend(
        &mut self,
        _key: Option<u64>,
        backends: &mut Vec<Rc<RefCell<Backend>>>,
    ) -> Option<Rc<RefCell<Backend>>> {
        // Guard against an empty set: `% backends.len()` would panic with a
        // divide-by-zero. This also covers the `Rendezvous`/`Maglev` policies,
        // which delegate here on `key == None`.
        if backends.is_empty() {
            return None;
        }

        let res = backends
            .get(self.next_backend as usize % backends.len())
            .map(|backend| (*backend).clone());

        self.next_backend = (self.next_backend + 1) % backends.len() as u32;
        res
    }
}

impl Default for RoundRobin {
    fn default() -> Self {
        Self::new()
    }
}

impl RoundRobin {
    pub fn new() -> Self {
        Self { next_backend: 0 }
    }
}

#[derive(Debug)]
pub struct Random;

impl LoadBalancingAlgorithm for Random {
    fn next_available_backend(
        &mut self,
        _key: Option<u64>,
        backends: &mut Vec<Rc<RefCell<Backend>>>,
    ) -> Option<Rc<RefCell<Backend>>> {
        let mut rng = rng();
        let weights: Vec<i32> = backends
            .iter()
            .map(|b| {
                b.borrow()
                    .load_balancing_parameters
                    .as_ref()
                    .map(|p| p.weight)
                    .unwrap_or(100)
            })
            .collect();

        if let Ok(dist) = WeightedIndex::new(weights) {
            let index = dist.sample(&mut rng);
            backends.get(index).cloned()
        } else {
            (*backends)
                .choose(&mut rng)
                .map(|backend| (*backend).clone())
        }
    }
}

#[derive(Debug)]
pub struct LeastLoaded {
    pub metric: LoadMetric,
}

impl LoadBalancingAlgorithm for LeastLoaded {
    fn next_available_backend(
        &mut self,
        _key: Option<u64>,
        backends: &mut Vec<Rc<RefCell<Backend>>>,
    ) -> Option<Rc<RefCell<Backend>>> {
        let opt_b = match self.metric {
            LoadMetric::Connections => backends
                .iter_mut()
                .min_by_key(|backend| backend.borrow().active_connections),
            LoadMetric::Requests => backends
                .iter_mut()
                .min_by_key(|backend| backend.borrow().active_requests),
            LoadMetric::ConnectionTime => {
                let mut b = None;
                for backend in backends.iter_mut() {
                    let cost2 = backend.borrow_mut().peak_ewma_connection();

                    match b.take() {
                        None => b = Some((cost2, backend)),
                        Some((cost1, back1)) => {
                            if cost1 <= cost2 {
                                b = Some((cost1, back1));
                            } else {
                                b = Some((cost2, backend));
                            }
                        }
                    }
                }

                b.map(|(_cost, backend)| backend)
            }
        };
        opt_b.map(|backend| (*backend).clone())
    }
}

#[derive(Debug)]
pub struct PowerOfTwo {
    pub metric: LoadMetric,
}

impl LoadBalancingAlgorithm for PowerOfTwo {
    fn next_available_backend(
        &mut self,
        _key: Option<u64>,
        backends: &mut Vec<Rc<RefCell<Backend>>>,
    ) -> Option<Rc<RefCell<Backend>>> {
        let mut first = None;
        let mut second = None;

        for backend in backends.iter_mut() {
            let measure = match self.metric {
                LoadMetric::Connections => backend.borrow().active_connections as f64,
                LoadMetric::Requests => backend.borrow().active_requests as f64,
                LoadMetric::ConnectionTime => backend.borrow_mut().peak_ewma_connection(),
            };

            if first.is_none() {
                first = Some((measure, backend));
            } else if second.is_none() {
                if first.as_ref().unwrap().0 <= measure {
                    second = Some((measure, backend));
                } else {
                    second = first.take();
                    first = Some((measure, backend));
                }
            } else if first.as_ref().unwrap().0 <= measure && measure < second.as_ref().unwrap().0 {
                second = Some((measure, backend));
                // other case: we don't change anything
            } else {
                second = first.take();
                first = Some((measure, backend));
            }
        }

        match (first, second) {
            (None, None) => None,
            (Some((_, b)), None) => Some(b.clone()),
            // should not happen, but let's be exhaustive
            (None, Some((_, b))) => Some(b.clone()),
            (Some((_, b1)), Some((_, b2))) => {
                if rng().random_bool(0.5) {
                    Some(b1.clone())
                } else {
                    Some(b2.clone())
                }
            }
        }
    }
}

/// Weighted Rendezvous (Highest Random Weight) hashing.
///
/// For an affinity `key`, the chosen backend is the one maximizing a stable,
/// per-(key, backend) score. With no key it degrades to plain round-robin.
///
/// # Weighting
///
/// We use the standard continuous weighted-rendezvous score
/// (Schindelhauer–Schomaker / "logarithmic method"):
///
/// ```text
///     score(key, backend) = -weight / ln(h)
/// ```
///
/// where `h = hash64(seed, key, backend_addr) / 2^64 ∈ (0, 1)`. Because
/// `ln(h) < 0`, the score is positive and grows with `weight`, so heavier
/// backends win proportionally more keys while the choice stays deterministic
/// and minimally disruptive: removing a non-winning backend cannot change the
/// winner, and adding one only steals the keys for which it now scores highest.
/// Selection is `O(N)` per key.
#[derive(Debug)]
pub struct Rendezvous {
    /// Reproducible hash seed (NOT a `RandomState`).
    seed: u64,
    /// Round-robin cursor used for the `key == None` fallback.
    round_robin: RoundRobin,
}

impl Default for Rendezvous {
    fn default() -> Self {
        Self::new()
    }
}

impl Rendezvous {
    pub fn new() -> Self {
        Self::with_seed(DEFAULT_HASH_SEED)
    }

    pub fn with_seed(seed: u64) -> Self {
        Self {
            seed,
            round_robin: RoundRobin::new(),
        }
    }

    /// Continuous weighted-rendezvous score for a (key, backend) pair.
    /// Larger is better. See the struct docs for the formula.
    fn score(&self, key: u64, backend: &Backend) -> f64 {
        let weight = backend_weight(backend) as f64;
        // Map the 64-bit hash into the open interval (0, 1). Guard the
        // endpoints so `ln` stays finite: 0 would give -inf, 1 would give 0.
        let h = hash_backend(self.seed, key, &backend.address);
        // (h + 0.5) / 2^64 keeps the value strictly inside (0, 1).
        let unit = (h as f64 + 0.5) / (u64::MAX as f64 + 1.0);
        -weight / unit.ln()
    }
}

impl LoadBalancingAlgorithm for Rendezvous {
    fn next_available_backend(
        &mut self,
        key: Option<u64>,
        backends: &mut Vec<Rc<RefCell<Backend>>>,
    ) -> Option<Rc<RefCell<Backend>>> {
        let Some(key) = key else {
            // No affinity key: behave exactly like RoundRobin.
            return self.round_robin.next_available_backend(None, backends);
        };

        if backends.is_empty() {
            return None;
        }

        let mut best: Option<(f64, &Rc<RefCell<Backend>>)> = None;
        for backend in backends.iter() {
            let score = self.score(key, &backend.borrow());
            match best {
                Some((best_score, _)) if best_score >= score => {}
                _ => best = Some((score, backend)),
            }
        }
        best.map(|(_, backend)| backend.clone())
    }
}

/// Maglev consistent hashing (Google, NSDI'16).
///
/// Builds a precomputed lookup table of `M` (a prime, default 65537) slots from
/// the **stable, full** backend set. Each backend derives a `(offset, skip)`
/// permutation from two seeded hashes and claims slots in permutation order
/// until the table is full; a backend's share of slots is proportional to its
/// weight.
///
/// # Rebuild discipline (never on the hot path)
///
/// The table is rebuilt **only when the full backend set changes**
/// ([`Maglev::rebuild`], wired into `BackendList::add_backend` /
/// `remove_backend`), never per packet. Building the table is `O(M)` = 65537
/// slot writes; doing it per datagram would be a DoS amplifier (one unhealthy
/// backend would trigger a full rebuild on every selection) and would also
/// destroy Maglev's stability, since the table would track a shifting subset.
///
/// # Selection (handles a shrunk healthy subset without rebuilding)
///
/// At selection time the caller passes the **healthy subset** of backends.
/// Lookup is near-`O(1)`: compute `slot = key % M`, then probe forward through
/// the table (`table[(slot + i) % M]`) and return the first entry whose address
/// is present in the healthy subset. The table maps to the full set it was
/// built from; membership is checked against the subset, so an unhealthy
/// backend is simply skipped — no rebuild, and healthy keys stay pinned. If no
/// table entry resolves to a healthy backend, fall back to round-robin over the
/// subset. With no key it falls back to round-robin.
#[derive(Debug)]
pub struct Maglev {
    seed: u64,
    /// Prime table size `M`.
    size: usize,
    /// `table[i]` is an index into `backends` (the set captured at the last
    /// [`rebuild`]). Empty when there are no backends.
    table: Vec<usize>,
    /// Backend addresses captured at the last rebuild, in the order used to
    /// build `table`. The lookup maps a table entry back to the live backend
    /// by address so it survives `Vec` reordering between rebuilds.
    backend_addrs: Vec<SocketAddr>,
    /// Round-robin cursor used for the `key == None` fallback.
    round_robin: RoundRobin,
}

impl Default for Maglev {
    fn default() -> Self {
        Self::new()
    }
}

impl Maglev {
    /// Default prime table size. 65537 is the smallest prime above 2^16; large
    /// enough to keep per-key disruption small on backend churn, small enough
    /// to rebuild cheaply off the datapath.
    pub const DEFAULT_TABLE_SIZE: usize = 65537;

    pub fn new() -> Self {
        Self::with_seed(DEFAULT_HASH_SEED)
    }

    pub fn with_seed(seed: u64) -> Self {
        Self::with_seed_and_size(seed, Self::DEFAULT_TABLE_SIZE)
    }

    pub fn with_seed_and_size(seed: u64, size: usize) -> Self {
        Self {
            seed,
            // The Maglev permutation uses `skip = h2 % (m - 1) + 1`, which
            // panics (divide-by-zero) when `m == 1`, and a single-slot table
            // is degenerate anyway. Clamp to the next prime `>= max(size, 2)`
            // so the table is always usable and the permutation stride is
            // coprime with `m` (a prime), guaranteeing the population loop
            // visits every slot.
            size: next_prime(size.max(2)),
            table: Vec::new(),
            backend_addrs: Vec::new(),
            round_robin: RoundRobin::new(),
        }
    }

    /// Rebuild the lookup table from `backends`. Called on backend-set change,
    /// NOT per packet. Honors backend weight via proportional slot share.
    pub fn rebuild(&mut self, backends: &[Rc<RefCell<Backend>>]) {
        let n = backends.len();
        self.backend_addrs.clear();
        self.table.clear();
        if n == 0 || self.size == 0 {
            return;
        }

        let m = self.size;

        // Per-backend (offset, skip) permutation parameters and weights.
        let mut offsets = Vec::with_capacity(n);
        let mut skips = Vec::with_capacity(n);
        let mut weights = Vec::with_capacity(n);
        let mut total_weight: u64 = 0;
        for backend in backends {
            let b = backend.borrow();
            let addr = b.address;
            self.backend_addrs.push(addr);
            // Two independent seeded hashes give the permutation seeds. We mix
            // distinct domain separators into the `key` slot of `hash_backend`
            // so `offset` and `skip` are uncorrelated.
            let h1 = hash_backend(self.seed, 0x6F66_6673_6574, &addr); // "offset"
            let h2 = hash_backend(self.seed, 0x736B_6970_5F5F, &addr); // "skip__"
            offsets.push((h1 % m as u64) as usize);
            skips.push((h2 % (m as u64 - 1)) as usize + 1);
            let w = backend_weight(&b) as u64;
            weights.push(w);
            total_weight += w;
        }

        // Target slot count per backend, proportional to weight. The sum of
        // targets equals `m` (remainder handed to the heaviest/first backends).
        let mut targets = vec![0usize; n];
        let mut assigned = 0usize;
        for (i, &w) in weights.iter().enumerate() {
            let t = ((w as u128 * m as u128) / total_weight as u128) as usize;
            targets[i] = t;
            assigned += t;
        }
        // Distribute the rounding remainder so the table fills exactly.
        let mut i = 0;
        while assigned < m {
            targets[i % n] += 1;
            assigned += 1;
            i += 1;
        }

        // Standard Maglev population loop, capped per backend by `targets`.
        let mut table = vec![usize::MAX; m];
        let mut next = vec![0usize; n];
        let mut filled = vec![0usize; n];
        let mut count = 0usize;
        while count < m {
            for b in 0..n {
                if filled[b] >= targets[b] {
                    continue;
                }
                // Find this backend's next free preferred slot.
                let mut c = (offsets[b] + next[b] * skips[b]) % m;
                while table[c] != usize::MAX {
                    next[b] += 1;
                    c = (offsets[b] + next[b] * skips[b]) % m;
                }
                table[c] = b;
                next[b] += 1;
                filled[b] += 1;
                count += 1;
                if count >= m {
                    break;
                }
            }
        }

        self.table = table;

        // Post-conditions (TigerStyle). The table is either fully built or empty:
        //   * `size` is the prime chosen at construction — rebuild never changes
        //     it (the permutation math depends on a coprime stride over a prime).
        //   * a populated table is exactly `size` slots, every entry a valid
        //     index into `backend_addrs` (`< backend_addrs.len() <= size`), so a
        //     later `table[slot]` lookup can never index out of `backend_addrs`.
        //   * `backend_addrs` is non-empty whenever the table is non-empty (the
        //     table maps slots to addresses; an empty address vector would make
        //     every lookup resolve to nothing).
        #[cfg(debug_assertions)]
        {
            debug_assert!(
                is_prime(self.size),
                "Maglev table size {} is not prime",
                self.size
            );
            if self.table.is_empty() {
                debug_assert!(
                    self.backend_addrs.is_empty(),
                    "Maglev: empty table but non-empty backend_addrs"
                );
            } else {
                debug_assert_eq!(
                    self.table.len(),
                    self.size,
                    "Maglev table must have exactly `size` slots"
                );
                debug_assert!(
                    !self.backend_addrs.is_empty(),
                    "Maglev: non-empty table but empty backend_addrs"
                );
                debug_assert!(
                    self.table.iter().all(|&idx| idx < self.backend_addrs.len()),
                    "Maglev table holds an index out of backend_addrs range"
                );
            }
        }
    }
}

impl LoadBalancingAlgorithm for Maglev {
    fn next_available_backend(
        &mut self,
        key: Option<u64>,
        backends: &mut Vec<Rc<RefCell<Backend>>>,
    ) -> Option<Rc<RefCell<Backend>>> {
        let Some(key) = key else {
            return self.round_robin.next_available_backend(None, backends);
        };

        if backends.is_empty() {
            return None;
        }

        // Cold start ONLY: the table is empty because no `rebuild` has fired
        // yet (the control plane wires `rebuild` into backend-set mutations,
        // but a freshly constructed policy or a never-mutated cluster may not
        // have one). Build once from the current set. This is NOT a per-packet
        // rebuild: once populated, the table is only refreshed by `rebuild` on
        // an actual set change — a partial outage (shrunk healthy subset) never
        // rebuilds, it is handled by the probe-forward below.
        if self.table.is_empty() {
            self.rebuild(backends);
        }

        if self.table.is_empty() {
            // Still empty (e.g. zero backends captured): nothing to select.
            return None;
        }

        // Probe forward from `key % M` through the table and return the first
        // entry whose backend address is present in the passed healthy subset.
        // The table was built from the full set; checking membership against
        // the subset lets us skip unhealthy backends WITHOUT rebuilding, which
        // is what keeps a partial outage off the hot path and keeps healthy
        // keys pinned to the same backend.
        let start = (key % self.size as u64) as usize;
        // `key % size` is always a valid table slot, and the table is exactly
        // `size` long here (non-empty checked above, rebuild post-condition).
        debug_assert!(start < self.size, "Maglev start slot out of range");
        debug_assert_eq!(
            self.table.len(),
            self.size,
            "Maglev lookup on a table whose length != size"
        );
        for i in 0..self.size {
            let slot = (start + i) % self.size;
            let idx = self.table[slot];
            // Resolve the table's backend index back to an address captured at
            // the last rebuild, then look it up in the live healthy subset.
            if let Some(addr) = self.backend_addrs.get(idx) {
                if let Some(backend) = backends.iter().find(|b| b.borrow().address == *addr) {
                    return Some(backend.clone());
                }
            }
        }

        // None of the table entries resolved to a healthy backend (every
        // backend the table knows about is currently unhealthy/absent). Fall
        // back to round-robin over the healthy subset so we still route.
        self.round_robin.next_available_backend(None, backends)
    }

    fn rebuild(&mut self, backends: &[Rc<RefCell<Backend>>]) {
        Maglev::rebuild(self, backends);
    }
}

#[cfg(test)]
mod test {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use super::*;
    use crate::{
        PeakEWMA,
        backends::{BackendStatus, HealthState},
        retry::{ExponentialBackoffPolicy, RetryPolicyWrapper},
        sozu_command::proto::command::{LoadBalancingParams, LoadMetric},
    };

    fn create_backend(id: String, connections: Option<usize>) -> Backend {
        Backend {
            sticky_id: None,
            backend_id: id,
            address: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080),
            status: BackendStatus::Normal,
            retry_policy: RetryPolicyWrapper::ExponentialBackoff(ExponentialBackoffPolicy::new(1)),
            active_connections: connections.unwrap_or(0),
            active_requests: 0,
            failures: 0,
            load_balancing_parameters: None,
            backup: false,
            connection_time: PeakEWMA::new(),
            health: HealthState::default(),
        }
    }

    #[test]
    fn it_should_find_the_backend_with_least_connections() {
        let backend_with_least_connection =
            Rc::new(RefCell::new(create_backend("yolo".to_string(), Some(1))));

        let mut backends = vec![
            Rc::new(RefCell::new(create_backend("nolo".to_string(), Some(10)))),
            Rc::new(RefCell::new(create_backend("philo".to_string(), Some(20)))),
            backend_with_least_connection.clone(),
        ];

        let mut least_connection_algorithm = LeastLoaded {
            metric: LoadMetric::Connections,
        };

        let backend_res = least_connection_algorithm
            .next_available_backend(None, &mut backends)
            .unwrap();
        let backend = backend_res.borrow();

        assert!(*backend == *backend_with_least_connection.borrow());
    }

    #[test]
    fn it_shouldnt_find_backend_with_least_connections_when_list_is_empty() {
        let mut backends = vec![];

        let mut least_connection_algorithm = LeastLoaded {
            metric: LoadMetric::Connections,
        };

        let backend = least_connection_algorithm.next_available_backend(None, &mut backends);
        assert!(backend.is_none());
    }

    #[test]
    fn it_should_find_backend_with_roundrobin_when_some_backends_were_removed() {
        let mut backends = vec![
            Rc::new(RefCell::new(create_backend("toto".to_string(), None))),
            Rc::new(RefCell::new(create_backend("voto".to_string(), None))),
            Rc::new(RefCell::new(create_backend("yoto".to_string(), None))),
        ];

        let mut roundrobin = RoundRobin { next_backend: 1 };
        let backend = roundrobin.next_available_backend(None, &mut backends);
        assert_eq!(backend.as_ref(), backends.get(1));

        backends.remove(1);

        let backend2 = roundrobin.next_available_backend(None, &mut backends);
        assert_eq!(backend2.as_ref(), backends.first());
    }

    // ----- HRW (Rendezvous) and Maglev affinity tests -----

    /// Build a backend with a distinct address so the address-based affinity
    /// hashers (HRW / Maglev) see distinct identities, and an optional weight.
    fn addr_backend(id: &str, last_octet: u8, port: u16, weight: Option<i32>) -> Backend {
        let mut b = create_backend(id.to_string(), None);
        b.address = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, last_octet)), port);
        b.load_balancing_parameters = weight.map(|weight| LoadBalancingParams { weight });
        b
    }

    fn rc(b: Backend) -> Rc<RefCell<Backend>> {
        Rc::new(RefCell::new(b))
    }

    fn make_backends(n: u8) -> Vec<Rc<RefCell<Backend>>> {
        (0..n)
            .map(|i| rc(addr_backend(&format!("b{i}"), i + 1, 8000 + i as u16, None)))
            .collect()
    }

    fn chosen_addr(b: &Rc<RefCell<Backend>>) -> SocketAddr {
        b.borrow().address
    }

    #[test]
    fn hrw_is_deterministic_for_a_fixed_key() {
        let mut backends = make_backends(5);
        let mut hrw = Rendezvous::new();

        let first = hrw
            .next_available_backend(Some(42), &mut backends)
            .map(|b| chosen_addr(&b));
        for _ in 0..50 {
            let again = hrw
                .next_available_backend(Some(42), &mut backends)
                .map(|b| chosen_addr(&b));
            assert_eq!(first, again, "HRW must be deterministic for a fixed key");
        }
    }

    #[test]
    fn hrw_none_key_falls_back_to_round_robin() {
        let mut backends = make_backends(3);
        let mut hrw = Rendezvous::new();

        // With None it should cycle round-robin: addresses in order then wrap.
        let a = chosen_addr(&hrw.next_available_backend(None, &mut backends).unwrap());
        let b = chosen_addr(&hrw.next_available_backend(None, &mut backends).unwrap());
        let c = chosen_addr(&hrw.next_available_backend(None, &mut backends).unwrap());
        let d = chosen_addr(&hrw.next_available_backend(None, &mut backends).unwrap());
        assert_eq!(a, chosen_addr(&backends[0]));
        assert_eq!(b, chosen_addr(&backends[1]));
        assert_eq!(c, chosen_addr(&backends[2]));
        assert_eq!(d, a, "round-robin should wrap around");
    }

    #[test]
    fn hrw_minimal_disruption_when_removing_a_non_winner() {
        // For every key whose winner is NOT the removed backend, the choice
        // must be unchanged after removal (HRW minimal-disruption property).
        let mut backends = make_backends(6);
        let mut hrw = Rendezvous::new();

        // Pick a backend to remove (index 3).
        let removed_addr = chosen_addr(&backends[3]);

        // Record winners for many keys on the full set.
        let mut before = std::collections::HashMap::new();
        for key in 0..2000u64 {
            let w = chosen_addr(
                &hrw.next_available_backend(Some(key), &mut backends)
                    .unwrap(),
            );
            before.insert(key, w);
        }

        // Remove the backend and re-evaluate.
        backends.remove(3);
        for key in 0..2000u64 {
            let after = chosen_addr(
                &hrw.next_available_backend(Some(key), &mut backends)
                    .unwrap(),
            );
            let prev = before[&key];
            if prev != removed_addr {
                assert_eq!(
                    prev, after,
                    "removing a non-winner changed the choice for key {key}"
                );
            }
        }
    }

    #[test]
    fn hrw_distribution_is_roughly_even() {
        let n = 5u8;
        let mut backends = make_backends(n);
        let mut hrw = Rendezvous::new();

        let total = 20_000u64;
        let mut counts: std::collections::HashMap<SocketAddr, u64> =
            std::collections::HashMap::new();
        for key in 0..total {
            let w = chosen_addr(
                &hrw.next_available_backend(Some(key), &mut backends)
                    .unwrap(),
            );
            *counts.entry(w).or_default() += 1;
        }

        let expected = total / n as u64;
        for b in &backends {
            let c = counts.get(&chosen_addr(b)).copied().unwrap_or(0);
            // Allow generous +/-35% slack: this is a statistical property.
            assert!(
                c > expected * 65 / 100 && c < expected * 135 / 100,
                "HRW distribution skewed: backend got {c}, expected ~{expected}"
            );
        }
    }

    #[test]
    fn maglev_is_deterministic_for_a_fixed_key() {
        let backends = make_backends(7);
        let mut mag = Maglev::new();
        mag.rebuild(&backends);

        let mut sel = backends.clone();
        let first = chosen_addr(&mag.next_available_backend(Some(12345), &mut sel).unwrap());
        for _ in 0..50 {
            let again = chosen_addr(&mag.next_available_backend(Some(12345), &mut sel).unwrap());
            assert_eq!(first, again, "Maglev must be deterministic for a fixed key");
        }
    }

    #[test]
    fn maglev_none_key_falls_back_to_round_robin() {
        let mut backends = make_backends(3);
        let mut mag = Maglev::new();
        mag.rebuild(&backends);

        let a = chosen_addr(&mag.next_available_backend(None, &mut backends).unwrap());
        let b = chosen_addr(&mag.next_available_backend(None, &mut backends).unwrap());
        let c = chosen_addr(&mag.next_available_backend(None, &mut backends).unwrap());
        let d = chosen_addr(&mag.next_available_backend(None, &mut backends).unwrap());
        assert_eq!(a, chosen_addr(&backends[0]));
        assert_eq!(b, chosen_addr(&backends[1]));
        assert_eq!(c, chosen_addr(&backends[2]));
        assert_eq!(d, a);
    }

    #[test]
    fn maglev_distribution_is_roughly_even() {
        let n = 5u8;
        let backends = make_backends(n);
        // Use a small prime table so the test is fast but still representative.
        let mut mag = Maglev::with_seed_and_size(DEFAULT_HASH_SEED, 1009);
        mag.rebuild(&backends);

        let total = 50_000u64;
        let mut counts: std::collections::HashMap<SocketAddr, u64> =
            std::collections::HashMap::new();
        let mut sel = backends.clone();
        for key in 0..total {
            let w = chosen_addr(&mag.next_available_backend(Some(key), &mut sel).unwrap());
            *counts.entry(w).or_default() += 1;
        }

        let expected = total / n as u64;
        for b in &backends {
            let c = counts.get(&chosen_addr(b)).copied().unwrap_or(0);
            // Maglev is more even than HRW; +/-15% is comfortable.
            assert!(
                c > expected * 85 / 100 && c < expected * 115 / 100,
                "Maglev distribution skewed: backend got {c}, expected ~{expected}"
            );
        }
    }

    #[test]
    fn maglev_table_rebuild_keeps_most_keys_stable() {
        // Adding a backend should move only a bounded fraction of keys
        // (Maglev disruption bound ~ 1/N of the table, well under 50%).
        let backends5 = make_backends(5);
        let mut mag = Maglev::with_seed_and_size(DEFAULT_HASH_SEED, 1009);
        mag.rebuild(&backends5);

        let total = 20_000u64;
        let mut before = std::collections::HashMap::new();
        let mut sel = backends5.clone();
        for key in 0..total {
            before.insert(
                key,
                chosen_addr(&mag.next_available_backend(Some(key), &mut sel).unwrap()),
            );
        }

        // Add a sixth backend and rebuild.
        let mut backends6 = backends5.clone();
        backends6.push(rc(addr_backend("b5", 6, 8005, None)));
        mag.rebuild(&backends6);

        let mut moved = 0u64;
        let mut sel6 = backends6.clone();
        for key in 0..total {
            let after = chosen_addr(&mag.next_available_backend(Some(key), &mut sel6).unwrap());
            if after != before[&key] {
                moved += 1;
            }
        }

        // Disruption should be well under half: most keys stay on their backend.
        assert!(
            moved < total / 2,
            "Maglev rebuild moved too many keys: {moved}/{total}"
        );
    }

    #[test]
    fn maglev_partial_outage_does_not_rebuild_and_stays_stable() {
        // FIX 1 guard: a partial outage (one backend missing from the healthy
        // subset passed to `next_available_backend`) must NOT rebuild the
        // table — the table tracks the full set captured at `rebuild`, and the
        // unhealthy backend is simply skipped via probe-forward. Selection for
        // keys that did not land on the unhealthy backend stays identical.
        let full = make_backends(5);
        let mut mag = Maglev::with_seed_and_size(DEFAULT_HASH_SEED, 1009);
        mag.rebuild(&full);

        // Snapshot the table state built from the FULL set.
        let table_before = mag.table.clone();
        let addrs_before = mag.backend_addrs.clone();
        assert_eq!(addrs_before.len(), 5, "table built from the full set");

        // Record winners on the full healthy set.
        let total = 4000u64;
        let mut before = std::collections::HashMap::new();
        let mut sel_full = full.clone();
        for key in 0..total {
            before.insert(
                key,
                chosen_addr(
                    &mag.next_available_backend(Some(key), &mut sel_full)
                        .unwrap(),
                ),
            );
        }

        // Now mark backend index 2 unhealthy by passing a REDUCED subset
        // (everything except index 2). This is exactly what
        // `BackendList::next_available_backend_with_key` does when a backend
        // fails its health check / enters retry backoff.
        let unhealthy_addr = chosen_addr(&full[2]);
        let mut subset: Vec<_> = full
            .iter()
            .filter(|b| chosen_addr(b) != unhealthy_addr)
            .cloned()
            .collect();

        for key in 0..total {
            let after = chosen_addr(&mag.next_available_backend(Some(key), &mut subset).unwrap());
            // The unhealthy backend must never be selected.
            assert_ne!(
                after, unhealthy_addr,
                "selection returned the unhealthy backend for key {key}"
            );
            // Keys that did NOT previously land on the unhealthy backend must
            // keep their original backend — probe-forward only reroutes the
            // keys that were pinned to the now-missing backend.
            if before[&key] != unhealthy_addr {
                assert_eq!(
                    before[&key], after,
                    "a healthy key moved during a partial outage (key {key})"
                );
            }
        }

        // The crux of FIX 1: the table was NOT rebuilt by the partial outage.
        // Neither the table contents nor the captured address set changed.
        assert_eq!(
            mag.table, table_before,
            "partial outage must not rebuild the Maglev table"
        );
        assert_eq!(
            mag.backend_addrs, addrs_before,
            "partial outage must not change the captured backend set"
        );
    }

    #[test]
    fn maglev_all_table_backends_unhealthy_falls_back_to_round_robin() {
        // When the healthy subset shares NO address with the table (every
        // table backend is down), probe-forward finds nothing and we fall back
        // to round-robin over the subset rather than returning None.
        let table_set = make_backends(3);
        let mut mag = Maglev::with_seed_and_size(DEFAULT_HASH_SEED, 1009);
        mag.rebuild(&table_set);
        let table_before = mag.table.clone();

        // A disjoint healthy subset (different addresses than the table).
        let mut fresh = vec![
            rc(addr_backend("n0", 50, 9000, None)),
            rc(addr_backend("n1", 51, 9001, None)),
        ];
        let fresh_addrs: Vec<_> = fresh.iter().map(chosen_addr).collect();

        let picked = chosen_addr(&mag.next_available_backend(Some(7), &mut fresh).unwrap());
        assert!(
            fresh_addrs.contains(&picked),
            "fallback must route to a backend in the healthy subset"
        );
        // And still no rebuild happened.
        assert_eq!(
            mag.table, table_before,
            "fallback must not rebuild the table"
        );
    }

    #[test]
    fn maglev_cold_start_builds_table_once() {
        // A freshly constructed Maglev with no prior `rebuild` must still
        // select (one-time cold-start build), then keep the table populated.
        let mut backends = make_backends(4);
        let mut mag = Maglev::with_seed_and_size(DEFAULT_HASH_SEED, 1009);
        assert!(mag.table.is_empty(), "table starts empty (cold)");

        let _ = mag.next_available_backend(Some(99), &mut backends).unwrap();
        assert_eq!(mag.table.len(), mag.size, "cold start populated the table");
        let table_after_cold = mag.table.clone();

        // A subsequent selection (still the full set) must NOT rebuild.
        let _ = mag
            .next_available_backend(Some(100), &mut backends)
            .unwrap();
        assert_eq!(
            mag.table, table_after_cold,
            "selection after cold start must not rebuild"
        );
    }

    #[test]
    fn round_robin_empty_set_returns_none_without_panic() {
        // FIX 2 guard: `% backends.len()` would panic on an empty Vec.
        let mut empty: Vec<Rc<RefCell<Backend>>> = vec![];
        let mut rr = RoundRobin::new();
        assert!(rr.next_available_backend(None, &mut empty).is_none());

        // Delegators (Rendezvous / Maglev with key == None) route through
        // RoundRobin and must be safe on an empty set too.
        let mut hrw = Rendezvous::new();
        assert!(hrw.next_available_backend(None, &mut empty).is_none());
        let mut mag = Maglev::new();
        assert!(mag.next_available_backend(None, &mut empty).is_none());
    }

    #[test]
    fn maglev_table_size_one_is_clamped_to_a_prime() {
        // FIX 3 guard: requesting size 1 would make `skip = h2 % (m - 1)`
        // divide by zero. The constructor must clamp to a prime >= 2.
        let mut mag = Maglev::with_seed_and_size(DEFAULT_HASH_SEED, 1);
        assert!(
            mag.size >= 2,
            "size must be clamped to >= 2, got {}",
            mag.size
        );

        let mut backends = make_backends(3);
        // Building and selecting must not panic.
        mag.rebuild(&backends);
        assert_eq!(mag.table.len(), mag.size);
        let _ = mag.next_available_backend(Some(1), &mut backends).unwrap();
    }

    #[test]
    fn next_prime_picks_the_smallest_prime_at_least_n() {
        assert_eq!(next_prime(0), 2);
        assert_eq!(next_prime(1), 2);
        assert_eq!(next_prime(2), 2);
        assert_eq!(next_prime(3), 3);
        assert_eq!(next_prime(4), 5);
        assert_eq!(next_prime(1009), 1009); // already prime — preserved
        assert_eq!(next_prime(65537), 65537); // default size is prime
    }

    #[test]
    fn maglev_honors_weight() {
        // A backend with 4x weight should win clearly more slots.
        let backends = vec![
            rc(addr_backend("light", 1, 8001, Some(100))),
            rc(addr_backend("heavy", 2, 8002, Some(400))),
        ];
        let mut mag = Maglev::with_seed_and_size(DEFAULT_HASH_SEED, 1009);
        mag.rebuild(&backends);

        let heavy_addr = chosen_addr(&backends[1]);
        let total = 20_000u64;
        let mut heavy = 0u64;
        let mut sel = backends.clone();
        for key in 0..total {
            if chosen_addr(&mag.next_available_backend(Some(key), &mut sel).unwrap()) == heavy_addr
            {
                heavy += 1;
            }
        }
        // Expected ~80% (400 / 500); allow a margin.
        assert!(
            heavy > total * 70 / 100,
            "weighted Maglev did not favor the heavy backend: {heavy}/{total}"
        );
    }
}
