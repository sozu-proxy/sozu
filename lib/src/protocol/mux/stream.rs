//! Per-request stream state shared by the H1 and H2 mux paths.
//!
//! A [`Stream`] owns the front/back kawa buffers, HTTP context, and metrics
//! for a single request/response pair. [`StreamParts`] splits it along the
//! read/write axis so callers can borrow both sides of the pipe at the same
//! time without fighting the borrow checker.

use std::{
    cell::RefCell,
    fmt::Debug,
    rc::{Rc, Weak},
    time::Duration,
};

use mio::Token;
use sozu_command::logging::ansi_palette;

use super::{GenericHttpStream, Position};
use crate::metrics::names;
use crate::{
    L7ListenerHandler, ListenerHandler, Protocol, SessionMetrics, pool::Pool,
    protocol::http::editor::HttpContext,
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
    pub context: HttpContext,
    pub metrics: SessionMetrics,
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
}

impl Stream {
    pub fn new(pool: Weak<RefCell<Pool>>, context: HttpContext, window: u32) -> Option<Self> {
        let (front_buffer, back_buffer) = match pool.upgrade() {
            Some(pool) => {
                let mut pool = pool.borrow_mut();
                match (pool.checkout(), pool.checkout()) {
                    (Some(front_buffer), Some(back_buffer)) => (front_buffer, back_buffer),
                    _ => return None,
                }
            }
            None => return None,
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
            context,
            metrics: SessionMetrics::new(None),
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
            },
            Position::Server => StreamParts {
                window: &mut self.window,
                rbuffer: &mut self.front,
                wbuffer: &mut self.back,
                received_end_of_stream: &mut self.front_received_end_of_stream,
                data_received: &mut self.front_data_received,
                context: &mut self.context,
                metrics: &mut self.metrics,
            },
        }
    }
    /// Emit the access log for this stream.
    ///
    /// `client_rtt`/`server_rtt` are passed in by the caller because the
    /// `Stream` does not own a socket reference — the frontend socket lives
    /// on the parent `Mux`/connection and the backend socket lives on
    /// `Router.backends.get(token)`. Each caller snapshots the two
    /// `getsockopt(TCP_INFO)` values from the sockets it can reach, mirroring
    /// the inline pattern used by the `kawa_h1`, `pipe`, and TCP-frontend
    /// access-log sites.
    pub fn generate_access_log<L>(
        &mut self,
        error: bool,
        message: Option<&str>,
        listener: Rc<RefCell<L>>,
        client_rtt: Option<Duration>,
        server_rtt: Option<Duration>,
    ) where
        L: ListenerHandler + L7ListenerHandler,
    {
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
            gauge_add!(names::http::ACTIVE_REQUESTS, -1);
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
            // Labelled with `(cluster_id, backend_id)`; see the matching
            // emission in `kawa_h1::log_request_error` for the cardinality
            // contract (`metrics::filter_labels_for_detail`).
            incr!(
                "http.errors",
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
        // the short-list shared with the H1 path (`save_http_status_metric`).
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

        if let Some(status) = context.status {
            if let Some(per_code) = crate::metrics::http_status_code_metric_name(status) {
                incr!(
                    per_code,
                    context.cluster_id.as_deref(),
                    context.backend_id.as_deref()
                );
            }
        }

        let endpoint = sozu_command::logging::EndpointRecord::Http {
            method: context.method.as_deref(),
            authority: context.authority.as_deref(),
            path: context.path.as_deref(),
            reason: context.reason.as_deref(),
            status: context.status,
        };

        let listener = listener.borrow();
        let tags = context.authority.as_deref().and_then(|host| {
            let hostname = match host.split_once(':') {
                None => host,
                Some((hostname, _)) => hostname,
            };
            listener.get_tags(hostname)
        });

        log_access! {
            error,
            on_failure: { incr!(names::access_logs::UNSENT) },
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
    }
}
