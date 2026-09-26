use std::{cell::RefCell, fmt::Debug, hash::Hasher, net::SocketAddr, ops::Index, rc::Rc};

use rand::{
    RngExt, SeedableRng,
    distr::uniform::{UniformInt, UniformSampler},
    prelude::IndexedRandom,
    rngs::{StdRng, SysRng},
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
    let weight = backend
        .load_balancing_parameters
        .as_ref()
        .map(|p| p.weight)
        .unwrap_or(DEFAULT_WEIGHT)
        .max(1) as u32;
    // The clamp guarantees every backend carries a strictly positive weight,
    // otherwise weighted hashing (HRW score, Maglev slot share) would zero out
    // the backend's share and skew or divide-by-zero the distribution.
    debug_assert!(weight >= 1, "backend weight must be clamped to at least 1");
    weight
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
    // The result is the smallest prime not below `max(n, 2)`: it is prime, it
    // is at least 2, and it never went below the requested floor.
    debug_assert!(is_prime(candidate), "next_prime must return a prime");
    debug_assert!(candidate >= 2, "next_prime must return at least 2");
    debug_assert!(
        candidate >= n,
        "next_prime ({candidate}) must be >= the requested floor ({n})"
    );
    candidate
}

/// Trial-division primality test. Module-level (not nested in `next_prime`) so
/// the Maglev `rebuild` post-condition can re-assert that the table size stayed
/// the prime it was constructed with.
fn is_prime(x: usize) -> bool {
    if x < 2 {
        return false;
    }
    if x.is_multiple_of(2) {
        return x == 2;
    }
    let mut d = 3;
    while d * d <= x {
        if x.is_multiple_of(d) {
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

/// The candidates of one selection: a borrowed view of a cluster's backend
/// list, restricted to the positions a caller retained.
///
/// Position `i` of the view is `backends[indices[i]]`, so a policy that
/// indexes the view (`RoundRobin`'s cursor, `PowerOfTwo`'s two samples,
/// `Random`'s draw) computes exactly the index it computed over the
/// `Vec` of cloned candidates this view replaces. Building the view borrows
/// and copies nothing: the caller owns `indices`, and only the backend a
/// policy returns has its `Rc` cloned.
#[derive(Clone, Copy, Debug)]
pub struct Candidates<'a> {
    backends: &'a [Rc<RefCell<Backend>>],
    indices: &'a [usize],
}

impl<'a> Candidates<'a> {
    /// View `backends` through `indices`. Every index must address a slot of
    /// `backends`, and the indices must be strictly increasing.
    pub fn new(backends: &'a [Rc<RefCell<Backend>>], indices: &'a [usize]) -> Self {
        debug_assert!(
            indices.iter().all(|&index| index < backends.len()),
            "a candidate index must address a slot of the backend list"
        );
        // Strictly increasing positions keep the view in list order, which
        // the first-wins tie-breaks of `LeastLoaded` and `Rendezvous` and the
        // probe order of `Maglev` depend on, and rule out a duplicate.
        debug_assert!(
            indices.windows(2).all(|pair| pair[0] < pair[1]),
            "candidate indices must be strictly increasing"
        );
        Self { backends, indices }
    }

    pub fn len(&self) -> usize {
        self.indices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }

    /// The candidate at position `index` of the view.
    pub fn get(&self, index: usize) -> Option<&'a Rc<RefCell<Backend>>> {
        self.indices.get(index).map(|&slot| &self.backends[slot])
    }

    /// The candidates in view order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &'a Rc<RefCell<Backend>>> + use<'a> {
        let backends = self.backends;
        self.indices.iter().map(move |&slot| &backends[slot])
    }

    /// A uniformly chosen candidate, drawn exactly as
    /// `IndexedRandom::choose` draws over a slice of the same length.
    fn choose<R: rand::Rng + ?Sized>(&self, rng: &mut R) -> Option<&'a Rc<RefCell<Backend>>> {
        self.indices.choose(rng).map(|&slot| &self.backends[slot])
    }
}

impl Index<usize> for Candidates<'_> {
    type Output = Rc<RefCell<Backend>>;

    fn index(&self, index: usize) -> &Self::Output {
        &self.backends[self.indices[index]]
    }
}

pub trait LoadBalancingAlgorithm: Debug {
    /// Select the next backend among `candidates`.
    ///
    /// `key` carries an optional affinity hash (e.g. a UDP flow key). The
    /// stateless/round-robin policies ignore it; the consistent-hashing
    /// policies ([`Rendezvous`], [`Maglev`]) use it to pin a key to a backend.
    /// Passing `None` preserves the historical, key-agnostic behavior.
    ///
    /// A policy reads the candidates through the borrowed [`Candidates`] view
    /// and clones the `Rc` of the one backend it returns, nothing else, so a
    /// selection allocates nothing.
    fn next_available_backend(
        &mut self,
        key: Option<u64>,
        candidates: Candidates<'_>,
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
        backends: Candidates<'_>,
    ) -> Option<Rc<RefCell<Backend>>> {
        // Guard against an empty set: `% backends.len()` would panic with a
        // divide-by-zero. This also covers the `Rendezvous`/`Maglev` policies,
        // which delegate here on `key == None`.
        if backends.is_empty() {
            return None;
        }
        debug_assert!(
            !backends.is_empty(),
            "round-robin index math runs only on a non-empty set"
        );

        let index = self.next_backend as usize % backends.len();
        // The reduced index always addresses a real slot, so the lookup yields
        // a backend (never the `None` arm of `get`).
        debug_assert!(index < backends.len(), "round-robin index out of bounds");
        let res = backends.get(index).cloned();
        debug_assert!(
            res.is_some(),
            "round-robin must select a backend from a non-empty set"
        );

        self.next_backend = (self.next_backend + 1) % backends.len() as u32;
        // The cursor stays a valid index into the current set after advancing,
        // so the next call never starts out of range.
        debug_assert!(
            (self.next_backend as usize) < backends.len(),
            "round-robin cursor must stay within bounds"
        );
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

/// Uniform (optionally weight-biased) random backend selection.
///
/// The RNG is read off the datapath, at construction, instead of reaching for
/// the ambient thread-local `rand::rng()` on the selection hot path — but,
/// unlike [`Rendezvous`]/[`Maglev`], it is NOT seeded from the shared
/// [`DEFAULT_HASH_SEED`] constant. `Rendezvous`/`Maglev` feed their seed into
/// a *pure, stateless* hash function (`hash_backend`): same `(seed, key,
/// addr)` in, same score out, independent of call history, so sharing the
/// seed fleet-wide is exactly the point — it is what makes the same affinity
/// key land on the same backend across workers and restarts.
/// `Random` feeds its seed into a *stateful, advancing* `StdRng`, where the
/// n-th pick depends on the whole call history. Seeding that from a shared
/// compile-time constant would make every worker, on every cold start, with
/// an identically-ordered backend list, draw from a bit-for-bit identical
/// keystream — so every fresh worker's first pick over N equal-weight
/// backends would be the same backend index, fleet-wide, on every restart:
/// exactly the correlated-load event uniform selection exists to prevent,
/// arriving at the worst possible moment (a synchronised redeploy, when every
/// worker's call counter resets together and initial load is otherwise
/// indistinguishable). So `new()` reads a fresh seed from the OS
/// (`SysRng`) once, at construction — off the datapath, satisfying the
/// no-ambient-entropy-on-the-hot-path rule without recreating the old
/// thread-local `rng()`'s cross-process correlation. `with_seed(seed)` stays
/// available for tests and any future deterministic simulator that needs a
/// fixed, reproducible sequence (mirrors `quinn-proto`'s
/// `rng_seed`/`SysRng` construction shape).
///
/// This does NOT make `Random` return a fixed backend: `next_available_backend`
/// *advances* the internal RNG on every call, so a sequence of calls still
/// draws a well-distributed spread of backends. "Random" here means
/// "statistically spread, and — given `with_seed` — reproducible for a fixed
/// seed and exact call sequence" — not "constant". Two instances built with
/// `with_seed` on the same seed and driven through the same sequence of
/// backend sets reproduce the same sequence of picks (what makes this
/// testable); a single instance called repeatedly keeps drawing fresh values
/// from its advancing RNG state, exactly like a real RNG would; two `new()`
/// instances draw from independent OS-seeded keystreams, exactly like the old
/// ambient `rng()` did.
#[derive(Debug)]
pub struct Random {
    rng: StdRng,
}

impl Default for Random {
    fn default() -> Self {
        Self::new()
    }
}

impl Random {
    /// Seed from the OS, once, off the datapath. Deliberately NOT
    /// `DEFAULT_HASH_SEED` — see the struct docs for why sharing a
    /// compile-time constant here would correlate every worker's pick
    /// sequence instead of merely making each one internally reproducible.
    pub fn new() -> Self {
        Self {
            rng: StdRng::try_from_rng(&mut SysRng)
                .expect("failed to seed random number generator from system"),
        }
    }

    /// Deterministic construction for tests / simulation. NOT used for the
    /// production default — see [`Random::new`].
    pub fn with_seed(seed: u64) -> Self {
        Self {
            rng: StdRng::seed_from_u64(seed),
        }
    }
}

impl Random {
    /// Weighted draw over `backends`, identical, RNG draw for RNG draw, to
    /// `rand::distr::weighted::WeightedIndex::<i32>` built over the same
    /// weights, without the two `Vec`s building one allocates (the weights and
    /// their cumulative sums).
    ///
    /// `WeightedIndex::new` accepts a set that is non-empty, holds no negative
    /// weight, sums without `i32` overflow, and sums to more than zero; this
    /// accepts exactly those, and returns `None` otherwise so the caller falls
    /// back to the uniform draw as it did on `new`'s error. On acceptance it
    /// draws once from the same `UniformInt::<i32>` over `[0, total)`, then
    /// returns the first position whose inclusive prefix sum exceeds the draw:
    /// the `partition_point` `WeightedIndex::sample` runs over its cumulative
    /// weights, as a scan. `random_weighted_pick_matches_weighted_index`
    /// holds the equivalence.
    fn weighted_index(&mut self, backends: Candidates<'_>) -> Option<usize> {
        if backends.is_empty() {
            return None;
        }
        let mut total: i32 = 0;
        for backend in backends.iter() {
            let weight = random_weight(&backend.borrow());
            if weight < 0 {
                return None;
            }
            total = total.checked_add(weight)?;
        }
        if total == 0 {
            return None;
        }
        let drawn = UniformInt::<i32>::new(0, total)
            .expect("a positive total is a non-empty range")
            .sample(&mut self.rng);
        debug_assert!(
            (0..total).contains(&drawn),
            "the weighted draw must land inside [0, total)"
        );

        let last = backends.len() - 1;
        let mut prefix: i32 = 0;
        for (index, backend) in backends.iter().take(last).enumerate() {
            prefix += random_weight(&backend.borrow());
            if prefix > drawn {
                return Some(index);
            }
        }
        // Every earlier prefix is at most the draw, and the full sum `total`
        // exceeds it: the draw falls in the last backend's share.
        Some(last)
    }
}

/// The weight `Random` draws with: the configured weight, or `DEFAULT_WEIGHT`
/// when none is set. Unlike [`backend_weight`] it is not clamped, so a zero or
/// negative weight reaches `Random::weighted_index` and is rejected there.
fn random_weight(backend: &Backend) -> i32 {
    backend
        .load_balancing_parameters
        .as_ref()
        .map(|p| p.weight)
        .unwrap_or(DEFAULT_WEIGHT)
}

impl LoadBalancingAlgorithm for Random {
    fn next_available_backend(
        &mut self,
        _key: Option<u64>,
        backends: Candidates<'_>,
    ) -> Option<Rc<RefCell<Backend>>> {
        let len = backends.len();
        if let Some(index) = self.weighted_index(backends) {
            // The weighted draw only returns a position of the view.
            debug_assert!(index < len, "Random sampled an out-of-range index");
            backends.get(index).cloned()
        } else {
            // The weighted draw declines only an empty set, a negative weight,
            // an overflowing sum or an all-zero set; the uniform `choose` then
            // selects iff non-empty.
            let chosen = backends.choose(&mut self.rng).cloned();
            debug_assert_eq!(
                chosen.is_some(),
                len > 0,
                "Random fallback selects iff the set is non-empty"
            );
            chosen
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
        backends: Candidates<'_>,
    ) -> Option<Rc<RefCell<Backend>>> {
        let was_empty = backends.is_empty();
        let opt_b = match self.metric {
            LoadMetric::Connections => backends
                .iter()
                .min_by_key(|backend| backend.borrow().active_connections),
            LoadMetric::Requests => backends
                .iter()
                .min_by_key(|backend| backend.borrow().active_requests),
            LoadMetric::ConnectionTime => {
                let mut b = None;
                for backend in backends.iter() {
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
        // Least-loaded over a non-empty set always finds a minimum; an empty
        // set yields nothing. (`min_by_key` / the manual fold both have this
        // shape.)
        debug_assert_eq!(
            opt_b.is_some(),
            !was_empty,
            "LeastLoaded selects iff the candidate set is non-empty"
        );
        opt_b.cloned()
    }
}

/// Power-of-two-choices (P2C) load-aware selection: sample two candidates,
/// keep the lighter one, and coin-flip a tie.
///
/// # Cost: two load reads per selection, whatever the cluster size
///
/// The two candidates are drawn uniformly at random from the candidate set
/// and only those two are measured, so a selection costs `O(1)` load reads
/// against [`LeastLoaded`]'s `O(n)` scan.
///
/// That is a statement about load READS and nothing more. It is NOT a claim
/// that a selection is `O(1)`: every selection, under every policy, first
/// collects the candidate positions in
/// [`crate::backends::BackendList::next_available_backend_with_key`], which
/// walks the cluster's backend list and records the position of each healthy
/// backend in a buffer the list reuses across selections, before any policy
/// runs. That walk is in the caller, this policy does not remove it, and no
/// policy here is sub-linear per request.
/// So P2C is not the "cheap" alternative to a scan — picking it to shorten a
/// per-request walk that lives somewhere else buys nothing.
///
/// What the two-read bound does buy is real but narrower: under
/// [`LoadMetric::ConnectionTime`] a load read is a `PeakEWMA::observe` call
/// that takes an `Instant::now()` stamp and decays the backend's average, so
/// two of them per selection instead of `n` is measurable work removed —
/// under `Connections`/`Requests` a load read is a field read and the saving
/// is small.
///
/// The reason to choose this policy over [`LeastLoaded`] is balance, not
/// cost: P2C buys most of least-loaded's balance — the classic result is a
/// maximum load of `O(log log n)` where uniform-random gives `O(log n)` —
/// without ever computing a global minimum. Sōzu's workers each select from
/// their own view, and that missing global minimum is what keeps them from
/// herding: no worker can steer toward "the least loaded backend" because no
/// worker ever computes one.
///
/// `power_of_two_touches_exactly_two_backends` pins the read count, and
/// `power_of_two_sample_size_is_two_not_the_whole_set` measures the same
/// property from the selection distribution alone. Both exist because a test
/// that only checks "a backend came back" passes for an `O(n)` scan too.
///
/// # Tie-break: seeded-random, deliberately NOT deterministic-by-id
///
/// Unlike [`Rendezvous`]/[`Maglev`], `PowerOfTwo` carries no affinity `key` —
/// every call is an independent, unlabeled selection event, so there is no
/// "the same key must land on the same backend across workers/restarts"
/// requirement to *preserve* here. But answering "must we preserve
/// agreement?" is not the same question as "does this mechanism *create*
/// agreement?" — see the next section for why that distinction is the whole
/// reason `new()` does not use [`DEFAULT_HASH_SEED`].
///
/// A deterministic tie-break (e.g. "lowest backend id wins") was considered
/// and rejected: ties are common, not rare — every backend starts at zero
/// load, so a cold start, a post-scale-up rebalance, or any tick where two
/// backends carry identical load all resolve through this branch. Always
/// awarding the tie to the same backend (e.g. the lowest id) would bias the
/// distribution toward that backend on exactly the events P2C exists to
/// spread out, reintroducing the herding effect P2C is designed to avoid —
/// on every one of Sōzu's single-threaded workers simultaneously, since they
/// would all observe the same tie and resolve it the same deterministic way.
/// A seeded RNG that *advances* per call keeps ties spread across the
/// candidate set over the process lifetime while still satisfying the
/// no-ambient-entropy-on-the-hot-path rule.
///
/// # Seed source: OS entropy at construction, NOT `DEFAULT_HASH_SEED`
///
/// `Rendezvous`/`Maglev` feed their seed into a *pure, stateless* hash
/// function: sharing [`DEFAULT_HASH_SEED`] fleet-wide is exactly what gives
/// them cross-worker/cross-restart agreement for the same key. `PowerOfTwo`
/// feeds its seed into a *stateful, advancing* `StdRng`, where the n-th
/// tie-break depends on the whole call history. Seeding that from the same
/// shared constant would make every worker, on every cold start, draw from a
/// bit-for-bit identical keystream — so the very first tie any two workers
/// hit after a synchronised redeploy (and ties are the common case here, see
/// above) would resolve identically, fleet-wide, on every restart. That is
/// the deterministic-tie-break herding problem above, reintroduced by the
/// "fix" instead of prevented by it. `new()` therefore reads a fresh seed
/// from the OS (`SysRng`) once, at construction — off the datapath — so each
/// worker's keystream is independent, the same property the old thread-local
/// `rng()` had. `with_seed(seed, metric)` stays available for tests and any
/// future deterministic simulator that needs a fixed, reproducible sequence.
#[derive(Debug)]
pub struct PowerOfTwo {
    pub metric: LoadMetric,
    rng: StdRng,
}

impl PowerOfTwo {
    /// Seed from the OS, once, off the datapath. Deliberately NOT
    /// `DEFAULT_HASH_SEED` — see the struct docs.
    pub fn new(metric: LoadMetric) -> Self {
        Self {
            metric,
            rng: StdRng::try_from_rng(&mut SysRng)
                .expect("failed to seed random number generator from system"),
        }
    }

    /// Deterministic construction for tests / simulation. NOT used for the
    /// production default — see [`PowerOfTwo::new`].
    pub fn with_seed(seed: u64, metric: LoadMetric) -> Self {
        Self {
            metric,
            rng: StdRng::seed_from_u64(seed),
        }
    }

    /// Read ONE backend's load under the configured metric.
    ///
    /// Called exactly twice per selection, whatever the cluster size — that
    /// call count is the algorithm, not an implementation detail of it. The
    /// `ConnectionTime` arm needs `borrow_mut` because `peak_ewma_connection`
    /// decays the EWMA as it reads it.
    fn measure(&self, backend: &Rc<RefCell<Backend>>) -> f64 {
        match self.metric {
            LoadMetric::Connections => backend.borrow().active_connections as f64,
            LoadMetric::Requests => backend.borrow().active_requests as f64,
            LoadMetric::ConnectionTime => backend.borrow_mut().peak_ewma_connection(),
        }
    }
}

impl LoadBalancingAlgorithm for PowerOfTwo {
    fn next_available_backend(
        &mut self,
        _key: Option<u64>,
        backends: Candidates<'_>,
    ) -> Option<Rc<RefCell<Backend>>> {
        let len = backends.len();
        match len {
            0 => return None,
            // A singleton set has no second candidate to compare against, so
            // the sample degenerates to the only backend there is.
            1 => return backends.get(0).cloned(),
            _ => {}
        }

        // Sample two DISTINCT backends uniformly at random. The second index
        // is drawn from the `len - 1` remaining slots and shifted past the
        // first, which is a uniform draw over "every index except `first`"
        // with no rejection loop — so the sampling has no unbounded worst
        // case, and the whole selection reads exactly two backends whatever
        // the cluster size.
        let first = self.rng.random_range(0..len);
        let mut second = self.rng.random_range(0..len - 1);
        if second >= first {
            second += 1;
        }
        debug_assert_ne!(
            first, second,
            "power-of-two must sample two distinct backends"
        );
        debug_assert!(
            second < len,
            "the shifted second index must stay inside the candidate set"
        );

        let first_measure = self.measure(&backends[first]);
        let second_measure = self.measure(&backends[second]);

        // Keep the lighter of the two samples. An exact tie is broken by a
        // coin flip rather than by index order: ties are the common case
        // (every backend starts at zero load), and always awarding them to
        // the lower index would reintroduce the herding P2C exists to avoid
        // — see the struct docs.
        let chosen = if first_measure < second_measure {
            first
        } else if second_measure < first_measure {
            second
        } else if self.rng.random_bool(0.5) {
            first
        } else {
            second
        };
        // Asserted on the already-computed measures on purpose: re-reading a
        // backend under `LoadMetric::ConnectionTime` decays its EWMA, and a
        // `debug_assert!` that mutates would make debug and release builds
        // diverge.
        debug_assert!(
            (chosen == first && first_measure <= second_measure)
                || (chosen == second && second_measure <= first_measure),
            "power-of-two must never keep the strictly heavier of its two samples"
        );

        backends.get(chosen).cloned()
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
        // `unit` must land strictly inside (0, 1) so `ln(unit) < 0` and the
        // score stays a positive, finite number — the whole HRW ordering relies
        // on a well-defined comparable score for every backend.
        debug_assert!(
            unit > 0.0 && unit < 1.0,
            "HRW unit must lie strictly inside (0, 1), got {unit}"
        );
        let score = -weight / unit.ln();
        debug_assert!(
            score.is_finite() && score > 0.0,
            "HRW score must be finite and positive, got {score}"
        );
        score
    }
}

impl LoadBalancingAlgorithm for Rendezvous {
    fn next_available_backend(
        &mut self,
        key: Option<u64>,
        backends: Candidates<'_>,
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
        // A non-empty set always yields a winner, and that winner's score is the
        // maximum over the whole set (HRW = highest random weight).
        debug_assert!(
            best.is_some(),
            "HRW must select a winner from a non-empty set"
        );
        #[cfg(debug_assertions)]
        if let Some((best_score, _)) = best {
            for backend in backends.iter() {
                debug_assert!(
                    best_score >= self.score(key, &backend.borrow()),
                    "HRW winner does not maximize the score over the set"
                );
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
        self.rebuild_from(backends.iter());
    }

    /// [`Maglev::rebuild`] over any ordered backend sequence, so the cold-start
    /// build in `next_available_backend` can read its [`Candidates`] view
    /// without collecting it.
    fn rebuild_from<'a>(
        &mut self,
        backends: impl ExactSizeIterator<Item = &'a Rc<RefCell<Backend>>>,
    ) {
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
            let offset = (h1 % m as u64) as usize;
            let skip = (h2 % (m as u64 - 1)) as usize + 1;
            // The permutation parameters must address valid slots and keep a
            // non-zero stride; `skip ∈ [1, m-1]` is coprime with the prime `m`,
            // which is what guarantees the population loop visits every slot.
            debug_assert!(offset < m, "Maglev offset must be a valid slot");
            debug_assert!(
                (1..m).contains(&skip),
                "Maglev skip must lie in [1, m-1] to stay coprime with the prime table"
            );
            offsets.push(offset);
            skips.push(skip);
            let w = backend_weight(&b) as u64;
            weights.push(w);
            total_weight += w;
        }
        // Every backend contributes weight >= 1, so a non-empty set has a
        // strictly positive total — the proportional target math divides by it.
        debug_assert!(
            total_weight > 0,
            "Maglev total weight must be positive for a non-empty backend set"
        );

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
        // The targets must sum to exactly `m`: this is the termination
        // guarantee for the population loop below — it writes one slot per unit
        // of target budget and stops at `count == m`, so an under/over sum would
        // either leave holes (`usize::MAX` entries) or loop forever.
        debug_assert_eq!(
            assigned, m,
            "Maglev target budget must sum to the table size"
        );
        debug_assert_eq!(
            targets.iter().sum::<usize>(),
            m,
            "Maglev per-backend targets must sum to the table size"
        );

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
        // The loop terminates with every slot claimed exactly once: `count`
        // reached `m`, no slot still holds the `usize::MAX` sentinel, and each
        // backend filled precisely its target budget.
        debug_assert_eq!(count, m, "Maglev population loop must fill exactly m slots");
        debug_assert!(
            table.iter().all(|&slot| slot != usize::MAX),
            "Maglev population loop left an unfilled slot"
        );
        debug_assert!(
            filled == targets,
            "Maglev filled counts must match the per-backend targets"
        );

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
        backends: Candidates<'_>,
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
            self.rebuild_from(backends.iter());
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
            // Both the running slot and the index it stores are in range: the
            // table is `size` long and every entry is a valid `backend_addrs`
            // index (rebuild post-condition), so the resolution below is total.
            debug_assert!(slot < self.size, "Maglev probe slot out of range");
            let idx = self.table[slot];
            debug_assert!(
                idx < self.backend_addrs.len(),
                "Maglev table entry indexes outside the captured address set"
            );
            // Resolve the table's backend index back to an address captured at
            // the last rebuild, then look it up in the live healthy subset.
            if let Some(addr) = self.backend_addrs.get(idx)
                && let Some(backend) = backends.iter().find(|b| b.borrow().address == *addr)
            {
                // The chosen backend is, by construction, a member of the
                // healthy subset we were handed.
                debug_assert!(
                    backends
                        .iter()
                        .any(|b| b.borrow().address == backend.borrow().address),
                    "Maglev must return a backend from the healthy subset"
                );
                return Some(backend.clone());
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
    use std::{
        net::{IpAddr, Ipv4Addr, SocketAddr},
        time::Instant,
    };

    use super::*;
    use crate::{
        PeakEWMA,
        backends::{BackendStatus, HealthState},
        retry::{ExponentialBackoffPolicy, RetryPolicyWrapper},
        sozu_command::proto::command::{LoadBalancingParams, LoadMetric},
    };

    /// Run `policy` over every backend of `backends`, the way a caller whose
    /// whole list is available would.
    fn pick(
        policy: &mut (impl LoadBalancingAlgorithm + ?Sized),
        key: Option<u64>,
        backends: &[Rc<RefCell<Backend>>],
    ) -> Option<Rc<RefCell<Backend>>> {
        let indices: Vec<usize> = (0..backends.len()).collect();
        policy.next_available_backend(key, Candidates::new(backends, &indices))
    }

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

        let backends = vec![
            Rc::new(RefCell::new(create_backend("nolo".to_string(), Some(10)))),
            Rc::new(RefCell::new(create_backend("philo".to_string(), Some(20)))),
            backend_with_least_connection.clone(),
        ];

        let mut least_connection_algorithm = LeastLoaded {
            metric: LoadMetric::Connections,
        };

        let backend_res = pick(&mut least_connection_algorithm, None, &backends).unwrap();
        let backend = backend_res.borrow();

        assert!(*backend == *backend_with_least_connection.borrow());
    }

    #[test]
    fn it_shouldnt_find_backend_with_least_connections_when_list_is_empty() {
        let backends = vec![];

        let mut least_connection_algorithm = LeastLoaded {
            metric: LoadMetric::Connections,
        };

        let backend = pick(&mut least_connection_algorithm, None, &backends);
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
        let backend = pick(&mut roundrobin, None, &backends);
        assert_eq!(backend.as_ref(), backends.get(1));

        backends.remove(1);

        let backend2 = pick(&mut roundrobin, None, &backends);
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
        let backends = make_backends(5);
        let mut hrw = Rendezvous::new();

        let first = pick(&mut hrw, Some(42), &backends).map(|b| chosen_addr(&b));
        for _ in 0..50 {
            let again = pick(&mut hrw, Some(42), &backends).map(|b| chosen_addr(&b));
            assert_eq!(first, again, "HRW must be deterministic for a fixed key");
        }
    }

    #[test]
    fn hrw_none_key_falls_back_to_round_robin() {
        let backends = make_backends(3);
        let mut hrw = Rendezvous::new();

        // With None it should cycle round-robin: addresses in order then wrap.
        let a = chosen_addr(&pick(&mut hrw, None, &backends).unwrap());
        let b = chosen_addr(&pick(&mut hrw, None, &backends).unwrap());
        let c = chosen_addr(&pick(&mut hrw, None, &backends).unwrap());
        let d = chosen_addr(&pick(&mut hrw, None, &backends).unwrap());
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
            let w = chosen_addr(&pick(&mut hrw, Some(key), &backends).unwrap());
            before.insert(key, w);
        }

        // Remove the backend and re-evaluate.
        backends.remove(3);
        for key in 0..2000u64 {
            let after = chosen_addr(&pick(&mut hrw, Some(key), &backends).unwrap());
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
        let backends = make_backends(n);
        let mut hrw = Rendezvous::new();

        let total = 20_000u64;
        let mut counts: std::collections::HashMap<SocketAddr, u64> =
            std::collections::HashMap::new();
        for key in 0..total {
            let w = chosen_addr(&pick(&mut hrw, Some(key), &backends).unwrap());
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

        let sel = backends.clone();
        let first = chosen_addr(&pick(&mut mag, Some(12345), &sel).unwrap());
        for _ in 0..50 {
            let again = chosen_addr(&pick(&mut mag, Some(12345), &sel).unwrap());
            assert_eq!(first, again, "Maglev must be deterministic for a fixed key");
        }
    }

    #[test]
    fn maglev_none_key_falls_back_to_round_robin() {
        let backends = make_backends(3);
        let mut mag = Maglev::new();
        mag.rebuild(&backends);

        let a = chosen_addr(&pick(&mut mag, None, &backends).unwrap());
        let b = chosen_addr(&pick(&mut mag, None, &backends).unwrap());
        let c = chosen_addr(&pick(&mut mag, None, &backends).unwrap());
        let d = chosen_addr(&pick(&mut mag, None, &backends).unwrap());
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
        let sel = backends.clone();
        for key in 0..total {
            let w = chosen_addr(&pick(&mut mag, Some(key), &sel).unwrap());
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
        let sel = backends5.clone();
        for key in 0..total {
            before.insert(key, chosen_addr(&pick(&mut mag, Some(key), &sel).unwrap()));
        }

        // Add a sixth backend and rebuild.
        let mut backends6 = backends5.clone();
        backends6.push(rc(addr_backend("b5", 6, 8005, None)));
        mag.rebuild(&backends6);

        let mut moved = 0u64;
        let sel6 = backends6.clone();
        for key in 0..total {
            let after = chosen_addr(&pick(&mut mag, Some(key), &sel6).unwrap());
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
        let sel_full = full.clone();
        for key in 0..total {
            before.insert(
                key,
                chosen_addr(&pick(&mut mag, Some(key), &sel_full).unwrap()),
            );
        }

        // Now mark backend index 2 unhealthy by passing a REDUCED subset
        // (everything except index 2). This is exactly what
        // `BackendList::next_available_backend_with_key` does when a backend
        // fails its health check / enters retry backoff.
        let unhealthy_addr = chosen_addr(&full[2]);
        let subset: Vec<_> = full
            .iter()
            .filter(|b| chosen_addr(b) != unhealthy_addr)
            .cloned()
            .collect();

        for key in 0..total {
            let after = chosen_addr(&pick(&mut mag, Some(key), &subset).unwrap());
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
        let fresh = vec![
            rc(addr_backend("n0", 50, 9000, None)),
            rc(addr_backend("n1", 51, 9001, None)),
        ];
        let fresh_addrs: Vec<_> = fresh.iter().map(chosen_addr).collect();

        let picked = chosen_addr(&pick(&mut mag, Some(7), &fresh).unwrap());
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
        let backends = make_backends(4);
        let mut mag = Maglev::with_seed_and_size(DEFAULT_HASH_SEED, 1009);
        assert!(mag.table.is_empty(), "table starts empty (cold)");

        let _ = pick(&mut mag, Some(99), &backends).unwrap();
        assert_eq!(mag.table.len(), mag.size, "cold start populated the table");
        let table_after_cold = mag.table.clone();

        // A subsequent selection (still the full set) must NOT rebuild.
        let _ = pick(&mut mag, Some(100), &backends).unwrap();
        assert_eq!(
            mag.table, table_after_cold,
            "selection after cold start must not rebuild"
        );
    }

    #[test]
    fn round_robin_empty_set_returns_none_without_panic() {
        // FIX 2 guard: `% backends.len()` would panic on an empty Vec.
        let empty: Vec<Rc<RefCell<Backend>>> = vec![];
        let mut rr = RoundRobin::new();
        assert!(pick(&mut rr, None, &empty).is_none());

        // Delegators (Rendezvous / Maglev with key == None) route through
        // RoundRobin and must be safe on an empty set too.
        let mut hrw = Rendezvous::new();
        assert!(pick(&mut hrw, None, &empty).is_none());
        let mut mag = Maglev::new();
        assert!(pick(&mut mag, None, &empty).is_none());
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

        let backends = make_backends(3);
        // Building and selecting must not panic.
        mag.rebuild(&backends);
        assert_eq!(mag.table.len(), mag.size);
        let _ = pick(&mut mag, Some(1), &backends).unwrap();
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
        let sel = backends.clone();
        for key in 0..total {
            if chosen_addr(&pick(&mut mag, Some(key), &sel).unwrap()) == heavy_addr {
                heavy += 1;
            }
        }
        // Expected ~80% (400 / 500); allow a margin.
        assert!(
            heavy > total * 70 / 100,
            "weighted Maglev did not favor the heavy backend: {heavy}/{total}"
        );
    }

    // ----- Random / PowerOfTwo: injected, seedable entropy -----

    /// Map a test backend's address back to the index `make_backends` gave it
    /// (`last_octet - 1`), so a selection sequence can be asserted as a
    /// compact `Vec<u8>` instead of a `Vec<SocketAddr>`.
    fn addr_index(addr: SocketAddr) -> u8 {
        match addr.ip() {
            IpAddr::V4(v4) => v4.octets()[3] - 1,
            IpAddr::V6(_) => unreachable!("test backends are always IPv4"),
        }
    }

    #[test]
    fn random_is_deterministic_and_matches_an_expected_sequence() {
        let backends = make_backends(4);
        let mut r1 = Random::with_seed(DEFAULT_HASH_SEED);
        let mut r2 = Random::with_seed(DEFAULT_HASH_SEED);

        let sel1 = backends.clone();
        let sel2 = backends.clone();

        let seq1: Vec<u8> = (0..20)
            .map(|_| addr_index(chosen_addr(&pick(&mut r1, None, &sel1).unwrap())))
            .collect();
        let seq2: Vec<u8> = (0..20)
            .map(|_| addr_index(chosen_addr(&pick(&mut r2, None, &sel2).unwrap())))
            .collect();

        // Reproducibility: same seed + same inputs + same call sequence must
        // give the same picks.
        assert_eq!(
            seq1, seq2,
            "Random::with_seed must reproduce the same sequence for the same seed"
        );

        // Absolute-value check (paired with the determinism check above so a
        // test that is merely "reproducible" but reproducibly wrong still
        // fails): the sequence for DEFAULT_HASH_SEED over 4 uniformly
        // weighted backends, captured from this implementation.
        //
        // Brittleness note: this literal is coupled to `rand` 0.10.2's exact
        // `StdRng` algorithm (ChaCha12) and `seed_from_u64`'s splitmix64
        // expansion. A `rand` version bump that changes either (the type's
        // own docs disclaim portability/reproducibility across versions) will
        // break this assertion for reasons unrelated to Sōzu's load-balancing
        // logic — recapture the sequence, don't "fix" the algorithm.
        let expected: Vec<u8> = vec![1, 2, 1, 1, 0, 2, 1, 3, 1, 0, 3, 1, 0, 0, 3, 2, 1, 3, 3, 1];
        assert_eq!(
            seq1, expected,
            "Random sequence for DEFAULT_HASH_SEED regressed"
        );
    }

    #[test]
    fn random_new_instances_are_not_correlated() {
        // Regression guard for a real defect a review caught in an earlier
        // revision of this change: `Random::new()` must NOT seed from
        // `DEFAULT_HASH_SEED` (or any other shared constant). If it did,
        // every freshly constructed `Random` — i.e. every worker process, on
        // every cold start — would draw from a bit-for-bit identical
        // keystream, so two independently constructed instances would pick
        // the exact same backend at every step. That is precisely the
        // correlated-load event uniform selection exists to prevent, and it
        // would arrive at the worst possible moment: a synchronised
        // redeploy, when every worker's call counter resets together and
        // initial load is otherwise indistinguishable. `Random::new()` reads
        // a fresh seed from the OS per instance, so two instances must
        // diverge — this asserts that property directly, not just that
        // `new()` compiles.
        let backends = make_backends(4);
        let mut r1 = Random::new();
        let mut r2 = Random::new();

        let sel1 = backends.clone();
        let sel2 = backends.clone();

        let seq1: Vec<u8> = (0..32)
            .map(|_| addr_index(chosen_addr(&pick(&mut r1, None, &sel1).unwrap())))
            .collect();
        let seq2: Vec<u8> = (0..32)
            .map(|_| addr_index(chosen_addr(&pick(&mut r2, None, &sel2).unwrap())))
            .collect();

        // Collision probability over 32 draws across 4 backends is
        // astronomically small (~4^-32) if the two instances are genuinely
        // independently seeded, so this is not a flaky assertion in
        // practice.
        assert_ne!(
            seq1, seq2,
            "two Random::new() instances must NOT draw from a shared/correlated keystream"
        );
    }

    #[test]
    fn random_distribution_does_not_collapse() {
        // A seeded RNG must still spread draws across every backend rather
        // than pinning one — determinism is per-(seed, sequence), not
        // "always the same backend".
        //
        // This is a smoke check, not a uniformity test: the +/-35% band is
        // wide enough (roughly ±12 sigma at n=5, total=5000) to reliably
        // catch full collapse (one backend gets ~0 or ~100%) or a gross bias,
        // but a systematic ~20-25% skew would pass it. Tightening the band
        // to something a real skew would fail is a legitimate follow-up; the
        // name deliberately promises no more than "does not collapse".
        let n = 5u8;
        let backends = make_backends(n);
        let mut r = Random::with_seed(DEFAULT_HASH_SEED);
        let sel = backends.clone();

        let total = 5_000u64;
        let mut counts = [0u64; 5];
        for _ in 0..total {
            let idx = addr_index(chosen_addr(&pick(&mut r, None, &sel).unwrap()));
            counts[idx as usize] += 1;
        }

        let expected = total / n as u64;
        for (idx, &c) in counts.iter().enumerate() {
            assert!(
                c > expected * 65 / 100 && c < expected * 135 / 100,
                "Random distribution skewed for backend {idx}: got {c}, expected ~{expected}"
            );
        }
    }

    #[test]
    fn power_of_two_tie_break_is_deterministic_and_matches_an_expected_sequence() {
        // All backends report the same load (0 active connections), so both
        // sampled candidates always tie and every call reaches the coin-flip
        // branch.
        let backends = make_backends(4);
        let mut p1 = PowerOfTwo::with_seed(DEFAULT_HASH_SEED, LoadMetric::Connections);
        let mut p2 = PowerOfTwo::with_seed(DEFAULT_HASH_SEED, LoadMetric::Connections);

        let sel1 = backends.clone();
        let sel2 = backends.clone();

        let seq1: Vec<u8> = (0..20)
            .map(|_| addr_index(chosen_addr(&pick(&mut p1, None, &sel1).unwrap())))
            .collect();
        let seq2: Vec<u8> = (0..20)
            .map(|_| addr_index(chosen_addr(&pick(&mut p2, None, &sel2).unwrap())))
            .collect();

        // Reproducibility: `with_seed` is deterministic given the same seed
        // and call sequence — the property tests/simulation need. This is
        // NOT a claim about production: `PowerOfTwo::new()` seeds from OS
        // entropy specifically so two independent worker processes do NOT
        // draw from the same keystream (see the struct docs' "Seed source"
        // section for why sharing a seed here would be a regression, not a
        // feature).
        assert_eq!(
            seq1, seq2,
            "PowerOfTwo::with_seed must reproduce the same tie-break sequence for the same seed"
        );

        // Absolute-value check: the sequence ranges over all four backends,
        // because each call samples a fresh uniformly random PAIR and the
        // tie then resolves inside that pair. That is the visible signature
        // of random sampling — the O(n) fold this replaced could only ever
        // emit backends 2 and 3, the last two it happened to retain.
        //
        // Brittleness note: this literal is coupled to `rand` 0.10.2's exact
        // `StdRng` algorithm (ChaCha12), to `seed_from_u64`'s splitmix64
        // expansion, AND to the exact order and shape of the draws
        // `PowerOfTwo::next_available_backend` makes. TWO kinds of change
        // move it legitimately, not one: a `rand` version bump that alters
        // either generator (the type's own docs disclaim
        // portability/reproducibility across versions), and a deliberate
        // change to the selection procedure itself. The second is not
        // hypothetical — this literal was recaptured once already, when the
        // two-lightest fold became real power-of-two-choices and the
        // sequence stopped being confined to backends 2 and 3.
        //
        // HOW TO TELL A LEGITIMATE RECAPTURE FROM PAPERING OVER A
        // REGRESSION. Every other `power_of_two_*` test in this module
        // asserts a SEMANTIC property that no RNG stream can shift. There
        // are EIGHT of them — `grep -cE '^\s+fn power_of_two_'` in this file
        // gives nine, this test included — and the rule below covers all
        // eight, not a convenient subset. The pattern is anchored on
        // purpose: unanchored it also counts this very comment, reports
        // ten, and sends the reader after a test that does not exist. The
        // eight are:
        // `power_of_two_new_instances_are_not_correlated`,
        // `power_of_two_tie_break_distribution_does_not_collapse`,
        // `power_of_two_tie_break_is_decided_by_the_coin_flip_not_by_sample_position`,
        // `power_of_two_touches_exactly_two_backends`,
        // `power_of_two_always_returns_the_lighter_of_two_backends`,
        // `power_of_two_handles_empty_and_singleton_sets_without_panic`,
        // `power_of_two_never_returns_the_strictly_heaviest_backend` and
        // `power_of_two_sample_size_is_two_not_the_whole_set`.
        // Recapture only when THIS assertion is the only failing one and all
        // eight are green with their bodies untouched. If any of them is
        // red, or one had to be edited to get green, the algorithm regressed
        // and the new sequence is evidence of it — do not capture it. Check
        // the grep count first: a `power_of_two_*` test added after this
        // comment was written belongs in the rule too.
        let expected: Vec<u8> = vec![1, 3, 1, 1, 1, 2, 1, 2, 0, 1, 2, 3, 1, 3, 3, 3, 0, 2, 0, 2];
        assert_eq!(
            seq1, expected,
            "PowerOfTwo selection sequence for DEFAULT_HASH_SEED regressed"
        );
    }

    #[test]
    fn power_of_two_new_instances_are_not_correlated() {
        // Same regression guard as `random_new_instances_are_not_correlated`,
        // for the tie-break: `PowerOfTwo::new()` must NOT seed from
        // `DEFAULT_HASH_SEED`. Every backend starts at zero load, so a tie
        // (and therefore a coin flip) is the COMMON case, not the rare one —
        // if `new()` shared a seed, every worker's first tie-break after a
        // synchronised redeploy would resolve identically, fleet-wide,
        // reintroducing exactly the herding effect P2C exists to prevent.
        let backends = make_backends(4);
        let mut p1 = PowerOfTwo::new(LoadMetric::Connections);
        let mut p2 = PowerOfTwo::new(LoadMetric::Connections);

        let sel1 = backends.clone();
        let sel2 = backends.clone();

        let seq1: Vec<u8> = (0..48)
            .map(|_| addr_index(chosen_addr(&pick(&mut p1, None, &sel1).unwrap())))
            .collect();
        let seq2: Vec<u8> = (0..48)
            .map(|_| addr_index(chosen_addr(&pick(&mut p2, None, &sel2).unwrap())))
            .collect();

        // Each draw picks one of four backends, so 48 draws give a collision
        // probability well under 2^-48 if the two instances are genuinely
        // independently seeded — not a flaky assertion.
        assert_ne!(
            seq1, seq2,
            "two PowerOfTwo::new() instances must NOT draw from a shared/correlated keystream"
        );
    }

    #[test]
    fn power_of_two_tie_break_distribution_does_not_collapse() {
        // Every backend reports the same load, so the pair sample is uniform
        // and the tie inside it resolves by coin flip: the selection must
        // spread over ALL FOUR backends, not pin one — otherwise the seeded
        // RNG would have traded "ambient nondeterminism" for "deterministic
        // bias", the regression the herding argument in `PowerOfTwo`'s doc
        // comment warns about.
        //
        // This assertion got STRICTER when the O(n) scan became real
        // power-of-two-choices: it used to require backends 0 and 1 to win
        // exactly ZERO times, because the old fold could only ever retain
        // the last two backends it walked. Requiring all four to take a
        // roughly equal share is a stronger statement about the same draws.
        //
        // Smoke check, not a uniformity test: the +/-35% band (~14 sigma at
        // p=0.25, total=5000) reliably catches a collapse onto a subset but
        // would pass a systematic skew — see the equivalent note on
        // `random_distribution_does_not_collapse`.
        let backends = make_backends(4);
        let mut p = PowerOfTwo::with_seed(DEFAULT_HASH_SEED, LoadMetric::Connections);
        let sel = backends.clone();

        let total = 5_000u64;
        let mut counts = [0u64; 4];
        for _ in 0..total {
            let idx = addr_index(chosen_addr(&pick(&mut p, None, &sel).unwrap()));
            counts[idx as usize] += 1;
        }

        // Uniform pair sampling over four equally loaded backends makes
        // every backend equally likely, so each must take roughly a quarter
        // and none may be starved.
        let expected = total / 4;
        for (idx, &c) in counts.iter().enumerate() {
            assert!(
                c > expected * 65 / 100 && c < expected * 135 / 100,
                "PowerOfTwo distribution skewed for backend {idx}: got {c}, expected ~{expected}"
            );
        }
    }

    #[test]
    fn power_of_two_tie_break_is_decided_by_the_coin_flip_not_by_sample_position() {
        // `power_of_two_tie_break_distribution_does_not_collapse` CANNOT see
        // the coin flip, and neither can any other distribution test here.
        // The sampler draws an ordered pair uniformly — `first` over
        // `0..len`, `second` uniformly over the rest — so the two positions
        // are exchangeable, and "always keep `first`" has exactly the same
        // marginal distribution as a fair coin. Replacing
        // `self.rng.random_bool(0.5)` with `true` leaves every other test in
        // this module green except the captured-sequence one, which would
        // only fail through RNG-stream drift — the same way a `rand` upgrade
        // fails it, and whose own comment invites a recapture. A dropped
        // coin flip could ride in behind such a recapture unnoticed.
        //
        // This test makes the tie-break directly observable instead. Under
        // `LoadMetric::ConnectionTime` each measurement runs
        // `PeakEWMA::observe`, which stamps `last_event = Instant::now()`,
        // and `next_available_backend` measures its samples in order: the
        // backend it drew as `first` carries the EARLIER stamp. So the pair's
        // order is readable from outside, and "which member of a tie won" is
        // decidable.
        //
        // The decay has to be frozen for the tie to exist at all. `observe`
        // ages `rtt` by `exp(-elapsed / decay)`, so at the default 1s decay
        // the backend measured second has aged longer, comes back strictly
        // lighter, and the coin-flip branch is never reached. With a decay
        // this large the weight rounds to exactly 1.0 and `rtt` survives
        // bit-for-bit, so both measures tie exactly while `observe` still
        // stamps `last_event`.
        const TOTAL: usize = 2_000;
        let backends = make_backends(2);
        for backend in &backends {
            backend.borrow_mut().connection_time.decay = 1e300;
        }

        let mut p = PowerOfTwo::with_seed(DEFAULT_HASH_SEED, LoadMetric::ConnectionTime);
        let sel = backends.clone();
        let mut decided = 0usize;
        let mut second_measured_won = 0usize;
        for _ in 0..TOTAL {
            let picked = addr_index(chosen_addr(&pick(&mut p, None, &sel).unwrap()));
            let stamp_0 = backends[0].borrow().connection_time.last_event;
            let stamp_1 = backends[1].borrow().connection_time.last_event;
            // Two reads that the monotonic clock could not separate leave the
            // pair's order unknowable, so the sample is dropped rather than
            // guessed. `decided` is asserted below so dropping them all can
            // never be mistaken for a pass.
            if stamp_0 == stamp_1 {
                continue;
            }
            decided += 1;
            let measured_second = u8::from(stamp_0 < stamp_1);
            if picked == measured_second {
                second_measured_won += 1;
            }
        }

        assert!(
            decided >= TOTAL / 2,
            "only {decided} of {TOTAL} selections had distinguishable measurement stamps; the \
             clock is too coarse to decide this test rather than the tie-break being wrong"
        );

        // A fair coin gives the second-measured backend half the ties. The
        // band is +/-20 percentage points around 50%, so its half-width is
        // `0.2 * decided` against a standard deviation of
        // `sqrt(decided * 0.25)`. Quote it at the FLOOR the guard above
        // permits, which is the only bound that has to hold: at
        // decided = 1000 that is 200 against sigma 15.8, ~12.6 sigma (~17.9
        // sigma at the full 2000). It cannot flake, and it is two-sided:
        // pinning the tie-break to `first` drives this to 0, pinning it to
        // `second` drives it to `decided`.
        let low = decided * 30 / 100;
        let high = decided * 70 / 100;
        assert!(
            second_measured_won > low && second_measured_won < high,
            "power-of-two resolved {second_measured_won} of {decided} exact ties in favour of \
             the second-measured sample; a coin flip must land near {}, and a count at either \
             end means the tie is being awarded by sample position instead",
            decided / 2
        );
    }

    // ----- PowerOfTwo: the sampling itself, not just the outcome -----

    #[test]
    fn power_of_two_touches_exactly_two_backends() {
        // The two-load-reads claim, MEASURED rather than asserted
        // structurally. It is a claim about how many backends the POLICY
        // reads, not about the cost of a selection: the caller has already
        // walked every backend in `BackendList::next_available_backend_with_key`
        // to build the candidate set handed in here, so a selection is `O(n)` whatever
        // this policy does. `LoadMetric::ConnectionTime` reads a backend's load
        // through `Backend::peak_ewma_connection` -> `PeakEWMA::get` ->
        // `PeakEWMA::observe`, and `observe` stamps `last_event =
        // Instant::now()`. That stamp is an exact per-backend receipt saying
        // "this backend's load was measured", so the number of fresh stamps
        // after one selection IS the number of backends the algorithm
        // consulted: a full scan leaves `n` of them, power-of-two-choices
        // leaves exactly 2.
        //
        // This is the assertion a "did a backend come back?" test cannot
        // make: the scan-then-coin-flip shape this replaced also returned a
        // backend, it just read all 64 first.
        const N: u8 = 64;
        let backends = make_backends(N);

        // Stamp every backend with one instant, then spin until the
        // monotonic clock is strictly past it. After that point every
        // `Instant::now()` is `> before`, so "was this backend measured?"
        // is decidable without depending on the clock's resolution.
        let before = Instant::now();
        for backend in &backends {
            backend.borrow_mut().connection_time.last_event = before;
        }
        while Instant::now() <= before {
            std::hint::spin_loop();
        }

        let mut p = PowerOfTwo::with_seed(DEFAULT_HASH_SEED, LoadMetric::ConnectionTime);
        assert!(pick(&mut p, None, &backends).is_some());

        let touched = backends
            .iter()
            .filter(|backend| backend.borrow().connection_time.last_event > before)
            .count();
        assert_eq!(
            touched, 2,
            "power-of-two must measure exactly 2 of the {N} backends; measuring {touched} means \
             the policy reads every backend's load, which the name exists to rule out"
        );
    }

    #[test]
    fn power_of_two_always_returns_the_lighter_of_two_backends() {
        // With exactly two backends the sample IS the whole set, so P2C is
        // fully determined and needs no statistics: the lighter backend must
        // win every single call. The scan-then-coin-flip shape this replaced
        // computed both measures, asserted their order, and then discarded
        // that order for a coin flip — returning the HEAVIER backend about
        // half the time.
        let backends = make_backends(2);
        backends[0].borrow_mut().active_connections = 7;
        backends[1].borrow_mut().active_connections = 1;

        let mut p = PowerOfTwo::with_seed(DEFAULT_HASH_SEED, LoadMetric::Connections);
        let sel = backends.clone();
        for call in 0..200 {
            let picked = addr_index(chosen_addr(&pick(&mut p, None, &sel).unwrap()));
            assert_eq!(
                picked, 1,
                "power-of-two must keep the lighter of the two sampled backends (call {call} \
                 picked backend {picked}, which carries 7 connections against 1)"
            );
        }
    }

    #[test]
    fn power_of_two_handles_empty_and_singleton_sets_without_panic() {
        // Sampling a second DISTINCT index draws from `len - 1` slots, which
        // is an empty range for a one-backend set — `random_range` panics on
        // an empty range, so the singleton case must short-circuit before the
        // draw. A cluster scaled down to one backend is ordinary, not exotic.
        let mut p = PowerOfTwo::with_seed(DEFAULT_HASH_SEED, LoadMetric::Connections);

        let empty: Vec<Rc<RefCell<Backend>>> = vec![];
        assert!(
            pick(&mut p, None, &empty).is_none(),
            "power-of-two selects nothing from an empty candidate set"
        );

        let single = make_backends(1);
        for _ in 0..10 {
            assert_eq!(
                addr_index(chosen_addr(&pick(&mut p, None, &single).unwrap())),
                0,
                "power-of-two returns the only backend of a singleton set"
            );
        }
    }

    #[test]
    fn power_of_two_never_returns_the_strictly_heaviest_backend() {
        // Regression guard for the load profile that broke the O(n) fold
        // this replaced. With three strictly ordered loads the heaviest
        // backend loses EVERY pairing it can be drawn into, so P2C can never
        // return it — a deterministic assertion over any number of calls.
        //
        // The previous implementation walked every backend keeping a
        // `(first, second)` pair, and its "otherwise" branch fired both when
        // the new measure was lighter than `first` AND when it was heavier
        // than `second`, evicting both candidates in the second case. On
        // loads [0, 1, 5] that left `first = 5` and `second = 0`, tripping
        // its own `first <= second` invariant (a debug-build panic) and, in
        // a release build, coin-flipping the HEAVIEST backend into the
        // result half the time.
        let backends = make_backends(3);
        backends[0].borrow_mut().active_connections = 0;
        backends[1].borrow_mut().active_connections = 1;
        backends[2].borrow_mut().active_connections = 5;

        let mut p = PowerOfTwo::with_seed(DEFAULT_HASH_SEED, LoadMetric::Connections);
        let sel = backends.clone();
        for call in 0..2_000 {
            let picked = addr_index(chosen_addr(&pick(&mut p, None, &sel).unwrap()));
            assert_ne!(
                picked, 2,
                "power-of-two returned the strictly heaviest backend on call {call}: it is \
                 heavier than either backend it can be paired against"
            );
        }
    }

    #[test]
    fn power_of_two_sample_size_is_two_not_the_whole_set() {
        // Same two-load-reads property as `power_of_two_touches_exactly_two_backends`,
        // measured from the OUTSIDE — through the selection distribution
        // alone, with no access to a backend's internals.
        //
        // Give one backend a uniquely light load among `N`. A policy that
        // samples `k` backends uniformly and keeps the lightest returns that
        // backend with probability exactly `k / N`, so the observed hit rate
        // MEASURES the sample size: `k = hit_rate * N`. Power-of-two-choices
        // gives `k = 2`.
        //
        // The fold this replaced scores worse here than ANY sample size.
        // Restored under this exact test it gives `k_est = 0.00`: the unique
        // minimum came back 0 times out of 100_000. It was `first` after
        // backend 0, but from backend 2 on every remaining measure tied
        // `second`, so the fold's "otherwise" branch fired on every step,
        // shifting `first` into `second` and evicting the minimum for good.
        // What it returned was a coin flip between the last two backends it
        // happened to walk.
        const N: u8 = 100;
        let backends = make_backends(N);
        for backend in backends.iter().skip(1) {
            backend.borrow_mut().active_connections = 1_000;
        }
        // `backends[0]` keeps 0 active connections: the unique minimum.

        let mut p = PowerOfTwo::with_seed(DEFAULT_HASH_SEED, LoadMetric::Connections);
        let sel = backends.clone();
        let total = 100_000u32;
        let mut hits = 0u32;
        for _ in 0..total {
            if addr_index(chosen_addr(&pick(&mut p, None, &sel).unwrap())) == 0 {
                hits += 1;
            }
        }

        // At k = 2, N = 100 and total = 100_000 the standard deviation of
        // `k_est` is `N * sqrt(p * (1 - p) / total)` ~ 0.044, so the
        // [1.5, 2.5] band is a ~11-sigma envelope: wide enough never to
        // flake, narrow enough to exclude k = 1 (plain random), k = 3, and
        // the k_est = 0.00 the old fold produced.
        let k_est = f64::from(hits) / f64::from(total) * f64::from(N);
        assert!(
            (1.5..=2.5).contains(&k_est),
            "power-of-two consulted ~{k_est:.2} of {N} backends (unique minimum returned \
             {hits}/{total} times); the algorithm must sample exactly 2"
        );
    }

    /// The weighted draw `Random` made before it stopped allocating: collect
    /// the weights, build a `WeightedIndex`, and fall back to a uniform
    /// `choose` when `WeightedIndex::new` rejects them.
    fn weighted_index_reference(rng: &mut StdRng, weights: &[i32]) -> Option<usize> {
        use rand::distr::{Distribution, weighted::WeightedIndex};
        match WeightedIndex::new(weights.to_vec()) {
            Ok(distribution) => Some(distribution.sample(rng)),
            Err(_) => {
                let positions: Vec<usize> = (0..weights.len()).collect();
                positions.choose(rng).copied()
            }
        }
    }

    #[test]
    fn random_weighted_pick_matches_weighted_index() {
        // Weight sets covering every branch of `WeightedIndex::new`: valid,
        // zero weights among positive ones, all zero, a negative weight
        // (first and later), an `i32` overflow, a singleton and the empty set.
        let weight_sets: &[&[i32]] = &[
            &[100, 100, 100],
            &[1, 2, 3, 4, 5, 6, 7],
            &[0, 5, 0, 0, 9, 0],
            &[7, 0, 0],
            &[0, 0, 0, 3],
            &[0, 0, 0],
            &[-1, 5, 5],
            &[5, 5, -1, 5],
            &[i32::MAX, 1],
            &[i32::MAX / 2, i32::MAX / 2, 1],
            &[i32::MAX / 2, i32::MAX / 2, 2],
            &[42],
            &[],
        ];
        for (set, weights) in weight_sets.iter().enumerate() {
            let backends: Vec<_> = weights
                .iter()
                .enumerate()
                .map(|(index, &weight)| rc(addr_backend("w", index as u8 + 1, 80, Some(weight))))
                .collect();
            for seed in 0..64 {
                let mut policy = Random::with_seed(seed);
                let mut reference = StdRng::seed_from_u64(seed);
                for draw in 0..32 {
                    let expected = weighted_index_reference(&mut reference, weights);
                    let got = pick(&mut policy, None, &backends).map(|backend| {
                        backends
                            .iter()
                            .position(|candidate| Rc::ptr_eq(candidate, &backend))
                            .expect("the pick is one of the candidates")
                    });
                    assert_eq!(
                        got, expected,
                        "weight set {set} {weights:?}, seed {seed}, draw {draw}"
                    );
                }
            }
        }
    }
}
