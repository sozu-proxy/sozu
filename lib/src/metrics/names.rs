//! Canonical metric-name constants.
//!
//! Every metric string emitted by Sōzu and consumed by the StatsD/Prometheus/
//! TUI surface should reference a constant declared here rather than being
//! repeated as a literal. The constants intentionally preserve the dotted
//! string values byte-for-byte so dashboards and scrapers stay valid.
//!
//! Layout: one submodule per metric family (`http`, `h2`, `tcp`, `tls`, …),
//! constants inside are `UPPER_SNAKE_CASE` derived from the dotted suffix.
//! E.g. `h2.frames.tx.data` lives at [`h2::FRAMES_TX_DATA`].
//!
//! When adding a new metric:
//! 1. Add the constant in the matching submodule (create one if needed).
//! 2. Reference the constant from the emission site
//!    (`incr!(names::h2::FRAMES_TX_DATA)`) instead of repeating the literal.
//! 3. If the TUI or any scraper reads this metric, reference the same
//!    constant on the read side too.

/// Accept-queue counters and gauges, fed by the worker accept loop in
/// `lib/src/server.rs`.
pub mod accept_queue {
    pub const BACKPRESSURE: &str = "accept_queue.backpressure";
    pub const CONNECTIONS: &str = "accept_queue.connections";
    pub const SATURATED_SECONDS: &str = "accept_queue.saturated_seconds";
    pub const TIMEOUT: &str = "accept_queue.timeout";
    pub const WAIT_TIME: &str = "accept_queue.wait_time";
}

/// Access-log infrastructure counters.
pub mod access_logs {
    pub const COUNT: &str = "access_logs.count";
    pub const UNSENT: &str = "unsent-access-logs";
}

/// Per-backend bandwidth + connection counters emitted by both H1 and the
/// H2 mux. Front-bytes and back-bytes are accounted separately so dashboards
/// can compare upstream vs downstream traffic.
pub mod backend {
    pub const BYTES_IN: &str = "bytes_in";
    pub const BYTES_OUT: &str = "bytes_out";
    pub const BACK_BYTES_IN: &str = "back_bytes_in";
    pub const BACK_BYTES_OUT: &str = "back_bytes_out";
    pub const AVAILABLE: &str = "backend.available";
    pub const CONNECTIONS: &str = "backend.connections";
    pub const FLOW_CONTROL_PAUSED: &str = "backend.flow_control.paused";
    pub const POOL_HIT: &str = "backend.pool.hit";
    pub const POOL_MISS: &str = "backend.pool.miss";
    pub const POOL_SIZE: &str = "backend.pool.size";
    pub const CONNECTIONS_PER_BACKEND: &str = "connections_per_backend";
    pub const CONNECTION_TIME: &str = "backend_connection_time";
    pub const RESPONSE_TIME: &str = "backend_response_time";
    pub const REQUESTS: &str = "requests";
    pub const FAIL_OPEN: &str = "backends.fail_open";
}

/// Buffer-pool gauges and counters.
pub mod buffer {
    pub const CAPACITY: &str = "buffer.capacity";
    pub const IN_USE: &str = "buffer.in_use";
    pub const USAGE_PERCENT: &str = "buffer.usage_percent";
}

/// Client-side connection gauges, populated by the worker accept loop.
pub mod client {
    pub const CONNECTIONS: &str = "client.connections";
    pub const CONNECTIONS_MAX: &str = "client.connections_max";
}

/// Per-cluster aggregate gauges.
pub mod cluster {
    pub const AVAILABLE_BACKENDS: &str = "cluster.available_backends";
    pub const AVAILABLE_RECOVERED: &str = "cluster.available_recovered";
    pub const NO_AVAILABLE_BACKENDS: &str = "cluster.no_available_backends";
    pub const TOTAL_BACKENDS: &str = "cluster.total_backends";
}

/// Configuration-state inventory gauges, refreshed when the master
/// fans out a state change.
pub mod configuration {
    pub const BACKENDS: &str = "configuration.backends";
    pub const CLUSTERS: &str = "configuration.clusters";
    pub const FRONTENDS: &str = "configuration.frontends";
}

/// Event-loop timing counters.
pub mod event_loop {
    pub const EPOLL_TIME: &str = "epoll_time";
    pub const EVENT_LOOP_TIME: &str = "event_loop_time";
    pub const FRONTEND_MATCHING_TIME: &str = "frontend_matching_time";
    pub const REGEX_MATCHING_TIME: &str = "regex_matching_time";
    pub const REQUEST_TIME: &str = "request_time";
    pub const SERVICE_TIME: &str = "service_time";
}

/// Health-check transition counters.
pub mod health_check {
    pub const UP: &str = "health_check.up";
    pub const DOWN: &str = "health_check.down";
    pub const SUCCESS: &str = "health_check.success";
    pub const FAILURE: &str = "health_check.failure";
}

/// H1 protocol counters.
pub mod h1 {
    pub const BACKEND_EOF_BEFORE_MESSAGE_COMPLETE: &str = "h1.backend_eof_before_message_complete";
}

/// H2 multiplexer counters — frame TX, flood mitigations, header policy
/// rejections, signal-writable rearm sites, and other H2-specific buckets.
pub mod h2 {
    pub const CLOSE_WITH_ACTIVE_STREAMS: &str = "h2.close_with_active_streams";
    pub const COALESCING_ACCEPTED: &str = "h2.coalescing.accepted";
    pub const CONNECTION_ACTIVE_STREAMS: &str = "h2.connection.active_streams";
    pub const CONNECTION_PENDING_WINDOW_UPDATES: &str = "h2.connection.pending_window_updates";
    pub const CONNECTION_WINDOW_BYTES: &str = "h2.connection.window_bytes";
    pub const FLOW_CONTROL_STALL: &str = "h2.flow_control_stall";

    // Per-stream reap counters — `cancel_timed_out_streams` breaks reaps down by
    // which guard tripped (M1), plus a stall-budget subset for the
    // `WINDOW_UPDATE`-drip vector the cumulative-stall budget closes (M2).
    // `reaped.stall_budget` is a subset of `reaped.window_stall`.
    pub const STREAMS_REAPED_IDLE_TIMEOUT: &str = "h2.streams.reaped.idle_timeout";
    pub const STREAMS_REAPED_WINDOW_STALL: &str = "h2.streams.reaped.window_stall";
    pub const STREAMS_REAPED_STALL_BUDGET: &str = "h2.streams.reaped.stall_budget";

    // Frame-TX counters (frame type fanout).
    pub const FRAMES_TX_CONTINUATION: &str = "h2.frames.tx.continuation";
    pub const FRAMES_TX_DATA: &str = "h2.frames.tx.data";
    pub const FRAMES_TX_GOAWAY: &str = "h2.frames.tx.goaway";
    pub const FRAMES_TX_HEADERS: &str = "h2.frames.tx.headers";
    pub const FRAMES_TX_PING_ACK: &str = "h2.frames.tx.ping_ack";
    pub const FRAMES_TX_RST_STREAM: &str = "h2.frames.tx.rst_stream";
    pub const FRAMES_TX_SETTINGS: &str = "h2.frames.tx.settings";
    pub const FRAMES_TX_WINDOW_UPDATE: &str = "h2.frames.tx.window_update";

    // Header-policy rejections (the `h2.headers.rejected.*` family).
    pub const HEADERS_NO_STREAM_ERROR: &str = "h2.headers_no_stream.error";
    pub const HEADERS_REJECTED_BUDGET_OVERRUN: &str = "h2.headers.rejected.budget_overrun";
    pub const HEADERS_REJECTED_TOTAL: &str = "h2.headers.rejected.total";

    pub const RST_STREAM_RECEIVED_PRE_RESPONSE_START: &str =
        "h2.rst_stream.received.pre_response_start";

    // Writable-rearm signal counters — one per rearm reason.
    pub const SIGNAL_WRITABLE_REARMED_CONTROL_QUEUE: &str =
        "h2.signal.writable.rearmed.control_queue";
    pub const SIGNAL_WRITABLE_REARMED_DEFAULT_ANSWER: &str =
        "h2.signal.writable.rearmed.default_answer";
    pub const SIGNAL_WRITABLE_REARMED_FORCEFULLY_TERMINATE_ANSWER: &str =
        "h2.signal.writable.rearmed.forcefully_terminate_answer";
    pub const SIGNAL_WRITABLE_REARMED_PEER_DATA: &str = "h2.signal.writable.rearmed.peer_data";
    pub const SIGNAL_WRITABLE_REARMED_PEER_HEADERS: &str =
        "h2.signal.writable.rearmed.peer_headers";
    pub const SIGNAL_WRITABLE_REARMED_PRIORITY_UPDATE: &str =
        "h2.signal.writable.rearmed.priority_update";

    pub const TRAILERS_DROPPED_CONTENT_LENGTH: &str = "h2.trailers_dropped_content_length";
    pub const TRAILER_SPOOF_VECTOR_ELIDED: &str = "h2.trailer.spoof_vector_elided";
    pub const WINDOW_UPDATE_DROPPED: &str = "h2.window_update_dropped";

    // Flood-mitigation violation counters — one per flood class the H2
    // mux's `H2FloodDetector` recognises. Surfaced in the TUI's H2 pane
    // so operators can spot a flood-pattern before it pages.
    pub const FLOOD_VIOLATION_CONTINUATION: &str = "h2.flood.violation.continuation";
    pub const FLOOD_VIOLATION_GLITCH_WINDOW: &str = "h2.flood.violation.glitch_window";
    pub const FLOOD_VIOLATION_MADE_YOU_RESET: &str = "h2.flood.violation.made_you_reset";
    pub const FLOOD_VIOLATION_PING: &str = "h2.flood.violation.ping";
    pub const FLOOD_VIOLATION_PRIORITY: &str = "h2.flood.violation.priority";
    pub const FLOOD_VIOLATION_RAPID_RESET: &str = "h2.flood.violation.rapid_reset";
    pub const FLOOD_VIOLATION_SETTINGS: &str = "h2.flood.violation.settings";
}

/// HTTP counters (H1 + H2 share these); see `https` for the HTTPS-specific
/// variants and `h2` for H2-frame-level counters.
pub mod http {
    pub const ERR_400: &str = "http.400.errors";
    pub const ERR_404: &str = "http.404.errors";
    pub const ACTIVE_REQUESTS: &str = "http.active_requests";
    pub const ALPN_H2: &str = "http.alpn.h2";
    pub const ALPN_HTTP11: &str = "http.alpn.http11";
    pub const BACKEND_PARSE_ERRORS: &str = "http.backend_parse_errors";
    pub const E2E_H2: &str = "http.e2e.h2";
    pub const E2E_HTTP11: &str = "http.e2e.http11";
    pub const EARLY_RESPONSE_CLOSE: &str = "http.early_response_close";
    pub const FAILED_BACKEND_MATCHING: &str = "http.failed_backend_matching";
    pub const FRONTEND_PARSE_ERRORS: &str = "http.frontend_parse_errors";
    pub const HSTS_FRONTEND_ADDED: &str = "http.hsts.frontend_added";
    pub const HSTS_FRONTEND_REFRESHED: &str = "http.hsts.frontend_refreshed";
    pub const HSTS_LISTENER_DEFAULT_PATCHED: &str = "http.hsts.listener_default_patched";
    pub const HSTS_SUPPRESSED_PLAINTEXT: &str = "http.hsts.suppressed_plaintext";
    pub const HSTS_UNRENDERED: &str = "http.hsts.unrendered";
    pub const INFINITE_LOOP_ERROR: &str = "http.infinite_loop.error";
    pub const REDIRECT_TEMPLATE_COMPILE_ERROR: &str = "http.redirect_template.compile_error";
    pub const REQUESTS: &str = "http.requests";
    pub const SNI_AUTHORITY_MISMATCH: &str = "http.sni_authority_mismatch";
    pub const STATUS_1XX: &str = "http.status.1xx";
    pub const STATUS_2XX: &str = "http.status.2xx";
    pub const STATUS_3XX: &str = "http.status.3xx";
    pub const STATUS_4XX: &str = "http.status.4xx";
    pub const STATUS_5XX: &str = "http.status.5xx";
    pub const STATUS_OTHER: &str = "http.status.other";
    pub const TRUSTING_X_PORT: &str = "http.trusting.x_port";
    pub const TRUSTING_X_PORT_DIFF: &str = "http.trusting.x_port.diff";
    pub const TRUSTING_X_PROTO: &str = "http.trusting.x_proto";
    pub const TRUSTING_X_PROTO_DIFF: &str = "http.trusting.x_proto.diff";
    pub const UPGRADE_EXPECT_FAILED: &str = "http.upgrade.expect.failed";
    pub const UPGRADE_MUX_FAILED: &str = "http.upgrade.mux.failed";
    pub const UPGRADE_WS_FAILED: &str = "http.upgrade.ws.failed";
    pub const X_REQUEST_ID_GENERATED: &str = "http.x_request_id.generated";
    pub const X_REQUEST_ID_PROPAGATED: &str = "http.x_request_id.propagated";
}

/// Per-status-code HTTP counters. Only the codes Sōzu either generates as
/// a default answer or that operators routinely chart get a dedicated bucket;
/// the rest fold into the `http::STATUS_*XX` parent buckets.
pub mod http_status {
    pub const S200: &str = "http.status.200";
    pub const S201: &str = "http.status.201";
    pub const S204: &str = "http.status.204";
    pub const S301: &str = "http.status.301";
    pub const S302: &str = "http.status.302";
    pub const S304: &str = "http.status.304";
    pub const S400: &str = "http.status.400";
    pub const S401: &str = "http.status.401";
    pub const S403: &str = "http.status.403";
    pub const S404: &str = "http.status.404";
    pub const S408: &str = "http.status.408";
    pub const S413: &str = "http.status.413";
    pub const S429: &str = "http.status.429";
    pub const S500: &str = "http.status.500";
    pub const S502: &str = "http.status.502";
    pub const S503: &str = "http.status.503";
    pub const S504: &str = "http.status.504";
    pub const S507: &str = "http.status.507";
}

/// HTTPS-specific counters; see `http` for the HTTP family these complement.
pub mod https {
    pub const ALPN_REJECTED_HTTP11_DISABLED: &str = "https.alpn.rejected.http11_disabled";
    pub const ALPN_REJECTED_UNSUPPORTED: &str = "https.alpn.rejected.unsupported";
    pub const UPGRADE_EXPECT_FAILED: &str = "https.upgrade.expect.failed";
    pub const UPGRADE_HANDSHAKE_FAILED: &str = "https.upgrade.handshake.failed";
    pub const UPGRADE_MUX_FAILED: &str = "https.upgrade.mux.failed";
    pub const UPGRADE_WSS_FAILED: &str = "https.upgrade.wss.failed";
}

/// Per-listener counters.
pub mod listener {
    pub const ACCEPTED_TOTAL: &str = "listener.accepted.total";
    pub const CONNECTION_CAPPED: &str = "listener.connection_capped";
}

/// Pipe-protocol counters.
pub mod pipe {
    pub const ERRORS: &str = "pipe.errors";
}

/// Protocol-type counters that increment once per session and track which
/// protocol carried it end-to-end.
pub mod protocol {
    pub const HTTP: &str = "protocol.http";
    pub const HTTPS: &str = "protocol.https";
    pub const PROXY_EXPECT: &str = "protocol.proxy.expect";
    pub const PROXY_RELAY: &str = "protocol.proxy.relay";
    pub const PROXY_SEND: &str = "protocol.proxy.send";
    pub const TCP: &str = "protocol.tcp";
    pub const TLS_HANDSHAKE: &str = "protocol.tls.handshake";
    pub const WS: &str = "protocol.ws";
    pub const WSS: &str = "protocol.wss";
}

/// PROXY-protocol counters.
pub mod proxy_protocol {
    pub const ERRORS: &str = "proxy_protocol.errors";
}

/// `rustls`-specific counters for read/write infinite-loop guards.
pub mod rustls {
    pub const READ_ERROR: &str = "rustls.read.error";
    pub const READ_INFINITE_LOOP_ERROR: &str = "rustls.read.infinite_loop.error";
    pub const WRITE_ERROR: &str = "rustls.write.error";
    pub const WRITE_INFINITE_LOOP_ERROR: &str = "rustls.write.infinite_loop.error";
}

/// Generic session-level counters.
pub mod sessions {
    pub const EVICTED: &str = "sessions.evicted";
}

/// Slab-allocator gauges.
pub mod slab {
    pub const CAPACITY: &str = "slab.capacity";
    pub const ENTRIES: &str = "slab.entries";
    pub const USAGE_PERCENT: &str = "slab.usage_percent";
}

/// Raw-socket counters.
pub mod socket {
    pub const READ_INFINITE_LOOP_ERROR: &str = "socket.read.infinite_loop.error";
    pub const WRITE_INFINITE_LOOP_ERROR: &str = "socket.write.infinite_loop.error";
}

/// TCP protocol counters.
pub mod tcp {
    pub const INFINITE_LOOP_ERROR: &str = "tcp.infinite_loop.error";
    pub const READ_ERROR: &str = "tcp.read.error";
    pub const REQUESTS: &str = "tcp.requests";
    pub const UPGRADE_EXPECT_FAILED: &str = "tcp.upgrade.expect.failed";
    pub const UPGRADE_PIPE_FAILED: &str = "tcp.upgrade.pipe.failed";
    pub const UPGRADE_RELAY_FAILED: &str = "tcp.upgrade.relay.failed";
    pub const UPGRADE_SEND_FAILED: &str = "tcp.upgrade.send.failed";
    pub const WRITE_ERROR: &str = "tcp.write.error";
}

/// UDP datapath counters/gauges, fed by the UDP I/O shell in `lib/src/udp.rs`.
///
/// `IN`/`OUT` follow the client perspective: `IN` = client→backend
/// (request), `OUT` = backend→client (reply). `ACTIVE_FLOWS` is a gauge that
/// must never underflow — it is incremented exactly once per admitted flow
/// (`FLOWS_CREATED`) and decremented exactly once per close.
pub mod udp {
    /// Client→backend datagrams forwarded.
    pub const DATAGRAMS_IN: &str = "udp.datagrams.in";
    /// Backend→client datagrams returned.
    pub const DATAGRAMS_OUT: &str = "udp.datagrams.out";
    /// Payload bytes forwarded client→backend (excludes PPv2 prefix).
    pub const BYTES_IN: &str = "udp.bytes.in";
    /// Payload bytes returned backend→client.
    pub const BYTES_OUT: &str = "udp.bytes.out";
    /// Gauge: currently admitted flows. Never underflows.
    pub const ACTIVE_FLOWS: &str = "udp.active_flows";
    /// Flows admitted since boot.
    pub const FLOWS_CREATED: &str = "udp.flows.created";
    /// Flows torn down (idle / teardown / drain).
    pub const FLOWS_EVICTED: &str = "udp.flows.evicted";
    /// New flows shed at the `max_flows` cap or under fd pressure.
    pub const FLOWS_SHED: &str = "udp.flows.shed";
    /// Datagrams dropped before allocation (aggregate). Reason-specific
    /// counters below carry the dotted `.<reason>` suffix; both are emitted
    /// so dashboards can chart the total or break it down. The `incr!` macro
    /// needs `&'static str`, so the by-reason names are fixed constants
    /// rather than a runtime-formatted suffix.
    pub const DATAGRAMS_DROPPED: &str = "udp.datagrams.dropped";
    /// Dropped: failed validation (empty / extractor rejected).
    pub const DROPPED_INVALID: &str = "udp.datagrams.dropped.invalid";
    /// Dropped: exceeded `max_rx_datagram_size` (truncation surrogate).
    pub const DROPPED_TRUNCATED: &str = "udp.datagrams.dropped.truncated";
    /// Dropped: no cluster/backend configured for the listener.
    pub const DROPPED_NO_BACKEND: &str = "udp.datagrams.dropped.no_backend";
    /// Dropped: flow-table cap reached (shed).
    pub const DROPPED_SHED: &str = "udp.datagrams.dropped.shed";
    /// Dropped: referenced an unknown / already-closed flow.
    pub const DROPPED_UNKNOWN_FLOW: &str = "udp.datagrams.dropped.unknown_flow";
    /// Dropped: the bounded per-flow / client-return write queue was at
    /// capacity (genuine egress-queue overflow under sustained `WouldBlock`).
    pub const DROPPED_WQ_FULL: &str = "udp.datagrams.dropped.wq_full";
    /// Dropped: a hard `send`/`send_to` error (e.g. ECONNREFUSED) on the egress
    /// path, distinct from a queue-full overflow. The egress "no resolved flow /
    /// socket gone" case routes to [`DROPPED_UNKNOWN_FLOW`] instead, so
    /// `wq_full` only counts genuine bounded-queue overflow.
    pub const DROPPED_SEND_ERROR: &str = "udp.datagrams.dropped.send_error";
    /// Backend health transitions observed by the UDP health prober.
    pub const BACKEND_HEALTH: &str = "udp.backend.health";
    /// `time!` of a flow's lifetime, recorded on close.
    pub const FLOW_DURATION: &str = "udp.flow.duration";
}

/// TLS counters (certificate inventory, handshake timing).
pub mod tls {
    pub const CERT_MIN_EXPIRES_AT_SECONDS: &str = "tls.cert.min_expires_at_seconds";
    pub const DEFAULT_CERT_USED: &str = "tls.default_cert_used";
    pub const HANDSHAKE_MS: &str = "tls.handshake_ms";
}

/// WebSocket counters.
pub mod websocket {
    pub const ACTIVE_REQUESTS: &str = "websocket.active_requests";
}

/// Miscellaneous counters that don't fit a richer family.
pub mod misc {
    pub const PANIC: &str = "panic";
    pub const ZOMBIES: &str = "zombies";
}
