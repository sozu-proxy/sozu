use rand::{self, Rng};

use std::{cmp, time};
use std::fmt::Debug;

#[derive(Debug, PartialEq, Eq)]
pub enum RetryAction {
    OKAY,
    WAIT
}

pub trait RetryPolicy: Debug + PartialEq + Eq {
    fn max_tries(&self) -> usize;
    fn current_tries(&self) -> usize;

    fn fail(&mut self);
    fn succeed(&mut self);

    fn can_try(&self) -> Option<RetryAction> {
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
    ExponentialBackoff(ExponentialBackoffPolicy)
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct ExponentialBackoffPolicy {
    max_tries: usize,
    current_tries: usize,
    last_try: time::Instant,
    wait: time::Duration
}

impl ExponentialBackoffPolicy {
    pub fn new(max_tries: usize) -> Self {
        ExponentialBackoffPolicy {
            max_tries,
            current_tries: 0,
            last_try: time::Instant::now(),
            wait: time::Duration::default()
        }
    }
}

impl RetryPolicy for ExponentialBackoffPolicy {
    fn max_tries(&self) -> usize {
        self.max_tries
    }

    fn current_tries(&self) -> usize {
        self.current_tries
    }

    fn fail(&mut self) {
        if self.last_try.elapsed().lt(&self.wait) {
          //we're already in back off
          return;
        }

        let max_secs = cmp::max(1, 1u64.wrapping_shl(self.current_tries as u32));
        let wait = if max_secs == 1 {
            1
        } else {
            let mut rng = rand::thread_rng();
            rng.gen_range(1, max_secs)
        };

        self.wait = time::Duration::from_secs(wait);
        self.last_try = time::Instant::now();
        self.current_tries = cmp::min(self.current_tries + 1, self.max_tries);

    }

    fn succeed(&mut self) {
        self.wait = time::Duration::default();
        self.last_try = time::Instant::now();
        self.current_tries = 0;
    }

    fn can_try(&self) -> Option<RetryAction> {

        let action = if self.last_try.elapsed().gt(&self.wait) {
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

impl Into<RetryPolicyWrapper> for ExponentialBackoffPolicy {
    fn into(self) -> RetryPolicyWrapper {
        RetryPolicyWrapper::ExponentialBackoff(self)
    }
}

impl RetryPolicy for RetryPolicyWrapper {
    fn max_tries(&self) -> usize {
        match *self {
            RetryPolicyWrapper::ExponentialBackoff(ref policy) => policy
        }.max_tries()
    }

    fn current_tries(&self) -> usize {
        match *self {
            RetryPolicyWrapper::ExponentialBackoff(ref policy) => policy
        }.current_tries()
    }

    fn fail(&mut self) {
        match *self {
            RetryPolicyWrapper::ExponentialBackoff(ref mut policy) => policy
        }.fail()
    }

    fn succeed(&mut self) {
        match *self {
            RetryPolicyWrapper::ExponentialBackoff(ref mut policy) => policy
        }.succeed()
    }

    fn can_try(&self) -> Option<RetryAction> {
        match *self {
            RetryPolicyWrapper::ExponentialBackoff(ref policy) => policy
        }.can_try()
    }

    fn is_down(&self) -> bool {
        match *self {
            RetryPolicyWrapper::ExponentialBackoff(ref policy) => policy
        }.is_down()
    }
}

#[cfg(test)]
mod tests {
    use super::{RetryAction, RetryPolicy, ExponentialBackoffPolicy};

    const MAX_FAILS: usize = 10;

    #[test]
    fn no_fail() {
        let policy = ExponentialBackoffPolicy::new(MAX_FAILS);
        let can_try = policy.can_try();

        assert_eq!(Some(RetryAction::OKAY), can_try)
    }

    #[test]
    fn single_fail() {
        let mut policy = ExponentialBackoffPolicy::new(MAX_FAILS);
        policy.fail();
        let can_try = policy.can_try();

        // The wait will be >= 1s, so we'll be WAIT by the time we do the assert
        assert_eq!(Some(RetryAction::WAIT), can_try)
    }

    #[test]
    fn max_fails() {
        let mut policy = ExponentialBackoffPolicy::new(MAX_FAILS);

        for _ in 0..MAX_FAILS {
            policy.fail();
        }

        let can_try = policy.can_try();

        assert_eq!(Some(RetryAction::WAIT), can_try)
    }

    #[test]
    fn recover_from_fail() {
        let mut policy = ExponentialBackoffPolicy::new(MAX_FAILS);

        // Stop just before total failure
        for _ in 0..(MAX_FAILS - 1) {
            policy.fail();
        }

        policy.succeed();
        policy.fail();
        policy.fail();
        policy.fail();

        let can_try = policy.can_try();

        assert_eq!(Some(RetryAction::WAIT), can_try)
    }
}
