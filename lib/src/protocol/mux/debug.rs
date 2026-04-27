//! Debug history ring buffer and event enum.
//!
//! Used by the mux layer to record a bounded trail of per-session events for
//! post-mortem inspection when `debug_assertions` are enabled. In release
//! builds `push` and `set_interesting` are no-ops.

use std::collections::VecDeque;

use kawa::ParsingPhase;
use mio::Token;
use sozu_command::ready::Ready;

use super::{BackendConnectionError, MuxResult, Readiness, StreamState};

/// Maximum number of debug events retained in the ring buffer.
/// Oldest entries are dropped when this limit is reached.
pub(super) const DEBUG_HISTORY_CAPACITY: usize = 512;

pub struct DebugHistory {
    pub events: VecDeque<DebugEvent>,
    pub is_interesting: bool,
}
impl Default for DebugHistory {
    fn default() -> Self {
        Self {
            events: VecDeque::with_capacity(DEBUG_HISTORY_CAPACITY),
            is_interesting: false,
        }
    }
}
impl DebugHistory {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn push(&mut self, _event: DebugEvent) {
        #[cfg(debug_assertions)]
        {
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
