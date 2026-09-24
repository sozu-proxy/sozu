//! Where the mux core gets memory, and the one way it is allowed to fail.
//!
//! The core does not allocate for itself and does not name
//! [`crate::pool::Pool`]. Every buffer it needs — a connection's stream-0
//! landing zone, a stream's request/response pair, the HPACK scratch — comes
//! from a caller-supplied [`BufferSource`], which the caller implements and
//! which **may refuse**. That is the whole contract, and both halves matter:
//!
//! - *Caller-supplied* is what lets something other than the real pool stand
//!   behind it. A deterministic simulator that wants to drive the core into
//!   memory pressure has somewhere to stand, instead of having to arrange the
//!   pressure through a global allocator it does not control.
//! - *May refuse* is what keeps the existing behaviour expressible. Refusal
//!   is **not** an error condition here. A source that answers `None`
//!   mid-request is reporting a transient shortage, and the caller's
//!   obligation is to degrade the smallest unit it can: one stream is refused
//!   with `RST_STREAM(REFUSED_STREAM)` and the connection carries on, because
//!   the streams already running will hand their buffers back. Escalating a
//!   shortage to a connection error would convert it into the loss of every
//!   in-flight request that would have survived it.
//!
//! Neither half is new behaviour. Both checkout sites were already
//! `Option`-fallible with a `REFUSED_STREAM`-shaped fallback; what was missing
//! was a name for the thing being called, so the core reached for a concrete
//! `Weak<RefCell<Pool>>` instead. This module supplies the name and
//! [`PoolBufferSource`] supplies the one implementation the proxy itself uses,
//! which does exactly what the direct reach did.
//!
//! ## What is under the contract, and what is not
//!
//! Under it: the connection's own `zero` buffer, a stream's request/response
//! buffer pair, and the HPACK codec pair together with the three reusable
//! scratch buffers beside them (see `HpackState`, `hpack_state.rs`).
//!
//! Not under it, deliberately:
//!
//! - **`loona_hpack`'s own allocations.** The crate exposes `Decoder::new` /
//!   `Encoder::new` and no capacity-supplied constructor, so its dynamic
//!   tables cannot be routed through a source without forking it. They are
//!   brought under the contract at *acquisition* — the pair is built inside a
//!   fallible `HpackState::new` — and bounded where they already were, by the
//!   RFC 7541 §4.2 table-size settings. Growth after acquisition still reaches
//!   the global allocator.
//! - **Growth of an already-acquired scratch buffer.** `scratch` hands over an
//!   empty `Vec` that the core then grows and reclaims on its own schedule.
//!   Pre-sizing one and refusing to grow it is a behaviour change — it needs
//!   an answer for what a header block larger than the reservation does — and
//!   is not made here.
//! - **`H2Scheduler`'s pass-order buffer (`h2_scheduler.rs`).** It holds
//!   stream ids rather than octets, and it is already bounded by the
//!   concurrent-stream admission gate that decides how many ids can exist.

use std::{cell::RefCell, rc::Weak};

use crate::pool::{Checkout, Pool};

/// The caller-supplied origin of every buffer the mux core uses.
///
/// Implementors may refuse either request at any time, including in the
/// middle of a request. See the module documentation for what a refusal
/// obliges the caller to do — in short, degrade one stream, never the
/// connection.
pub trait BufferSource {
    /// A fixed-size buffer for wire octets: a connection's stream-0 landing
    /// zone, or one half of a stream's request/response pair.
    ///
    /// `None` means the source is momentarily out. It is not a permanent
    /// condition and must not be treated as one.
    fn checkout(&mut self) -> Option<Checkout>;

    /// A growable scratch buffer the core keeps for the lifetime of the
    /// connection and reuses across passes, rather than one it hands back
    /// per call.
    ///
    /// `None` refuses the construction that asked for it, which propagates as
    /// a connection that is never built — the same outcome, on the same path,
    /// as [`Self::checkout`] refusing the connection's own buffer.
    fn scratch(&mut self) -> Option<Vec<u8>>;
}

/// The [`BufferSource`] the proxy itself runs on: the worker's shared
/// [`Pool`], reached through the same weak handle the core used to hold
/// directly.
///
/// Every method is the expression that used to sit at the call site, moved
/// here unchanged — an upgrade-then-checkout for [`BufferSource::checkout`],
/// and a plain empty `Vec` for [`BufferSource::scratch`]. The pool has no
/// scratch arena to draw the latter from, and inventing one would be a
/// behaviour change rather than the relocation this is.
#[derive(Clone)]
pub struct PoolBufferSource {
    pool: Weak<RefCell<Pool>>,
}

impl PoolBufferSource {
    pub fn new(pool: Weak<RefCell<Pool>>) -> Self {
        PoolBufferSource { pool }
    }
}

impl BufferSource for PoolBufferSource {
    /// A dropped pool is indistinguishable from an exhausted one here, and
    /// deliberately so: both mean "no buffer now", and the caller's answer to
    /// either is the same refusal. This is the `upgrade()?` that
    /// `Stream::new` and `ConnectionH2::new` each spelled out.
    fn checkout(&mut self) -> Option<Checkout> {
        self.pool.upgrade()?.borrow_mut().checkout()
    }

    fn scratch(&mut self) -> Option<Vec<u8>> {
        Some(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use super::*;

    /// The pool's ceiling is the source's ceiling, and reaching it produces a
    /// refusal rather than a panic or a block. This is the property the H2
    /// core's `REFUSED_STREAM`-on-exhaustion path stands on.
    #[test]
    fn a_pool_source_refuses_once_its_pool_is_exhausted() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(1, 2, 4096)));
        let mut source = PoolBufferSource::new(Rc::downgrade(&pool));

        let first = source
            .checkout()
            .expect("a fresh pool yields its first buffer");
        let second = source
            .checkout()
            .expect("a pool with a maximum of 2 yields a second");
        assert!(
            source.checkout().is_none(),
            "a pool at its maximum must refuse rather than exceed it",
        );

        // Refusal is transient: returning a buffer makes the source answer
        // again. A caller that treated `None` as terminal would strand a
        // connection that was about to recover.
        drop(first);
        assert!(
            source.checkout().is_some(),
            "a returned buffer must make the source answer again",
        );
        drop(second);
    }

    /// A source whose pool has been dropped refuses in the same shape it
    /// refuses exhaustion, so no caller needs a second failure path.
    #[test]
    fn a_pool_source_refuses_once_its_pool_is_gone() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(1, 1, 4096)));
        let mut source = PoolBufferSource::new(Rc::downgrade(&pool));
        drop(pool);

        assert!(
            source.checkout().is_none(),
            "a dropped pool must refuse, not panic",
        );
    }
}
