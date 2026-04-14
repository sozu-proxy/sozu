//! Helpers shared between the H1 and H2 multiplexers.
//!
//! Both protocol state machines have converged on a handful of small
//! routines that operate on SocketHandler and StreamState in the same way.
//! Keeping them here prevents drift between the two write paths — which is
//! load-bearing for TLS close_notify ordering (see agent memory
//! `feedback_tls_write_symmetry`).

use crate::socket::{SocketHandler, SocketResult};

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
