//! Backoff after a failed backend connection.
//!
//! Nothing here reads a clock or draws from an ambient RNG: every method that
//! depends on time takes `now`, and [`RetryPolicy::fail`] draws its jitter from
//! the RNG it is lent. The worker passes the pass's clock sample and the
//! backend map's RNG; a simulator passes its own, so the backoff windows a
//! seed produces are a pure function of that seed and of the instants it
//! supplies (#1684).

use std::{
    cmp,
    fmt::Debug,
    time::{Duration, Instant},
};

use rand::{Rng, RngExt};

#[derive(Debug, PartialEq, Eq)]
pub enum RetryAction {
    OKAY,
    WAIT,
}

pub trait RetryPolicy: Debug + PartialEq + Eq {
    fn max_tries(&self) -> usize;
    fn current_tries(&self) -> usize;

    /// Record a failed attempt at `now`, drawing the backoff jitter from `rng`.
    fn fail<R: Rng + ?Sized>(&mut self, now: Instant, rng: &mut R);
    /// Record a successful attempt at `now`.
    fn succeed(&mut self, now: Instant);

    /// Whether an attempt may be made at `now`.
    fn can_try(&self, _now: Instant) -> Option<RetryAction> {
        if self.current_tries() >= self.max_tries() {
            None
        } else {
            Some(RetryAction::OKAY)
        }
    }

    fn is_down(&self) -> bool;
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum RetryPolicyWrapper {
    ExponentialBackoff(ExponentialBackoffPolicy),
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct ExponentialBackoffPolicy {
    max_tries: usize,
    current_tries: usize,
    /// When the last attempt was recorded; `None` until the first one.
    ///
    /// `wait` is zero whenever this is `None`, so no instant is needed at
    /// construction: a policy that has never failed can always be tried.
    last_try: Option<Instant>,
    wait: Duration,
}

impl ExponentialBackoffPolicy {
    pub fn new(max_tries: usize) -> Self {
        ExponentialBackoffPolicy {
            max_tries,
            current_tries: 0,
            last_try: None,
            wait: Duration::ZERO,
        }
    }

    /// Time since the last recorded attempt, as seen at `now`.
    ///
    /// Saturates at zero, so an injected `now` earlier than the last attempt
    /// reads as "no time has passed" rather than panicking. With no attempt
    /// recorded, `wait` is zero and any value compares the same way.
    fn since_last_try(&self, now: Instant) -> Duration {
        self.last_try.map_or(Duration::ZERO, |last_try| {
            now.saturating_duration_since(last_try)
        })
    }
}

impl RetryPolicy for ExponentialBackoffPolicy {
    fn max_tries(&self) -> usize {
        self.max_tries
    }

    fn current_tries(&self) -> usize {
        self.current_tries
    }

    fn fail<R: Rng + ?Sized>(&mut self, now: Instant, rng: &mut R) {
        if self.since_last_try(now).lt(&self.wait) {
            //we're already in back off
            return;
        }

        let max_secs = cmp::max(
            1,
            1u64.checked_shl(self.current_tries as u32)
                .unwrap_or(u64::MAX),
        );
        let wait = if max_secs == 1 {
            1
        } else {
            rng.random_range(1..max_secs)
        };

        self.wait = Duration::from_secs(wait);
        self.last_try = Some(now);
        self.current_tries = cmp::min(self.current_tries + 1, self.max_tries);
    }

    fn succeed(&mut self, now: Instant) {
        self.wait = Duration::default();
        self.last_try = Some(now);
        self.current_tries = 0;
    }

    fn can_try(&self, now: Instant) -> Option<RetryAction> {
        let action = if self.since_last_try(now).ge(&self.wait) {
            RetryAction::OKAY
        } else {
            RetryAction::WAIT
        };

        Some(action)
    }

    fn is_down(&self) -> bool {
        self.current_tries() >= self.max_tries()
    }
}

#[cfg(test)]
impl ExponentialBackoffPolicy {
    /// Test-only helper that drives the policy directly to the
    /// "exhausted-budget" state. The natural path requires `max_tries`
    /// successive `fail()` calls separated by the exponential-backoff
    /// wait window (up to 32+ s with the default `max_tries = 6`), which
    /// is not practical inside a unit test. This helper forces
    /// `current_tries == max_tries`, so `is_down()` returns true and
    /// `Backend::is_available()` flips off without sleeping.
    pub(crate) fn force_down(&mut self) {
        self.current_tries = self.max_tries;
    }
}

#[cfg(test)]
impl RetryPolicyWrapper {
    /// Test-only forwarder for [`ExponentialBackoffPolicy::force_down`].
    pub(crate) fn force_down(&mut self) {
        match self {
            RetryPolicyWrapper::ExponentialBackoff(p) => p.force_down(),
        }
    }
}

impl From<ExponentialBackoffPolicy> for RetryPolicyWrapper {
    fn from(val: ExponentialBackoffPolicy) -> Self {
        RetryPolicyWrapper::ExponentialBackoff(val)
    }
}

impl RetryPolicy for RetryPolicyWrapper {
    fn max_tries(&self) -> usize {
        match *self {
            RetryPolicyWrapper::ExponentialBackoff(ref policy) => policy,
        }
        .max_tries()
    }

    fn current_tries(&self) -> usize {
        match *self {
            RetryPolicyWrapper::ExponentialBackoff(ref policy) => policy,
        }
        .current_tries()
    }

    fn fail<R: Rng + ?Sized>(&mut self, now: Instant, rng: &mut R) {
        match *self {
            RetryPolicyWrapper::ExponentialBackoff(ref mut policy) => policy,
        }
        .fail(now, rng)
    }

    fn succeed(&mut self, now: Instant) {
        match *self {
            RetryPolicyWrapper::ExponentialBackoff(ref mut policy) => policy,
        }
        .succeed(now)
    }

    fn can_try(&self, now: Instant) -> Option<RetryAction> {
        match *self {
            RetryPolicyWrapper::ExponentialBackoff(ref policy) => policy,
        }
        .can_try(now)
    }

    fn is_down(&self) -> bool {
        match *self {
            RetryPolicyWrapper::ExponentialBackoff(ref policy) => policy,
        }
        .is_down()
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use rand::{SeedableRng, rngs::StdRng};

    use super::{ExponentialBackoffPolicy, RetryAction, RetryPolicy};

    const MAX_FAILS: usize = 10;

    fn rng() -> StdRng {
        StdRng::seed_from_u64(0x5eed)
    }

    #[test]
    fn no_fail() {
        let policy = ExponentialBackoffPolicy::new(MAX_FAILS);
        let can_try = policy.can_try(Instant::now());

        assert_eq!(Some(RetryAction::OKAY), can_try)
    }

    #[test]
    fn single_fail() {
        let t0 = Instant::now();
        let mut policy = ExponentialBackoffPolicy::new(MAX_FAILS);
        policy.fail(t0, &mut rng());
        let can_try = policy.can_try(t0);

        // The wait is >= 1s, and no time has passed on the injected clock.
        assert_eq!(Some(RetryAction::WAIT), can_try)
    }

    #[test]
    fn max_fails() {
        let t0 = Instant::now();
        let mut rng = rng();
        let mut policy = ExponentialBackoffPolicy::new(MAX_FAILS);

        for _ in 0..MAX_FAILS {
            policy.fail(t0, &mut rng);
        }

        let can_try = policy.can_try(t0);

        assert_eq!(Some(RetryAction::WAIT), can_try)
    }

    #[test]
    fn recover_from_fail() {
        let t0 = Instant::now();
        let mut rng = rng();
        let mut policy = ExponentialBackoffPolicy::new(MAX_FAILS);

        // Stop just before total failure
        for _ in 0..(MAX_FAILS - 1) {
            policy.fail(t0, &mut rng);
        }

        policy.succeed(t0);
        policy.fail(t0, &mut rng);
        policy.fail(t0, &mut rng);
        policy.fail(t0, &mut rng);

        let can_try = policy.can_try(t0);

        assert_eq!(Some(RetryAction::WAIT), can_try)
    }

    /// #1684: the backoff window is measured on the clock the caller passes,
    /// not on the wall clock.
    ///
    /// The first failure always waits exactly one second (`max_secs == 1`,
    /// no jitter), so the window closes at `t0 + 1s` on the injected clock
    /// while almost no real time has passed.
    ///
    /// TO SEE THIS RED: measure `since_last_try` from `Instant::now()`
    /// instead of `now`; `can_try(t0 + 1s)` then still answers `WAIT`.
    #[test]
    fn the_backoff_window_follows_the_injected_clock() {
        let t0 = Instant::now();
        let mut policy = ExponentialBackoffPolicy::new(MAX_FAILS);
        policy.fail(t0, &mut rng());

        let one_second = Duration::from_secs(1);
        assert_eq!(
            policy.can_try(t0 + one_second - Duration::from_nanos(1)),
            Some(RetryAction::WAIT),
            "one nanosecond before the window closes, the policy must wait"
        );
        assert_eq!(
            policy.can_try(t0 + one_second),
            Some(RetryAction::OKAY),
            "the window closes at t0 + 1s on the injected clock"
        );
    }

    /// Every backoff window a policy chooses, failing again the instant the
    /// previous window closes, until the budget is spent.
    fn windows(seed: u64) -> Vec<Duration> {
        let mut rng = StdRng::seed_from_u64(seed);
        let mut policy = ExponentialBackoffPolicy::new(MAX_FAILS);
        let mut now = Instant::now();
        let mut windows = Vec::with_capacity(MAX_FAILS);
        while !policy.is_down() {
            policy.fail(now, &mut rng);
            windows.push(policy.wait);
            now += policy.wait;
        }
        windows
    }

    /// #1684: the jitter of every backoff window is drawn from the RNG the
    /// caller lends, so one seed always yields one sequence of windows.
    ///
    /// TO SEE THIS RED: draw the jitter in `fail` from `rand::rng()` and
    /// ignore the argument; two runs of one seed then disagree.
    #[test]
    fn one_seed_yields_one_sequence_of_backoff_windows() {
        let first = windows(7);
        assert_eq!(first.len(), MAX_FAILS);
        assert_eq!(first, windows(7), "one seed must yield one sequence");
        assert_ne!(
            first,
            windows(8),
            "the jitter must come from the lent RNG, so another seed differs"
        );
    }
}
