//! Per-request stream state shared by the H1 and H2 mux paths.
//!
//! A [`Stream`] owns the front/back kawa buffers, HTTP context, and metrics
//! for a single request/response pair. [`StreamParts`] splits it along the
//! read/write axis so callers can borrow both sides of the pipe at the same
//! time without fighting the borrow checker.

use std::{
    cell::{Cell, RefCell},
    fmt::Debug,
    ops::{Deref, DerefMut},
    rc::Rc,
    time::Duration,
};

use mio::Token;
use sozu_command::logging::ansi_palette;

use super::{GenericHttpStream, Position, h2::MetricEvent};
use crate::metrics::names;
use crate::{
    L7ListenerHandler, ListenerHandler, Protocol, SessionMetrics,
    protocol::http::{answers::HttpAnswers, editor::HttpContext, parser::Method},
};

/// Module-level prefix used on every log line emitted from the stream module.
/// Streams have no direct peer reference so a single `MUX-STREAM` label is
/// used, colored bold bright-white (uniform across every protocol) when the
/// logger supports ANSI.
macro_rules! log_module_context {
    () => {{
        let (open, reset, _, _, _) = ansi_palette();
        format!("{open}MUX-STREAM{reset}\t >>>", open = open, reset = reset)
    }};
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    Idle,
    /// the Stream is asking for connection, this will trigger a call to connect
    Link,
    /// the Stream is linked to a Client (note that the client might not be connected)
    Linked(Token),
    /// the Stream was linked to a Client, but the connection closed, the client was removed
    /// and this Stream could not be retried (it should be terminated)
    Unlinked,
    /// the Stream is unlinked and can be reused
    Recycle,
}

impl StreamState {
    pub fn is_open(&self) -> bool {
        !matches!(self, StreamState::Idle | StreamState::Recycle)
    }
}

/// Ceiling on the request captures one worker may hold armed at once
/// (sozu-proxy/sozu#1450).
///
/// Captures live on the global allocator, outside the buffer
/// [`Pool`](crate::pool::Pool), so no
/// `max_buffers` accounting sees them. Before this ceiling their only bound
/// was transitive: a capture can exist only on a live [`Stream`], and
/// [`Stream::new`] takes exactly two [`BufferSource::checkout`](super::buffer_source::BufferSource::checkout)
/// calls, each served by that same [`Pool`](crate::pool::Pool), so at most
/// `max_buffers / 2` captures can be armed at once. At the defaults
/// (`max_buffers` 1000, `buffer_size` 16393) that is 500 captures of at most
/// 16400 bytes each, about 8.2 MB — and it scales with `max_buffers`, so
/// raising that knob to 20000 would take the same heap to roughly 164 MB.
///
/// 512 is chosen so the ceiling does NOT bite at the defaults: 500 possible
/// captures fit under it, so a stock deployment replays exactly what it
/// replayed before. What it removes is the growth: the heap stops tracking
/// `max_buffers` and becomes a constant.
///
/// The bound is `2 * MAX_ARMED_REPLAY_CAPTURES * buffer_size` and not a byte
/// budget, because each capture is independently bounded — a write that would
/// carry one past its front kawa's `storage.capacity()` drops it whole
/// (`super::h1::ConnectionH1::writable`), which is HAProxy's "Requests not
/// fitting in a single buffer will never be retried". Counting captures is
/// therefore a memory bound, not a proxy for one.
///
/// The factor of two is `Vec`'s amortized growth and is part of the bound,
/// not a footnote to it. `ConnectionH1::writable` calls `Vec::reserve`, which
/// grows to `max(2 * old_capacity, required)`, while the guard beside it
/// bounds `len` — not `capacity` — by `storage.capacity()`. A capture written
/// in one pass has `capacity == len`; one assembled over several partial
/// writes can reach twice it. Measured on this tree at the defaults: a single
/// 16393-byte write gives `len 16393 / capacity 16393`, and writes of
/// `[8192, 8192, 9]` give `len 16393 / capacity 32768`. So 512 captures carry
/// at most about 8.4 MB and may have allocated about 16.8 MB.
///
/// Past the ceiling nothing is refused and no frontend parks:
/// [`Stream::arm_upstream_replay`] installs no buffer, the request proceeds
/// un-replayable, and a stale pooled upstream answers the same
/// `502 Bad Gateway` it answered before the replay existed. That graceful
/// degradation is the reason the captures are not pool-allocated, where the
/// same memory pressure would answer `503` and park frontends instead.
pub const MAX_ARMED_REPLAY_CAPTURES: usize = 512;

thread_local! {
    /// Request captures currently armed on this worker, the quantity
    /// [`MAX_ARMED_REPLAY_CAPTURES`] bounds.
    ///
    /// Thread-local rather than a process-wide static because that is the
    /// scope of everything it is reconciled against: a Sōzu worker is one
    /// event-loop thread, its [`Pool`](crate::pool::Pool) is an `Rc<RefCell<Pool>>` that cannot
    /// leave it, and the `backend.retry.captures_armed` gauge this counter
    /// feeds lives in the `thread_local!` `crate::metrics::METRICS`. A shared
    /// static would let one worker thread in a multi-worker test process
    /// exhaust another's budget while their gauges disagreed about it.
    static ARMED_REPLAY_CAPTURES: Cell<usize> = const { Cell::new(0) };
}

/// A request capture that owns its charge against
/// [`MAX_ARMED_REPLAY_CAPTURES`].
///
/// The charge is acquired by `ReplayCapture::try_arm` and released by
/// [`Drop`], so it is released wherever the capture is — `Option::take`,
/// assigning `None` through [`StreamParts`], or the [`Stream`] being dropped
/// on a client hangup, an idle timeout or a session teardown. That last path
/// has no code site of its own, and wiring decrements into the four that do
/// would leak a charge on every torn-down armed request until the ceiling
/// disarmed replay for the rest of the worker's life. This is the reasoning
/// `super::h2::ConnectionH2`'s gauge teardown and `crate::pool::Checkout`
/// already apply: teardown in `Drop` is symmetric whichever path ran.
///
/// Derefs to the captured bytes, so the write path appends to it exactly as
/// it appended to the bare `Vec<u8>` this replaced.
pub struct ReplayCapture {
    bytes: Vec<u8>,
}

impl ReplayCapture {
    /// Charge one capture against this worker's budget, or refuse.
    ///
    /// `None` means the budget is full; the caller emits
    /// `backend.retry.captures_declined` and carries on without a capture.
    fn try_arm() -> Option<Self> {
        let armed = ARMED_REPLAY_CAPTURES.get();
        if armed >= MAX_ARMED_REPLAY_CAPTURES {
            return None;
        }
        ARMED_REPLAY_CAPTURES.set(armed + 1);
        gauge_add!(names::backend::RETRY_CAPTURES_ARMED, 1);
        Some(Self { bytes: Vec::new() })
    }

    /// Take the captured bytes out, releasing the charge as `self` drops.
    ///
    /// `Vec` cannot be moved out of a type that implements [`Drop`], so the
    /// bytes are swapped for an empty `Vec` and the husk is dropped normally
    /// — which is what keeps the release on the single `Drop` site.
    ///
    /// Do NOT "simplify" this with `std::mem::forget(self)` to skip the drop.
    /// The drop IS the release: forgetting it leaks one charge per replay
    /// until the ceiling disarms replay for the rest of the worker's life,
    /// with `backend.retry.captures_armed` pinned at
    /// [`MAX_ARMED_REPLAY_CAPTURES`] to show it. `tests::
    /// queueing_a_replay_releases_the_capture_charge` is the guard, and it is
    /// the only test in the suite that catches that mutation.
    fn into_bytes(mut self) -> Vec<u8> {
        std::mem::take(&mut self.bytes)
    }
}

impl Drop for ReplayCapture {
    fn drop(&mut self) {
        let armed = ARMED_REPLAY_CAPTURES.get();
        // Budget-accounting invariant: every live `ReplayCapture` was paired
        // with a `set(armed + 1)` in `try_arm`, so the counter must be
        // strictly positive when one is dropped. A zero here is an unbalanced
        // arm/release, which would wrap the counter to `usize::MAX` and
        // disarm replay for the life of the worker. Mirrors the same guard on
        // `crate::pool::Checkout`'s buffer gauge.
        debug_assert!(
            armed >= 1,
            "armed-capture budget underflow on release: count was {armed} before decrement"
        );
        ARMED_REPLAY_CAPTURES.set(armed.saturating_sub(1));
        gauge_add!(names::backend::RETRY_CAPTURES_ARMED, -1);
    }
}

impl Deref for ReplayCapture {
    type Target = Vec<u8>;

    fn deref(&self) -> &Self::Target {
        &self.bytes
    }
}

impl DerefMut for ReplayCapture {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.bytes
    }
}

pub struct Stream {
    pub window: i32,
    pub attempts: u8,
    pub state: StreamState,
    /// True when the frontend connection has received end_of_stream from the client.
    pub front_received_end_of_stream: bool,
    /// True when the backend connection has received end_of_stream from the backend server.
    pub back_received_end_of_stream: bool,
    /// Tracks total DATA payload bytes received on the frontend for content-length validation (RFC 9113 §8.1.1)
    pub front_data_received: usize,
    /// Tracks total DATA payload bytes received on the backend for content-length validation (RFC 9113 §8.1.1)
    pub back_data_received: usize,
    /// True when `gauge_add!(names::http::ACTIVE_REQUESTS, 1)` was emitted for this stream.
    /// Prevents underflow when `generate_access_log` is called for streams that never
    /// had their request fully parsed (idle timeouts, malformed requests).
    pub request_counted: bool,
    pub front: GenericHttpStream,
    pub back: GenericHttpStream,
    /// The serialized request already written to the upstream, kept so it can
    /// be replayed on a fresh connection when a POOLED keep-alive upstream
    /// turns out to have been closed by its peer before it answered.
    ///
    /// `None` means "not replayable" and is the state on every path that is
    /// not the stale-pool race: a freshly dialled upstream never fills it
    /// (see [`super::h1::ConnectionH1`]'s `reused_from_pool`), a response
    /// byte clears it, and a request too large to fit one front buffer
    /// truncates it back to `None`. `Some` therefore carries both the bytes
    /// and the proof that replay is allowed.
    ///
    /// This mirrors pingora's `RetryType::ReusedOnly` + `retry_buffer_
    /// truncated()` pair and HAProxy's "requires to allocate a buffer and
    /// copy the whole request into it […] Requests not fitting in a single
    /// buffer will never be retried" — replaying a request that kawa has
    /// already consumed is impossible without holding its bytes.
    ///
    /// The capture is charged against [`MAX_ARMED_REPLAY_CAPTURES`] for as
    /// long as it is `Some`; see [`ReplayCapture`].
    pub retry_buffer: Option<ReplayCapture>,
    pub context: HttpContext,
    pub metrics: SessionMetrics,
    /// The listener answer registry this stream renders its default answers
    /// from, captured when the request that owns the slot arrived and held
    /// for the rest of the stream's life.
    ///
    /// Every `set_default_answer` site in the mux reads this handle instead
    /// of borrowing [`Context::listener`](super::Context::listener) again, so
    /// an operator listener reload landing mid-request cannot change the
    /// template a stream already in flight is about to render.
    /// `HttpListener::update_config` / `HttpsListener::update_config`
    /// publish a *new* registry rather than overwriting this one, which is
    /// what makes the captured handle a snapshot and not just a second name
    /// for the live one. See `LIFECYCLE.md` §2.5.
    ///
    /// `Rc` so the capture is a refcount bump on a path that runs once per
    /// request; the inner `RefCell` is the registry's own, not a mutation
    /// point for the mux — nothing under `mux/` ever takes it mutably.
    pub answers: Rc<RefCell<HttpAnswers>>,
}

struct KawaSummary<'a>(&'a GenericHttpStream);
impl Debug for KawaSummary<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kawa")
            .field("kind", &self.0.kind)
            .field("parsing_phase", &self.0.parsing_phase)
            .field("body_size", &self.0.body_size)
            .field("consumed", &self.0.consumed)
            .field("expects", &self.0.expects)
            .field("blocks", &self.0.blocks.len())
            .field("out", &self.0.out.len())
            .field("storage_start", &self.0.storage.start)
            .field("storage_head", &self.0.storage.head)
            .field("storage_end", &self.0.storage.end)
            .finish()
    }
}
impl Debug for Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stream")
            .field("window", &self.window)
            .field("attempts", &self.attempts)
            .field("state", &self.state)
            .field(
                "front_received_end_of_stream",
                &self.front_received_end_of_stream,
            )
            .field(
                "back_received_end_of_stream",
                &self.back_received_end_of_stream,
            )
            .field("front_data_received", &self.front_data_received)
            .field("back_data_received", &self.back_data_received)
            .field("request_counted", &self.request_counted)
            .field("front", &KawaSummary(&self.front))
            .field("back", &KawaSummary(&self.back))
            .field(
                "retry_buffer",
                &self.retry_buffer.as_ref().map(|bytes| bytes.len()),
            )
            .field("context", &self.context)
            .field("metrics", &self.metrics)
            .finish()
    }
}

/// This struct allows to mutably borrow the read and write buffers (dependant on the position)
/// as well as the context and metrics of a Stream at the same time
pub struct StreamParts<'a> {
    pub window: &'a mut i32,
    pub rbuffer: &'a mut GenericHttpStream,
    pub wbuffer: &'a mut GenericHttpStream,
    /// Tracks whether end_of_stream has been received on the read side of this connection.
    pub received_end_of_stream: &'a mut bool,
    /// Tracks total DATA payload bytes received on the read side (for content-length validation).
    pub data_received: &'a mut usize,
    pub context: &'a mut HttpContext,
    pub metrics: &'a mut SessionMetrics,
    /// [`Stream::retry_buffer`], reachable from the write path so the H1
    /// client can record the exact bytes it hands the upstream socket before
    /// `kawa::Kawa::consume` drops them from the front buffer.
    pub retry_buffer: &'a mut Option<ReplayCapture>,
}

impl Stream {
    /// `None` when `buffers` cannot supply the pair. Both halves are asked
    /// for together and neither is kept without the other, so a stream that
    /// cannot be served leaves the source exactly as it found it — which is
    /// what makes the caller's `RST_STREAM(REFUSED_STREAM)` a refusal of one
    /// stream rather than a leak charged to the next.
    pub fn new(
        buffers: &mut dyn super::buffer_source::BufferSource,
        context: HttpContext,
        answers: Rc<RefCell<HttpAnswers>>,
        window: u32,
    ) -> Option<Self> {
        let (front_buffer, back_buffer) = match (buffers.checkout(), buffers.checkout()) {
            (Some(front_buffer), Some(back_buffer)) => (front_buffer, back_buffer),
            _ => return None,
        };
        let stream = Self {
            state: StreamState::Idle,
            attempts: 0,
            window: i32::try_from(window).unwrap_or(i32::MAX),
            front_received_end_of_stream: false,
            back_received_end_of_stream: false,
            front_data_received: 0,
            back_data_received: 0,
            request_counted: false,
            front: GenericHttpStream::new(kawa::Kind::Request, kawa::Buffer::new(front_buffer)),
            back: GenericHttpStream::new(kawa::Kind::Response, kawa::Buffer::new(back_buffer)),
            retry_buffer: None,
            context,
            metrics: SessionMetrics::new(None),
            answers,
        };
        // Post: a freshly checked-out stream is a clean, closed slot — no
        // request has been counted yet (so `generate_access_log` won't
        // gauge-underflow `http.active_requests`) and no DATA has been seen on
        // either half (the content-length reconciliation counters start at 0).
        debug_assert_eq!(stream.state, StreamState::Idle, "new stream must be Idle");
        debug_assert!(
            !stream.state.is_open(),
            "an Idle stream slot must not report as open"
        );
        debug_assert!(
            !stream.request_counted,
            "new stream must not have a counted request (gauge-underflow guard)"
        );
        debug_assert_eq!(
            (stream.front_data_received, stream.back_data_received),
            (0, 0),
            "new stream DATA counters must start at 0"
        );
        #[cfg(debug_assertions)]
        stream.check_invariants();
        Some(stream)
    }

    /// Cross-field invariant sweep for the per-request stream state machine.
    ///
    /// Encodes the relationships that must hold for ANY `Stream` regardless of
    /// the mux path (H1 or H2) that drives it:
    /// - `state.is_open()` agrees with the `Idle`/`Recycle` discriminants
    ///   (the open/closed split is the load-bearing predicate for shutdown and
    ///   slot reuse).
    /// - a `Recycle` slot is fully reset — no counted request can be left
    ///   pending on a slot advertised as reusable, or `create_stream` would
    ///   resurrect a stale `http.active_requests` charge.
    /// - a `Linked` stream names a backend token; the `Linked(token)`
    ///   discriminant and `linked_token()` must agree (the access-log and
    ///   reverse-index lookups both depend on this equivalence).
    ///
    /// Compiled only with `debug_assertions`; the optimizer drops every call
    /// in release. Network input never reaches a hard `assert!` here — these
    /// fire only on our own logic bugs.
    #[cfg(debug_assertions)]
    pub(super) fn check_invariants(&self) {
        debug_assert_eq!(
            self.state.is_open(),
            !matches!(self.state, StreamState::Idle | StreamState::Recycle),
            "is_open() must agree with the Idle/Recycle discriminants"
        );
        if self.state == StreamState::Recycle {
            debug_assert!(
                !self.request_counted,
                "a Recycle slot must not carry a counted request (active-requests leak)"
            );
        }
        // `linked_token()` is the canonical accessor for the backend token; it
        // must return Some iff the slot is `Linked`, since the reverse index
        // and the access-log RTT lookup both branch on it.
        debug_assert_eq!(
            self.linked_token().is_some(),
            matches!(self.state, StreamState::Linked(_)),
            "linked_token() must be Some iff the stream is Linked"
        );
    }
    /// Convenience accessor for the backend token when the stream is `Linked`.
    /// Used by access-log emission sites to look up the backend socket on the
    /// owning `Endpoint`/`Router` without re-pattern-matching `state` inline.
    pub fn linked_token(&self) -> Option<Token> {
        match self.state {
            StreamState::Linked(token) => Some(token),
            _ => None,
        }
    }

    /// Returns true when both front and back kawa buffers are in a terminal
    /// or initial state with no pending data. Used during shutdown to skip
    /// streams that have already completed their work.
    pub fn is_quiesced(&self) -> bool {
        let front_done =
            (self.front.is_initial() || self.front.is_completed() || self.front.is_terminated())
                && self.front.storage.is_empty();
        let back_done =
            (self.back.is_initial() || self.back.is_completed() || self.back.is_terminated())
                && self.back.storage.is_empty();
        front_done && back_done
    }

    pub fn split(&mut self, position: &Position) -> StreamParts<'_> {
        // Pre: the front buffer always parses requests and the back buffer
        // always parses responses. `split` only re-labels them as read/write
        // for the caller's position — it must never swap their kawa kinds.
        debug_assert_eq!(
            self.front.kind,
            kawa::Kind::Request,
            "front buffer must hold a Request kawa"
        );
        debug_assert_eq!(
            self.back.kind,
            kawa::Kind::Response,
            "back buffer must hold a Response kawa"
        );
        match position {
            Position::Client(..) => StreamParts {
                window: &mut self.window,
                rbuffer: &mut self.back,
                wbuffer: &mut self.front,
                received_end_of_stream: &mut self.back_received_end_of_stream,
                data_received: &mut self.back_data_received,
                context: &mut self.context,
                metrics: &mut self.metrics,
                retry_buffer: &mut self.retry_buffer,
            },
            Position::Server => StreamParts {
                window: &mut self.window,
                rbuffer: &mut self.front,
                wbuffer: &mut self.back,
                received_end_of_stream: &mut self.front_received_end_of_stream,
                data_received: &mut self.front_data_received,
                context: &mut self.context,
                metrics: &mut self.metrics,
                retry_buffer: &mut self.retry_buffer,
            },
        }
    }

    /// Arm request capture for an attempt on a reused keep-alive upstream.
    ///
    /// Idempotent WITHIN one attempt: an already-armed buffer is left alone
    /// so a multi-pass write keeps accumulating into the same allocation.
    ///
    /// It is NOT once per request. `super::h1::ConnectionH1::start_stream`
    /// calls this on every `KeepAlive -> Connected` transition and
    /// `reused_from_pool` is never cleared, so a replay that lands on
    /// another pooled connection re-arms a fresh capture and may itself be
    /// replayed. `Router::plan_connect`'s `stream.attempts >= CONN_RETRIES` gate
    /// is the only bound on how many times one request is re-issued.
    ///
    /// Arming is BEST EFFORT. Past [`MAX_ARMED_REPLAY_CAPTURES`] no buffer is
    /// installed, `backend.retry.captures_declined` is incremented, and the
    /// request proceeds un-replayable: [`Stream::can_replay_on_fresh_upstream`]
    /// reads the absent buffer as "not replayable", so a stale pooled upstream
    /// yields the same `502 Bad Gateway` it yielded before the replay existed.
    /// Nothing is refused and no frontend parks — which is exactly what
    /// pool-allocating the captures would have cost under the same memory
    /// pressure (sozu-proxy/sozu#1450).
    ///
    /// A NON-IDEMPOTENT request is not captured at all, and does not spend a
    /// charge. [`Stream::can_replay_on_fresh_upstream`] vetoes the method, so
    /// such a capture could only ever be held for the life of the attempt and
    /// thrown away. Before the ceiling that was pure waste; with one it is
    /// also a slot a replayable request cannot have — on a POST-heavy pooled
    /// workload it would be most of the budget, while
    /// `backend.retry.captures_armed` sat at the ceiling showing an operator
    /// nothing about why.
    ///
    /// The method is known here. `crate::protocol::http::editor::HttpContext`
    /// sets it in `on_request_headers` during the frontend parse, and
    /// `super::router::Router::plan_connect` routes on it — `route_from_request`,
    /// which fails with `RetrieveClusterError::NoMethod` without it — before
    /// it reaches `super::h1::ConnectionH1::start_stream`, the sole caller of
    /// this function.
    ///
    /// This makes the idempotence conjunct in
    /// [`Stream::can_replay_on_fresh_upstream`] unreachable in production
    /// rather than redundant by accident. It is deliberately KEPT as defense
    /// in depth: it is the conjunct a later widening of this guard would
    /// silently undo, and it is the one the replay's whole justification
    /// rests on (RFC 9110 §9.2.2).
    pub fn arm_upstream_replay(&mut self) {
        if self.retry_buffer.is_some() {
            return;
        }
        if !self
            .context
            .method
            .as_ref()
            .is_some_and(Method::is_idempotent)
        {
            return;
        }
        match ReplayCapture::try_arm() {
            Some(capture) => self.retry_buffer = Some(capture),
            None => incr!(names::backend::RETRY_CAPTURES_DECLINED),
        }
    }

    /// Forget the captured request.
    ///
    /// Called as soon as the upstream produces its first byte: past that
    /// point the attempt is no longer replayable (the boundary below), so
    /// holding the copy would only cost memory.
    pub fn forget_upstream_replay(&mut self) {
        self.retry_buffer = None;
    }

    /// May this stream's request be re-issued on a fresh upstream
    /// connection?
    ///
    /// The boundary is **no response byte has been received**, narrowed to
    /// the stale-pool race:
    ///
    /// - `retry_buffer.is_some()` proves BOTH that the whole serialized
    ///   request is still held AND that it went onto a reused keep-alive
    ///   connection, because that is the only path that arms the capture.
    ///   This is pingora's `RetryType::ReusedOnly`. It is a POLICY, not a
    ///   diagnosis: sozu cannot tell a stale pool socket from an origin
    ///   that half-closed after processing the request, or from one that
    ///   crashed mid-request — `super::h1::ConnectionH1::readable` treats
    ///   every `size == 0` alike. Restricting replay to pooled connections
    ///   narrows it to the case where an unobserved idle close is
    ///   plausible; the conjunct below is what makes re-issuing permissible.
    /// - the method is idempotent, which RFC 9110 §9.2.2 defines as exactly
    ///   this permission: re-issuing it has the same intended effect as
    ///   issuing it once, so a client cannot observe the difference. That is
    ///   the load-bearing justification for the whole feature. nginx refuses
    ///   `POST, LOCK, PATCH` "if a request has been sent to an upstream
    ///   server" unless `non_idempotent` is set; pingora vetoes on
    ///   `!method.is_idempotent()`; HAProxy provides an
    ///   `http-request disable-l7-retry` action and gives POST as the
    ///   rationale for reaching for it.
    /// - the upstream produced nothing at all: no byte forwarded
    ///   (`!back.consumed`) and no byte even buffered
    ///   (`back.storage.is_empty()`). nginx: "passing a request to the next
    ///   server is only possible if nothing has been sent to a client yet".
    ///   Requiring an empty back buffer is stricter than that — a partial
    ///   status line is HAProxy's `junk-response`, which it keeps out of
    ///   every default.
    ///
    /// The caller must additionally have established that no response is
    /// available at all; `super::shared::end_stream_decision` owns that part
    /// and is the only caller.
    pub fn can_replay_on_fresh_upstream(&self) -> bool {
        self.retry_buffer.is_some()
            && self
                .context
                .method
                .as_ref()
                .is_some_and(Method::is_idempotent)
            && !self.back.consumed
            && self.back.storage.is_empty()
    }

    /// Queue the captured request for re-serialization onto a fresh upstream.
    ///
    /// The bytes go back into `front.out` as an owned `Store::Alloc`, and
    /// they are PREPENDED. `out` is not necessarily empty at this point:
    /// [`super::h1::ConnectionH1::writable`] answers a partial
    /// `socket_write_vectored` by signalling a pending write and returning,
    /// and `kawa::Kawa::consume` pushes the partially consumed store back to
    /// the FRONT of `out` (kawa-0.7.1 `storage/repr.rs`), so the bytes the
    /// kernel refused are still queued here. The capture holds exactly the
    /// bytes the socket DID accept, so capture and remainder are the
    /// complementary halves of one serialization and the capture belongs
    /// ahead of the remainder. `kawa::Kawa::push_out` appends, which would
    /// put `[tail][head]` on the wire — a request mangled mid-token.
    ///
    /// `VecDeque::push_front` preserves the relative order of what is already
    /// queued, so this is correct however many entries the remainder spans:
    /// `consume` pushes back at most ONE partially consumed store and drops
    /// every entry ahead of it.
    ///
    /// Prepending an owned `Store::Alloc` is also safe for kawa's storage
    /// bookkeeping. `Kawa::leftmost_ref` scans `out` for the first
    /// `Store::Slice` and skips an `Alloc`, so buffer reclamation is
    /// unchanged, and `Store::push_left` is a no-op on an `Alloc`, whose
    /// bytes live outside `storage` and survive a shift.
    ///
    /// `front.blocks` was drained by the `prepare` that preceded the write,
    /// so the next `writable` pass's `prepare` contributes nothing of its
    /// own. Replaying the SERIALIZED form (rather than re-running the block
    /// converter) also makes the retried request byte-identical to the first
    /// attempt, `Sozu-Id` and `X-Forwarded-*` included.
    ///
    /// Returns the number of bytes queued, or `None` when nothing was held.
    pub fn queue_upstream_replay(&mut self) -> Option<usize> {
        let request = self.retry_buffer.take()?.into_bytes();
        let len = request.len();
        self.front
            .out
            .push_front(kawa::OutBlock::Store(kawa::Store::from_vec(request)));
        Some(len)
    }

    /// Emit the access log for this stream.
    ///
    /// `client_rtt`/`server_rtt` are passed in by the caller because the
    /// `Stream` does not own a socket reference — the frontend socket lives
    /// on the parent `Mux`/connection and the backend socket lives on
    /// `Router.backends.get(token)`. Each caller snapshots the two
    /// `getsockopt(TCP_INFO)` values from the sockets it can reach, mirroring
    /// the inline pattern used by the `pipe` and TCP-frontend access-log
    /// sites.
    /// Generate the access log, and return the metric events the caller must
    /// record.
    ///
    /// **The bound is two, and it is saturated by exactly these two events**:
    /// [`MetricEvent::ActiveRequestFinished`], emitted when `request_counted`
    /// was set, and [`MetricEvent::AccessLogUnsent`], emitted when the logger
    /// refused the record. A fixed array rather than a `Vec` because the
    /// count is known: this function is allocation-free and must stay so, and
    /// a caller that is its own shell has no queue to lend. Slots are filled
    /// by explicit index, never through a `push` helper — a `push` that
    /// overflowed a fixed array would silently drop an event, and a dropped
    /// `-1` is an upward drift that never underflows, so nothing logs and
    /// nothing saturates. A third event means changing this type, which is a
    /// compile error at every call site rather than a truncation.
    ///
    /// The three remaining metric sites in this body stay `incr!` macros on
    /// purpose: they carry `cluster_id`/`backend_id` labels, and carrying
    /// those in a `MetricEvent` means owned `Option<String>`s, which measures
    /// at `size_of::<MetricEvent>()` 24 -> 48 bytes for every queued event
    /// and costs the enum its `Copy` — for sites that fire on every access
    /// log. Same trade as `names::backend::RETRY_STALE_UPSTREAM`, and worse,
    /// because that one fires only on a stale-upstream retry.
    #[must_use]
    pub(super) fn generate_access_log<L>(
        &mut self,
        error: bool,
        message: Option<&str>,
        listener: Rc<RefCell<L>>,
        client_rtt: Option<Duration>,
        server_rtt: Option<Duration>,
    ) -> [Option<MetricEvent>; 2]
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        let mut events: [Option<MetricEvent>; 2] = [None, None];
        let context = &self.context;
        // Fall back to the per-stream timeout discriminator
        // (`access_log_message`) when the caller did not supply an explicit
        // `message`. The discriminator is set by `MuxState::timeout` before
        // `set_default_answer` / `forcefully_terminate_answer` so the
        // access log can distinguish a timeout-driven 408/504 from a
        // backend-error 504. Caller-supplied `message` (e.g. parsing
        // errors) takes precedence when both are present.
        let message = message.or(context.access_log_message);
        // Pair the `http.active_requests` gauge `-1` with `request_counted`:
        // it must transition true -> false exactly once so a re-entry (H1
        // keep-alive, double access-log on the same stream) cannot
        // double-decrement the gauge into underflow. `request_counted` is set
        // true at the matching `gauge_add!(.., 1)` in the H1/H2 readable paths.
        let was_counted = self.request_counted;
        if self.request_counted {
            events[0] = Some(MetricEvent::ActiveRequestFinished);
            self.request_counted = false;
        }
        debug_assert!(
            !self.request_counted,
            "generate_access_log must leave request_counted false (gauge-underflow guard)"
        );
        // The flag may only move true->false here (one `-1`); it must never be
        // observed flipping back on within this call.
        debug_assert!(
            was_counted >= self.request_counted,
            "request_counted must only clear here, never spontaneously set"
        );
        if error {
            // Labelled with `(cluster_id, backend_id)`. Since the unreachable
            // `kawa_h1::Http` session was removed (sozu#1346) this is the SOLE
            // `names::http::ERRORS` emission site in the crate, so the labels
            // written here are the whole cardinality contract for `http.errors`;
            // the backend label is dropped centrally, by detail level, in
            // `metrics::filter_labels_for_detail` (`lib/src/metrics/mod.rs`).
            // `pipe::log_request_error` is a different metric
            // (`names::pipe::ERRORS`) and is not a second source of this one.
            incr!(
                names::http::ERRORS,
                context.cluster_id.as_deref(),
                context.backend_id.as_deref()
            );
        }
        let protocol = match context.protocol {
            Protocol::HTTP => "http",
            Protocol::HTTPS => "https",
            other => {
                error!(
                    "{} mux streams only handle HTTP or HTTPS protocols, got {:?}",
                    log_module_context!(),
                    other
                );
                "unknown"
            }
        };

        // Save the HTTP status code of the backend response. Emits the bucket
        // counter unconditionally, plus the per-code counter from
        // `crate::metrics::http_status_code_metric_name` when the status is on
        // the short-list that file maintains. This is the ONLY HTTP status
        // bucketer left since `kawa_h1::save_http_status_metric` was removed
        // with the unreachable H1 session (sozu#1346, 2026-09-20).
        let bucket_key = if let Some(status) = context.status {
            match status {
                100..=199 => names::http::STATUS_1XX,
                200..=299 => names::http::STATUS_2XX,
                300..=399 => names::http::STATUS_3XX,
                400..=499 => names::http::STATUS_4XX,
                500..=599 => names::http::STATUS_5XX,
                _ => names::http::STATUS_OTHER,
            }
        } else {
            "http.status.none"
        };
        incr!(
            bucket_key,
            context.cluster_id.as_deref(),
            context.backend_id.as_deref()
        );

        if let Some(status) = context.status
            && let Some(per_code) = crate::metrics::http_status_code_metric_name(status)
        {
            incr!(
                per_code,
                context.cluster_id.as_deref(),
                context.backend_id.as_deref()
            );
        }

        // A request the parse rejected before `HttpContext` captured its
        // request line (sozu-proxy/sozu#1085) is logged from the front
        // buffer, borrowed: see `rejected_request_line` for why this is a
        // second path and not a call to the nominal capture.
        let rejected = if context.method.is_none() {
            rejected_request_line(&self.front)
        } else {
            None
        };
        debug_assert!(
            rejected.is_none() || self.front.is_error(),
            "only a rejected request is logged from the front buffer"
        );
        debug_assert!(
            rejected.is_none() || context.method.is_none(),
            "a request line the context captured must never be overridden"
        );
        let endpoint = match &rejected {
            Some(line) => sozu_command::logging::EndpointRecord::Http {
                method: line.method,
                authority: line.authority,
                path: line.path,
                reason: context.reason.as_deref(),
                status: context.status,
            },
            None => sozu_command::logging::EndpointRecord::Http {
                method: context.method.as_deref(),
                authority: context.authority.as_deref(),
                path: context.path.as_deref(),
                reason: context.reason.as_deref(),
                status: context.status,
            },
        };

        let listener = listener.borrow();
        // Tags resolved by the router from the frontend rule that actually
        // matched this request win: they are the only ones that can be
        // right for a wildcard, regex, ported or differently-cased
        // frontend. The listener's authority-keyed map is written under
        // the frontend RULE's hostname and read here under the REQUEST's
        // authority, so it only ever answers for an exact literal
        // frontend (sozu#1379).
        //
        // It stays as the fallback for a request that never reached
        // routing at all — a malformed request, an unknown host, a TLS
        // SNI/authority mismatch — where there is no matched frontend to
        // ask and the pre-existing best-effort answer is better than none.
        let tags = context.tags.as_deref().or_else(|| {
            context.authority.as_deref().and_then(|host| {
                let hostname = match host.split_once(':') {
                    None => host,
                    Some((hostname, _)) => hostname,
                };
                listener.get_tags(hostname)
            })
        });

        log_access! {
            error,
            on_failure: { events[1] = Some(MetricEvent::AccessLogUnsent) },
            message,
            context: context.log_context(),
            session_address: context.session_address,
            backend_address: context.backend_address,
            protocol,
            endpoint,
            tags,
            client_rtt,
            server_rtt,
            service_time: self.metrics.service_time(),
            response_time: self.metrics.backend_response_time(),
            request_time: self.metrics.request_time(),
            start_time_ns: self.metrics.start_wall_ns(),
            bytes_in: self.metrics.bin,
            bytes_out: self.metrics.bout,
            user_agent: context.user_agent.as_deref(),
            x_request_id: context.x_request_id.as_deref(),
            tls_version: context.tls_version,
            tls_cipher: context.tls_cipher,
            tls_sni: context.tls_server_name.as_deref(),
            tls_alpn: context.tls_alpn,
            xff_chain: context.xff_chain.as_deref(),
            #[cfg(feature = "opentelemetry")]
            otel: context.otel.as_ref(),
            #[cfg(not(feature = "opentelemetry"))]
            otel: None,
        };
        self.metrics.register_end_of_session(&context.log_context());

        events
    }
}

/// The request line of a rejected request, borrowed from the front buffer.
/// See [`rejected_request_line`].
struct RejectedRequestLine<'a> {
    method: Option<&'a str>,
    authority: Option<&'a str>,
    path: Option<&'a str>,
}

/// The request line of a request the parse rejected before [`HttpContext`]
/// captured it, borrowed from the front buffer (sozu-proxy/sozu#1085).
/// `None` unless `front` is in error and holds a parsed request line.
///
/// # Two paths fill the access log's request line, on purpose
///
/// The nominal one is `HttpContext::on_request_headers`
/// (`lib/src/protocol/kawa_h1/editor.rs`), which copies method, authority
/// and path into owned `Option<String>`s: routing, redirects and header
/// edits read them long after the parse. This one serves the access log
/// alone, on the rejection path, and BORROWS: `EndpointRecord::Http` already
/// takes `&str`, and the access-log path stays allocation-free. Do not
/// replace it with a call to the nominal capture — that brings back two to
/// three allocations per rejected request, a rate the client chooses.
///
/// Borrowing is sound because the bytes are still there: an H1 front is only
/// ever reset through `kawa::Kawa::clear`, which returns
/// `detached.status_line` to `StatusLine::Unknown` in the same call, so a
/// `StatusLine::Request` here always points into the request it was parsed
/// from.
///
/// # What is recovered, and what is refused
///
/// - kawa refused a header, or the framing in `process_headers`, after the
///   request line. Its authority and path are still empty, so the
///   request-target is split with `kawa::h1::parser::primitives::parse_url`,
///   which answers `Store::Slice`/`Store::Static` and allocates nothing.
/// - Sōzu's CL.TE guard in `HttpContext::on_request_headers` refused a
///   request kawa had accepted, returning before the capture. kawa already
///   resolved the authority and the path; they are logged as-is.
///
/// The authority of an origin-form request is NEVER read from a `Host`
/// block. When kawa refuses a header, whether the `Host` line was reached is
/// the client's choice — put the bad line first and no `Host` block exists —
/// and a reached one was never validated. Only an authority the request
/// line itself carries (absolute-form, `CONNECT`) is logged. Do not
/// "complete" this with a `Host` lookup.
///
/// H2 does not reach here with a request line: every rejection in
/// `handle_header` (`lib/src/protocol/mux/pkawa.rs`) returns before the
/// status line is assigned, so it stays `StatusLine::Unknown`.
fn rejected_request_line(front: &GenericHttpStream) -> Option<RejectedRequestLine<'_>> {
    if !front.is_error() {
        return None;
    }
    let kawa::StatusLine::Request {
        method,
        uri,
        authority,
        path,
        ..
    } = &front.detached.status_line
    else {
        return None;
    };
    let buf = front.storage.buffer();
    let method_bytes = borrowed_bytes(method, buf)?;
    let method = std::str::from_utf8(method_bytes).ok();

    if let Some(path) = borrowed_str(path, buf) {
        // `process_headers` ran, so the refusal is Sōzu's own.
        return Some(RejectedRequestLine {
            method,
            authority: borrowed_str(authority, buf),
            path: Some(path),
        });
    }
    debug_assert!(
        borrowed_bytes(authority, buf).is_none(),
        "kawa resolves the authority and the path together"
    );
    let (authority, path) = borrowed_bytes(uri, buf)
        .and_then(|uri| kawa::h1::parser::primitives::parse_url(buf, method_bytes, uri))
        .map_or((None, None), |(authority, path)| {
            (borrowed_str(&authority, buf), borrowed_str(&path, buf))
        });
    Some(RejectedRequestLine {
        method,
        authority,
        path,
    })
}

/// A `kawa::Store`'s bytes, borrowed from `buf` or from static data. `None`
/// for every variant that owns its bytes, and for an empty store.
fn borrowed_bytes<'a>(store: &kawa::Store, buf: &'a [u8]) -> Option<&'a [u8]> {
    match store {
        kawa::Store::Slice(slice) => slice.data_opt(buf),
        kawa::Store::Static(bytes) => Some(bytes),
        _ => None,
    }
}

/// [`borrowed_bytes`], as UTF-8. Non-UTF-8 bytes are dropped, the way the
/// nominal capture in `HttpContext::on_request_headers` drops them.
fn borrowed_str<'a>(store: &kawa::Store, buf: &'a [u8]) -> Option<&'a str> {
    borrowed_bytes(store, buf).and_then(|bytes| std::str::from_utf8(bytes).ok())
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use rusty_ulid::Ulid;
    use sozu_command::proto::command::filtered_metrics;

    use super::*;
    use crate::{
        metrics::METRICS,
        pool::Pool,
        protocol::mux::{
            buffer_source::PoolBufferSource,
            shared::{self, EndStreamAction},
            test_support::TestListener,
        },
    };

    /// A bare `Stream` holding two real pool buffers, built the way the
    /// sibling test above builds one. Enough for the capture-budget tests:
    /// they exercise `Stream`'s own arm/release surface, not a mux.
    ///
    /// `context.method` is left UNSET deliberately. `arm_upstream_replay`
    /// captures only an idempotent request, so every test that means to arm
    /// one states its own method and none of them inherits that precondition
    /// from the fixture.
    fn test_stream(pool: &Rc<RefCell<Pool>>) -> Stream {
        let context = HttpContext::new(
            Ulid::generate(),
            Ulid::generate(),
            Protocol::HTTP,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080),
            Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
                54321,
            )),
            "SERVERID".to_owned(),
            "Sozu-Id".to_owned(),
            false,
            false,
        );
        Stream::new(
            &mut PoolBufferSource::new(Rc::downgrade(pool)),
            context,
            crate::protocol::mux::test_support::test_answers(),
            65535,
        )
        .expect("test stream checkout")
    }

    /// The current value of one proxy-level metric for this test thread.
    /// `METRICS` is a `thread_local!`, so this reads what this thread emitted;
    /// every assertion below is nonetheless a DELTA against a baseline taken
    /// at the top of the test, which stays correct under `--test-threads=1`
    /// where libtest reuses one thread. Mirrors `server.rs`'s
    /// `active_flows_gauge`.
    fn proxy_metric(name: &str) -> i64 {
        METRICS.with(|metrics| {
            metrics
                .borrow_mut()
                .dump_local_proxy_metrics()
                .get(name)
                .and_then(|metric| match metric.inner {
                    Some(filtered_metrics::Inner::Gauge(value)) => Some(value as i64),
                    Some(filtered_metrics::Inner::Count(value)) => Some(value),
                    _ => None,
                })
                .unwrap_or(0)
        })
    }

    /// Take every charge the budget has left, and hold them.
    ///
    /// Bounded by construction rather than by `while try_arm().is_some()`: a
    /// ceiling-less `try_arm` would never end that loop, and a test that hangs
    /// instead of failing proves nothing. Charges are taken through the same
    /// `ReplayCapture::try_arm` `Stream::arm_upstream_replay` calls, so this
    /// fills the real budget and not a stand-in for it.
    fn fill_the_capture_budget() -> Vec<ReplayCapture> {
        let held: Vec<ReplayCapture> = (0..MAX_ARMED_REPLAY_CAPTURES)
            .filter_map(|_| ReplayCapture::try_arm())
            .collect();
        // Reachability, not the property under test: the budget was empty, so
        // every charge was granted and it is now exactly full. It holds with
        // or without the CEILING, so removing the ceiling never reddens a test
        // here — but it does NOT hold under a mutation that leaks charges,
        // where it fires at `left: 511 / right: 512` before the assertion the
        // test is actually about. Any release-path mutation must therefore be
        // checked against a test that does not call this helper;
        // `queueing_a_replay_releases_the_capture_charge` is that test.
        assert_eq!(
            held.len(),
            MAX_ARMED_REPLAY_CAPTURES,
            "an empty budget must grant every one of its charges"
        );
        held
    }

    /// `queue_upstream_replay` is the one release path that works AROUND
    /// [`Drop`]: a `Vec` cannot be moved out of a type that implements it, so
    /// [`ReplayCapture::into_bytes`] swaps in an empty `Vec` and lets the husk
    /// drop. The simplification a later contributor reaches for — a
    /// `std::mem::forget(self)` to "avoid the pointless drop" — leaks the
    /// charge instead, one per replay, until the ceiling disarms replay for
    /// the rest of the worker's life with `backend.retry.captures_armed`
    /// pinned at 512.
    ///
    /// This is the release path that most needs a test. The other three
    /// clearing sites are plain `= None` assignments, where drop-on-assign is
    /// not something an edit can quietly remove; this one is a hand-written
    /// dance around `Drop`. `super::tests::
    /// queueing_a_replay_moves_the_captured_bytes_into_the_front_buffer`
    /// covers the bytes and the taken buffer, and looks at neither the budget
    /// nor the gauge.
    ///
    /// Deliberately does not call `fill_the_capture_budget`: that helper's own
    /// reachability assertion fires under a leaking mutation, and would hide
    /// this one.
    ///
    /// To SEE THIS RED: insert `std::mem::forget(self);` before the returned
    /// `bytes` in [`ReplayCapture::into_bytes`].
    #[test]
    fn queueing_a_replay_releases_the_capture_charge() {
        setup_test_logger!();
        const REQUEST: &[u8] = b"GET /api HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let pool = Rc::new(RefCell::new(Pool::with_capacity(2, 2, 16384)));
        let charged_before = ARMED_REPLAY_CAPTURES.get();
        let gauged_before = proxy_metric(names::backend::RETRY_CAPTURES_ARMED);

        let mut stream = test_stream(&pool);
        stream.context.method = Some(Method::Get);
        stream.arm_upstream_replay();
        stream
            .retry_buffer
            .as_mut()
            .expect("an idempotent request under the ceiling must be captured")
            .extend_from_slice(REQUEST);

        let queued = stream
            .queue_upstream_replay()
            .expect("the armed stream carries a capture");

        // Reachability: the capture really carried the request and really was
        // spent, so what is asserted below is the release of a SPENT capture
        // and not of an empty one.
        assert_eq!(
            queued,
            REQUEST.len(),
            "the whole captured request must be queued for the fresh upstream"
        );

        assert_eq!(
            ARMED_REPLAY_CAPTURES.get(),
            charged_before,
            "queueing a replay must release the capture's charge"
        );
        assert_eq!(
            proxy_metric(names::backend::RETRY_CAPTURES_ARMED),
            gauged_before,
            "queueing a replay must return the armed gauge to where it started"
        );
    }

    /// A non-idempotent request is not captured and spends no charge.
    /// [`Stream::can_replay_on_fresh_upstream`] vetoes the method, so the
    /// capture could only ever be held for the life of the attempt and thrown
    /// away. Before the ceiling that was waste; with one it is also a slot a
    /// replayable request cannot have — on a POST-heavy pooled workload it
    /// would be most of the budget.
    ///
    /// The idempotent half is load-bearing: without it this test would pass
    /// against an `arm_upstream_replay` that captured nothing at all.
    ///
    /// The final assertion pins the metric's MEANING, not just its value:
    /// `backend.retry.captures_declined` counts refusals for BUDGET, and a
    /// method veto is not one. Counting it there would make the counter read
    /// as lost retries when nothing was lost.
    ///
    /// To SEE THIS RED: delete the `Method::is_idempotent` early return from
    /// [`Stream::arm_upstream_replay`].
    #[test]
    fn a_non_idempotent_request_is_never_charged_to_the_capture_budget() {
        setup_test_logger!();
        let pool = Rc::new(RefCell::new(Pool::with_capacity(2, 2, 16384)));
        let charged_before = ARMED_REPLAY_CAPTURES.get();
        let declined_before = proxy_metric(names::backend::RETRY_CAPTURES_DECLINED);

        let mut idempotent = test_stream(&pool);
        idempotent.context.method = Some(Method::Get);
        idempotent.arm_upstream_replay();
        assert!(
            idempotent.retry_buffer.is_some(),
            "an idempotent request under the ceiling must be captured"
        );
        assert_eq!(
            ARMED_REPLAY_CAPTURES.get(),
            charged_before + 1,
            "an idempotent request must spend a charge"
        );
        drop(idempotent);

        // `PATCH` parses as `Method::Custom`, which is how sozu refuses every
        // method whose semantics it does not know.
        for method in [Method::Post, Method::new(b"PATCH")] {
            let mut stream = test_stream(&pool);
            stream.context.method = Some(method);

            stream.arm_upstream_replay();

            assert!(
                stream.retry_buffer.is_none(),
                "a non-idempotent request must not be captured"
            );
            assert_eq!(
                ARMED_REPLAY_CAPTURES.get(),
                charged_before,
                "a non-idempotent request must not spend a capture charge"
            );
        }

        assert_eq!(
            proxy_metric(names::backend::RETRY_CAPTURES_DECLINED) - declined_before,
            0,
            "a method veto is not a budget refusal and must not be counted as one"
        );
    }

    /// The ceiling of sozu-proxy/sozu#1450. Past [`MAX_ARMED_REPLAY_CAPTURES`]
    /// armed captures, [`Stream::arm_upstream_replay`] installs NO buffer —
    /// and the request it belongs to still completes, un-replayable, answered
    /// with exactly the `502 Bad Gateway` `end_stream_decision` returned for
    /// every consumed request before the replay existed. Nothing is refused,
    /// no frontend parks, no session is answered `503`: that graceful
    /// degradation is the whole reason the captures were not moved into the
    /// buffer [`Pool`].
    ///
    /// The under-ceiling half is load-bearing. Without it this test would pass
    /// against an `arm_upstream_replay` that armed nothing at all, which is
    /// the same observation as "past the ceiling it arms nothing".
    ///
    /// To SEE THIS RED: in [`ReplayCapture::try_arm`], delete the
    /// `if armed >= MAX_ARMED_REPLAY_CAPTURES { return None; }` early return so
    /// the budget is never consulted.
    #[test]
    fn past_the_ceiling_a_pooled_request_is_armed_with_no_buffer_and_still_completes() {
        setup_test_logger!();
        let pool = Rc::new(RefCell::new(Pool::with_capacity(2, 2, 16384)));

        let mut under_the_ceiling = test_stream(&pool);
        under_the_ceiling.context.method = Some(Method::Get);
        under_the_ceiling.arm_upstream_replay();
        assert!(
            under_the_ceiling.retry_buffer.is_some(),
            "under the ceiling arm_upstream_replay must install a capture"
        );
        // Releases both the charge and the two pool buffers the next stream
        // checks out.
        drop(under_the_ceiling);

        let _held = fill_the_capture_budget();

        let mut stream = test_stream(&pool);
        stream.context.method = Some(Method::Get);
        stream.arm_upstream_replay();
        assert!(
            stream.retry_buffer.is_none(),
            "past the ceiling arm_upstream_replay must install no capture"
        );

        // The request was written to the upstream (`front.consumed`) and the
        // upstream produced nothing — the exact shape sozu-proxy/sozu#1442
        // replays. With no capture it is not replayable, and the answer is the
        // pre-#1442 one rather than a refusal.
        stream.front.consumed = true;
        assert_eq!(
            shared::end_stream_decision(&stream),
            EndStreamAction::SendDefault(502),
            "a request the budget declined to capture must still be answered, \
             un-replayable, exactly as it was before the replay existed"
        );
    }

    /// The two metrics of sozu-proxy/sozu#1450.
    /// `backend.retry.captures_armed` follows arm and release;
    /// `backend.retry.captures_declined` moves when — and only when — a
    /// request is refused a capture for budget. An invisible heap
    /// proportional to a fraction of the pool is not an operational signal;
    /// these two are.
    ///
    /// To SEE THIS RED, one revert per assertion:
    /// - the arm gauge: delete the
    ///   `gauge_add!(names::backend::RETRY_CAPTURES_ARMED, 1)` from
    ///   [`ReplayCapture::try_arm`];
    /// - the release gauge: delete the matching
    ///   `gauge_add!(names::backend::RETRY_CAPTURES_ARMED, -1)` from
    ///   `ReplayCapture`'s `impl Drop`;
    /// - the declined counter: replace
    ///   `None => incr!(names::backend::RETRY_CAPTURES_DECLINED)` in
    ///   [`Stream::arm_upstream_replay`] with `None => {}`.
    #[test]
    fn the_armed_gauge_and_the_declined_counter_track_the_capture_budget() {
        setup_test_logger!();
        let pool = Rc::new(RefCell::new(Pool::with_capacity(2, 2, 16384)));
        let armed_before = proxy_metric(names::backend::RETRY_CAPTURES_ARMED);
        let declined_before = proxy_metric(names::backend::RETRY_CAPTURES_DECLINED);

        let mut stream = test_stream(&pool);
        stream.context.method = Some(Method::Get);
        stream.arm_upstream_replay();
        assert_eq!(
            proxy_metric(names::backend::RETRY_CAPTURES_ARMED) - armed_before,
            1,
            "arming a capture must raise the armed gauge by exactly one"
        );
        assert_eq!(
            proxy_metric(names::backend::RETRY_CAPTURES_DECLINED) - declined_before,
            0,
            "a capture the budget granted must not be counted as declined"
        );

        stream.forget_upstream_replay();
        assert_eq!(
            proxy_metric(names::backend::RETRY_CAPTURES_ARMED) - armed_before,
            0,
            "releasing a capture must return the armed gauge to where it started"
        );

        let _held = fill_the_capture_budget();
        let armed_at_the_ceiling = proxy_metric(names::backend::RETRY_CAPTURES_ARMED);

        stream.arm_upstream_replay();

        assert_eq!(
            proxy_metric(names::backend::RETRY_CAPTURES_DECLINED) - declined_before,
            1,
            "a capture refused for budget must increment the declined counter \
             exactly once"
        );
        assert_eq!(
            proxy_metric(names::backend::RETRY_CAPTURES_ARMED),
            armed_at_the_ceiling,
            "a declined capture installs nothing, so it must not move the \
             armed gauge"
        );
    }

    /// A stream torn down while a capture is still armed — a client hangup, an
    /// idle timeout, a session teardown — must release its charge.
    ///
    /// That path has no code site of its own: nothing clears `retry_buffer`
    /// there, the `Stream` is simply dropped. Releasing at the four sites that
    /// DO clear the field would leak a charge on every one of those teardowns
    /// until the budget was exhausted, at which point replay would be silently
    /// disarmed for the rest of the worker's life. So the release belongs to
    /// [`ReplayCapture`]'s `impl Drop` and nowhere else — the reasoning
    /// `super::super::h2::ConnectionH2`'s gauge teardown and
    /// [`crate::pool::Checkout`] already apply.
    ///
    /// To SEE THIS RED: delete the `ARMED_REPLAY_CAPTURES.set(...)` line from
    /// `ReplayCapture`'s `impl Drop`, leaving its `gauge_add!` in place. Its
    /// `debug_assert!` does not fire on this path — the counter is 1, not 0 —
    /// so the failure is this test's own.
    #[test]
    fn a_stream_dropped_with_a_capture_armed_releases_its_charge() {
        setup_test_logger!();
        let pool = Rc::new(RefCell::new(Pool::with_capacity(2, 2, 16384)));
        let armed_before = ARMED_REPLAY_CAPTURES.get();

        let mut stream = test_stream(&pool);
        stream.context.method = Some(Method::Get);
        stream.arm_upstream_replay();
        assert_eq!(
            ARMED_REPLAY_CAPTURES.get(),
            armed_before + 1,
            "arming a capture must charge the budget"
        );

        drop(stream);

        assert_eq!(
            ARMED_REPLAY_CAPTURES.get(),
            armed_before,
            "a stream dropped with a capture still armed must release its charge"
        );
    }

    /// A backend response status line is wire data, not a Sōzu-side invariant.
    ///
    /// kawa 0.7.1 reads the status with `take(3)` followed by
    /// `str::parse::<u16>()` and applies no range check
    /// (`kawa/src/protocol/h1/parser/primitives.rs:194-203`); the
    /// `Kind::Response` arm stores the resulting `code` verbatim
    /// (`.../h1/parser/mod.rs:249-258`). `"000".parse::<u16>()` is `Ok(0)`, so
    /// a backend that answers `HTTP/1.1 000 …` drives `code == 0` through
    /// [`HttpContext::on_response_headers`] into `context.status` and then into
    /// the status bucketer at the top of [`Stream::generate_access_log`], which
    /// buckets it as `http.status.other` — exactly what the catch-all arm is
    /// for.
    ///
    /// Ported on 2026-09-20 from
    /// `kawa_h1::tests::a_backend_status_line_below_100_is_bucketed_not_asserted`,
    /// which asserted the same property against `kawa_h1::save_http_status_metric`.
    /// That function and the `kawa_h1::Http` session it belonged to were removed
    /// with the unreachable H1 state machine (sozu#1346), so the assertion now
    /// guards the LIVE mux bucketer instead of a path no binary could enter.
    ///
    /// To SEE THIS RED: in [`Stream::generate_access_log`], insert
    /// `debug_assert!((100..=999).contains(&status), "generate_access_log got a
    /// non-3-digit status: {status}");` as the first statement of the
    /// `if let Some(status) = context.status` bucket arm — this test then panics
    /// with `generate_access_log got a non-3-digit status: 0`, i.e. a panic on
    /// network input in every debug, test, e2e and fuzz build (sozu#1279).
    #[test]
    fn a_backend_status_line_below_100_is_bucketed_not_asserted() {
        setup_test_logger!();
        const RESPONSE: &[u8] = b"HTTP/1.1 000 Nope\r\nContent-Length: 0\r\n\r\n";

        let pool = Rc::new(RefCell::new(Pool::with_capacity(2, 2, 4096)));
        let context = HttpContext::new(
            Ulid::generate(),
            Ulid::generate(),
            Protocol::HTTP,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080),
            Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
                54321,
            )),
            "SERVERID".to_owned(),
            "Sozu-Id".to_owned(),
            false,
            false,
        );
        let mut stream = Stream::new(
            &mut PoolBufferSource::new(Rc::downgrade(&pool)),
            context,
            crate::protocol::mux::test_support::test_answers(),
            65535,
        )
        .expect("test stream checkout");

        let space = stream.back.storage.space();
        space[..RESPONSE.len()].copy_from_slice(RESPONSE);
        stream.back.storage.fill(RESPONSE.len());
        kawa::h1::parse(&mut stream.back, &mut stream.context);

        // Reachability: the value handed to the metric bucketer came off the
        // wire through the real parser and the real response-header callback,
        // not from a hand-written `Some(0)`.
        assert_eq!(
            stream.context.status,
            Some(0),
            "kawa must surface the backend's out-of-range status verbatim"
        );

        // The call itself is the assertion: with a range precondition in place
        // this panics instead of incrementing `http.status.other`.
        // This test asserts on the access log, not on metrics.
        let _ = stream.generate_access_log(
            false,
            None,
            Rc::new(RefCell::new(TestListener::new())),
            None,
            None,
        );
    }
}
