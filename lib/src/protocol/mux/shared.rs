//! Helpers shared between the H1 and H2 multiplexers.
//!
//! Both protocol state machines have converged on a handful of small
//! routines that operate on SocketHandler and StreamState in the same way.
//! Keeping them here prevents drift between the two write paths — which is
//! load-bearing for TLS close_notify ordering (see agent memory
//! `feedback_tls_write_symmetry`).

use crate::socket::{SocketHandler, SocketResult};

use super::Stream;

/// Decision returned by [`end_stream_decision`] for the server-side end-of-stream path.
///
/// Both H1 and H2 must take the same action when a server-side stream ends, based
/// on whether the request was already partially consumed and whether the backend
/// has produced a forwardable response. Centralising the decision here keeps the
/// two protocols from drifting (e.g. H1 used to send 503 where H2 sent 502 for
/// "backend closed before any response").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EndStreamAction {
    /// Backend already produced a fully terminated response — finish forwarding
    /// it and mark the stream `Unlinked`.
    ForwardTerminated,
    /// Backend produced a partial response that the caller cannot finish (no
    /// keep-alive AND no Content-Length); mark the response terminated so the
    /// converter emits a final DATA frame with END_STREAM.
    CloseDelimited,
    /// Backend produced a partial response but the connection is keep-alive
    /// (or otherwise expected to terminate cleanly): the backend went away
    /// mid-response — caller must forcefully terminate with an internal error.
    ForwardUnterminated,
    /// No response is available and the request was already partially consumed,
    /// so retrying is unsafe — send the given default status (502 Bad Gateway).
    SendDefault(u16),
    /// No response is available and the request is untouched, so the caller may
    /// link the stream to a fresh backend and retry.
    Reconnect,
}

/// Compute the canonical end-of-stream decision for a server-side stream.
///
/// Mirrors the H1 and H2 server end-of-stream logic so both protocols agree on
/// the outcome (in particular, "backend closed without response" is normalised
/// to **502 Bad Gateway**).
pub(super) fn end_stream_decision(stream: &Stream) -> EndStreamAction {
    if stream.back.is_main_phase() {
        if stream.back.is_terminated() {
            EndStreamAction::ForwardTerminated
        } else if !stream.context.keep_alive_backend {
            EndStreamAction::CloseDelimited
        } else {
            EndStreamAction::ForwardUnterminated
        }
    } else if stream.front.consumed {
        EndStreamAction::SendDefault(502)
    } else {
        EndStreamAction::Reconnect
    }
}

/// Drain rustls's pending TLS output before the underlying TCP socket is
/// shut down. A single write attempt is insufficient when the kernel send
/// buffer is full (common during large-response transfers): without this
/// drain loop, a partial TLS record can be left in-flight, producing
/// "TLS decode error / unexpected eof" on the client side.
///
/// Emits close_notify the first time it is called for a given session
/// (tracked by the caller's `close_notify_sent` flag) and then attempts
/// up to `MAX_DRAIN_ROUNDS` empty-vectored writes to flush rustls.
///
/// Returns `(tls_still_pending, drain_rounds)` so the caller can log or
/// react to an incomplete drain.
pub(super) fn drain_tls_close_notify<S: SocketHandler>(
    socket: &mut S,
    close_notify_sent: &mut bool,
) -> (bool, u32) {
    const MAX_DRAIN_ROUNDS: u32 = 16;
    if !*close_notify_sent {
        socket.socket_close();
        *close_notify_sent = true;
    }
    let mut drain_rounds = 0;
    while socket.socket_wants_write() && drain_rounds < MAX_DRAIN_ROUNDS {
        let (_size, status) = socket.socket_write_vectored(&[]);
        drain_rounds += 1;
        match status {
            SocketResult::WouldBlock | SocketResult::Error | SocketResult::Closed => break,
            SocketResult::Continue => {}
        }
    }
    (socket.socket_wants_write(), drain_rounds)
}
