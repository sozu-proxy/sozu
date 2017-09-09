use rand;
use rand::distributions::{IndependentSample, Range};

use std::{cmp, thread, time};
use std::fmt::Debug;

#[derive(Debug, PartialEq, Eq)]
pub enum RetryAction {
    OKAY,
    WAIT
}

pub trait RetryPolicy : Debug + PartialEq + Eq {
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
}

#[derive(Debug, PartialEq, Eq)]
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
        {
            let max_secs = cmp::max(1, 1 << self.current_tries);

            let mut rng = rand::thread_rng();
            let range = Range::new(1, max_secs);
            let wait = range.ind_sample(&mut rng);

            self.wait = time::Duration::from_secs(wait);
        }

        self.last_try = time::Instant::now();
        self.current_tries += 1;
    }

    fn succeed(&mut self) {
        self.wait = time::Duration::default();
        self.last_try = time::Instant::now();
        self.current_tries = 0;
    }

    fn can_try(&self) -> Option<RetryAction> {
        if self.current_tries() >= self.max_tries() {
            return None;
        }

        let action = if self.last_try.elapsed().gt(&self.wait) {
            RetryAction::OKAY
        } else {
            RetryAction::WAIT
        };

        Some(action)
    }
}