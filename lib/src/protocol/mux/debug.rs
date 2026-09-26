//! Debug history ring buffer and event enum.
//!
//! Used by the mux layer to record a bounded trail of per-session events for
//! post-mortem inspection when `debug_assertions` are enabled. In release
//! builds `push` and `set_interesting` are no-ops, so the ring is allocated
//! lazily on the first `push` and a release session never allocates it.

use std::collections::VecDeque;

use kawa::ParsingPhase;
use mio::Token;
use sozu_command::ready::Ready;

use super::{BackendConnectionError, MuxResult, Readiness, StreamState};

/// Maximum number of debug events retained in the ring buffer.
/// Oldest entries are dropped when this limit is reached.
pub(super) const DEBUG_HISTORY_CAPACITY: usize = 512;

#[derive(Default)]
pub struct DebugHistory {
    /// Starts empty and unallocated: only a debug-build `push` ever records,
    /// and it reserves the whole ring on first use.
    pub events: VecDeque<DebugEvent>,
    pub is_interesting: bool,
}
impl DebugHistory {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn push(&mut self, _event: DebugEvent) {
        #[cfg(debug_assertions)]
        {
            if self.events.capacity() == 0 {
                self.events.reserve_exact(DEBUG_HISTORY_CAPACITY);
            }
            if self.events.len() >= DEBUG_HISTORY_CAPACITY {
                self.events.pop_front();
            }
            self.events.push_back(_event);
        }
    }
    pub fn set_interesting(&mut self, _interesting: bool) {
        #[cfg(debug_assertions)]
        {
            self.is_interesting = _interesting;
        }
    }
    pub fn is_interesting(&self) -> bool {
        #[cfg(debug_assertions)]
        {
            self.is_interesting
        }
        #[cfg(not(debug_assertions))]
        {
            false
        }
    }
}

#[derive(Debug)]
pub enum DebugEvent {
    EV(Token, Ready),
    ReadyTimestamp(usize),
    LoopStart,
    LoopIteration(i32),
    SR(Token, MuxResult, Readiness),
    SW(Token, MuxResult, Readiness),
    CW(Token, MuxResult, Readiness),
    CR(Token, MuxResult, Readiness),
    CC(usize, StreamState),
    CCS(Token, String),
    CCF(usize, BackendConnectionError),
    CH(Token, Readiness),
    S(u32, usize, ParsingPhase, usize, usize),
    Str(String),
    StreamEvent(usize, usize),
    SocketIO(usize, usize, usize),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_allocations::allocations;

    /// Every session builds one `DebugHistory` through `Context::new`; release
    /// builds never record into it, so building one must not allocate the
    /// ring ([#1585](https://github.com/sozu-proxy/sozu/issues/1585)).
    #[test]
    fn creating_a_debug_history_allocates_nothing() {
        let before = allocations();
        let history = DebugHistory::new();
        let after = allocations();
        assert_eq!(after - before, 0, "DebugHistory::new must not allocate");
        assert_eq!(history.events.capacity(), 0);
    }

    /// Debug builds keep the full ring: one allocation of the whole capacity
    /// on the first push, then oldest-first eviction at the bound.
    #[cfg(debug_assertions)]
    #[test]
    fn debug_history_keeps_the_newest_events_up_to_its_capacity() {
        let mut history = DebugHistory::new();
        let before = allocations();
        for i in 0..=DEBUG_HISTORY_CAPACITY as i32 {
            history.push(DebugEvent::LoopIteration(i));
        }
        let after = allocations();
        assert_eq!(
            after - before,
            1,
            "the ring is allocated once, at full capacity"
        );
        assert!(history.events.capacity() >= DEBUG_HISTORY_CAPACITY);
        assert_eq!(history.events.len(), DEBUG_HISTORY_CAPACITY);
        assert!(matches!(
            history.events.front(),
            Some(DebugEvent::LoopIteration(1))
        ));
        assert!(matches!(
            history.events.back(),
            Some(DebugEvent::LoopIteration(i)) if *i == DEBUG_HISTORY_CAPACITY as i32
        ));
        history.set_interesting(true);
        assert!(history.is_interesting());
        let dump = format!("{:?}", history.events);
        assert!(dump.starts_with("[LoopIteration(1), "), "{dump}");
    }

    /// Release builds record nothing, so pushing never allocates either.
    #[cfg(not(debug_assertions))]
    #[test]
    fn release_history_records_nothing() {
        let mut history = DebugHistory::new();
        let before = allocations();
        history.push(DebugEvent::LoopIteration(0));
        history.set_interesting(true);
        let after = allocations();
        assert_eq!(after - before, 0);
        assert!(history.events.is_empty());
        assert!(!history.is_interesting());
    }
}
