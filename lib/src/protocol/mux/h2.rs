//! H2 mux connection wrapper (RFC 9113).
//!
//! Owns wire-side connection state: HPACK encoder/decoder, peer settings,
//! flow window, GOAWAY/RST attribution, and the [`H2FloodDetector`] backing
//! the CVE-2023-44487 / CVE-2024-27316 / CVE-2025-8671 mitigations. Stream
//! storage lives in the sibling `Context<L>` (`mux/mod.rs`); this module is
//! the canonical home for the edge-trigger discipline — paths that queue
//! bytes for a later event-loop pass must arm writable / signal pending
//! write (cf. `arm_writable()` at the deferred-control-frame sites and
//! `lib/src/lib.rs:1006`-`1010`).

use std::{
    cmp::min,
    collections::{HashMap, HashSet},
    io::{IoSlice, Write as _},
    time::{Duration, Instant},
};

/// Compile-time guard: `payload_len as usize` casts in the H2 parser assume at
/// least 32-bit pointer width.  This prevents silent truncation on platforms
/// with smaller pointers (e.g. 16-bit embedded targets).
const _: () = assert!(
    std::mem::size_of::<usize>() >= 4,
    "sozu requires at least 32-bit pointers"
);

use rusty_ulid::Ulid;
use sozu_command::{logging::ansi_palette, ready::Ready};

use crate::{
    L7ListenerHandler, ListenerHandler, Protocol, Readiness, SessionMetrics,
    protocol::mux::{
        BackendStatus, Context, DebugEvent, DebugHistory, Endpoint, GenericHttpStream,
        GlobalStreamId, MuxResult, Position, Stream, StreamId, StreamState, converter,
        forcefully_terminate_answer,
        parser::{self, Frame, FrameHeader, FrameType, H2Error, Headers, WindowUpdate},
        pkawa, remove_backend_stream, serializer, set_default_answer,
        shared::{EndStreamAction, drain_tls_close_notify, end_stream_decision},
        update_readiness_after_read, update_readiness_after_write,
    },
    socket::{SocketHandler, SocketResult, stats::socket_rtt},
    timer::TimeoutContainer,
};

/// Protocol label + session descriptor used as a prefix on every
/// [`ConnectionH2`] log line. Matches the RUSTLS log-context convention:
/// `MUX-H2\tSession(...)\t >>>`. When colored output is enabled (via
/// [`ansi_palette`]) the label is wrapped in bold bright-white ANSI (uniform
/// across every protocol) and the session detail is rendered in light grey.
///
/// Fields included in the session block (chosen to surface the most common
/// H2 troubleshooting axes — flow stall, leaked stream, draining state,
/// peer-side gap, reset-flood exposure):
/// - `peer` — peer address (or `None` if the socket is gone)
/// - `position` — `Server` / `Client(...)` orientation
/// - `state` — current [`H2State`]
/// - `streams` — number of in-flight streams on this connection
/// - `last_peer_id` — `highest_peer_stream_id` (gap to the peer's view)
/// - `window` — connection-level send window (RFC 9113 §6.9)
/// - `draining` — set after the first GOAWAY of a graceful shutdown
/// - `total_rst_streams_emitted_lifetime` — MadeYouReset counter (CVE-2025-8671)
/// - `total_rst_received_lifetime` — Rapid Reset counter (CVE-2023-44487)
/// - `readiness` — connection-level mio readiness snapshot
///
/// Computed lazily on each callsite — the helper only materialises when the
/// log level is enabled, so uncolored hot paths keep a single thread-local
/// read (the colored check) and one `format!` allocation.
macro_rules! log_context {
    ($self:expr) => {{
        let (open, reset, grey, gray, white) = ansi_palette();
        format!(
            "[{ulid} - - -]\t{open}MUX-H2{reset}\t{grey}Session{reset}({gray}peer{reset}={white}{peer:?}{reset}, {gray}position{reset}={white}{position:?}{reset}, {gray}state{reset}={white}{state:?}{reset}, {gray}streams{reset}={white}{streams}{reset}, {gray}last_peer_id{reset}={white}{last_peer_id}{reset}, {gray}window{reset}={white}{window}{reset}, {gray}draining{reset}={white}{draining}{reset}, {gray}total_rst_streams_emitted_lifetime{reset}={white}{total_rst_streams_emitted_lifetime}{reset}, {gray}total_rst_received_lifetime{reset}={white}{total_rst_received_lifetime}{reset}, {gray}readiness{reset}={white}{readiness}{reset})\t >>>",
            open = open,
            reset = reset,
            grey = grey,
            gray = gray,
            white = white,
            ulid = $self.session_ulid,
            peer = $self.socket.socket_ref().peer_addr().ok(),
            position = $self.position,
            state = $self.state,
            streams = $self.streams.len(),
            last_peer_id = $self.highest_peer_stream_id,
            window = $self.flow_control.window,
            draining = $self.drain.draining,
            total_rst_streams_emitted_lifetime = $self.flood_detector.total_rst_streams_emitted_lifetime,
            total_rst_received_lifetime = $self.flood_detector.total_rst_received_lifetime,
            readiness = $self.readiness,
        )
    }};
}

/// Per-stream variant of [`log_context!`] used when a [`Stream`]'s
/// [`HttpContext`](crate::protocol::kawa_h1::editor::HttpContext) is in
/// scope. Populates the `request_id`, `cluster_id` and `backend_id` slots of
/// the bracket so the log line can be filtered by the specific H2 stream it
/// belongs to.
#[allow(unused_macros)]
macro_rules! log_context_stream {
    ($self:expr, $http_context:expr) => {{
        let (open, reset, grey, gray, white) = ansi_palette();
        format!(
            "[{ulid} {req} {cluster} {backend}]\t{open}MUX-H2{reset}\t{grey}Session{reset}({gray}peer{reset}={white}{peer:?}{reset}, {gray}position{reset}={white}{position:?}{reset}, {gray}state{reset}={white}{state:?}{reset}, {gray}streams{reset}={white}{streams}{reset}, {gray}last_peer_id{reset}={white}{last_peer_id}{reset}, {gray}window{reset}={white}{window}{reset}, {gray}draining{reset}={white}{draining}{reset}, {gray}total_rst_streams_emitted_lifetime{reset}={white}{total_rst_streams_emitted_lifetime}{reset}, {gray}total_rst_received_lifetime{reset}={white}{total_rst_received_lifetime}{reset}, {gray}readiness{reset}={white}{readiness}{reset})\t >>>",
            open = open,
            reset = reset,
            grey = grey,
            gray = gray,
            white = white,
            ulid = $self.session_ulid,
            req = $http_context.id,
            cluster = $http_context.cluster_id.as_deref().unwrap_or("-"),
            backend = $http_context.backend_id.as_deref().unwrap_or("-"),
            peer = $self.socket.socket_ref().peer_addr().ok(),
            position = $self.position,
            state = $self.state,
            streams = $self.streams.len(),
            last_peer_id = $self.highest_peer_stream_id,
            window = $self.flow_control.window,
            draining = $self.drain.draining,
            total_rst_streams_emitted_lifetime = $self.flood_detector.total_rst_streams_emitted_lifetime,
            total_rst_received_lifetime = $self.flood_detector.total_rst_received_lifetime,
            readiness = $self.readiness,
        )
    }};
}

/// Module-level prefix without session context, for logs emitted from
/// free functions, `H2ConnectionConfig` validation and other sites where no
/// `ConnectionH2` is in scope. Keeps the `MUX-H2` label consistent with
/// connection logs and honours the colored flag.
macro_rules! log_module_context {
    () => {{
        let (open, reset, _, _, _) = ansi_palette();
        format!("{open}MUX-H2{reset}\t >>>", open = open, reset = reset)
    }};
}

/// `if let Some(violation) = self.flood_detector.check_flood() { return self.handle_flood_violation(violation); }`
/// pattern wrapped as a single statement. Pure dispatch — the actual flood
/// thresholds and counters live inside `H2FloodDetector::check_flood` and
/// `ConnectionH2::handle_flood_violation`, which the macro does not touch.
/// Use this at every per-frame counter bump site so the wrapper stays
/// uniform and a future grep for "flood-check forgot to return" finds zero.
macro_rules! check_flood_or_return {
    ($self:expr) => {
        if let Some(violation) = $self.flood_detector.check_flood() {
            return $self.handle_flood_violation(violation);
        }
    };
}

/// Outcome of a single-stream write flush in write_streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlushOutcome {
    /// All queued bytes were drained to the socket.
    Drained,
    /// The socket blocked before the queue was drained. The caller must
    /// arrange to resume (set expect_write or return from write_streams).
    Stalled,
}

// ── RFC 9113 §6.5.2 Settings Defaults ───────────────────────────────────────

const DEFAULT_HEADER_TABLE_SIZE: u32 = 4096;
const DEFAULT_MAX_CONCURRENT_STREAMS: u32 = 100;
pub(super) const DEFAULT_INITIAL_WINDOW_SIZE: u32 = (1 << 16) - 1; // 65535
const DEFAULT_MAX_FRAME_SIZE: u32 = 1 << 14; // 16384

// RFC 9113 §6.5.2: SETTINGS_MAX_FRAME_SIZE valid range [2^14, 2^24)
const MIN_MAX_FRAME_SIZE: u32 = 1 << 14; // 16384
const MAX_MAX_FRAME_SIZE: u32 = 1 << 24; // 16777216 (exclusive upper bound)

// RFC 9113 §6.9: maximum flow control window size (2^31 - 1)
const FLOW_CONTROL_MAX_WINDOW: u32 = (1 << 31) - 1;
// RFC 9113 §5.1.1: stream identifiers are 31-bit unsigned integers (2^31 - 1).
const STREAM_ID_MAX: u32 = 0x7FFF_FFFF;

/// Allocate the next locally-initiated stream identifier given the current
/// `last_stream_id` watermark, returning `(issued_id, next_last_stream_id)`
/// or `None` when the 31-bit space is exhausted.
///
/// RFC 9113 §5.1.1 reserves odd identifiers for clients and even identifiers
/// for servers. Sōzu never server-pushes, so in practice this helper is
/// called on the backend (client) side via [`ConnectionH2::new_stream_id`].
/// The server branch is kept symmetrical so the behaviour is exercised by
/// the unit tests and remains correct if push is ever enabled.
///
/// `last_stream_id` tracks the even "watermark" (2, 4, 6, ...). A client call
/// issues `watermark - 1` (odd), a server call issues `watermark - 2` (even).
/// The helper enforces two invariants:
/// - the issued identifier never exceeds `STREAM_ID_MAX` (2³¹ - 1); and
/// - the returned watermark is a valid starting point for the next call.
///
/// Exhaustion is reported with `None` to the caller, which must emit
/// GOAWAY(NO_ERROR) and stop issuing new streams on this connection
/// (see `start_stream` for the client-side drain path).
pub(super) fn next_stream_id(
    last_stream_id: StreamId,
    is_client: bool,
) -> Option<(StreamId, StreamId)> {
    let next = last_stream_id.checked_add(2)?;
    let issued = if is_client {
        next.checked_sub(1)?
    } else {
        next.checked_sub(2)?
    };
    // RFC 9113 §5.1.1: stream identifiers are 31-bit. Reject any allocation
    // whose issued value would exceed `STREAM_ID_MAX`; the watermark itself
    // is allowed to sit at `STREAM_ID_MAX + 1` (the sentinel that fails the
    // next call).
    if issued > STREAM_ID_MAX {
        return None;
    }
    Some((issued, next))
}

/// Enlarged connection-level receive window (1 MB).
/// The RFC 9113 default is 65 535 bytes, which is too small for high-throughput
/// proxying and causes excessive WINDOW_UPDATE round-trips. 1 MB matches the
/// initial window used by HAProxy, the h2 crate, and other production proxies.
const ENLARGED_CONNECTION_WINDOW: u32 = 1_048_576;

/// H2 client connection preface size: 24-byte magic + 9-byte SETTINGS frame header
pub(super) const CLIENT_PREFACE_SIZE: usize = 24 + parser::FRAME_HEADER_SIZE;

// ── Flood Detection Thresholds (CVE mitigations) ────────────────────────────

/// Default maximum RST_STREAM frames per window (CVE-2023-44487 Rapid Reset + CVE-2019-9514)
const DEFAULT_MAX_RST_STREAM_PER_WINDOW: u32 = 100;
/// Hard lifetime cap on total RST_STREAM frames received on a single
/// connection (CVE-2023-44487 Rapid Reset).
///
/// The per-window counter half-decays, which allows a patient attacker to
/// sustain ~50 RST/sec indefinitely — each one costs the backend a request
/// that will be cancelled before any response work is produced. A lifetime
/// counter that never decays puts an absolute ceiling on that amplification
/// per connection. 10 000 is generous for legitimate traffic (months of
/// occasional client-side cancellations) but rapidly trips on the ~30/sec
/// abusive pace reported in the CVE-2023-44487 advisory (~5 minutes).
pub(super) const DEFAULT_MAX_RST_STREAM_LIFETIME: u64 = 10_000;
/// Hard lifetime cap on RST_STREAM frames received BEFORE the corresponding
/// backend response has started. These are the cheap-for-client /
/// expensive-for-us resets that characterise Rapid Reset: the client pays
/// one RST frame, we pay a round-trip to the backend plus request parsing.
/// A much lower ceiling kills the attack well before 10 000 lifetime total.
pub(super) const DEFAULT_MAX_RST_STREAM_ABUSIVE_LIFETIME: u64 = 50;
/// Absolute lifetime cap on **server-emitted** RST_STREAM frames on a single
/// connection (CVE-2025-8671 — "MadeYouReset"). Distinct from
/// [`DEFAULT_MAX_RST_STREAM_LIFETIME`] which caps *received* RSTs
/// (CVE-2023-44487 Rapid Reset).
///
/// MadeYouReset has the server talk itself into flooding: the attacker sends
/// legitimate-looking frames that force the server to emit RST_STREAM (content
/// -length mismatch, header parse error, rejected priority, zero-increment
/// `WINDOW_UPDATE` on an open stream, …). Each forced RST costs the server a
/// header-decode, kawa buffer setup and frame serialisation; uncapped, it
/// becomes the same class of DoS as Rapid Reset but with a flipped emission
/// direction.
///
/// 500 is conservative: legitimate traffic very rarely triggers a
/// server-initiated RST (aside from graceful `NoError` cancels which are not
/// counted), so crossing 500 on a single connection is a strong abuse signal.
pub(super) const DEFAULT_MAX_RST_STREAM_EMITTED_LIFETIME: u64 = 500;
/// Default maximum PING frames per window (CVE-2019-9512 Ping Flood)
const DEFAULT_MAX_PING_PER_WINDOW: u32 = 100;
/// Absolute lifetime cap on PING frames received on a single connection.
/// Mirrors DEFAULT_MAX_RST_STREAM_LIFETIME — generous for legitimate
/// keep-alives but trips on sustained low-rate abuse (CVE-2019-9512).
const DEFAULT_MAX_PING_LIFETIME: u32 = 10_000;
/// Default maximum SETTINGS frames per window (CVE-2019-9515 Settings Flood)
const DEFAULT_MAX_SETTINGS_PER_WINDOW: u32 = 50;
/// Absolute lifetime cap on SETTINGS frames received on a single connection.
/// Mirrors DEFAULT_MAX_RST_STREAM_LIFETIME — generous for legitimate
/// renegotiations but trips on sustained low-rate abuse (CVE-2019-9515).
const DEFAULT_MAX_SETTINGS_LIFETIME: u32 = 10_000;
/// Default maximum empty DATA frames per window (CVE-2019-9518 Empty Frames)
const DEFAULT_MAX_EMPTY_DATA_PER_WINDOW: u32 = 100;
/// Default maximum connection-level (stream 0) WINDOW_UPDATE frames per
/// sliding window. Non-zero stream-0 WINDOW_UPDATE frames are otherwise
/// uncounted by the generic glitch detector — a peer could burn proxy CPU by
/// sending millions of legal-looking stream-0 WINDOW_UPDATEs. Value mirrors
/// [`DEFAULT_MAX_EMPTY_DATA_PER_WINDOW`] / [`DEFAULT_MAX_PING_PER_WINDOW`] —
/// legitimate proxies only need a handful per second.
const DEFAULT_MAX_WINDOW_UPDATE_STREAM0_PER_WINDOW: u32 = 100;
/// Default maximum CONTINUATION frames per header block (CVE-2024-27316)
const DEFAULT_MAX_CONTINUATION_FRAMES: u32 = 20;
/// Maximum accumulated header block size across CONTINUATION frames (64KB)
pub(super) const MAX_HEADER_LIST_SIZE: usize = 65536;
/// Default maximum HPACK dynamic table size (SETTINGS_HEADER_TABLE_SIZE)
/// accepted from the peer. 64 KB is well above the RFC default of 4 KB
/// while preventing a malicious peer from advertising up to 4 GB.
const DEFAULT_MAX_HEADER_TABLE_SIZE: u32 = 65536;
/// Duration of the sliding window for rate-based flood counters
const FLOOD_WINDOW_DURATION: std::time::Duration = std::time::Duration::from_secs(1);
/// Default maximum general anomaly count before triggering ENHANCE_YOUR_CALM
const DEFAULT_MAX_GLITCH_COUNT: u32 = 100;

/// RFC 9113 §5.1.2: threshold of `REFUSED_STREAM` emissions per
/// [`BACKPRESSURE_WINDOW_DURATION`] that triggers back-pressure — at this
/// point we halve the advertised `SETTINGS_MAX_CONCURRENT_STREAMS` so the
/// peer throttles its request rate instead of paying the RST round-trip for
/// every new stream.
const BACKPRESSURE_REFUSAL_THRESHOLD: u32 = 50;
/// Sliding window used to detect refusal bursts for SETTINGS back-pressure.
const BACKPRESSURE_WINDOW_DURATION: std::time::Duration = std::time::Duration::from_secs(60);

/// Configurable thresholds for H2 flood detection.
///
/// All values have safe defaults matching the compile-time constants.
/// When configured via listener config, `None` values fall back to these defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct H2FloodConfig {
    /// Maximum RST_STREAM frames per second window (CVE-2023-44487, CVE-2019-9514)
    pub max_rst_stream_per_window: u32,
    /// Maximum PING frames per second window (CVE-2019-9512)
    pub max_ping_per_window: u32,
    /// Maximum SETTINGS frames per second window (CVE-2019-9515)
    pub max_settings_per_window: u32,
    /// Maximum empty DATA frames per second window (CVE-2019-9518)
    pub max_empty_data_per_window: u32,
    /// Maximum connection-level (stream 0) WINDOW_UPDATE frames per sliding
    /// window. Caps the CPU cost of a peer sending a flood of non-zero
    /// stream-0 WINDOW_UPDATEs — each is individually legal so the generic
    /// glitch counter does not trip, yet millions per connection still burn
    /// server CPU parsing and updating the flow window.
    pub max_window_update_stream0_per_window: u32,
    /// Maximum CONTINUATION frames per header block (CVE-2024-27316)
    pub max_continuation_frames: u32,
    /// Maximum accumulated protocol anomalies before ENHANCE_YOUR_CALM
    pub max_glitch_count: u32,
    /// Absolute lifetime cap on RST_STREAM frames received on a single
    /// connection (CVE-2023-44487). Never decays — provides a ceiling the
    /// per-window counter cannot.
    pub max_rst_stream_lifetime: u64,
    /// Lifetime cap on "abusive" (pre-response-start) RST_STREAM frames —
    /// the Rapid Reset signature (CVE-2023-44487).
    pub max_rst_stream_abusive_lifetime: u64,
    /// Absolute lifetime cap on **server-emitted** RST_STREAM frames for this
    /// connection (CVE-2025-8671 "MadeYouReset"). Only non-`NoError` resets
    /// count — graceful cancels are exempt.
    pub max_rst_stream_emitted_lifetime: u64,
    /// Maximum accumulated HPACK-decoded header list size per request
    /// (SETTINGS_MAX_HEADER_LIST_SIZE, RFC 9113 §6.5.2).
    pub max_header_list_size: u32,
    /// Maximum HPACK dynamic table size (SETTINGS_HEADER_TABLE_SIZE) accepted
    /// from the peer. Caps the value the peer advertises in SETTINGS frames to
    /// prevent unbounded HPACK encoder memory growth.
    pub max_header_table_size: u32,
}

impl Default for H2FloodConfig {
    fn default() -> Self {
        Self {
            max_rst_stream_per_window: DEFAULT_MAX_RST_STREAM_PER_WINDOW,
            max_ping_per_window: DEFAULT_MAX_PING_PER_WINDOW,
            max_settings_per_window: DEFAULT_MAX_SETTINGS_PER_WINDOW,
            max_empty_data_per_window: DEFAULT_MAX_EMPTY_DATA_PER_WINDOW,
            max_window_update_stream0_per_window: DEFAULT_MAX_WINDOW_UPDATE_STREAM0_PER_WINDOW,
            max_continuation_frames: DEFAULT_MAX_CONTINUATION_FRAMES,
            max_glitch_count: DEFAULT_MAX_GLITCH_COUNT,
            max_rst_stream_lifetime: DEFAULT_MAX_RST_STREAM_LIFETIME,
            max_rst_stream_abusive_lifetime: DEFAULT_MAX_RST_STREAM_ABUSIVE_LIFETIME,
            max_rst_stream_emitted_lifetime: DEFAULT_MAX_RST_STREAM_EMITTED_LIFETIME,
            max_header_list_size: MAX_HEADER_LIST_SIZE as u32,
            max_header_table_size: DEFAULT_MAX_HEADER_TABLE_SIZE,
        }
    }
}

impl H2FloodConfig {
    /// Create a validated config, clamping all thresholds to at least 1.
    /// Zero thresholds would cause immediate flood detection on any frame.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        max_rst_stream_per_window: u32,
        max_ping_per_window: u32,
        max_settings_per_window: u32,
        max_empty_data_per_window: u32,
        max_window_update_stream0_per_window: u32,
        max_continuation_frames: u32,
        max_glitch_count: u32,
        max_rst_stream_lifetime: u64,
        max_rst_stream_abusive_lifetime: u64,
        max_rst_stream_emitted_lifetime: u64,
        max_header_list_size: u32,
        max_header_table_size: u32,
    ) -> Self {
        Self {
            max_rst_stream_per_window: max_rst_stream_per_window.max(1),
            max_ping_per_window: max_ping_per_window.max(1),
            max_settings_per_window: max_settings_per_window.max(1),
            max_empty_data_per_window: max_empty_data_per_window.max(1),
            max_window_update_stream0_per_window: max_window_update_stream0_per_window.max(1),
            max_continuation_frames: max_continuation_frames.max(1),
            max_glitch_count: max_glitch_count.max(1),
            max_rst_stream_lifetime: max_rst_stream_lifetime.max(1),
            max_rst_stream_abusive_lifetime: max_rst_stream_abusive_lifetime.max(1),
            max_rst_stream_emitted_lifetime: max_rst_stream_emitted_lifetime.max(1),
            max_header_list_size: max_header_list_size.max(1),
            max_header_table_size: max_header_table_size.max(1),
        }
    }
}

/// Default stream Vec shrink ratio: shrink when total > active * ratio.
const DEFAULT_STREAM_SHRINK_RATIO: u32 = 2;

/// Configurable H2 connection tuning parameters.
///
/// All values have safe defaults. When configured via listener config,
/// absent values fall back to compile-time defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct H2ConnectionConfig {
    /// Connection-level receive window size in bytes (RFC 9113 §6.9.2).
    pub initial_connection_window: u32,
    /// Maximum concurrent streams (SETTINGS_MAX_CONCURRENT_STREAMS).
    pub max_concurrent_streams: u32,
    /// Shrink threshold ratio for recycled stream slots.
    pub stream_shrink_ratio: u32,
}

impl Default for H2ConnectionConfig {
    fn default() -> Self {
        Self {
            initial_connection_window: ENLARGED_CONNECTION_WINDOW,
            max_concurrent_streams: DEFAULT_MAX_CONCURRENT_STREAMS,
            stream_shrink_ratio: DEFAULT_STREAM_SHRINK_RATIO,
        }
    }
}

impl H2ConnectionConfig {
    /// Create a validated config, clamping to safe bounds.
    ///
    /// - `initial_connection_window`: clamped to \[65535, 2^31-1\] per RFC 9113 §6.9
    /// - `max_concurrent_streams`: minimum 1
    /// - `stream_shrink_ratio`: minimum 2 (1 would defeat slot recycling)
    pub fn new(
        initial_connection_window: u32,
        max_concurrent_streams: u32,
        stream_shrink_ratio: u32,
    ) -> Self {
        let clamped_window =
            initial_connection_window.clamp(DEFAULT_INITIAL_WINDOW_SIZE, FLOW_CONTROL_MAX_WINDOW);
        if clamped_window != initial_connection_window {
            warn!(
                "{} h2_initial_connection_window {} clamped to [{}, {}]",
                log_module_context!(),
                initial_connection_window,
                DEFAULT_INITIAL_WINDOW_SIZE,
                FLOW_CONTROL_MAX_WINDOW
            );
        }
        const MAX_SAFE_CONCURRENT_STREAMS: u32 = 10_000;
        let clamped_streams = max_concurrent_streams.clamp(1, MAX_SAFE_CONCURRENT_STREAMS);
        if max_concurrent_streams > MAX_SAFE_CONCURRENT_STREAMS {
            error!(
                "{} h2_max_concurrent_streams={} exceeds safe limit, clamped to {}",
                log_module_context!(),
                max_concurrent_streams,
                MAX_SAFE_CONCURRENT_STREAMS
            );
        }
        if clamped_streams != max_concurrent_streams
            && max_concurrent_streams <= MAX_SAFE_CONCURRENT_STREAMS
        {
            warn!(
                "{} h2_max_concurrent_streams {} clamped to minimum 1",
                log_module_context!(),
                max_concurrent_streams
            );
        }
        let clamped_ratio = stream_shrink_ratio.max(2);
        if clamped_ratio != stream_shrink_ratio {
            warn!(
                "{} h2_stream_shrink_ratio {} clamped to minimum 2",
                log_module_context!(),
                stream_shrink_ratio
            );
        }
        Self {
            initial_connection_window: clamped_window,
            max_concurrent_streams: clamped_streams,
            stream_shrink_ratio: clamped_ratio,
        }
    }

    /// Create from optional config values, falling back to compile-time defaults.
    /// Combines unwrap-or-default with validation clamping.
    pub fn from_optional(
        window: Option<u32>,
        max_streams: Option<u32>,
        shrink_ratio: Option<u32>,
    ) -> Self {
        let defaults = Self::default();
        Self::new(
            window.unwrap_or(defaults.initial_connection_window),
            max_streams.unwrap_or(defaults.max_concurrent_streams),
            shrink_ratio.unwrap_or(defaults.stream_shrink_ratio),
        )
    }
}

/// Default pending WINDOW_UPDATE capacity (used in tests).
/// The actual per-connection cap is computed from `connection_config.max_concurrent_streams`.
#[cfg(test)]
const DEFAULT_MAX_PENDING_WINDOW_UPDATES: usize = 1 + DEFAULT_MAX_CONCURRENT_STREAMS as usize * 4;

/// Maximum number of pending RST_STREAM frames before triggering GOAWAY.
/// When a peer causes excessive RST_STREAM queueing (e.g. rapid stream creation
/// beyond MAX_CONCURRENT_STREAMS), this cap prevents unbounded memory growth
/// and triggers an ENHANCE_YOUR_CALM connection error.
const MAX_PENDING_RST_STREAMS: usize = 200;

/// RFC 9113 §6.5: maximum time (in seconds) to wait for SETTINGS ACK before
/// sending GOAWAY with SETTINGS_TIMEOUT error code.
const SETTINGS_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[inline(always)]
fn error_nom_to_h2(error: nom::Err<parser::ParserError>) -> H2Error {
    match error {
        nom::Err::Error(parser::ParserError {
            kind: parser::ParserErrorKind::H2(e),
            ..
        }) => e,
        nom::Err::Failure(parser::ParserError {
            kind: parser::ParserErrorKind::H2(e),
            ..
        }) => e,
        _ => H2Error::ProtocolError,
    }
}

/// Distribute connection-level byte overhead proportionally to a single stream.
///
/// Overhead is distributed in proportion to the bytes this stream transferred
/// relative to the total across all active streams. A stream that transferred
/// 60% of total bytes gets 60% of the overhead.
///
/// `stream_bytes` and `total_bytes` are `(bytes_in, bytes_out)` tuples.
/// Falls back to even distribution (1/active_streams) when no stream has
/// transferred any bytes yet (total is zero).
///
/// Extracted as a free function to avoid borrow conflicts when `self` fields
/// (e.g. `encoder`) are borrowed by the converter while we need to update
/// per-stream metrics and connection overhead counters.
fn distribute_overhead(
    metrics: &mut SessionMetrics,
    overhead_bin: &mut usize,
    overhead_bout: &mut usize,
    stream_bytes: (usize, usize),
    total_bytes: (usize, usize),
    active_streams: usize,
    is_last_stream: bool,
) {
    let share_in = if is_last_stream {
        // Last stream gets all remaining overhead to avoid losing remainder bytes
        // from integer division across earlier streams.
        *overhead_bin
    } else if total_bytes.0 > 0 {
        // Clamp to remaining overhead — integer division rounding across multiple
        // streams can cause accumulated shares to exceed the total.
        (*overhead_bin * stream_bytes.0 / total_bytes.0).min(*overhead_bin)
    } else {
        // No stream has transferred any inbound bytes — fall back to even split.
        *overhead_bin / active_streams.max(1)
    };
    let share_out = if is_last_stream {
        *overhead_bout
    } else if total_bytes.1 > 0 {
        (*overhead_bout * stream_bytes.1 / total_bytes.1).min(*overhead_bout)
    } else {
        // No stream has transferred any outbound bytes — fall back to even split.
        *overhead_bout / active_streams.max(1)
    };
    metrics.bin += share_in;
    metrics.bout += share_out;
    *overhead_bin -= share_in;
    *overhead_bout -= share_out;
}

/// LIFECYCLE §9 invariant 16 probe: returns `true` if any open stream still
/// has outbound kawa bytes queued (`back.out` non-empty or `back.blocks`
/// non-drained).
///
/// Used by `finalize_write` to preserve `Ready::WRITABLE` across a voluntary
/// scheduler yield, and by `has_pending_write_full` to block shutdown-drain
/// while bytes are still owed to the frontend.
///
/// `.get()` rather than direct indexing: an unknown `GlobalStreamId` is
/// treated as "no pending bytes" rather than panicking — defence-in-depth
/// against a stream-removal race during shutdown.
fn any_stream_has_pending_back(
    streams: &HashMap<StreamId, GlobalStreamId>,
    context_streams: &[Stream],
) -> bool {
    any_stream_id_matches(streams, |gid| {
        context_streams
            .get(gid)
            .is_some_and(|s| !s.back.out.is_empty() || !s.back.blocks.is_empty())
    })
}

/// Iteration core of [`any_stream_has_pending_back`], split out so the
/// invariant-16 dispatch is unit-testable without a full [`Stream`] fixture
/// (the existing test module only covers `H2FloodDetector`).
fn any_stream_id_matches<F>(streams: &HashMap<StreamId, GlobalStreamId>, mut probe: F) -> bool
where
    F: FnMut(GlobalStreamId) -> bool,
{
    streams.values().any(|gid| probe(*gid))
}

/// Core of [`ConnectionH2::enqueue_rst`], extracted so the RST-queueing
/// semantics (dedupe, queued-cap counter bump, invariant-15 readiness rearm)
/// can be unit-tested without building a full `ConnectionH2<Front>` fixture.
///
/// Invariants enforced:
/// - **Dedupe** via `rst_sent`: at most one queued RST per wire stream id.
///   `HashSet::insert` returns `false` when the id is already present; we
///   short-circuit on that branch to keep `pending_rst_streams`,
///   `total_rst_streams_queued` and the wire counts consistent.
/// - **MadeYouReset queued cap** (`MAX_PENDING_RST_STREAMS`): each freshly
///   queued RST bumps `total_rst_streams_queued`, which
///   `flush_pending_control_frames` polices to escalate to
///   `GOAWAY(ENHANCE_YOUR_CALM)` when exceeded.
/// - **Invariant 15** (edge-triggered epoll): pair `Ready::WRITABLE` interest
///   with the event bit so `writable()` is scheduled on the next tick.
///
/// Returns `true` when the RST was freshly queued, `false` when the
/// stream was already in `rst_sent` (the caller asked to RST the same
/// stream twice — a benign re-entrant idempotency, NOT a new wire
/// emission). The boolean lets [`ConnectionH2::enqueue_rst`] account
/// the RST only on the freshly-queued path so duplicate calls do not
/// inflate the per-error counter or trip the MadeYouReset flood cap
/// for frames that never reach the wire.
fn enqueue_rst_into(
    pending: &mut Vec<(StreamId, H2Error)>,
    total: &mut usize,
    rst_sent: &mut HashSet<StreamId>,
    readiness: &mut Readiness,
    wire_stream_id: StreamId,
    error: H2Error,
) -> bool {
    if !rst_sent.insert(wire_stream_id) {
        return false;
    }
    pending.push((wire_stream_id, error));
    *total += 1;
    readiness.arm_writable();
    true
}

/// Detail of a flood-threshold violation returned by
/// [`H2FloodDetector::check_flood`] and [`H2FloodDetector::record_rst_lifetime`].
///
/// Carrying `(reason, count, threshold)` lets the caller emit a session-scoped
/// log line with full context — the detector itself is connection-agnostic and
/// never logs.
#[derive(Debug, Clone, PartialEq)]
pub struct H2FloodViolation {
    /// HTTP/2 error code to emit on the GOAWAY.
    pub error: H2Error,
    /// Human-readable name of the counter that tripped (e.g. `"RST_STREAM"`).
    pub reason: &'static str,
    /// Statsd metric key emitted by [`ConnectionH2::handle_flood_violation`].
    /// Carried alongside `reason` so a single field maps to both the log line
    /// and the dashboard counter — adding a new violation kind requires
    /// choosing both at the construction site, preventing drift.
    pub metric_key: &'static str,
    /// Observed counter value at the moment of detection.
    pub count: u64,
    /// Configured ceiling that was crossed.
    pub threshold: u64,
}

/// Tracks per-connection frame rates to detect and mitigate H2 flood attacks.
///
/// Monitors RST_STREAM (CVE-2023-44487), PING (CVE-2019-9512), SETTINGS (CVE-2019-9515),
/// empty DATA (CVE-2019-9518), and CONTINUATION (CVE-2024-27316) flood patterns.
/// When any counter exceeds its threshold, `check_flood()` returns the violation
/// detail so callers can log with connection context before sending GOAWAY.
///
/// Thresholds are configurable via [`H2FloodConfig`], with safe defaults matching
/// the original compile-time constants.
#[derive(Debug)]
pub struct H2FloodDetector {
    /// RST_STREAM frames received in current window (CVE-2023-44487 + CVE-2019-9514)
    pub(super) rst_stream_count: u32,
    /// Lifetime RST_STREAM frames received on this connection.
    ///
    /// Never decays — provides an absolute ceiling that the half-decaying
    /// per-window counter cannot, preventing a sustained ~50 RST/sec burst
    /// from running forever.
    pub(super) total_rst_received_lifetime: u64,
    /// Lifetime RST_STREAM frames received that targeted a stream whose
    /// backend response had not yet started. These are the "Rapid Reset"
    /// signature — cheap for the attacker, expensive for the proxy — and
    /// trip on a much lower ceiling than the generic lifetime counter.
    pub(super) total_abusive_rst_received_lifetime: u64,
    /// Lifetime RST_STREAM frames **emitted by the server** on this
    /// connection (CVE-2025-8671 "MadeYouReset" mitigation). Incremented
    /// inside [`ConnectionH2::reset_stream`] whenever a non-`NoError` reset
    /// is triggered by an attacker-crafted frame (content-length mismatch,
    /// header parse error, priority rejection, zero-increment WINDOW_UPDATE
    /// on an open stream). Never decays — provides an absolute ceiling that
    /// short-circuits patient-attacker patterns that stay under any windowed
    /// counter.
    pub(super) total_rst_streams_emitted_lifetime: u64,
    /// PING frames received in current window (CVE-2019-9512)
    pub(super) ping_count: u32,
    /// Lifetime PING frames received on this connection.
    ///
    /// Never decays — provides an absolute ceiling that the half-decaying
    /// per-window counter cannot, preventing sustained low-rate PING abuse.
    pub(super) total_ping_received_lifetime: u32,
    /// SETTINGS frames received in current window (CVE-2019-9515)
    pub(super) settings_count: u32,
    /// Lifetime SETTINGS frames received on this connection.
    ///
    /// Never decays — provides an absolute ceiling that the half-decaying
    /// per-window counter cannot, preventing sustained low-rate SETTINGS abuse.
    pub(super) total_settings_received_lifetime: u32,
    /// Empty DATA frames received in current window (CVE-2019-9518)
    pub(super) empty_data_count: u32,
    /// Connection-level (stream 0) WINDOW_UPDATE frames received in current
    /// sliding window. Half-decays with [`maybe_reset_window`] like other
    /// rate counters. Increments on non-zero stream-0 WINDOW_UPDATEs only —
    /// zero-increment frames short-circuit into GOAWAY(PROTOCOL_ERROR) per
    /// RFC 9113 §6.9 before reaching this counter.
    pub(super) window_update_stream0_count: u32,
    /// CONTINUATION frames received for current header block (CVE-2024-27316)
    pub(super) continuation_count: u32,
    /// Total accumulated header block size across CONTINUATION frames
    pub(super) accumulated_header_size: u32,
    /// General anomaly counter
    pub(super) glitch_count: u32,
    /// Window start for rate-based counters
    pub(super) window_start: Instant,
    /// Configurable thresholds for flood detection
    pub(super) config: H2FloodConfig,
}

impl Default for H2FloodDetector {
    fn default() -> Self {
        Self::new(H2FloodConfig::default())
    }
}

impl H2FloodDetector {
    pub fn new(config: H2FloodConfig) -> Self {
        Self {
            rst_stream_count: 0,
            total_rst_received_lifetime: 0,
            total_abusive_rst_received_lifetime: 0,
            total_rst_streams_emitted_lifetime: 0,
            ping_count: 0,
            total_ping_received_lifetime: 0,
            settings_count: 0,
            total_settings_received_lifetime: 0,
            empty_data_count: 0,
            window_update_stream0_count: 0,
            continuation_count: 0,
            accumulated_header_size: 0,
            glitch_count: 0,
            window_start: Instant::now(),
            config,
        }
    }

    /// Increment the lifetime RST_STREAM counters and return a
    /// [`H2FloodViolation`] if either the global or the abusive
    /// (pre-response-start) lifetime cap has been exceeded.
    ///
    /// `response_started` indicates whether the backend response had already
    /// begun when the RST arrived; `false` is the cheap-for-client /
    /// expensive-for-us Rapid Reset signature (CVE-2023-44487).
    pub fn record_rst_lifetime(&mut self, response_started: bool) -> Option<H2FloodViolation> {
        self.total_rst_received_lifetime = self.total_rst_received_lifetime.saturating_add(1);
        if !response_started {
            self.total_abusive_rst_received_lifetime =
                self.total_abusive_rst_received_lifetime.saturating_add(1);
        }
        if self.total_rst_received_lifetime > self.config.max_rst_stream_lifetime {
            return Some(H2FloodViolation {
                error: H2Error::EnhanceYourCalm,
                reason: "Rapid Reset: lifetime RST_STREAM",
                metric_key: "h2.flood.violation.rst_stream_lifetime",
                count: self.total_rst_received_lifetime,
                threshold: self.config.max_rst_stream_lifetime,
            });
        }
        if self.total_abusive_rst_received_lifetime > self.config.max_rst_stream_abusive_lifetime {
            return Some(H2FloodViolation {
                error: H2Error::EnhanceYourCalm,
                reason: "Rapid Reset: lifetime pre-response RST_STREAM",
                metric_key: "h2.flood.violation.rst_stream_pre_response_lifetime",
                count: self.total_abusive_rst_received_lifetime,
                threshold: self.config.max_rst_stream_abusive_lifetime,
            });
        }
        None
    }

    /// Increment the lifetime **server-emitted** RST_STREAM counter and
    /// return a [`H2FloodViolation`] once the configured ceiling is exceeded.
    ///
    /// Call sites are the error paths inside [`ConnectionH2::reset_stream`]
    /// where an attacker-crafted frame coerces the server into emitting a
    /// RST_STREAM (CVE-2025-8671 "MadeYouReset"). Only non-`NoError` resets
    /// are reported — callers must exclude graceful cancels.
    pub fn record_rst_emitted(&mut self) -> Option<H2FloodViolation> {
        self.total_rst_streams_emitted_lifetime =
            self.total_rst_streams_emitted_lifetime.saturating_add(1);
        if self.total_rst_streams_emitted_lifetime > self.config.max_rst_stream_emitted_lifetime {
            return Some(H2FloodViolation {
                error: H2Error::EnhanceYourCalm,
                reason: "MadeYouReset: lifetime server-emitted RST_STREAM",
                metric_key: "h2.flood.violation.rst_stream_emitted_lifetime",
                count: self.total_rst_streams_emitted_lifetime,
                threshold: self.config.max_rst_stream_emitted_lifetime,
            });
        }
        None
    }

    /// Half-decay rate-based counters if the current window has expired.
    /// Uses half-window decay instead of full reset to catch burst-then-wait attacks.
    fn maybe_reset_window(&mut self) {
        if self.window_start.elapsed() >= FLOOD_WINDOW_DURATION {
            self.rst_stream_count /= 2;
            self.ping_count /= 2;
            self.settings_count /= 2;
            self.empty_data_count /= 2;
            self.window_update_stream0_count /= 2;
            self.glitch_count /= 2;
            self.window_start = Instant::now();
        }
    }

    /// Check all flood counters. Returns a [`H2FloodViolation`] when a threshold
    /// is exceeded; the caller is responsible for logging with session context
    /// and escalating to GOAWAY.
    pub fn check_flood(&mut self) -> Option<H2FloodViolation> {
        self.maybe_reset_window();

        fn flag(
            reason: &'static str,
            metric_key: &'static str,
            count: u32,
            threshold: u32,
        ) -> Option<H2FloodViolation> {
            if count > threshold {
                Some(H2FloodViolation {
                    error: H2Error::EnhanceYourCalm,
                    reason,
                    metric_key,
                    count: count as u64,
                    threshold: threshold as u64,
                })
            } else {
                None
            }
        }

        flag(
            "RST_STREAM",
            "h2.flood.violation.rst_stream_window",
            self.rst_stream_count,
            self.config.max_rst_stream_per_window,
        )
        .or_else(|| {
            flag(
                "PING",
                "h2.flood.violation.ping_window",
                self.ping_count,
                self.config.max_ping_per_window,
            )
        })
        .or_else(|| {
            flag(
                "PING lifetime",
                "h2.flood.violation.ping_lifetime",
                self.total_ping_received_lifetime,
                DEFAULT_MAX_PING_LIFETIME,
            )
        })
        .or_else(|| {
            flag(
                "SETTINGS",
                "h2.flood.violation.settings_window",
                self.settings_count,
                self.config.max_settings_per_window,
            )
        })
        .or_else(|| {
            flag(
                "SETTINGS lifetime",
                "h2.flood.violation.settings_lifetime",
                self.total_settings_received_lifetime,
                DEFAULT_MAX_SETTINGS_LIFETIME,
            )
        })
        .or_else(|| {
            flag(
                "empty DATA",
                "h2.flood.violation.empty_data_window",
                self.empty_data_count,
                self.config.max_empty_data_per_window,
            )
        })
        .or_else(|| {
            flag(
                "CONTINUATION",
                "h2.flood.violation.continuation_per_block",
                self.continuation_count,
                self.config.max_continuation_frames,
            )
        })
        .or_else(|| {
            flag(
                "WINDOW_UPDATE stream 0",
                "h2.flood.violation.window_update_stream0_window",
                self.window_update_stream0_count,
                self.config.max_window_update_stream0_per_window,
            )
        })
        .or_else(|| {
            flag(
                "accumulated header size",
                "h2.flood.violation.header_size_per_block",
                self.accumulated_header_size,
                self.config.max_header_list_size,
            )
        })
        .or_else(|| {
            flag(
                "glitch",
                "h2.flood.violation.glitch_window",
                self.glitch_count,
                self.config.max_glitch_count,
            )
        })
    }

    /// Reset CONTINUATION-specific counters when a header block is complete.
    pub fn reset_continuation(&mut self) {
        self.continuation_count = 0;
        self.accumulated_header_size = 0;
    }
}

#[derive(Debug)]
pub enum H2State {
    ClientPreface,
    ClientSettings,
    ServerSettings,
    Header,
    Frame(FrameHeader),
    ContinuationHeader(Headers),
    ContinuationFrame(Headers),
    GoAway,
    Error,
    Discard,
}

#[derive(Debug, Clone, Copy)]
pub struct H2Settings {
    pub settings_header_table_size: u32,
    pub settings_enable_push: bool,
    pub settings_max_concurrent_streams: u32,
    pub settings_initial_window_size: u32,
    pub settings_max_frame_size: u32,
    pub settings_max_header_list_size: u32,
    /// RFC 8441
    pub settings_enable_connect_protocol: bool,
    /// RFC 9218
    pub settings_no_rfc7540_priorities: bool,
}

impl Default for H2Settings {
    fn default() -> Self {
        Self {
            settings_header_table_size: DEFAULT_HEADER_TABLE_SIZE,
            settings_enable_push: false,
            settings_max_concurrent_streams: DEFAULT_MAX_CONCURRENT_STREAMS,
            settings_initial_window_size: DEFAULT_INITIAL_WINDOW_SIZE,
            settings_max_frame_size: DEFAULT_MAX_FRAME_SIZE,
            settings_max_header_list_size: MAX_HEADER_LIST_SIZE as u32,
            settings_enable_connect_protocol: false,
            settings_no_rfc7540_priorities: true,
        }
    }
}

/// RFC 9218 Extensible Priorities for HTTP stream scheduling.
///
/// Stores per-stream urgency (0-7, lower = more important) and incremental
/// flag. Used by `writable()` to sort streams: lower urgency first, then
/// stream ID for stability among same-urgency non-incremental streams.
///
/// Within a same-urgency bucket the scheduler (see
/// [`ConnectionH2::write_streams`]) drains non-incremental streams
/// sequentially, then applies RFC 9218 §4 round-robin to the incremental
/// streams starting from [`Self::incremental_cursor`], so multiple concurrent
/// downloads at the same urgency interleave their DATA frames fairly.
///
/// Streams without an explicit `priority` header get the RFC 9218 defaults:
/// urgency 3, incremental false.
#[derive(Default)]
pub struct Prioriser {
    /// Per-stream priority: stream_id -> (urgency 0-7, incremental flag)
    priorities: HashMap<StreamId, (u8, bool)>,
    /// RFC 9218 §4 round-robin cursor: stream ID that fired first in the
    /// last write pass over the incremental tail of the lowest-urgency
    /// bucket that contained at least one incremental stream. The next pass
    /// starts from the stream immediately after this ID (wrapping around),
    /// so a single slow-draining stream cannot hog the connection.
    ///
    /// `0` is the "no cursor yet" sentinel and means "start from the
    /// smallest ID in the bucket" — H2 stream IDs are always > 0.
    incremental_cursor: StreamId,
}

/// RFC 9218 §4 default urgency value.
const DEFAULT_URGENCY: u8 = 3;

/// Maximum entries in the priority map to prevent flooding via PRIORITY frames.
const MAX_PRIORITIES: usize = 4096;

/// Small look-ahead window (in stream IDs) for PRIORITY frames that arrive
/// slightly before the peer opens the corresponding stream. RFC 9218 allows
/// PRIORITY to be sent for an idle stream that the peer intends to open
/// soon. Past this budget we assume the ID will never be used and drop the
/// entry, preventing flooding with far-future stream IDs.
const PRIORITY_IDLE_LOOKAHEAD: u32 = 64;

impl Prioriser {
    /// Record or update the priority for a stream that we know exists or are
    /// currently processing (used from pkawa's header-handling path where the
    /// owning stream's HEADERS frame is being decoded).
    ///
    /// Returns `true` if the priority is invalid (self-dependency for RFC 7540),
    /// signalling the caller should reset the stream with a protocol error.
    pub fn push_priority(&mut self, stream_id: StreamId, priority: parser::PriorityPart) -> bool {
        trace!(
            "{} PRIORITY REQUEST FOR {}: {:?}",
            log_module_context!(),
            stream_id,
            priority
        );
        // Cap the priority map to prevent flooding via PRIORITY frames
        if !self.priorities.contains_key(&stream_id) && self.priorities.len() >= MAX_PRIORITIES {
            return false;
        }
        match priority {
            parser::PriorityPart::Rfc7540 {
                stream_dependency,
                weight: _,
            } => {
                // RFC 9113 §5.3.1: a stream cannot depend on itself; signal
                // the caller to RST_STREAM with PROTOCOL_ERROR. Otherwise the
                // RFC 7540 priority tree is deprecated and silently ignored.
                stream_dependency.stream_id == stream_id
            }
            parser::PriorityPart::Rfc9218 {
                urgency,
                incremental,
            } => {
                // RFC 9218 §7.1: a malformed or out-of-range priority field
                // MUST be "treated as absent", NOT as a stream error. Clamping
                // an urgency > 7 to 7 is the policy-correct interpretation:
                // the field is still present (so defaulting would lose
                // information) but its value is normalised to the RFC's
                // allowed range [0..=7]. Intentionally not PROTOCOL_ERROR.
                self.priorities
                    .insert(stream_id, (urgency.min(7), incremental));
                false
            }
        }
    }

    /// Record or update the priority for a stream ID that arrived via a
    /// standalone PRIORITY frame.
    ///
    /// Pass 3 Medium #4: without this guard, a peer could send PRIORITY for
    /// arbitrary stream IDs (e.g. 2^31 ever-increasing IDs) and pin up to
    /// `MAX_PRIORITIES` entries of memory. Accept only:
    /// - an ID that corresponds to a currently-open stream (`open_streams`);
    /// - an idle ID slightly ahead of `last_stream_id` (within
    ///   [`PRIORITY_IDLE_LOOKAHEAD`]), matching RFC 9218's "set priority for
    ///   a stream about to be opened" pattern.
    ///
    /// IDs in the past that we do not currently track (already closed) and
    /// IDs too far in the future are silently dropped. The `MAX_PRIORITIES`
    /// ceiling is preserved as a defensive backstop if both filters are ever
    /// circumvented.
    ///
    /// Returns the same value semantics as [`Self::push_priority`].
    pub fn push_priority_guarded(
        &mut self,
        stream_id: StreamId,
        priority: parser::PriorityPart,
        last_stream_id: StreamId,
        open_streams: &HashMap<StreamId, GlobalStreamId>,
    ) -> bool {
        if !self.is_acceptable(stream_id, last_stream_id, open_streams) {
            trace!(
                "{} PRIORITY dropped for unknown/far stream {} (last_stream_id={})",
                log_module_context!(),
                stream_id,
                last_stream_id
            );
            return false;
        }
        self.push_priority(stream_id, priority)
    }

    fn is_acceptable(
        &self,
        stream_id: StreamId,
        last_stream_id: StreamId,
        open_streams: &HashMap<StreamId, GlobalStreamId>,
    ) -> bool {
        if open_streams.contains_key(&stream_id) {
            return true;
        }
        // Idle stream ahead of the current counter: accept a small look-ahead.
        // Past IDs that are NOT in `open_streams` are closed — drop them.
        let upper = last_stream_id.saturating_add(PRIORITY_IDLE_LOOKAHEAD);
        stream_id > last_stream_id && stream_id <= upper
    }

    /// Remove a stream's priority entry (called when the stream is recycled).
    pub fn remove(&mut self, stream_id: &StreamId) {
        self.priorities.remove(stream_id);
    }

    /// Look up the priority for a stream, returning RFC 9218 defaults if absent.
    #[inline]
    pub fn get(&self, stream_id: &StreamId) -> (u8, bool) {
        self.priorities
            .get(stream_id)
            .copied()
            .unwrap_or((DEFAULT_URGENCY, false))
    }

    /// Reorder a pre-sorted slice of writable stream IDs so that inside each
    /// urgency bucket, incremental streams appear after non-incremental ones,
    /// and the incremental tail is rotated by [`Self::incremental_cursor`]
    /// (RFC 9218 §4).
    ///
    /// The input `buf` must already be sorted by `(urgency, stream_id)`:
    /// this routine only partitions and rotates inside same-urgency
    /// contiguous runs, it does not re-sort.
    ///
    /// Returns the total number of incremental streams seen, so callers that
    /// need to update the cursor at the end of the write pass can early-exit
    /// when the count is zero.
    pub fn apply_incremental_rotation(&self, buf: &mut [StreamId]) -> usize {
        let mut total_incremental = 0usize;
        let mut i = 0;
        while i < buf.len() {
            let (urgency_i, _) = self.get(&buf[i]);
            let mut j = i + 1;
            while j < buf.len() {
                let (urgency_j, _) = self.get(&buf[j]);
                if urgency_j != urgency_i {
                    break;
                }
                j += 1;
            }
            // `buf[i..j]` is a contiguous run of same-urgency stream IDs.
            let bucket = &mut buf[i..j];
            if bucket.len() > 1 {
                // Stable partition: non-incremental first, incremental last,
                // each subrange staying in ascending stream-id order.
                bucket.sort_by_key(|id| self.get(id).1);
                let split = bucket.partition_point(|id| !self.get(id).1);
                let incremental_tail = &mut bucket[split..];
                if incremental_tail.len() > 1 {
                    // Rotate so the pass starts right after the stream that
                    // fired first previously. `partition_point` returns the
                    // first index whose stream ID > cursor (so cursor itself
                    // is still drained, but after the streams ahead of it).
                    let start =
                        incremental_tail.partition_point(|id| *id <= self.incremental_cursor);
                    incremental_tail.rotate_left(start);
                }
                total_incremental += incremental_tail.len();
            } else if bucket.len() == 1 && self.get(&bucket[0]).1 {
                total_incremental += 1;
            }
            i = j;
        }
        total_incremental
    }

    /// Advance the RFC 9218 §4 round-robin cursor after a write pass.
    ///
    /// `first_incremental_fired` is the stream ID that headed the incremental
    /// tail we just drained; the next pass will start at the next stream
    /// after that ID. Callers may pass `None` when no incremental streams
    /// were eligible, leaving the cursor where it was.
    pub fn advance_incremental_cursor(&mut self, first_incremental_fired: Option<StreamId>) {
        if let Some(id) = first_incremental_fired {
            self.incremental_cursor = id;
        }
    }
}

/// Connection-level flow control state (RFC 9113 §6.9).
pub struct H2FlowControl {
    /// Connection-level send window (can go negative per RFC 9113 §6.9.2).
    pub window: i32,
    /// Bytes received since last connection-level WINDOW_UPDATE.
    pub received_bytes_since_update: u32,
    /// Queued stream_id -> accumulated increment for WINDOW_UPDATE frames (O(1) coalescing).
    pub pending_window_updates: HashMap<u32, u32>,
}

/// Byte accounting for connection overhead attribution.
pub struct H2ByteAccounting {
    /// Bytes read on the zero stream not yet attributed to a stream.
    pub zero_bytes_read: usize,
    /// Overhead bytes received (connection-level frames).
    pub overhead_bin: usize,
    /// Overhead bytes sent (connection-level frames).
    pub overhead_bout: usize,
}

/// Connection draining state for graceful shutdown.
pub struct H2DrainState {
    /// True when we've sent GOAWAY and are draining.
    pub draining: bool,
    /// Last stream ID from peer's GOAWAY (for retry decisions).
    pub peer_last_stream_id: Option<StreamId>,
    /// Wall-clock timestamp captured the first time this connection entered
    /// `draining` during soft-stop. Used together with
    /// [`Self::graceful_shutdown_deadline`] to decide when to force-close.
    /// Remains `None` until the proxy-initiated drain begins (peer-initiated
    /// drains via `handle_goaway_frame` don't arm the forced-close timer —
    /// the caller in `Mux::shutting_down` is the only writer).
    pub started_at: Option<Instant>,
    /// Wall-clock budget granted to in-flight streams after the phase-1
    /// `GOAWAY(NO_ERROR)`. `None` means "wait indefinitely" (knob value `0`).
    /// Default when unset upstream: 5 s (see `L7ListenerHandler`).
    pub graceful_shutdown_deadline: Option<std::time::Duration>,
}

pub struct ConnectionH2<Front: SocketHandler> {
    /// Connection/session ULID propagated from the parent [`Mux`]. Used to
    /// stamp the session slot of the `[session req cluster backend]` log
    /// prefix emitted by this module's `log_context!` / `log_context_stream!`
    /// macros.
    pub session_ulid: Ulid,
    pub decoder: loona_hpack::Decoder<'static>,
    pub encoder: loona_hpack::Encoder<'static>,
    pub expect_read: Option<(H2StreamId, usize)>,
    pub expect_write: Option<H2StreamId>,
    pub last_stream_id: StreamId,
    pub local_settings: H2Settings,
    pub peer_settings: H2Settings,
    pub position: Position,
    pub prioriser: Prioriser,
    pub readiness: Readiness,
    pub socket: Front,
    pub state: H2State,
    pub streams: HashMap<StreamId, GlobalStreamId>,
    pub timeout_container: TimeoutContainer,
    /// Connection-level flow control state (send window, receive tracking, pending updates).
    pub flow_control: H2FlowControl,
    /// Highest stream ID accepted from the peer (used for GoAway last_stream_id).
    pub highest_peer_stream_id: StreamId,
    /// RFC 7541 §4.2 / §6.3 pending dynamic-table-size-update signal.
    ///
    /// `Some(new_size)` when a peer SETTINGS frame adjusted
    /// `SETTINGS_HEADER_TABLE_SIZE` and we have not yet prepended the
    /// matching `001xxxxx` HPACK directive to a header block. Consumed and
    /// cleared by [`H2BlockConverter::emit_pending_size_update_if_new_block`]
    /// on the next `Block::StatusLine` or `Block::Header` encoded for the
    /// connection. Until then the peer's decoder still has its previous
    /// (possibly larger) table cap, so emitting is a correctness
    /// requirement, not a nicety — see the RFC 9113 encoder-decoder
    /// synchronisation contract (§6.5.2).
    pub pending_table_size_update: Option<u32>,
    /// Reusable buffer for HPACK-encoded headers in the H2 block converter.
    pub converter_buf: Vec<u8>,
    /// Reusable buffer for lowercasing header keys in the H2 block converter.
    pub lowercase_buf: Vec<u8>,
    /// Reusable buffer for assembling cookie values in the H2 block converter.
    pub cookie_buf: Vec<u8>,
    /// Connection draining state for graceful shutdown.
    pub drain: H2DrainState,
    pub zero: GenericHttpStream,
    /// Byte accounting for connection overhead attribution.
    pub bytes: H2ByteAccounting,
    /// Flood detector for CVE mitigations (Rapid Reset, CONTINUATION, Ping, Settings floods).
    pub flood_detector: H2FloodDetector,
    /// RFC 9113 §6.5: timestamp when we sent SETTINGS and are awaiting ACK.
    /// If the peer does not ACK within SETTINGS_ACK_TIMEOUT, we send GOAWAY
    /// with SettingsTimeout error.
    pub settings_sent_at: Option<Instant>,
    /// Queued RST_STREAM frames to send: Vec<(stream_id, error_code)>.
    /// Used when refusing streams (MAX_CONCURRENT_STREAMS, buffer exhaustion)
    /// during readable — the actual write happens in the writable preamble
    /// to avoid conflicting with kawa.storage usage for frame payload discard.
    pub pending_rst_streams: Vec<(StreamId, H2Error)>,
    /// RFC 9113 §6.8: tracks stream IDs for which RST_STREAM has already been sent,
    /// preventing duplicate RST_STREAM frames on the wire.
    pub rst_sent: HashSet<StreamId>,
    /// Lifetime counter of RST_STREAM frames queued (pending + already flushed).
    /// Used to detect sustained misbehavior even when writable() drains the
    /// pending queue between readable() calls.
    pub total_rst_streams_queued: usize,
    /// Reusable buffer for priority-sorted stream IDs in write_streams().
    /// Cleared and reused each call to avoid per-frame allocation.
    priorities_buf: Vec<StreamId>,
    /// True once we've asked rustls to emit TLS close_notify for this frontend.
    close_notify_sent: bool,
    /// Per-listener H2 connection tuning (window size, max streams, shrink ratio).
    pub connection_config: H2ConnectionConfig,
    /// Maximum pending WINDOW_UPDATE entries before dropping.
    /// Derived from `connection_config.max_concurrent_streams` at construction.
    max_pending_window_updates: usize,
    /// Last `(connection_window, active_streams, pending_window_updates)` snapshot
    /// emitted by [`Self::gauge_connection_state`]. The snapshot represents this
    /// connection's *contribution* to the three `h2.connection.*` aggregate
    /// gauges; each call emits the signed delta against this snapshot via
    /// [`gauge_add!`] so the gauge sums across connections.
    ///
    /// Stays `None` until the first emission. [`Drop`] applies the negative of
    /// this snapshot so the connection's contribution is always rebalanced to
    /// zero on teardown — independent of which close path runs.
    last_gauge_snapshot: Option<(usize, usize, usize)>,
    /// Per-stream wall-clock timestamp of last meaningful activity (DATA or
    /// HEADERS frame receipt). Used to cancel streams that make no forward
    /// progress within [`Self::stream_idle_timeout`] — mitigates slow-multiplex
    /// Slowloris: connection-level idle timers reset on every frame, so a
    /// misbehaving peer can otherwise pin up to `max_concurrent_streams` slots
    /// for the full nominal connection timeout.
    ///
    /// Initialized when the stream is created and refreshed on each non-empty
    /// inbound DATA frame and on HEADERS for an existing stream (trailers).
    /// Empty DATA frames (CVE-2019-9518 vector) do NOT refresh the timer.
    pub stream_last_activity_at: HashMap<StreamId, Instant>,
    /// Per-stream idle cap. Streams with no activity for longer than this are
    /// RST_STREAM(CANCEL)'d by [`Self::cancel_timed_out_streams`].
    pub stream_idle_timeout: std::time::Duration,
    /// RFC 9113 §5.1.2 back-pressure: count of stream refusals
    /// (REFUSED_STREAM emitted via [`Self::refuse_stream_and_discard`]) within
    /// the current back-pressure window. When the count exceeds
    /// [`BACKPRESSURE_REFUSAL_THRESHOLD`] inside one
    /// [`BACKPRESSURE_WINDOW_DURATION`] we halve the advertised
    /// `SETTINGS_MAX_CONCURRENT_STREAMS` to signal the peer to slow down.
    refuse_count_window: u32,
    /// Start timestamp for the current back-pressure window.
    refuse_window_start: Instant,
    /// Set once we have halved `local_settings.settings_max_concurrent_streams`
    /// in response to a refusal burst. Prevents the cap from collapsing to 0
    /// on sustained abuse — a single halving per connection is sufficient to
    /// signal back-pressure; further bursts trigger `EnhanceYourCalm`.
    mcs_backpressure_applied: bool,
}
impl<Front: SocketHandler> std::fmt::Debug for ConnectionH2<Front> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionH2")
            .field("position", &self.position)
            .field("state", &self.state)
            .field("expect", &self.expect_read)
            .field("readiness", &self.readiness)
            .field("local_settings", &self.local_settings)
            .field("peer_settings", &self.peer_settings)
            .field("socket", &self.socket.socket_ref())
            .field("streams", &self.streams)
            .field("zero", &self.zero.storage.meter(20))
            .field("window", &self.flow_control.window)
            .field("total_rst_streams_queued", &self.total_rst_streams_queued)
            .finish()
    }
}

/// Symmetric tear-down for the three `h2.connection.*` aggregate gauges:
/// whatever positive contribution this connection made via
/// [`ConnectionH2::gauge_connection_state`] is subtracted back out when the
/// connection is dropped.
///
/// Using `Drop` (rather than wiring decrements into every close path —
/// `graceful_goaway`, `force_disconnect`, `handle_goaway_frame`, `Mux::close`,
/// stream-id exhaustion, panic-unwind) is what guarantees the gauge is
/// arithmetically symmetric regardless of which path teardown took. Past
/// underflow incidents (commits a650ad69, d2f01ed4) have all been
/// missing-decrement bugs that `Drop` makes structurally impossible.
impl<Front: SocketHandler> Drop for ConnectionH2<Front> {
    fn drop(&mut self) {
        self.release_connection_gauges();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum H2StreamId {
    Zero,
    Other { id: StreamId, gid: GlobalStreamId },
}

impl<Front: SocketHandler> ConnectionH2<Front> {
    fn frontend_hung_up_while_draining(&self) -> bool {
        matches!(self.position, Position::Server)
            && self.drain.draining
            && (self.readiness.event.is_hup() || self.readiness.event.is_error())
    }

    /// Once the final GOAWAY has been queued and all streams/control frames are
    /// gone, a peer-side HUP/ERR means any remaining rustls backlog is no
    /// longer deliverable. Waiting on `socket_wants_write()` in that state can
    /// deadlock shutdown forever because GOAWAY disables further frame reads.
    fn peer_gone_after_final_goaway(&self) -> bool {
        self.frontend_hung_up_while_draining()
            && matches!(self.state, H2State::GoAway | H2State::Error)
            && self.streams.is_empty()
            && self.expect_write.is_none()
            && self.zero.storage.is_empty()
    }

    /// Shared constructor for both server and client H2 connections.
    ///
    /// Differences between server and client are captured by the caller-provided
    /// `position`, `expect_read`, and `readiness_interest` parameters.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        session_ulid: Ulid,
        socket: Front,
        position: super::Position,
        pool: std::rc::Weak<std::cell::RefCell<crate::pool::Pool>>,
        flood_config: H2FloodConfig,
        connection_config: H2ConnectionConfig,
        stream_idle_timeout: std::time::Duration,
        graceful_shutdown_deadline: Option<std::time::Duration>,
        timeout_container: crate::timer::TimeoutContainer,
        expect_read: Option<(H2StreamId, usize)>,
        readiness_interest: sozu_command::ready::Ready,
    ) -> Option<Self> {
        let buffer = pool
            .upgrade()
            .and_then(|pool| pool.borrow_mut().checkout())?;
        let local_settings = H2Settings {
            settings_max_concurrent_streams: connection_config.max_concurrent_streams,
            ..H2Settings::default()
        };
        let mut decoder = loona_hpack::Decoder::new();
        // RFC 7541 §4.2: enforce SETTINGS_HEADER_TABLE_SIZE as the upper bound
        // for dynamic table size updates from the peer
        decoder.set_max_allowed_table_size(local_settings.settings_header_table_size as usize);
        Some(ConnectionH2 {
            session_ulid,
            decoder,
            encoder: loona_hpack::Encoder::new(),
            expect_read,
            expect_write: None,
            last_stream_id: 0,
            local_settings,
            peer_settings: H2Settings::default(),
            position,
            prioriser: Prioriser::default(),
            readiness: crate::Readiness {
                interest: readiness_interest,
                event: Ready::EMPTY,
            },
            socket,
            state: H2State::ClientPreface,
            streams: std::collections::HashMap::with_capacity(8),
            timeout_container,
            flow_control: H2FlowControl {
                window: DEFAULT_INITIAL_WINDOW_SIZE as i32,
                received_bytes_since_update: 0,
                pending_window_updates: HashMap::new(),
            },
            highest_peer_stream_id: 0,
            pending_table_size_update: None,
            converter_buf: Vec::new(),
            lowercase_buf: Vec::new(),
            cookie_buf: Vec::new(),
            drain: H2DrainState {
                draining: false,
                peer_last_stream_id: None,
                started_at: None,
                graceful_shutdown_deadline,
            },
            zero: kawa::Kawa::new(kawa::Kind::Request, kawa::Buffer::new(buffer)),
            bytes: H2ByteAccounting {
                zero_bytes_read: 0,
                overhead_bin: 0,
                overhead_bout: 0,
            },
            flood_detector: H2FloodDetector::new(flood_config),
            settings_sent_at: None,
            pending_rst_streams: Vec::new(),
            rst_sent: std::collections::HashSet::new(),
            total_rst_streams_queued: 0,
            priorities_buf: Vec::new(),
            close_notify_sent: false,
            max_pending_window_updates: 1 + connection_config.max_concurrent_streams as usize * 4,
            connection_config,
            last_gauge_snapshot: None,
            stream_last_activity_at: HashMap::new(),
            stream_idle_timeout,
            refuse_count_window: 0,
            refuse_window_start: Instant::now(),
            mcs_backpressure_applied: false,
        })
    }

    /// Start TLS close_notify on the frontend and keep the session alive until
    /// rustls has flushed the generated records.
    pub fn initiate_close_notify(&mut self) -> bool {
        if !self.position.is_server()
            || matches!(
                self.state,
                H2State::ClientPreface | H2State::ClientSettings | H2State::ServerSettings
            )
        {
            return false;
        }
        if !self.close_notify_sent {
            trace!("{} H2 initiating CLOSE_NOTIFY", log_context!(self));
            self.socket.socket_close();
            self.close_notify_sent = true;
        }
        if self.socket.socket_wants_write() {
            self.readiness.interest = Ready::WRITABLE | Ready::HUP | Ready::ERROR;
            self.ensure_tls_flushed();
            true
        } else {
            false
        }
    }

    fn expect_header(&mut self) {
        self.state = H2State::Header;
        self.expect_read = Some((H2StreamId::Zero, 9));
    }

    /// Process the `H2State::Header` state: parse a 9-byte frame header from
    /// `self.zero`, validate the stream, create new streams if needed, and
    /// transition to `H2State::Frame` for the payload.
    ///
    /// Returns `MuxResult` — the caller should propagate the result directly.
    fn handle_header_state<L>(&mut self, context: &mut Context<L>) -> MuxResult
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        let i = self.zero.storage.data();
        trace!("{}   header: {:?}", log_context!(self), i);
        match parser::frame_header(i, self.local_settings.settings_max_frame_size) {
            Ok((_, header)) => {
                trace!("{} {:#?}", log_context!(self), header);
                self.zero.storage.clear();
                let stream_id = header.stream_id;
                // RFC 9113 §6.10: CONTINUATION frames MUST be preceded by a
                // HEADERS or PUSH_PROMISE frame without END_HEADERS. When we
                // reach `handle_header_state`, we are between frames and no
                // header block is in progress (otherwise the state would be
                // `H2State::ContinuationHeader`). A CONTINUATION frame arriving
                // here is therefore standalone and MUST be treated as a
                // connection error of type PROTOCOL_ERROR.
                if header.frame_type == FrameType::Continuation {
                    error!(
                        "{} standalone CONTINUATION frame on stream {} without preceding HEADERS",
                        log_context!(self),
                        stream_id
                    );
                    return self.goaway(H2Error::ProtocolError);
                }
                // RFC 9113 §5.5: unknown frame types MUST be ignored and discarded.
                // Route unknown frames (and any stream_id == 0 control frame)
                // through stream 0 (the connection-level buffer) so
                // `handle_frame` can drop them without touching stream state.
                let read_stream = if stream_id == 0
                    || matches!(header.frame_type, FrameType::Unknown(_))
                {
                    H2StreamId::Zero
                } else if let Some(global_stream_id) = self.streams.get(&stream_id) {
                    let allowed_on_half_closed = header.frame_type == FrameType::WindowUpdate
                        || header.frame_type == FrameType::Priority
                        || header.frame_type == FrameType::RstStream;
                    let stream = &context.streams[*global_stream_id];
                    // Use the position-aware end_of_stream flag:
                    // - Server reads from front (client requests)
                    // - Client reads from back (backend responses)
                    let received_eos = if self.position.is_server() {
                        stream.front_received_end_of_stream
                    } else {
                        stream.back_received_end_of_stream
                    };
                    trace!(
                        "{} REQUESTING EXISTING STREAM {}: {}/{:?}",
                        log_context!(self),
                        stream_id,
                        received_eos,
                        stream.state
                    );
                    if !allowed_on_half_closed && (received_eos || !stream.state.is_open()) {
                        error!(
                            "{} CANNOT RECEIVE {:?} ON THIS STREAM {:?}",
                            log_context!(self),
                            header.frame_type,
                            stream.state
                        );
                        return self.goaway(H2Error::StreamClosed);
                    }
                    // RFC 9113 §8.1: a HEADERS frame received in the body
                    // phase is a trailer block and MUST carry END_STREAM. This
                    // closes the request-smuggling primitive where a peer sends
                    // HEADERS, DATA, HEADERS (no END_STREAM) to chain header
                    // blocks on the same stream ID.
                    //
                    // Discriminate from the read-side Kawa parsing phase rather
                    // than stream existence: on Position::Client the stream is
                    // created when we send the request to the backend, so the
                    // initial backend response HEADERS legitimately arrives on
                    // an existing stream. Similarly, 1xx→final transitions on
                    // either side may yield multiple HEADERS frames before the
                    // body begins (kawa clears back to initial / terminated on
                    // 1xx; neither is main_phase). Only HEADERS arriving once
                    // the read side has transitioned to Body/Chunks parsing —
                    // i.e. after headers were fully consumed and body framing
                    // is in progress — may be a trailer.
                    let read_in_body = if self.position.is_server() {
                        stream.front.is_main_phase()
                    } else {
                        stream.back.is_main_phase()
                    };
                    if header.frame_type == FrameType::Headers
                        && read_in_body
                        && header.flags & parser::FLAG_END_STREAM == 0
                    {
                        error!(
                            "{} HEADERS without END_STREAM on open stream {} in body phase: trailers MUST carry END_STREAM",
                            log_context!(self),
                            stream_id
                        );
                        return self.goaway(H2Error::ProtocolError);
                    }
                    if header.frame_type == FrameType::Data {
                        H2StreamId::Other {
                            id: stream_id,
                            gid: *global_stream_id,
                        }
                    } else {
                        H2StreamId::Zero
                    }
                } else {
                    // RFC 9113 §5.1.1: stream identifiers MUST be strictly
                    // increasing. Tightened from `>=` to `>` so that a peer
                    // cannot re-use `self.last_stream_id` (which would
                    // conflict with our own server-pushed streams if we
                    // ever enable push in the future). For the first
                    // request on a fresh connection `last_stream_id == 0`
                    // and any client-initiated odd stream still passes.
                    if header.frame_type == FrameType::Headers
                        && self.position.is_server()
                        && stream_id & 1 == 1
                        && stream_id > self.last_stream_id
                    {
                        // RFC 9113 §6.8: after sending a GOAWAY, the proxy
                        // MUST NOT accept new streams.
                        // `graceful_goaway` sets `drain.draining = true`
                        // and sends a phase-1 GOAWAY with last_stream_id =
                        // STREAM_ID_MAX (so in-flight requests are still
                        // accepted), but the contract for *new* peer-
                        // initiated streams is that they must be refused.
                        // Without this check, a peer racing the drain
                        // window could open arbitrary new streams between
                        // phase-1 and phase-2 GOAWAY emission.
                        if self.drain.draining {
                            if stream_id > self.highest_peer_stream_id {
                                self.highest_peer_stream_id = stream_id;
                            }
                            return self.refuse_stream_and_discard(
                                stream_id,
                                H2Error::RefusedStream,
                                header.payload_len,
                            );
                        }
                        if self.streams.len()
                            >= self.local_settings.settings_max_concurrent_streams as usize
                        {
                            error!(
                                "{} MAX CONCURRENT STREAMS: limit={}, current={}",
                                log_context!(self),
                                self.local_settings.settings_max_concurrent_streams,
                                self.streams.len()
                            );
                            // RFC 9113 §6.8: update highest_peer_stream_id BEFORE
                            // queueing RST_STREAM so GOAWAY reports the correct
                            // last_stream_id if the connection closes later.
                            if stream_id > self.highest_peer_stream_id {
                                self.highest_peer_stream_id = stream_id;
                            }
                            return self.refuse_stream_and_discard(
                                stream_id,
                                H2Error::RefusedStream,
                                header.payload_len,
                            );
                        }
                        match self.create_stream(stream_id, context) {
                            Some(_) => {}
                            None => {
                                // Buffer pool exhaustion is transient — refuse
                                // this stream but keep the connection alive so
                                // existing streams can complete and free buffers.
                                error!(
                                    "{} Could not create stream {}: buffer pool exhausted",
                                    log_context!(self),
                                    stream_id
                                );
                                // RFC 9113 §6.8: update highest_peer_stream_id BEFORE
                                // queueing RST_STREAM so GOAWAY reports the correct
                                // last_stream_id if the connection closes later.
                                if stream_id > self.highest_peer_stream_id {
                                    self.highest_peer_stream_id = stream_id;
                                }
                                return self.refuse_stream_and_discard(
                                    stream_id,
                                    H2Error::RefusedStream,
                                    header.payload_len,
                                );
                            }
                        }
                    } else if header.frame_type != FrameType::Priority {
                        // Distinguish closed vs idle: check whether the stream
                        // was previously opened. For Server position, compare
                        // against highest_peer_stream_id (client-initiated).
                        // For Client position, compare against last_stream_id
                        // (our own initiated streams) since the peer never
                        // initiates streams on a backend connection.
                        let is_closed_stream = if self.position.is_server() {
                            header.stream_id <= self.highest_peer_stream_id
                        } else {
                            header.stream_id < self.last_stream_id
                        };
                        if is_closed_stream {
                            match header.frame_type {
                                FrameType::RstStream | FrameType::WindowUpdate => {
                                    // RFC 9113 §5.1: RST_STREAM and WINDOW_UPDATE
                                    // on a closed stream can arrive due to race
                                    // conditions and should be consumed/discarded.
                                    debug!(
                                        "{} Ignoring {:?} on closed stream {}",
                                        log_context!(self),
                                        header.frame_type,
                                        header.stream_id
                                    );
                                    self.flood_detector.glitch_count += 1;
                                    check_flood_or_return!(self);
                                }
                                FrameType::Data => {
                                    // RFC 9113 §5.1: DATA on a closed stream is a
                                    // stream error of type STREAM_CLOSED. Queue
                                    // RST_STREAM (not GOAWAY) to preserve the
                                    // connection for other streams. The payload is
                                    // still routed through stream 0 so handle_frame
                                    // can do connection-level flow control accounting.
                                    debug!(
                                        "{} DATA on closed stream {}, sending RST_STREAM(STREAM_CLOSED)",
                                        log_context!(self),
                                        header.stream_id
                                    );
                                    self.flood_detector.glitch_count += 1;
                                    check_flood_or_return!(self);
                                    if let Some(result) =
                                        self.enqueue_rst(header.stream_id, H2Error::StreamClosed)
                                    {
                                        return result;
                                    }
                                }
                                _ => {
                                    // RFC 9113 §5.1: HEADERS or other frames on a
                                    // closed stream → connection error STREAM_CLOSED.
                                    error!(
                                        "{} Received {:?} on closed stream {}, sending GOAWAY(STREAM_CLOSED)",
                                        log_context!(self),
                                        header.frame_type,
                                        header.stream_id
                                    );
                                    return self.goaway(H2Error::StreamClosed);
                                }
                            }
                        } else {
                            error!(
                                "{} Received {:?} on idle stream {}, sending GOAWAY(PROTOCOL_ERROR)",
                                log_context!(self),
                                header.frame_type,
                                header.stream_id
                            );
                            return self.goaway(H2Error::ProtocolError);
                        }
                    }
                    H2StreamId::Zero
                };
                trace!(
                    "{} {} {:?} {:#?}",
                    log_context!(self),
                    header.stream_id,
                    stream_id,
                    self.streams
                );
                self.expect_read = Some((read_stream, header.payload_len as usize));
                self.state = H2State::Frame(header);
            }
            Err(error) => {
                let error = error_nom_to_h2(error);
                error!("{} COULD NOT PARSE FRAME HEADER", log_context!(self));
                return self.goaway(error);
            }
        };
        MuxResult::Continue
    }

    /// Process the `H2State::ContinuationHeader` state: parse a CONTINUATION
    /// frame header from `self.zero`, validate stream ID continuity, track
    /// flood detection counters, and transition to `ContinuationFrame`.
    ///
    /// The `headers` parameter is the accumulated HEADERS context from the
    /// initial HEADERS frame (cloned from the state enum to avoid borrow
    /// conflicts).
    fn handle_continuation_header_state(&mut self, headers: &Headers) -> MuxResult {
        let i = self.zero.storage.unparsed_data();
        trace!("{}   continuation header: {:?}", log_context!(self), i);
        match parser::frame_header(i, self.local_settings.settings_max_frame_size) {
            Ok((
                _,
                FrameHeader {
                    payload_len,
                    frame_type: FrameType::Continuation,
                    flags,
                    stream_id,
                },
            )) => {
                if self.zero.storage.end < 9 {
                    error!(
                        "{} CONTINUATION header: storage.end ({}) too small to remove frame header",
                        log_context!(self),
                        self.zero.storage.end
                    );
                    return self.goaway(H2Error::InternalError);
                }
                self.zero.storage.end -= 9;
                if stream_id != headers.stream_id {
                    error!(
                        "{} CONTINUATION stream_id {} does not match HEADERS stream_id {}",
                        log_context!(self),
                        stream_id,
                        headers.stream_id
                    );
                    return self.goaway(H2Error::ProtocolError);
                }
                // CVE-2024-27316: track CONTINUATION frame count and accumulated size
                self.flood_detector.continuation_count += 1;
                self.flood_detector.accumulated_header_size = self
                    .flood_detector
                    .accumulated_header_size
                    .saturating_add(payload_len);
                check_flood_or_return!(self);
                // RFC 9113 §10.5.1: reject header blocks that cannot be
                // buffered. Previously we silently removed READABLE interest
                // when amount > available_space, stalling the connection.
                // If the payload still fits in our zero buffer we can refuse
                // just this stream (RST_STREAM + drain); if not, the
                // connection can no longer decode header blocks safely and we
                // escalate to GOAWAY(EnhanceYourCalm).
                if self.flood_detector.accumulated_header_size
                    > self.flood_detector.config.max_header_list_size
                {
                    error!(
                        "{} CONTINUATION accumulated header size {} exceeds {}",
                        log_context!(self),
                        self.flood_detector.accumulated_header_size,
                        self.flood_detector.config.max_header_list_size
                    );
                    if (payload_len as usize) > self.zero.storage.available_space() {
                        return self.goaway(H2Error::EnhanceYourCalm);
                    }
                    // Remove the already-created stream slot before refusing,
                    // so it does not leak against MAX_CONCURRENT_STREAMS. Route
                    // through `remove_dead_stream` so the expect_write/read
                    // invariant (§LIFECYCLE.md 5.4) holds on this path too.
                    if let Some(global_stream_id) = self.streams.get(&stream_id).copied() {
                        self.remove_dead_stream(stream_id, global_stream_id);
                    }
                    return self.refuse_stream_and_discard(
                        stream_id,
                        H2Error::RefusedStream,
                        payload_len,
                    );
                }
                if (payload_len as usize) > self.zero.storage.available_space() {
                    error!(
                        "{} CONTINUATION payload {} exceeds buffer space {}",
                        log_context!(self),
                        payload_len,
                        self.zero.storage.available_space()
                    );
                    return self.goaway(H2Error::EnhanceYourCalm);
                }
                self.expect_read = Some((H2StreamId::Zero, payload_len as usize));
                let mut headers = headers.clone();
                headers.end_headers = flags & parser::FLAG_END_HEADERS != 0;
                headers.header_block_fragment.len = headers
                    .header_block_fragment
                    .len
                    .saturating_add(payload_len);
                self.state = H2State::ContinuationFrame(headers);
            }
            Err(error) => {
                let error = error_nom_to_h2(error);
                error!("{} COULD NOT PARSE CONTINUATION HEADER", log_context!(self));
                return self.goaway(error);
            }
            other => {
                error!(
                    "{} UNEXPECTED {:?} WHILE PARSING CONTINUATION HEADER",
                    log_context!(self),
                    other
                );
                return self.goaway(H2Error::ProtocolError);
            }
        };
        MuxResult::Continue
    }

    pub fn readable<E, L>(&mut self, context: &mut Context<L>, mut endpoint: E) -> MuxResult
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        self.prune_inactive_streams_while_closing(context);
        // Pass 4 Medium #3: per-stream idle guard. Slow-multiplex Slowloris
        // sends one byte or a control frame per stream just often enough to
        // reset the connection-level timer; per-stream deadlines catch it.
        self.cancel_timed_out_streams(context, &mut endpoint);

        // RFC 9113 §6.5: check if peer has timed out on SETTINGS ACK
        if let Some(sent_at) = self.settings_sent_at {
            if sent_at.elapsed() >= SETTINGS_ACK_TIMEOUT {
                warn!(
                    "{} SETTINGS ACK timeout: no SETTINGS ACK observed within {:?}",
                    log_context!(self),
                    SETTINGS_ACK_TIMEOUT
                );
                return self.goaway(H2Error::SettingsTimeout);
            }
        }

        // Don't reset the timeout unconditionally here. Only application data
        // (DATA/HEADERS frames) should reset the timeout. H2 control frames
        // (PING, WINDOW_UPDATE, SETTINGS) must NOT reset it, otherwise a peer
        // sending periodic PINGs prevents timeout detection on stuck sessions.
        // The timeout is reset:
        // - Below, when reading DATA payload (H2StreamId::Other)
        // - In handle_frame(), when processing HEADERS frames
        let (stream_id, kawa) = if let Some((stream_id, amount)) = self.expect_read {
            let (kawa, did) = match stream_id {
                H2StreamId::Zero => (&mut self.zero, usize::MAX),
                H2StreamId::Other {
                    gid: global_stream_id,
                    ..
                } => {
                    // Reading DATA frame payload for an application stream.
                    // This is real application activity — reset the timeout.
                    self.timeout_container.reset();
                    (
                        context.streams[global_stream_id]
                            .split(&self.position)
                            .rbuffer,
                        global_stream_id,
                    )
                }
            };
            trace!(
                "{} {:?}({:?}, {})",
                log_context!(self),
                self.state,
                stream_id,
                amount
            );
            if amount > 0 {
                if amount > kawa.storage.available_space() {
                    self.readiness.interest.remove(Ready::READABLE);
                    return MuxResult::Continue;
                }
                let (size, status) = self.socket.socket_read(&mut kawa.storage.space()[..amount]);
                context.debug.push(DebugEvent::SocketIO(0, did, size));
                kawa.storage.fill(size);
                self.position.count_bytes_in_counter(size);
                self.bytes.zero_bytes_read += size;
                if update_readiness_after_read(size, status, &mut self.readiness) {
                    if matches!(self.position, Position::Server)
                        && self.drain.draining
                        && matches!(status, SocketResult::Closed | SocketResult::Error)
                    {
                        // During graceful drain, a frontend EOF/HUP means no
                        // further frame headers or payload bytes can arrive.
                        // Keeping expect_read here strands the connection in
                        // Header/Frame forever even after the peer is gone.
                        self.expect_read = None;
                    }
                    return MuxResult::Continue;
                } else if size == amount {
                    self.expect_read = None;
                } else {
                    self.expect_read = Some((stream_id, amount - size));
                    if let (H2State::ClientPreface, Position::Server) =
                        (&self.state, &self.position)
                    {
                        let i = kawa.storage.data();
                        if !b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".starts_with(i) {
                            debug!("{} EARLY INVALID PREFACE: {:?}", log_context!(self), i);
                            return self.force_disconnect();
                        }
                    }
                    return MuxResult::Continue;
                }
            } else {
                self.expect_read = None;
            }
            (stream_id, kawa)
        } else {
            self.readiness.event.remove(Ready::READABLE);
            return MuxResult::Continue;
        };
        match (&self.state, &self.position) {
            (H2State::Error, _)
            | (H2State::GoAway, _)
            | (H2State::ServerSettings, Position::Server)
            | (H2State::ClientPreface, Position::Client(..))
            | (H2State::ClientSettings, Position::Client(..)) => {
                error!(
                    "{} Unexpected combination: (Readable, {:?}, {:?})",
                    log_context!(self),
                    self.state,
                    self.position
                );
                return self.force_disconnect();
            }
            (H2State::Discard, _) => {
                let _i = kawa.storage.data();
                trace!("{} DISCARDING: {:?}", log_context!(self), _i);
                kawa.storage.clear();
                self.attribute_bytes_to_overhead();
                self.expect_header();
            }
            (H2State::ClientPreface, Position::Server) => {
                let i = kawa.storage.data();
                let i = match parser::preface(i) {
                    Ok((i, _)) => i,
                    Err(_) => return self.force_disconnect(),
                };
                match parser::frame_header(i, self.local_settings.settings_max_frame_size) {
                    Ok((
                        _,
                        FrameHeader {
                            payload_len,
                            frame_type: FrameType::Settings,
                            flags: 0,
                            stream_id: 0,
                        },
                    )) => {
                        kawa.storage.clear();
                        self.state = H2State::ClientSettings;
                        self.expect_read = Some((H2StreamId::Zero, payload_len as usize));
                    }
                    _ => return self.force_disconnect(),
                };
            }
            (H2State::ClientSettings, Position::Server) => {
                let i = kawa.storage.data();
                let settings = match parser::settings_frame(
                    i,
                    &FrameHeader {
                        payload_len: i.len() as u32,
                        frame_type: FrameType::Settings,
                        flags: 0,
                        stream_id: 0,
                    },
                ) {
                    Ok((_, settings)) => {
                        kawa.storage.clear();
                        settings
                    }
                    Err(_) => return self.force_disconnect(),
                };
                let kawa = &mut self.zero;
                match serializer::gen_settings(kawa.storage.space(), &self.local_settings) {
                    Ok((_, size)) => {
                        kawa.storage.fill(size);
                        incr!("h2.frames.tx.settings");
                        // RFC 9113 §6.5: start tracking SETTINGS ACK timeout
                        self.settings_sent_at = Some(Instant::now());
                    }
                    Err(error) => {
                        error!(
                            "{} Could not serialize SettingsFrame: {:?}",
                            log_context!(self),
                            error
                        );
                        return self.force_disconnect();
                    }
                };

                self.state = H2State::ServerSettings;
                self.expect_write = Some(H2StreamId::Zero);
                self.readiness.signal_pending_write();
                return self.handle_frame(settings, 0, context, endpoint);
            }
            (H2State::ServerSettings, Position::Client(..)) => {
                let i = kawa.storage.data();
                match parser::frame_header(i, self.local_settings.settings_max_frame_size) {
                    Ok((
                        _,
                        header @ FrameHeader {
                            payload_len,
                            frame_type: FrameType::Settings,
                            flags: 0,
                            stream_id: 0,
                        },
                    )) => {
                        kawa.storage.clear();
                        self.expect_read = Some((H2StreamId::Zero, payload_len as usize));
                        self.state = H2State::Frame(header)
                    }
                    _ => return self.force_disconnect(),
                };
            }
            (H2State::Header, _) => {
                return self.handle_header_state(context);
            }
            (H2State::ContinuationHeader(headers), _) => {
                let headers = headers.clone();
                return self.handle_continuation_header_state(&headers);
            }
            (H2State::Frame(header), _) => {
                let i = kawa.storage.unparsed_data();
                trace!("{}   data: {:?}", log_context!(self), i);
                let wire_payload_len = header.payload_len;
                let frame = match parser::frame_body(i, header) {
                    Ok((_, frame)) => frame,
                    Err(error) => {
                        let error = error_nom_to_h2(error);
                        error!("{} COULD NOT PARSE FRAME BODY", log_context!(self));
                        return self.goaway(error);
                    }
                };
                if let H2StreamId::Zero = stream_id {
                    if header.frame_type == FrameType::Headers {
                        kawa.storage.head = kawa.storage.end;
                    } else {
                        kawa.storage.end = kawa.storage.head;
                    }
                }
                self.expect_header();
                return self.handle_frame(frame, wire_payload_len, context, endpoint);
            }
            (H2State::ContinuationFrame(headers), _) => {
                kawa.storage.head = kawa.storage.end;
                let i = kawa.storage.data();
                trace!("{}   data: {:?}", log_context!(self), i);
                let headers = headers.clone();
                self.expect_header();
                return self.handle_frame(Frame::Headers(headers), 0, context, endpoint);
            }
        }
        MuxResult::Continue
    }

    /// Update the H2 connection-level *aggregate* gauges with this connection's
    /// current contribution, expressed as a signed delta against the last
    /// snapshot we emitted.
    ///
    /// The three metrics are emitted via [`gauge_add!`] (lifecycle deltas) so
    /// that the dashboard sees the **sum across all live H2 connections**:
    ///
    /// - `h2.connection.window_bytes` — sum of available connection-level
    ///   send-window bytes. Negative per-connection windows clamp to 0 so the
    ///   aggregate represents only available capacity, not deficit.
    /// - `h2.connection.active_streams` — sum of in-flight streams across
    ///   every H2 connection.
    /// - `h2.connection.pending_window_updates` — sum of queued (un-flushed)
    ///   per-stream WINDOW_UPDATE entries across every H2 connection.
    ///
    /// Called from the write hot path; emits nothing when the snapshot is
    /// unchanged so the steady state stays cheap. The paired decrement for
    /// every increment is provided by [`Drop`], which subtracts the final
    /// snapshot when the connection is dropped — keeping the aggregate
    /// arithmetically symmetric independent of which close path runs
    /// (`graceful_goaway`, `force_disconnect`, `handle_goaway_frame`,
    /// `Mux::close`, panic-unwind, …).
    fn gauge_connection_state(&mut self) {
        let snapshot = (
            self.flow_control.window.max(0) as usize,
            self.streams.len(),
            self.flow_control.pending_window_updates.len(),
        );
        if self.last_gauge_snapshot == Some(snapshot) {
            return;
        }
        let prev = self.last_gauge_snapshot.unwrap_or((0, 0, 0));
        // Diff in i64 — usize cannot represent the negative side of the delta.
        let dw = snapshot.0 as i64 - prev.0 as i64;
        let ds = snapshot.1 as i64 - prev.1 as i64;
        let du = snapshot.2 as i64 - prev.2 as i64;
        if dw != 0 {
            gauge_add!("h2.connection.window_bytes", dw);
        }
        if ds != 0 {
            gauge_add!("h2.connection.active_streams", ds);
        }
        if du != 0 {
            gauge_add!("h2.connection.pending_window_updates", du);
        }
        self.last_gauge_snapshot = Some(snapshot);
    }

    /// Subtract this connection's contribution from the three aggregate
    /// `h2.connection.*` gauges. Idempotent: clears `last_gauge_snapshot` so a
    /// second call (or a [`Drop`] on top of an explicit reset) is a no-op.
    ///
    /// Pairs with every prior call to [`Self::gauge_connection_state`]; called
    /// from [`Drop`] so the symmetry is guaranteed regardless of the close
    /// path.
    fn release_connection_gauges(&mut self) {
        if let Some((w, s, u)) = self.last_gauge_snapshot.take() {
            if w != 0 {
                gauge_add!("h2.connection.window_bytes", -(w as i64));
            }
            if s != 0 {
                gauge_add!("h2.connection.active_streams", -(s as i64));
            }
            if u != 0 {
                gauge_add!("h2.connection.pending_window_updates", -(u as i64));
            }
        }
    }

    /// Write application data (request/response bodies, headers) across all
    /// active streams, respecting priority ordering and flow control.
    ///
    /// This is the main data-plane write path: it resumes any partially-written
    /// stream, prepares new frames via the H2 block converter, flushes them to
    /// the socket, and recycles completed streams.
    ///
    /// NOTE: The priority iteration loop and converter setup remain inline here
    /// because the converter borrows `self.encoder`, preventing further
    /// decomposition into `&mut self` methods within the loop body.
    fn write_streams<E, L>(&mut self, context: &mut Context<L>, mut endpoint: E) -> MuxResult
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        self.timeout_container.reset();
        // Pre-compute byte totals for proportional overhead distribution.
        let byte_totals = self.compute_stream_byte_totals(context);
        let mut io_slices: Vec<IoSlice<'static>> = Vec::new();

        if let Some(
            write_stream @ H2StreamId::Other {
                id: stream_id,
                gid: global_stream_id,
            },
        ) = self.expect_write
        {
            let stream = &mut context.streams[global_stream_id];
            let stream_state = stream.state;
            let parts = stream.split(&self.position);
            let kawa = parts.wbuffer;
            // Resume path: if the same stream is parked waiting for buffer
            // space (expect_read matches write_stream), pass the amount so
            // flush_stream_out can re-enable READABLE as soon as we drain.
            let cross_read_amount = match self.expect_read {
                Some((read_stream, amount)) if write_stream == read_stream => Some(amount),
                _ => None,
            };
            let mut resume_bytes: usize = 0;
            let outcome = Self::flush_stream_out(
                &mut self.socket,
                kawa,
                parts.metrics,
                &self.position,
                &mut self.readiness,
                &mut context.debug,
                2,
                global_stream_id,
                None,
                cross_read_amount,
                &mut io_slices,
                Some(&mut resume_bytes),
            );
            // Refresh the per-stream idle timer when outbound bytes move: a
            // large response delivered at low bandwidth is "active", not idle,
            // even when the peer sends no inbound frames.
            if resume_bytes > 0 {
                if let Some(t) = self.stream_last_activity_at.get_mut(&stream_id) {
                    *t = Instant::now();
                }
            }
            if outcome == FlushOutcome::Stalled {
                return MuxResult::Continue;
            }
            self.expect_write = None;
            if (kawa.is_terminated() || kawa.is_error())
                && kawa.is_completed()
                && !Self::handle_1xx_reset(kawa, stream_state, &mut endpoint)
            {
                let (client_rtt, server_rtt) = Self::snapshot_rtts(
                    &self.position,
                    &self.socket,
                    &endpoint,
                    stream.linked_token(),
                );

                if let Some((dead_id, token)) = Self::try_recycle_server_stream(
                    &self.position,
                    &mut self.bytes,
                    &self.streams,
                    stream,
                    global_stream_id,
                    stream_id,
                    byte_totals,
                    &mut context.debug,
                    context.listener.clone(),
                    client_rtt,
                    server_rtt,
                ) {
                    // Remove the recycled stream from the connection maps
                    // before endpoint.end_stream() can trigger teardown.
                    // Otherwise session close can observe a stale `Recycle`
                    // entry in self.streams and mis-handle the connection as
                    // if it still had an active H2 stream.
                    self.remove_dead_stream(dead_id, global_stream_id);
                    if let Some(token) = token {
                        remove_backend_stream(
                            &mut context.backend_streams,
                            token,
                            global_stream_id,
                        );
                        endpoint.end_stream(token, global_stream_id, context);
                    }
                }
            }
        }

        self.gauge_connection_state();

        let scheme: &'static [u8] = if context.listener.borrow().protocol() == Protocol::HTTPS {
            b"https"
        } else {
            b"http"
        };
        let mut completed_streams = Vec::new();
        let mut converter_buf = std::mem::take(&mut self.converter_buf);
        converter_buf.clear();
        let mut converter = converter::H2BlockConverter {
            max_frame_size: self.peer_settings.settings_max_frame_size as usize,
            window: 0,
            stream_id: 0,
            encoder: &mut self.encoder,
            out: converter_buf,
            scheme,
            lowercase_buf: std::mem::take(&mut self.lowercase_buf),
            cookie_buf: std::mem::take(&mut self.cookie_buf),
            // When this connection is a backend client we are writing
            // toward the upstream backend — flow-control stalls in that
            // direction are scoped to `backend.flow_control.paused` (in
            // addition to the existing direction-agnostic
            // `h2.flow_control_stall`).
            position_is_client: self.position.is_client(),
            // RFC 9218 §4: toggled per-stream in the loop below, driven by
            // `Prioriser::get(stream_id).1`. Non-incremental by default so
            // unit tests and non-scheduled callers (e.g. the resume path
            // above) keep the sequential semantics.
            incremental_mode: false,
            // Populated once per write pass from `apply_incremental_rotation`
            // below. The converter uses `incremental_peer_count <= 1` to skip
            // the RFC 9218 yield-after-one-DATA behaviour when there is no
            // peer to interleave with (solo-bucket fast path).
            incremental_peer_count: 0,
            // RFC 7541 §6.3: move the pending size-update onto the converter
            // so the first header block of this pass prepends the signal.
            // We clear the connection-side mirror only AFTER the write pass
            // confirms emission via `converter.size_update_emitted`, so a
            // DATA-only write pass (no header block) does not drop the
            // signal.
            pending_table_size_update: self.pending_table_size_update,
            size_update_emitted: false,
            // Reset on every write pass; `check_header_capacity` flips it
            // mid-call and `finalize` commits the abort by flipping
            // `kawa.parsing_phase` to Error so the next pass emits
            // RST_STREAM(InternalError).
            pending_oversized_abort: false,
        };
        self.priorities_buf.clear();
        self.priorities_buf.extend(self.streams.keys().copied());
        // RFC 9218 §4 primary sort: ascending urgency, then stream ID for
        // stability. The incremental flag is handled by
        // `apply_incremental_rotation` below so it does not perturb the
        // non-incremental fast path.
        self.priorities_buf.sort_by_cached_key(|id| {
            let (urgency, _) = self.prioriser.get(id);
            (urgency, *id)
        });
        // RFC 9218 §4: inside each urgency bucket, move incremental streams
        // to the tail and rotate them by the per-connection round-robin
        // cursor so no single slow-draining stream can starve its
        // same-urgency incremental peers.
        let incremental_count = self
            .prioriser
            .apply_incremental_rotation(&mut self.priorities_buf);

        // RFC 9218 §4 refinement (Tier 3a): the connection-global
        // `incremental_count` is too coarse for `converter.incremental_peer_count`.
        // A solo `u=0, i` stream with an unrelated `u=7, i` peer in a
        // different urgency bucket would still see `incremental_peer_count > 1`
        // and voluntarily yield — stranding bytes the invariant-15/16 guards
        // were meant to prevent. Scope the count to same-urgency streams that
        // are actually ready to emit this pass (eligibility mirrors the check
        // in the write loop below).
        let mut ready_incremental_by_urgency: HashMap<u8, usize> = HashMap::new();
        for &sid in self.priorities_buf.iter() {
            let (urgency, is_incremental) = self.prioriser.get(&sid);
            if !is_incremental {
                continue;
            }
            let Some(&gid) = self.streams.get(&sid) else {
                continue;
            };
            let wbuffer = match self.position {
                Position::Server => &context.streams[gid].back,
                Position::Client(..) => &context.streams[gid].front,
            };
            if wbuffer.is_main_phase()
                || (wbuffer.is_terminated() && !wbuffer.is_completed())
                || (wbuffer.is_error() && !self.rst_sent.contains(&sid))
            {
                *ready_incremental_by_urgency.entry(urgency).or_insert(0) += 1;
            }
        }

        trace!(
            "{} PRIORITIES: {:?} (incremental_count={}, per_bucket={:?})",
            log_context!(self),
            self.priorities_buf,
            incremental_count,
            ready_incremental_by_urgency
        );
        let mut socket_write = false;
        // RFC 9218 §4 round-robin: remember the first incremental stream we
        // served this pass so we can advance `Prioriser::incremental_cursor`
        // to it, causing the next pass to start with the stream just after.
        let mut first_incremental_fired: Option<StreamId> = None;
        // Total outbound bytes emitted across all stream flushes this pass —
        // `finalize_write` uses this to distinguish a voluntary scheduler
        // yield (progress + pending back-buffer, LIFECYCLE §9 invariant 16)
        // from a no-progress wait state (e.g. flow-control starvation).
        let mut total_bytes_written: usize = 0;
        // Collect every fresh RST_STREAM emitted via the converter
        // (`initialize` chokepoint or the HPACK over-budget abort path)
        // so we can run `account_emitted_rst` for each one AFTER the
        // converter is dropped — the converter holds `&mut self.encoder`
        // for the loop body so we cannot take `&mut self` until then.
        let mut freshly_emitted_rsts: Vec<H2Error> = Vec::new();
        'outer: for idx in 0..self.priorities_buf.len() {
            let stream_id = self.priorities_buf[idx];
            let Some(&global_stream_id) = self.streams.get(&stream_id) else {
                error!(
                    "{} stream_id {} from sorted keys missing in streams map",
                    log_context!(self),
                    stream_id
                );
                continue;
            };
            let (urgency, is_incremental) = self.prioriser.get(&stream_id);
            let stream = &mut context.streams[global_stream_id];
            let stream_state = stream.state;
            let parts = stream.split(&self.position);
            let kawa = parts.wbuffer;
            if kawa.is_main_phase()
                || (kawa.is_terminated() && !kawa.is_completed())
                || (kawa.is_error() && !self.rst_sent.contains(&stream_id))
            {
                let window = min(*parts.window, self.flow_control.window);
                converter.window = window;
                converter.stream_id = stream_id;
                // RFC 9218 §4: incremental streams yield the converter after
                // a single DATA frame so same-urgency peers interleave.
                converter.incremental_mode = is_incremental;
                // Same-urgency-bucket ready-peer count (Tier 3a, LIFECYCLE §9
                // invariant 17). The converter skips the yield when there is
                // no peer in the same bucket to interleave with — prevents
                // the `finalize_write` WRITABLE-withdrawal strand (see
                // `test_h2_solo_incremental_drains_fully`). A connection-wide
                // count would wrongly yield for a solo incremental stream
                // when another urgency bucket happens to contain an
                // incremental peer.
                converter.incremental_peer_count = ready_incremental_by_urgency
                    .get(&urgency)
                    .copied()
                    .unwrap_or(0);
                // Track RST_STREAM dedup: if kawa is in error state, the converter
                // will generate a RST_STREAM frame via `initialize`. Mark it so we
                // don't send a duplicate on the next writable cycle.
                if kawa.is_error() {
                    let freshly_rst = self.rst_sent.insert(stream_id);
                    // LIFECYCLE §9 invariant 17: any transition to ineligible
                    // mid-pass MUST decrement ready_incremental_by_urgency so
                    // later streams in the same 'outer iteration see the live
                    // count, not the snapshot. Missing this costs one voluntary
                    // yield per same-urgency peer that trails the RST.
                    if freshly_rst && is_incremental {
                        if let Some(c) = ready_incremental_by_urgency.get_mut(&urgency) {
                            *c = c.saturating_sub(1);
                        }
                    }
                    // Account for the RST that `initialize` is about to emit
                    // for this stream. Without this the MadeYouReset lifetime
                    // cap is evadable: any path that flips `parsing_phase` to
                    // Error before reaching this gate (oversized inbound
                    // trailers, malformed bodies, etc.) would land an
                    // unaccounted RST on the wire. We defer the actual
                    // accounting call until after `drop(converter)` — the
                    // converter holds `&mut self.encoder` here.
                    if freshly_rst {
                        freshly_emitted_rsts.push(rst_error_from_kawa(kawa));
                    }
                }
                // Apply per-frontend response-side header edits
                // (set/replace/delete) stashed by the routing layer at
                // request time. H2 frontends always run as Server
                // position; the back-side H2 client (when sozu speaks
                // H2 to a backend) is a request emission and was
                // already mutated by Router::route_from_request.
                if matches!(self.position, super::Position::Server)
                    && !parts.context.headers_response.is_empty()
                {
                    super::shared::apply_response_header_edits(
                        kawa,
                        &parts.context.headers_response,
                    );
                }
                kawa.prepare(&mut converter);
                // The pre-prepare gate at line 2483 only inserts into
                // `rst_sent` when `kawa.is_error()` is already true on
                // entry. The HPACK over-budget abort path
                // (`H2BlockConverter::check_header_capacity` →
                // `finalize`) flips `parsing_phase` to Error AND pushes
                // its own RST_STREAM frame inside this same prepare
                // pass; without a post-prepare insert here the next
                // writable cycle would gate-pass and double-emit a
                // RST_STREAM via the existing `initialize` chokepoint.
                //
                // Per Codex P2: the converter's direct RST emission
                // bypasses the metric/flood accounting that
                // `Self::reset_stream` performs. Mirror it here so a
                // peer that drives oversized headers across many
                // streams cannot escape the MadeYouReset emitted-RST
                // lifetime cap and so dashboards see the per-error
                // counter and the global tx counter.
                //
                // Per Codex P3: when an incremental stream flips to
                // Error mid-prepare, the RFC 9218 §4 yield-after-one
                // accounting must drop this stream from the
                // same-urgency ready bucket so trailing peers see the
                // live count.
                let freshly_rst_post_prepare = kawa.is_error() && self.rst_sent.insert(stream_id);
                if freshly_rst_post_prepare {
                    // Defer accounting until after `drop(converter)`; same
                    // reason as the pre-prepare collector above.
                    freshly_emitted_rsts.push(rst_error_from_kawa(kawa));
                    if is_incremental {
                        if let Some(c) = ready_incremental_by_urgency.get_mut(&urgency) {
                            *c = c.saturating_sub(1);
                        }
                    }
                }
                let consumed = window - converter.window;
                *parts.window = parts.window.saturating_sub(consumed);
                self.flow_control.window = self.flow_control.window.saturating_sub(consumed);
                if is_incremental && consumed > 0 && first_incremental_fired.is_none() {
                    first_incremental_fired = Some(stream_id);
                }
            }
            context.debug.push(DebugEvent::S(
                stream_id,
                global_stream_id,
                kawa.parsing_phase,
                kawa.blocks.len(),
                kawa.out.len(),
            ));
            let mut stream_bytes: usize = 0;
            let outcome = Self::flush_stream_out(
                &mut self.socket,
                kawa,
                parts.metrics,
                &self.position,
                &mut self.readiness,
                &mut context.debug,
                3,
                global_stream_id,
                Some(&mut socket_write),
                None,
                &mut io_slices,
                Some(&mut stream_bytes),
            );
            // Refresh the per-stream idle timer on outbound bytes. Without
            // this, a long-running response trickled at low bandwidth would
            // be killed by `cancel_timed_out_streams` mid-delivery — the
            // inbound-only refresh at h2.rs:3887-3895 / 4026-4031 never
            // fires while the peer is idle.
            if stream_bytes > 0 {
                if let Some(t) = self.stream_last_activity_at.get_mut(&stream_id) {
                    *t = Instant::now();
                }
            }
            total_bytes_written = total_bytes_written.saturating_add(stream_bytes);
            if outcome == FlushOutcome::Stalled {
                self.expect_write = Some(H2StreamId::Other {
                    id: stream_id,
                    gid: global_stream_id,
                });
                break 'outer;
            }
            self.expect_write = None;
            if (kawa.is_terminated() || kawa.is_error())
                && kawa.is_completed()
                && !Self::handle_1xx_reset(kawa, stream_state, &mut endpoint)
            {
                let close_frontend =
                    matches!(self.position, Position::Server) && !parts.context.keep_alive_frontend;
                let (client_rtt, server_rtt) = Self::snapshot_rtts(
                    &self.position,
                    &self.socket,
                    &endpoint,
                    stream.linked_token(),
                );

                if let Some((dead_id, token)) = Self::try_recycle_server_stream(
                    &self.position,
                    &mut self.bytes,
                    &self.streams,
                    stream,
                    global_stream_id,
                    stream_id,
                    byte_totals,
                    &mut context.debug,
                    context.listener.clone(),
                    client_rtt,
                    server_rtt,
                ) {
                    completed_streams.push((dead_id, global_stream_id, token, close_frontend));
                    // LIFECYCLE §9 invariant 17: decrement INSIDE 'outer so
                    // later iterations see the reduced count. The post-loop
                    // retirement at remove_dead_stream is too late.
                    if is_incremental {
                        if let Some(c) = ready_incremental_by_urgency.get_mut(&urgency) {
                            *c = c.saturating_sub(1);
                        }
                    }
                }
            }
        }
        gauge!(
            "h2.streams.ready_incremental.by_urgency",
            ready_incremental_by_urgency
                .values()
                .copied()
                .sum::<usize>()
        );
        // Reclaim the converter's reusable buffers before any &mut self calls,
        // since the converter borrows self.encoder.
        let converter_out = std::mem::take(&mut converter.out);
        let lowercase_buf = std::mem::take(&mut converter.lowercase_buf);
        let cookie_buf = std::mem::take(&mut converter.cookie_buf);
        // RFC 7541 §6.3: clear our mirror of the pending size-update only
        // AFTER the converter confirmed the signal was emitted to its
        // output buffer. A DATA-only pass leaves `size_update_emitted` as
        // `false` so the signal stays queued for the next pass with a
        // header block.
        let size_update_emitted = converter.size_update_emitted;
        drop(converter);
        if size_update_emitted {
            self.pending_table_size_update = None;
        }
        // Account every RST that the converter emitted during this pass
        // (pre-prepare gate + post-prepare HPACK over-budget abort) so
        // the global tx counter, the per-error breakdown, and the
        // MadeYouReset emitted-RST lifetime cap stay in step. If the
        // cap trips, propagate the GOAWAY result.
        for error in freshly_emitted_rsts {
            if let Some(result) = self.account_emitted_rst(error) {
                return result;
            }
        }
        self.converter_buf = converter_out;
        self.lowercase_buf = lowercase_buf;
        self.cookie_buf = cookie_buf;
        self.shrink_converter_buffers();
        // RFC 9218 §4: commit the round-robin cursor so the next writable
        // cycle begins with the stream immediately after the one we fired
        // first this pass.
        self.prioriser
            .advance_incremental_cursor(first_incremental_fired);
        let mut close_frontend_after_completed_stream = false;
        for (dead_id, global_stream_id, token, close_frontend) in completed_streams {
            // The main write loop borrows self.encoder, so we can't mutate the
            // H2 maps inline. Retire the recycled stream immediately after the
            // converter borrow ends, before endpoint.end_stream() can trigger
            // teardown and observe a stale `Recycle` entry in self.streams.
            self.remove_dead_stream(dead_id, global_stream_id);
            close_frontend_after_completed_stream |= close_frontend;
            if let Some(token) = token {
                remove_backend_stream(&mut context.backend_streams, token, global_stream_id);
                endpoint.end_stream(token, global_stream_id, context);
            }
        }
        if close_frontend_after_completed_stream && !self.drain.draining {
            return if self.streams.is_empty() {
                self.goaway(H2Error::NoError)
            } else {
                self.graceful_goaway()
            };
        }
        self.finalize_write(socket_write, total_bytes_written, context)
    }

    /// Remove streams that completed their lifecycle from all tracking maps.
    /// After forwarding a 1xx informational response (100 Continue, 103 Early Hints),
    /// reset the back buffer and re-enable backend readable so the final response
    /// can arrive on the same stream. Returns true if the response was 1xx.
    #[allow(clippy::too_many_arguments)]
    fn flush_stream_out(
        socket: &mut Front,
        kawa: &mut GenericHttpStream,
        metrics: &mut SessionMetrics,
        position: &Position,
        readiness: &mut Readiness,
        debug: &mut DebugHistory,
        debug_site: usize,
        global_stream_id: GlobalStreamId,
        mut wrote: Option<&mut bool>,
        cross_read_amount: Option<usize>,
        io_slices: &mut Vec<IoSlice<'static>>,
        mut bytes_written: Option<&mut usize>,
    ) -> FlushOutcome {
        while !kawa.out.is_empty() {
            if let Some(flag) = wrote.as_deref_mut() {
                *flag = true;
            }
            io_slices.clear();
            let buffer = kawa.storage.buffer();
            for block in kawa.out.iter() {
                match block {
                    kawa::OutBlock::Delimiter => break,
                    kawa::OutBlock::Store(store) => {
                        let data = store.data(buffer);
                        // SAFETY: the IoSlice references point into kawa's
                        // storage buffer. They are used only for the
                        // socket_write_vectored call below and cleared
                        // immediately after, before kawa.consume() which may
                        // relocate the buffer via ptr::copy (shift). No
                        // dangling 'static refs exist during consume().
                        let data: &'static [u8] =
                            unsafe { std::slice::from_raw_parts(data.as_ptr(), data.len()) };
                        io_slices.push(IoSlice::new(data));
                    }
                }
            }
            let (size, status) = socket.socket_write_vectored(io_slices);
            io_slices.clear();
            debug_assert!(
                io_slices.is_empty(),
                "IoSlice refs must be cleared before consume"
            );
            debug.push(DebugEvent::SocketIO(debug_site, global_stream_id, size));
            kawa.consume(size);
            position.count_bytes_out_counter(size);
            position.count_bytes_out(metrics, size);
            if let Some(counter) = bytes_written.as_deref_mut() {
                *counter = counter.saturating_add(size);
            }
            if let Some(amount) = cross_read_amount {
                // Resume path: same stream is parked waiting for buffer space.
                // Re-enable READABLE once the write freed enough room.
                if kawa.storage.available_space() >= amount {
                    readiness.interest.insert(Ready::READABLE);
                }
            }
            if update_readiness_after_write(size, status, readiness) {
                return FlushOutcome::Stalled;
            }
        }
        FlushOutcome::Drained
    }

    fn handle_1xx_reset<E: Endpoint>(
        kawa: &mut GenericHttpStream,
        stream_state: StreamState,
        endpoint: &mut E,
    ) -> bool {
        let is_1xx = matches!(
            kawa.detached.status_line,
            kawa::StatusLine::Response { code, .. } if (100..200).contains(&code)
        );
        if !is_1xx {
            return false;
        }
        debug!(
            "{} H2 write_streams: 1xx informational forwarded, resetting back buffer",
            log_module_context!()
        );
        kawa.clear();
        if let StreamState::Linked(token) = stream_state {
            let readiness = endpoint.readiness_mut(token);
            readiness.interest.insert(Ready::READABLE);
            readiness.signal_pending_read();
        }
        true
    }

    /// Re-arm edge-triggered WRITABLE event if rustls still has buffered TLS data.
    fn ensure_tls_flushed(&mut self) {
        if self.socket.socket_wants_write() {
            self.readiness.signal_pending_write();
        }
    }

    /// Evict every per-stream piece of state carried by this `ConnectionH2`.
    ///
    /// **Invariant**: `rst_sent`, `stream_last_activity_at` and `prioriser`
    /// MUST be emptied of `stream_id` here — they are the only three
    /// per-stream caches that are not stored in the slab-allocated
    /// `Context.streams[]`. Forgetting any of them causes unbounded memory
    /// growth on long-lived connections with many cancelled streams (Codex
    /// gap G11). The `debug_assert`s below fail loudly in test builds if
    /// someone adds a new per-stream cache without updating this function.
    fn remove_dead_stream(&mut self, stream_id: StreamId, global_stream_id: GlobalStreamId) {
        if self.streams.remove(&stream_id).is_none() {
            error!(
                "{} dead stream_id {} missing from streams map",
                log_context!(self),
                stream_id
            );
        }
        self.rst_sent.remove(&stream_id);
        self.stream_last_activity_at.remove(&stream_id);
        self.prioriser.remove(&stream_id);
        debug_assert!(
            !self.rst_sent.contains(&stream_id),
            "rst_sent still contains stream_id {stream_id} after eviction"
        );
        debug_assert!(
            !self.stream_last_activity_at.contains_key(&stream_id),
            "stream_last_activity_at still contains stream_id {stream_id} after eviction"
        );
        // Invariant: expect_write/expect_read must not reference a gid whose
        // context slot may be popped by shrink_trailing_recycle after eviction.
        if matches!(self.expect_write, Some(H2StreamId::Other { gid, .. }) if gid == global_stream_id)
        {
            self.expect_write = None;
        }
        if matches!(
            self.expect_read,
            Some((H2StreamId::Other { gid, .. }, _)) if gid == global_stream_id
        ) {
            self.expect_read = None;
        }
    }

    /// Drop stream-id mappings for streams that never became active before a
    /// connection-level close. This happens on incomplete/oversized header
    /// blocks: the stream slot is created on the initial HEADERS frame, then a
    /// GOAWAY closes the connection before the request is fully materialized.
    fn prune_inactive_streams_while_closing<L>(&mut self, context: &mut Context<L>)
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        if !self.drain.draining || !matches!(self.state, H2State::GoAway | H2State::Error) {
            return;
        }

        let stale_streams = self
            .streams
            .iter()
            .filter_map(|(&stream_id, &global_stream_id)| {
                (!context.streams[global_stream_id].state.is_open())
                    .then_some((stream_id, global_stream_id))
            })
            .collect::<Vec<_>>();

        for (stream_id, global_stream_id) in stale_streams {
            let stream = &mut context.streams[global_stream_id];
            if stream.state == StreamState::Idle {
                stream.front.clear();
                stream.front.storage.clear();
                stream.back.clear();
                stream.back.storage.clear();
                stream.metrics.reset();
                stream.state = StreamState::Recycle;
            }
            self.remove_dead_stream(stream_id, global_stream_id);
        }
    }

    /// Shrink reusable converter buffers when they grow beyond 16 KB to avoid
    /// holding memory after a burst of large headers.
    fn shrink_converter_buffers(&mut self) {
        if self.converter_buf.capacity() > 16_384 {
            self.converter_buf.shrink_to(4096);
        }
        if self.lowercase_buf.capacity() > 16_384 {
            self.lowercase_buf.shrink_to(4096);
        }
        if self.cookie_buf.capacity() > 16_384 {
            self.cookie_buf.shrink_to(4096);
        }
    }

    /// Post-write phase: check drain completion, flush TLS, and update readiness.
    ///
    /// `bytes_written_this_pass` reports the total outbound bytes `write_streams`
    /// pushed to the socket (across every stream), and is used to distinguish
    /// two very different "no `expect_write`" states:
    ///
    /// - **Voluntary yield with progress**: at least one DATA/HEADERS frame
    ///   emitted, but a stream left non-empty `back.out`/`back.blocks` because
    ///   the converter yielded (e.g. RFC 9218 incremental rotation). LIFECYCLE
    ///   §9 invariant 16: keep `Ready::WRITABLE` armed so the session loop can
    ///   resume flushing on the next tick without waiting for an external
    ///   wake-up that edge-triggered epoll will not deliver.
    /// - **No progress at all**: converter pushed every block back (e.g. flow
    ///   window exhausted, no HEADERS ready yet). Strip `Ready::WRITABLE` —
    ///   forward progress must come from an external trigger
    ///   (`WINDOW_UPDATE`, new request), not from looping writable().
    ///
    /// Returns `MuxResult::Continue` in the normal case, or triggers a graceful
    /// GOAWAY when draining and all streams have completed.
    fn finalize_write<L>(
        &mut self,
        socket_write: bool,
        bytes_written_this_pass: usize,
        context: &mut Context<L>,
    ) -> MuxResult
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        // RFC 9113 §6.8: if draining and all streams have completed,
        // send the final GOAWAY with the actual last_stream_id
        if self.drain.draining && self.streams.is_empty() {
            return self.graceful_goaway();
        }

        if self.socket.socket_wants_write() {
            if !socket_write {
                self.socket.socket_write(&[]);
            }
            // Edge-triggered epoll: re-arm WRITABLE if rustls still has
            // pending encrypted data (first check triggers flush, second re-checks).
            self.ensure_tls_flushed();
        } else if self.expect_write.is_none() {
            // LIFECYCLE §9 invariant 16: retain `Ready::WRITABLE` when a
            // voluntary scheduler yield leaves stranded bytes in a stream's
            // `back.out`/`back.blocks` *after* the pass made forward
            // progress. Requiring progress avoids the degenerate no-progress
            // loop (e.g. flow-control-starved streams) that would otherwise
            // busy-spin against the session dispatcher.
            if bytes_written_this_pass > 0
                && any_stream_has_pending_back(&self.streams, &context.streams)
            {
                #[cfg(debug_assertions)]
                context.debug.push(DebugEvent::Str(
                    "finalize_write: invariant 16 retained WRITABLE (pending back-buffer)"
                        .to_owned(),
                ));
            } else if !self.pending_rst_streams.is_empty()
                || !self.flow_control.pending_window_updates.is_empty()
            {
                // Control-frame liveness: `flush_pending_control_frames` is
                // gated on `expect_write.is_none()`, so when a prior partial
                // write deferred the flush the RST / WINDOW_UPDATE queues
                // stay non-empty after `expect_write` finally drains. Without
                // this rearm the next tick would drop `Ready::WRITABLE` and
                // the queued RST would stall until an unrelated event
                // re-triggered writable — which is exactly the scenario
                // h2spec trips by sending back-to-back malformed streams.
                #[cfg(debug_assertions)]
                context.debug.push(DebugEvent::Str(
                    "finalize_write: retained WRITABLE (control queue non-empty)".to_owned(),
                ));
                self.readiness.arm_writable();
                incr!("h2.signal.writable.rearmed.control_queue");
            } else {
                // We wrote everything
                #[cfg(debug_assertions)]
                context.debug.push(DebugEvent::Str(format!(
                    "Wrote everything: {:?}",
                    self.streams
                )));
                self.readiness.interest.remove(Ready::WRITABLE);
            }
        }
        MuxResult::Continue
    }

    /// Flush pending control frames (zero-buffer resume, WINDOW_UPDATEs, RST_STREAMs)
    /// before entering the main writable state machine.
    ///
    /// Returns `Some(result)` if the caller should return early (e.g. socket would
    /// block, GOAWAY triggered), or `None` if writable() should proceed normally.
    fn flush_pending_control_frames(&mut self) -> Option<MuxResult> {
        if self.frontend_hung_up_while_draining() {
            self.expect_write = None;
            self.zero.storage.clear();
            self.flow_control.pending_window_updates.clear();
            self.pending_rst_streams.clear();
        }

        // RFC 9113 §6.5: check if peer has timed out on SETTINGS ACK
        if let Some(sent_at) = self.settings_sent_at {
            if sent_at.elapsed() >= SETTINGS_ACK_TIMEOUT {
                warn!(
                    "{} SETTINGS ACK timeout: no SETTINGS ACK observed within {:?}",
                    log_context!(self),
                    SETTINGS_ACK_TIMEOUT
                );
                return Some(self.goaway(H2Error::SettingsTimeout));
            }
        }

        // Phase 1: Resume flushing the zero buffer if a previous write was partial.
        // Don't reset the timeout for control frame writes (SETTINGS ACK, PING
        // response, WINDOW_UPDATE). Only application data writes should reset it.
        if let Some(H2StreamId::Zero) = self.expect_write {
            if self.flush_zero_to_socket() {
                self.ensure_tls_flushed();
                return Some(MuxResult::Continue);
            }
            // When H2StreamId::Zero is used to write, READABLE is disabled —
            // re-enable it now that the flush is complete.
            self.readiness.interest.insert(Ready::READABLE);
            self.expect_write = None;
        }

        // Phase 2: Serialize and flush pending WINDOW_UPDATE frames.
        // Write them inline to avoid extra event loop iterations that could
        // cause response data to be sent before validating subsequent frames.
        if !self.flow_control.pending_window_updates.is_empty() && self.expect_write.is_none() {
            let kawa = &mut self.zero;
            kawa.storage.clear();
            let buf = kawa.storage.space();
            let mut offset = 0;
            // Track which entries we successfully serialized so we can remove them.
            // Each WINDOW_UPDATE frame is 13 bytes (9-byte header + 4-byte payload).
            let mut written_ids = Vec::new();
            for (&stream_id, &increment) in &self.flow_control.pending_window_updates {
                if increment == 0 {
                    written_ids.push(stream_id);
                    continue;
                }
                match serializer::gen_window_update(&mut buf[offset..], stream_id, increment) {
                    Ok((_, size)) => {
                        offset += size;
                        written_ids.push(stream_id);
                        incr!("h2.frames.tx.window_update");
                    }
                    Err(_) => {
                        // Buffer full — stop here, remaining entries stay in the map
                        break;
                    }
                }
            }
            // Remove only the entries we successfully wrote (or skipped)
            for id in written_ids {
                self.flow_control.pending_window_updates.remove(&id);
            }
            if offset > 0 {
                kawa.storage.fill(offset);
                if self.flush_zero_to_socket() {
                    self.expect_write = Some(H2StreamId::Zero);
                    // Edge-triggered epoll: ensure pending TLS data gets flushed
                    if self.socket.socket_wants_write() {
                        self.readiness.event.insert(Ready::WRITABLE);
                    }
                    return Some(MuxResult::Continue);
                }
            }
        }

        // Phase 3: Cap check + flush pending RST_STREAM frames.
        // Check lifetime total (not just pending queue length) because writable()
        // drains the queue between readable() calls, so the pending count alone
        // may never reach the cap even under sustained misbehavior.
        if !matches!(self.state, H2State::GoAway | H2State::Error)
            && self.total_rst_streams_queued >= MAX_PENDING_RST_STREAMS
        {
            error!(
                "{} total RST_STREAM count {} exceeds cap {}, sending GOAWAY(ENHANCE_YOUR_CALM)",
                log_context!(self),
                self.total_rst_streams_queued,
                MAX_PENDING_RST_STREAMS
            );
            return Some(self.goaway(H2Error::EnhanceYourCalm));
        }

        // Flush pending RST_STREAM frames (queued when refusing streams).
        // Accounting happens at queue-time inside `Self::enqueue_rst`, so
        // this drain only serialises and flushes — no metric/flood calls
        // here would double-count.
        if !self.pending_rst_streams.is_empty() && self.expect_write.is_none() {
            let kawa = &mut self.zero;
            kawa.storage.clear();
            let buf = kawa.storage.space();
            let mut offset = 0;
            let mut written_count = 0;
            for &(stream_id, ref error) in &self.pending_rst_streams {
                let frame_size =
                    parser::FRAME_HEADER_SIZE + parser::RST_STREAM_PAYLOAD_SIZE as usize;
                if offset + frame_size > buf.len() {
                    break;
                }
                match serializer::gen_rst_stream(&mut buf[offset..], stream_id, error.to_owned()) {
                    Ok((_, _)) => {
                        offset += frame_size;
                        written_count += 1;
                    }
                    Err(_) => break,
                }
            }
            self.pending_rst_streams.drain(..written_count);
            if offset > 0 {
                kawa.storage.fill(offset);
                if self.flush_zero_to_socket() {
                    self.expect_write = Some(H2StreamId::Zero);
                    // Edge-triggered epoll: ensure pending TLS data gets flushed
                    if self.socket.socket_wants_write() {
                        self.readiness.event.insert(Ready::WRITABLE);
                    }
                    return Some(MuxResult::Continue);
                }
            }
        }

        None
    }

    pub fn writable<E, L>(&mut self, context: &mut Context<L>, endpoint: E) -> MuxResult
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        self.prune_inactive_streams_while_closing(context);

        if let Some(result) = self.flush_pending_control_frames() {
            return result;
        }

        // Flush any pending TLS records before state-specific processing.
        // This ensures response DATA frames that were accepted by rustls
        // (via socket_write_vectored in write_streams) are pushed to the
        // TCP socket even when the connection is in GoAway or Error state.
        // Without this, the state-specific handlers may call force_disconnect()
        // before the response data reaches the kernel's TCP send buffer.
        if self.socket.socket_wants_write() {
            self.socket.socket_write(&[]);
        }

        match (&self.state, &self.position) {
            (H2State::Error, Position::Server) => {
                if self.socket.socket_wants_write() {
                    self.ensure_tls_flushed();
                    MuxResult::Continue
                } else {
                    MuxResult::CloseSession
                }
            }
            (H2State::Error, _)
            | (H2State::ClientSettings, Position::Server)
            | (H2State::ServerSettings, Position::Client(..)) => {
                error!(
                    "{} Unexpected combination: (Writable, {:?}, {:?})",
                    log_context!(self),
                    self.state,
                    self.position
                );
                self.force_disconnect()
            }
            (H2State::ClientPreface, Position::Server) => MuxResult::Continue,
            // Discard state: pending data (e.g. RST_STREAM) was already
            // written in the preamble above; let the readable path consume
            // the remaining frame payload.
            (H2State::Discard, _) => MuxResult::Continue,
            (H2State::GoAway, _) => {
                if self.peer_gone_after_final_goaway() {
                    return MuxResult::CloseSession;
                }
                // Flush any remaining TLS response data before disconnecting.
                // The GoAway state only enters after control frames (our GOAWAY
                // response) are flushed above, but response DATA frames may still
                // be in rustls's TLS output buffer — accepted by socket_write_vectored
                // during write_streams() but not yet flushed to TCP. Under TCP
                // backpressure (HAProxy chain), this is the primary truncation vector.
                if self.socket.socket_wants_write() {
                    self.socket.socket_write(&[]);
                    if self.socket.socket_wants_write() {
                        // TLS data still pending (TCP backpressure) — don't disconnect
                        // yet. Re-arm WRITABLE so the event loop retries the flush.
                        self.ensure_tls_flushed();
                        return MuxResult::Continue;
                    }
                }
                self.force_disconnect()
            }
            (H2State::ClientPreface, Position::Client(..)) => {
                trace!("{} Preparing preface and settings", log_context!(self));
                let pri = serializer::H2_PRI.as_bytes();
                let kawa = &mut self.zero;

                kawa.storage.space()[0..pri.len()].copy_from_slice(pri);
                kawa.storage.fill(pri.len());
                match serializer::gen_settings(kawa.storage.space(), &self.local_settings) {
                    Ok((_, size)) => {
                        kawa.storage.fill(size);
                        incr!("h2.frames.tx.settings");
                        // RFC 9113 §6.5: start tracking SETTINGS ACK timeout
                        self.settings_sent_at = Some(Instant::now());
                    }
                    Err(error) => {
                        error!(
                            "{} Could not serialize SettingsFrame: {:?}",
                            log_context!(self),
                            error
                        );
                        return self.force_disconnect();
                    }
                };

                self.state = H2State::ClientSettings;
                self.expect_write = Some(H2StreamId::Zero);
                MuxResult::Continue
            }
            (H2State::ClientSettings, Position::Client(..)) => {
                trace!("{} Sent preface and settings", log_context!(self));
                self.state = H2State::ServerSettings;
                self.expect_read = Some((H2StreamId::Zero, 9));
                self.readiness.interest.remove(Ready::WRITABLE);
                MuxResult::Continue
            }
            (H2State::ServerSettings, Position::Server) => {
                // Enlarge the connection-level receive window beyond the RFC default
                // of 65 535 bytes. The configured window size is too small for
                // high-throughput proxying and causes excessive WINDOW_UPDATE
                // round-trips. Use additive increment rather than unconditional
                // assignment to preserve any window changes that occurred during
                // setup. Skip if the configured window equals the default (no
                // enlargement needed), since a zero-increment WINDOW_UPDATE
                // violates RFC 9113 §6.9.
                let increment = self
                    .connection_config
                    .initial_connection_window
                    .saturating_sub(DEFAULT_INITIAL_WINDOW_SIZE);
                if increment > 0 {
                    self.queue_window_update(0, increment);
                }
                // Do NOT increment flow_control.window here: sending our own
                // WINDOW_UPDATE enlarges the peer's send allowance, not ours.
                // Our send window is only updated by WINDOW_UPDATEs we receive
                // from the peer (RFC 9113 §6.9).
                self.expect_header();
                // Keep WRITABLE so the queued WINDOW_UPDATE gets flushed.
                MuxResult::Continue
            }
            // Proxying states — writing application data (request/response).
            // Reset the timeout here, not at the top of writable(), so that
            // control frame writes (PING, WINDOW_UPDATE) don't reset it.
            (H2State::Header, _)
            | (H2State::Frame(_), _)
            | (H2State::ContinuationFrame(_), _)
            | (H2State::ContinuationHeader(_), _) => self.write_streams(context, endpoint),
        }
    }

    /// Snapshot the access-log RTTs for the local frontend and the linked backend.
    ///
    /// `Position::Server`-only. On a backend H2 connection (`Position::Client`)
    /// the snapshot would write swapped values onto the shared `Stream.metrics`:
    /// the connection's `socket` is the upstream and the corresponding
    /// `EndpointServer::socket` returns the frontend, so the per-stream
    /// `client_rtt`/`server_rtt` cells would be populated with mislabelled
    /// values. Gating keeps backend H2 from poisoning the access-log metric
    /// for the matching frontend stream.
    ///
    /// Callers must invoke this BEFORE `endpoint.end_stream(...)` on reset
    /// paths so the backend lookup does not depend on
    /// `EndpointClient::end_stream` continuing to leave entries in
    /// `Router.backends`.
    ///
    /// Takes individual field references (not `&self`) for the same reason
    /// `try_recycle_server_stream` does — to avoid borrow conflicts with the
    /// `H2BlockConverter` that holds `&mut self.encoder` during the per-stream
    /// write loop.
    fn snapshot_rtts<E: Endpoint>(
        position: &Position,
        socket: &Front,
        endpoint: &E,
        linked_token: Option<mio::Token>,
    ) -> (Option<Duration>, Option<Duration>) {
        if !position.is_server() {
            return (None, None);
        }
        (
            socket_rtt(socket.socket_ref()),
            linked_token
                .and_then(|t| endpoint.socket(t))
                .and_then(socket_rtt),
        )
    }

    /// Try to recycle a completed server-side stream by distributing overhead,
    /// generating access logs, and transitioning the stream to `Recycle` state.
    ///
    /// Returns `Some((stream_id, Option<token>))` if the stream was recycled, so the
    /// caller can add `stream_id` to the dead-streams list and call `endpoint.end_stream()`
    /// if a token was returned. Returns `None` if recycling was deferred or not applicable.
    ///
    /// Takes individual field references instead of `&mut self` to avoid borrow
    /// conflicts when the H2 block converter holds `&mut self.encoder`.
    /// `client_rtt`/`server_rtt` are snapshotted by the caller (which still
    /// owns `&self.socket` and `&endpoint`) and forwarded into the access log.
    #[allow(clippy::too_many_arguments)]
    fn try_recycle_server_stream<L>(
        position: &Position,
        bytes: &mut H2ByteAccounting,
        streams: &HashMap<StreamId, GlobalStreamId>,
        stream: &mut crate::protocol::mux::Stream,
        global_stream_id: GlobalStreamId,
        stream_id: StreamId,
        byte_totals: (usize, usize),
        debug: &mut DebugHistory,
        listener: std::rc::Rc<std::cell::RefCell<L>>,
        client_rtt: Option<Duration>,
        server_rtt: Option<Duration>,
    ) -> Option<(StreamId, Option<mio::Token>)>
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        match position {
            Position::Client(..) => None,
            Position::Server => {
                // Already logged by a reset path; retire the stream after its RST is flushed.
                if stream.metrics.start.is_none() {
                    let state = std::mem::replace(&mut stream.state, StreamState::Recycle);
                    return match state {
                        StreamState::Linked(token) => Some((stream_id, Some(token))),
                        _ => Some((stream_id, None)),
                    };
                }

                // Don't recycle if the client hasn't sent END_STREAM yet —
                // more DATA frames may arrive for this stream.
                if !stream.front_received_end_of_stream {
                    trace!(
                        "{} Defer recycle stream {}: client still sending",
                        log_module_context!(),
                        global_stream_id
                    );
                    return None;
                }
                let stream_bytes = (
                    stream.metrics.bin + stream.metrics.backend_bin,
                    stream.metrics.bout + stream.metrics.backend_bout,
                );
                distribute_overhead(
                    &mut stream.metrics,
                    &mut bytes.overhead_bin,
                    &mut bytes.overhead_bout,
                    stream_bytes,
                    byte_totals,
                    streams.len(),
                    streams.len() == 1,
                );
                debug.push(DebugEvent::StreamEvent(4, global_stream_id));
                trace!(
                    "{} Recycle stream: {}",
                    log_module_context!(),
                    global_stream_id
                );
                let token = Self::complete_server_stream(stream, listener, client_rtt, server_rtt);
                Some((stream_id, token))
            }
        }
    }

    /// Finalize a server-side stream after its response has been fully written.
    ///
    /// Generates an access log, resets metrics, and transitions the stream to `Recycle`.
    /// Returns the backend token if the stream was `Linked`, so the caller can call
    /// `endpoint.end_stream()` with the full `Context` (which can't be passed here
    /// because `stream` borrows from `context.streams`).
    ///
    /// Callers must distribute overhead *before* calling this, since the converter
    /// borrow may prevent `distribute_overhead()`.
    fn complete_server_stream<L>(
        stream: &mut crate::protocol::mux::Stream,
        listener: std::rc::Rc<std::cell::RefCell<L>>,
        client_rtt: Option<Duration>,
        server_rtt: Option<Duration>,
    ) -> Option<mio::Token>
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        incr!("http.e2e.h2");
        stream.metrics.backend_stop();
        stream.generate_access_log(
            false,
            Some("H2::Complete"),
            listener,
            client_rtt,
            server_rtt,
        );
        stream.metrics.reset();
        let state = std::mem::replace(&mut stream.state, StreamState::Recycle);
        if let StreamState::Linked(token) = state {
            Some(token)
        } else {
            None
        }
    }

    /// Compute the total bytes transferred across all active streams.
    ///
    /// Returns `(total_bytes_in, total_bytes_out)` where bytes_in = `bin + backend_bin`
    /// and bytes_out = `bout + backend_bout` for each stream.
    fn compute_stream_byte_totals<L: ListenerHandler + L7ListenerHandler>(
        &self,
        context: &Context<L>,
    ) -> (usize, usize) {
        let mut total_in = 0usize;
        let mut total_out = 0usize;
        for &gid in self.streams.values() {
            let m = &context.streams[gid].metrics;
            total_in += m.bin + m.backend_bin;
            total_out += m.bout + m.backend_bout;
        }
        (total_in, total_out)
    }

    /// Distribute connection-level byte overhead proportionally to a single stream.
    ///
    /// `totals` should be pre-computed via [`compute_stream_byte_totals`] **before**
    /// taking a mutable borrow on the target stream, to avoid borrow conflicts.
    /// Delegates to the free function [`distribute_overhead`].
    fn distribute_overhead(&mut self, metrics: &mut SessionMetrics, totals: (usize, usize)) {
        let stream_bytes = (
            metrics.bin + metrics.backend_bin,
            metrics.bout + metrics.backend_bout,
        );
        distribute_overhead(
            metrics,
            &mut self.bytes.overhead_bin,
            &mut self.bytes.overhead_bout,
            stream_bytes,
            totals,
            self.streams.len(),
            self.streams.len() <= 1,
        );
    }

    /// Attribute accumulated `zero_bytes_read` to the stream or to connection overhead.
    fn attribute_bytes_to_stream(&mut self, metrics: &mut SessionMetrics) {
        self.position
            .count_bytes_in(metrics, self.bytes.zero_bytes_read);
        self.bytes.zero_bytes_read = 0;
    }

    fn attribute_bytes_to_overhead(&mut self) {
        self.bytes.overhead_bin += self.bytes.zero_bytes_read;
        self.bytes.zero_bytes_read = 0;
    }

    /// Queue a WINDOW_UPDATE, coalescing with any existing entry for the same stream_id.
    /// RFC 9113 §6.9.1: window size increment MUST be 1..2^31-1 (0x7FFFFFFF).
    ///
    /// Always signals pending write so callers don't have to remember the
    /// edge-triggered epoll invariant (see memory feedback_epollet_signal_pending_write):
    /// under ET epoll a queued WINDOW_UPDATE without a live WRITABLE event bit
    /// is invisible to filter_interest() and will never get flushed.
    fn queue_window_update(&mut self, stream_id: u32, increment: u32) {
        let max_increment = i32::MAX as u32;
        if let Some(existing) = self.flow_control.pending_window_updates.get_mut(&stream_id) {
            let old = *existing;
            *existing = existing.saturating_add(increment).min(max_increment);
            trace!(
                "{} WINDOW_UPDATE coalesced: stream={} old={} new={}",
                log_context!(self),
                stream_id,
                old,
                *existing
            );
        } else if self.flow_control.pending_window_updates.len() < self.max_pending_window_updates {
            self.flow_control
                .pending_window_updates
                .insert(stream_id, increment.min(max_increment));
            trace!(
                "{} WINDOW_UPDATE queued: stream={} increment={}",
                log_context!(self),
                stream_id,
                increment.min(max_increment)
            );
        } else {
            error!(
                "{} WINDOW_UPDATE dropped: queue full ({} entries), stream={} increment={}",
                log_context!(self),
                self.max_pending_window_updates,
                stream_id,
                increment
            );
            incr!("h2.window_update_dropped");
        }
        self.readiness.arm_writable();
    }

    /// Re-enable READABLE if this connection is parked waiting for buffer space
    /// and the target stream's buffer now has enough room.
    ///
    /// This is the cross-readiness counterpart to the same-connection check in
    /// `writable()`. When the *other side* of a stream (frontend or backend)
    /// drains data via its own `writable()`, it frees buffer space that this
    /// connection was waiting for. Without this explicit wake-up the connection
    /// stays parked and the session deadlocks until a timeout fires.
    ///
    /// Returns `true` if READABLE was re-enabled.
    pub fn try_resume_reading<L>(&mut self, context: &Context<L>) -> bool
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        if let Some((
            H2StreamId::Other {
                gid: global_stream_id,
                ..
            },
            amount,
        )) = self.expect_read
        {
            let stream = &context.streams[global_stream_id];
            let kawa = match self.position {
                Position::Client(..) => &stream.back,
                Position::Server => &stream.front,
            };
            if kawa.storage.available_space() >= amount {
                self.readiness.interest.insert(Ready::READABLE);
                return true;
            }
        }
        false
    }

    /// Mark a stream's position-appropriate end-of-stream flag.
    ///
    /// Server reads from the front (client), so sets `front_received_end_of_stream`.
    /// Client reads from the back (backend), so sets `back_received_end_of_stream`.
    fn mark_end_of_stream(&self, stream: &mut crate::protocol::mux::Stream) {
        if self.position.is_server() {
            stream.front_received_end_of_stream = true;
        } else {
            stream.back_received_end_of_stream = true;
        }
    }

    /// Cancel streams that have been idle longer than [`Self::stream_idle_timeout`].
    ///
    /// A stream is considered idle when no meaningful application data (non-empty
    /// DATA frames or HEADERS) has been received since the last activity timestamp
    /// in [`Self::stream_last_activity_at`].
    ///
    /// Mitigates slow-multiplex Slowloris (Pass 4 Medium #3): the connection-level
    /// idle timer resets on every frame, so a peer sending periodic control frames
    /// can pin `max_concurrent_streams` slots for the full nominal connection timeout.
    /// Per-stream idle deadlines guarantee each stream terminates if it stops making
    /// forward progress, regardless of connection-level liveness.
    ///
    /// Timed-out streams receive RST_STREAM(CANCEL) and are immediately removed
    /// from the streams map so they no longer count against MAX_CONCURRENT_STREAMS.
    /// Backend endpoints are notified and metrics are finalized.
    pub fn cancel_timed_out_streams<E, L>(&mut self, context: &mut Context<L>, endpoint: &mut E)
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        // Per-connection scratch Vecs (`converter_buf`, `lowercase_buf`,
        // `cookie_buf`, `priorities_buf`) grow to a
        // high-water mark and never shrink. On a long-lived idle H2
        // connection that briefly carried a flurry of large headers, the
        // backing memory stays pinned indefinitely. Reclaim past
        // `SCRATCH_BUF_RETAIN` when the connection has live streams but
        // each scratch buffer holds 4× the cap. Quiet-time only — runs
        // at the top of every `cancel_timed_out_streams` invocation
        // (which is itself called from the readable hot loop, but only
        // on a session that has been idle long enough to risk timing
        // out a stream).
        const SCRATCH_BUF_RETAIN: usize = 16 * 1024;
        if self.converter_buf.capacity() > SCRATCH_BUF_RETAIN * 4 {
            self.converter_buf.shrink_to(SCRATCH_BUF_RETAIN);
        }
        if self.lowercase_buf.capacity() > SCRATCH_BUF_RETAIN * 4 {
            self.lowercase_buf.shrink_to(SCRATCH_BUF_RETAIN);
        }
        if self.cookie_buf.capacity() > SCRATCH_BUF_RETAIN * 4 {
            self.cookie_buf.shrink_to(SCRATCH_BUF_RETAIN);
        }
        if self.priorities_buf.capacity() > SCRATCH_BUF_RETAIN * 4 {
            self.priorities_buf.shrink_to(SCRATCH_BUF_RETAIN);
        }

        if self.streams.is_empty() || self.stream_last_activity_at.is_empty() {
            return;
        }
        let now = Instant::now();
        let deadline = self.stream_idle_timeout;
        let timed_out: Vec<StreamId> = self
            .stream_last_activity_at
            .iter()
            .filter_map(|(&sid, &t)| {
                (self.streams.contains_key(&sid)
                    && !self.rst_sent.contains(&sid)
                    && now.saturating_duration_since(t) > deadline)
                    .then_some(sid)
            })
            .collect();
        if timed_out.is_empty() {
            return;
        }
        for sid in timed_out {
            info!(
                "{} H2 stream {} idle > {:?}, cancelling (slow-multiplex guard)",
                log_context!(self),
                sid,
                deadline
            );
            // Route through the canonical chokepoint so dedupe (rst_sent),
            // queued-cap accounting (MAX_PENDING_RST_STREAMS via
            // total_rst_streams_queued), and edge-triggered-epoll arming
            // (Readiness::arm_writable) all stay consistent — see LIFECYCLE
            // §8.2. The previous direct push bypassed all three: a peer
            // that opens 200 streams and lets them all idle past
            // stream_idle_timeout could push past the queued cap silently
            // (no GOAWAY(ENHANCE_YOUR_CALM) escalation), a double-cancel
            // pass would grow pending_rst_streams instead of short-
            // circuiting on the existing rst_sent membership, and the
            // hand-rolled `interest.insert(WRITABLE) + signal_pending_write`
            // pair below skipped invariant 15. Counting these RSTs against
            // the cap is a deliberate behaviour change: 200 cumulative idle
            // cancellations from one peer IS abusive (pinning
            // MAX_CONCURRENT_STREAMS slots), and the GOAWAY(ENHANCE_YOUR_CALM)
            // escalation tells the peer to reconnect with a clean state.
            //
            // We deliberately ignore the `Option<MuxResult>` flood-violation
            // signal here — `cancel_timed_out_streams` returns `()` and is
            // called as best-effort housekeeping during the read path. A
            // flood violation that becomes visible mid-iteration will be
            // re-detected on the next `record_rst_emitted` call (the
            // counter is sticky), so dropping the early-return is safe.
            let _ = self.enqueue_rst(sid, H2Error::Cancel);

            // Remove from streams map and recycle the context stream so the slot
            // no longer counts against MAX_CONCURRENT_STREAMS.
            // Compute totals per-stream before remove (matches RST_STREAM handler).
            let byte_totals = self.compute_stream_byte_totals(context);
            if let Some(global_stream_id) = self.streams.get(&sid).copied() {
                {
                    let stream = &mut context.streams[global_stream_id];
                    self.attribute_bytes_to_stream(&mut stream.metrics);
                }
                // Check if stream is linked to a backend — borrow must be scoped
                // so end_stream can take &mut context.
                let linked_token = context.streams[global_stream_id].linked_token();
                let (client_rtt, server_rtt) =
                    Self::snapshot_rtts(&self.position, &self.socket, &*endpoint, linked_token);
                if let Some(token) = linked_token {
                    endpoint.end_stream(token, global_stream_id, context);
                }
                let stream = &mut context.streams[global_stream_id];
                match &self.position {
                    Position::Client(_, backend, BackendStatus::Connected) => {
                        let mut backend_borrow = backend.borrow_mut();
                        backend_borrow.active_requests =
                            backend_borrow.active_requests.saturating_sub(1);
                    }
                    Position::Client(..) => {}
                    Position::Server => {
                        self.distribute_overhead(&mut stream.metrics, byte_totals);
                        stream.metrics.backend_stop();
                        stream.generate_access_log(
                            true,
                            Some("H2::IdleTimeout"),
                            context.listener.clone(),
                            client_rtt,
                            server_rtt,
                        );
                        stream.state = StreamState::Recycle;
                    }
                }
                // Retire sid from streams/prioriser/stream_last_activity_at and
                // invalidate expect_write/expect_read if they reference this gid.
                self.remove_dead_stream(sid, global_stream_id);
            }
        }
        // Writable arming is already done by enqueue_rst -> arm_writable in
        // the loop above; the trailing pair was redundant after the chokepoint
        // routing landed.
    }

    /// Queue a `RST_STREAM` frame for serialisation by
    /// [`Self::flush_pending_control_frames`] on the next writable tick.
    ///
    /// This is the canonical entry point for proxy-emitted stream resets:
    /// `DATA` on a closed stream, `MAX_CONCURRENT_STREAMS` refusal, and the
    /// per-stream error paths in [`Self::reset_stream`] all funnel through
    /// here. Serialisation is independent of the owning `Stream` still
    /// existing in `self.streams`, which is what lets us emit even after a
    /// caller has already called [`Self::remove_dead_stream`].
    ///
    /// Delegates the primitive work to [`enqueue_rst_into`] so the invariants
    /// are covered by unit tests that don't need a full `ConnectionH2`
    /// fixture. See that function's doc-comment for the three invariants
    /// (dedupe via `rst_sent`, MadeYouReset queued cap via
    /// `total_rst_streams_queued`, edge-triggered-epoll arm via
    /// [`Readiness::arm_writable`]).
    fn enqueue_rst(&mut self, wire_stream_id: StreamId, error: H2Error) -> Option<MuxResult> {
        let freshly_queued = enqueue_rst_into(
            &mut self.pending_rst_streams,
            &mut self.total_rst_streams_queued,
            &mut self.rst_sent,
            &mut self.readiness,
            wire_stream_id,
            error,
        );
        // Account ONLY when a new RST actually entered the queue.
        // Calling `enqueue_rst` for a stream that already has a queued
        // (or already-flushed) RST is the dedup short-circuit — counting
        // those would inflate `h2.frames.tx.rst_stream` /
        // `h2.rst_stream.sent.*` and trip the CVE-2025-8671 MadeYouReset
        // lifetime cap on frames that never reached the wire.
        //
        // Account at queue-time, not at drain-time. Doing it later in
        // `flush_pending_control_frames` would double-count any RST that
        // a re-entrant call (DATA on a closed stream we already RSTed)
        // tried to enqueue — and missing it at queue-time leaves
        // `cancel_timed_out_streams` / `refuse_stream_and_discard` /
        // DATA-on-closed-stream paths bypassing the lifetime cap
        // (security review LISA-001 on commit `da845c71`).
        if freshly_queued {
            self.account_emitted_rst(error)
        } else {
            None
        }
    }

    /// Single accounting site for proxy-emitted RST_STREAM frames.
    /// Three things must happen for every emitted RST so flood-protection
    /// stays honest: the global tx counter, the per-error breakdown,
    /// and the MadeYouReset emitted-RST lifetime cap.
    ///
    /// Two distinct emission paths feed this helper:
    ///   * Queued frames — [`Self::enqueue_rst`] (and therefore every
    ///     callable that funnels through it: `reset_stream`,
    ///     `refuse_stream_and_discard`, `cancel_timed_out_streams`,
    ///     DATA-on-closed-stream) calls this once at queue-time. The
    ///     drain in `flush_pending_control_frames` does NOT call it
    ///     again — that would double-count.
    ///   * Converter-emitted frames — the converter's `initialize`
    ///     chokepoint (and the HPACK over-budget abort path) writes
    ///     RST_STREAM frames straight into `kawa.out` from inside
    ///     `kawa.prepare`. We collect those `H2Error` codes during the
    ///     `write_streams` loop and call this helper for each one
    ///     after `drop(converter)` (because the converter holds
    ///     `&mut self.encoder`).
    ///
    /// Returning `Some(MuxResult)` means the caller MUST short-circuit
    /// with that result — the flood detector tripped its lifetime cap
    /// and converted to a connection-wide GOAWAY.
    fn account_emitted_rst(&mut self, error: H2Error) -> Option<MuxResult> {
        incr!("h2.frames.tx.rst_stream");
        count!(metric_for_rst_stream_sent(error), 1);
        if !matches!(error, H2Error::NoError) {
            if let Some(violation) = self.flood_detector.record_rst_emitted() {
                return Some(self.handle_flood_violation(violation));
            }
        }
        None
    }

    /// Refuse a newly-opened stream with RST_STREAM and discard its HEADERS payload.
    ///
    /// Used when MAX_CONCURRENT_STREAMS is exceeded or buffer pool is exhausted.
    /// Queues the RST_STREAM for the writable path (can't write to kawa.storage
    /// here because it is needed to discard the HEADERS payload).
    ///
    /// Also applies SETTINGS back-pressure per RFC 9113 §5.1.2: if refusals
    /// burst past [`BACKPRESSURE_REFUSAL_THRESHOLD`] within
    /// [`BACKPRESSURE_WINDOW_DURATION`], the advertised
    /// `SETTINGS_MAX_CONCURRENT_STREAMS` is halved via
    /// [`Self::apply_mcs_backpressure`].
    fn refuse_stream_and_discard(
        &mut self,
        stream_id: StreamId,
        error: H2Error,
        payload_len: u32,
    ) -> MuxResult {
        if let Some(result) = self.enqueue_rst(stream_id, error) {
            return result;
        }
        self.state = H2State::Discard;
        self.expect_read = Some((H2StreamId::Zero, payload_len as usize));
        self.record_refusal_for_backpressure();
        MuxResult::Continue
    }

    /// RFC 9113 §5.1.2 SETTINGS back-pressure bookkeeping.
    ///
    /// Increments the refusal counter for the current back-pressure window
    /// and, when the burst threshold is crossed, halves the advertised
    /// `SETTINGS_MAX_CONCURRENT_STREAMS`. Further halving attempts in the
    /// same connection are suppressed by [`Self::mcs_backpressure_applied`]
    /// so sustained abuse does not collapse the cap to zero — callers can
    /// still promote the situation to `EnhanceYourCalm` via the flood
    /// detector.
    fn record_refusal_for_backpressure(&mut self) {
        if self.refuse_window_start.elapsed() >= BACKPRESSURE_WINDOW_DURATION {
            self.refuse_count_window = 0;
            self.refuse_window_start = Instant::now();
        }
        self.refuse_count_window = self.refuse_count_window.saturating_add(1);
        if !self.mcs_backpressure_applied
            && self.refuse_count_window >= BACKPRESSURE_REFUSAL_THRESHOLD
        {
            self.apply_mcs_backpressure();
        }
    }

    /// Halve the advertised `SETTINGS_MAX_CONCURRENT_STREAMS` and mark the
    /// back-pressure state as applied. The new value takes effect locally
    /// immediately — subsequent stream-open checks in `handle_header_state`
    /// compare `self.streams.len()` against this reduced cap, so the peer
    /// starts receiving `REFUSED_STREAM` earlier. A full SETTINGS re-send on
    /// the wire is deferred until we have a mid-connection SETTINGS queue
    /// (the existing path in `handle_preface_state` only fires during the
    /// handshake); this is noted in the task log as a minimal first step.
    fn apply_mcs_backpressure(&mut self) {
        let previous = self.local_settings.settings_max_concurrent_streams;
        let reduced = (previous / 2).max(1);
        warn!(
            "{} H2 SETTINGS back-pressure: refusals={} in {}s — halving \
             SETTINGS_MAX_CONCURRENT_STREAMS {} -> {}",
            log_context!(self),
            self.refuse_count_window,
            BACKPRESSURE_WINDOW_DURATION.as_secs(),
            previous,
            reduced,
        );
        self.local_settings.settings_max_concurrent_streams = reduced;
        self.mcs_backpressure_applied = true;
    }

    /// Log a flood violation with full session context and emit the GOAWAY.
    ///
    /// Centralises the "flood detected" reporting so every site that observes a
    /// [`H2FloodViolation`] gets the same session-scoped log line, matching the
    /// RUSTLS log-context convention. Also emits the per-kind statsd counter
    /// (`h2.flood.violation.<kind>`) so SOC dashboards can window the trip
    /// rate without parsing logs — every CVE-mitigation in the H2 family
    /// (Rapid Reset, MadeYouReset, CONTINUATION/PING/SETTINGS floods, header
    /// overflow, glitch) funnels through this site.
    pub fn handle_flood_violation(&mut self, violation: H2FloodViolation) -> MuxResult {
        count!(violation.metric_key, 1);
        warn!(
            "{} H2 flood detected: {} count {} exceeds threshold {}",
            log_context!(self),
            violation.reason,
            violation.count,
            violation.threshold,
        );
        self.goaway(violation.error)
    }
}

/// Recover the [`H2Error`] code that the converter's `initialize`
/// chokepoint will encode into the synthesised RST_STREAM frame for a
/// kawa stuck in [`kawa::ParsingPhase::Error`]. Mirrors the parse +
/// fallback at `lib/src/protocol/mux/converter.rs::initialize` so the
/// flood-accounting helper sees the same code that lands on the wire.
fn rst_error_from_kawa<T: kawa::AsBuffer>(kawa: &kawa::Kawa<T>) -> H2Error {
    match kawa.parsing_phase {
        kawa::ParsingPhase::Error {
            kind: kawa::ParsingErrorKind::Processing { message },
            ..
        } => message.parse::<H2Error>().unwrap_or(H2Error::InternalError),
        _ => H2Error::InternalError,
    }
}

/// Compile-time mapping from `(prefix, H2Error)` to a static metric key.
///
/// Materialises a `&'static str` literal via `concat!`, so the metric key
/// never crosses through a heap allocation and the statsd drain can store it
/// as `&'static str`. Adding a new `H2Error` variant fails the build here —
/// the metric breakdown stays in lock-step with RFC 9113 §7 codes.
///
/// Used for the per-error-code counters emitted around GOAWAY and RST_STREAM
/// in either direction (see `metric_for_goaway_sent` etc. below).
macro_rules! h2_error_metric_key {
    ($prefix:literal, $error:expr) => {
        match $error {
            H2Error::NoError => concat!($prefix, ".no_error"),
            H2Error::ProtocolError => concat!($prefix, ".protocol_error"),
            H2Error::InternalError => concat!($prefix, ".internal_error"),
            H2Error::FlowControlError => concat!($prefix, ".flow_control_error"),
            H2Error::SettingsTimeout => concat!($prefix, ".settings_timeout"),
            H2Error::StreamClosed => concat!($prefix, ".stream_closed"),
            H2Error::FrameSizeError => concat!($prefix, ".frame_size_error"),
            H2Error::RefusedStream => concat!($prefix, ".refused_stream"),
            H2Error::Cancel => concat!($prefix, ".cancel"),
            H2Error::CompressionError => concat!($prefix, ".compression_error"),
            H2Error::ConnectError => concat!($prefix, ".connect_error"),
            H2Error::EnhanceYourCalm => concat!($prefix, ".enhance_your_calm"),
            H2Error::InadequateSecurity => concat!($prefix, ".inadequate_security"),
            H2Error::HTTP11Required => concat!($prefix, ".http_1_1_required"),
        }
    };
}

/// Static metric key for an outbound GOAWAY. Same call shape as the other three
/// helpers below — keeps the call sites uniform.
fn metric_for_goaway_sent(error: H2Error) -> &'static str {
    h2_error_metric_key!("h2.goaway.sent", error)
}

/// Static metric key for an inbound GOAWAY by raw wire error code. Codes
/// outside RFC 9113 §7 fall into the dedicated `…unknown_error` bucket so the
/// breakdown stays bounded and operators can still spot non-standard peers.
fn metric_for_goaway_received(error_code: u32) -> &'static str {
    H2Error::try_from(error_code)
        .map(|e| h2_error_metric_key!("h2.goaway.received", e))
        .unwrap_or("h2.goaway.received.unknown_error")
}

/// Static metric key for an outbound RST_STREAM. Mirrors
/// [`metric_for_goaway_sent`] under a separate namespace so RST and GOAWAY
/// rates can be alerted on independently.
fn metric_for_rst_stream_sent(error: H2Error) -> &'static str {
    h2_error_metric_key!("h2.rst_stream.sent", error)
}

/// Static metric key for an inbound RST_STREAM by raw wire error code. Same
/// `…unknown_error` fallback as [`metric_for_goaway_received`].
fn metric_for_rst_stream_received(error_code: u32) -> &'static str {
    H2Error::try_from(error_code)
        .map(|e| h2_error_metric_key!("h2.rst_stream.received", e))
        .unwrap_or("h2.rst_stream.received.unknown_error")
}

/// Static metric key for an inbound H2 frame by RFC 9113 §6 frame type.
/// Emitted at the `handle_frame` dispatch — single chokepoint that any
/// new H2 frame type must traverse, so adding a `Frame::*` variant fails
/// the build here. Counts are per-frame, not per-byte; pair with
/// `bytes_in` for traffic-mix dashboards.
fn h2_frame_rx_metric_key(frame: &Frame) -> &'static str {
    match frame {
        Frame::Data(_) => "h2.frames.rx.data",
        Frame::Headers(_) => "h2.frames.rx.headers",
        Frame::PushPromise(_) => "h2.frames.rx.push_promise",
        Frame::Priority(_) => "h2.frames.rx.priority",
        Frame::RstStream(_) => "h2.frames.rx.rst_stream",
        Frame::Settings(_) => "h2.frames.rx.settings",
        Frame::Ping(_) => "h2.frames.rx.ping",
        Frame::GoAway(_) => "h2.frames.rx.goaway",
        Frame::WindowUpdate(_) => "h2.frames.rx.window_update",
        Frame::Continuation(_) => "h2.frames.rx.continuation",
        Frame::PriorityUpdate(_) => "h2.frames.rx.priority_update",
        Frame::Unknown(_) => "h2.frames.rx.unknown",
    }
}

impl<Front: SocketHandler> ConnectionH2<Front> {
    pub fn goaway(&mut self, error: H2Error) -> MuxResult {
        self.state = H2State::Error;
        self.drain.draining = true;
        self.expect_read = None;
        // Disarm the SETTINGS ACK timer: once we've committed to GOAWAY, the
        // timeout check at `readable()` / `flush_pending_control_frames()` must
        // not re-fire. Without this, `signal_pending_write()` below re-enters
        // `writable()` → `flush_pending_control_frames()` on the next tick,
        // the elapsed check is still true, and we emit another
        // `warn!` + `goaway()` pair, each bumping `h2.goaway.sent.*`.
        self.settings_sent_at = None;
        let kawa = &mut self.zero;
        kawa.storage.clear();
        // Severity tiering: only `InternalError` implies a sozu-side bug when
        // WE emit it. Every other non-`NoError` reason is "peer misbehaved,
        // sozu defended correctly" — operators don't need paging on abusive
        // or buggy peers. Caller sites already log the specific antecedent
        // (flood detected, parser failure, SETTINGS timeout, invalid window)
        // before reaching `goaway()`, so demoting this summary line avoids
        // duplicate noise without hiding the root cause.
        match error {
            H2Error::NoError => debug!("{} GOAWAY: {:?}", log_context!(self), error),
            H2Error::InternalError => error!("{} GOAWAY: {:?}", log_context!(self), error),
            _ => warn!("{} GOAWAY: {:?}", log_context!(self), error),
        }
        count!(metric_for_goaway_sent(error), 1);

        // RFC 9113 §6.8: last_stream_id is the highest peer-initiated stream we processed
        match serializer::gen_goaway(kawa.storage.space(), self.highest_peer_stream_id, error) {
            Ok((_, size)) => {
                kawa.storage.fill(size);
                incr!("h2.frames.tx.goaway");
                self.state = H2State::GoAway;
                self.expect_write = Some(H2StreamId::Zero);
                self.readiness.interest = Ready::WRITABLE | Ready::HUP | Ready::ERROR;
                self.readiness.signal_pending_write();
                MuxResult::Continue
            }
            Err(error) => {
                error!(
                    "{} Could not serialize GoAwayFrame: {:?}",
                    log_context!(self),
                    error
                );
                self.force_disconnect()
            }
        }
    }

    /// RFC 9113 §6.8: Initiate graceful shutdown using the double-GOAWAY pattern.
    ///
    /// First call sends GOAWAY with `last_stream_id = 0x7FFFFFFF` (MAX) to signal
    /// the intent to stop accepting new streams while allowing in-flight streams
    /// to complete. The connection enters draining mode.
    ///
    /// When `draining` is already true (second invocation), sends the final GOAWAY
    /// with the actual `highest_peer_stream_id` so the peer knows which streams
    /// were processed.
    pub fn graceful_goaway(&mut self) -> MuxResult {
        if self.drain.draining {
            // Second GOAWAY: send with the real last_stream_id
            return self.goaway(H2Error::NoError);
        }

        // First GOAWAY: advertise MAX stream ID so the peer knows we are draining
        // but does not yet know the cutoff. This gives in-flight requests a chance
        // to arrive before we commit to a final last_stream_id.
        self.drain.draining = true;
        // Arm the forced-close timer from the moment the proxy decides to drain.
        // `Mux::shutting_down` samples it against `graceful_shutdown_deadline`
        // and returns `true` once the budget is exhausted so the session loop
        // tears the connection down instead of waiting forever.
        self.drain.started_at = Some(Instant::now());
        // Keep expect_read as-is: existing streams should continue reading
        // data during phase 1. Only phase 2 (goaway()) removes READABLE.
        let kawa = &mut self.zero;
        kawa.storage.clear();
        debug!(
            "{} GOAWAY (graceful, phase 1): last_stream_id=0x7FFFFFFF",
            log_context!(self)
        );
        // Phase 1 sends a NO_ERROR GOAWAY on the wire — count it under the
        // same per-code key as phase 2. The downstream alert that wants to
        // distinguish drain from termination compares against the
        // `h2.goaway.sent.no_error` rate (drain) vs the other variants
        // (termination on error).
        count!(metric_for_goaway_sent(H2Error::NoError), 1);

        match serializer::gen_goaway(kawa.storage.space(), STREAM_ID_MAX, H2Error::NoError) {
            Ok((_, size)) => {
                kawa.storage.fill(size);
                incr!("h2.frames.tx.goaway");
                // Stay in the current state so the connection can continue processing
                // existing streams. The second GOAWAY will transition to GoAway state.
                // Keep READABLE so in-flight request bodies can still be received
                // during phase 1. Only remove READABLE in the second GOAWAY (goaway()).
                self.expect_write = Some(H2StreamId::Zero);
                self.readiness.arm_writable();
                MuxResult::Continue
            }
            Err(error) => {
                error!(
                    "{} Could not serialize graceful GoAwayFrame: {:?}",
                    log_context!(self),
                    error
                );
                self.force_disconnect()
            }
        }
    }

    /// Returns `true` when the graceful-shutdown budget armed by
    /// [`Self::graceful_goaway`] has elapsed. A return of `true` signals
    /// the enclosing session loop that the proxy-initiated drain must
    /// transition to a forced close: remaining streams will not complete
    /// in time and keeping the connection open past the deadline defeats
    /// the soft-stop SLA.
    ///
    /// Returns `false` when:
    /// - drain has not started yet (`started_at` is `None`),
    /// - the knob is `0` / `None` (indefinite wait explicitly opted in),
    /// - or the elapsed time is still within the configured budget.
    pub fn graceful_shutdown_deadline_elapsed(&self) -> bool {
        match (self.drain.started_at, self.drain.graceful_shutdown_deadline) {
            (Some(started_at), Some(deadline)) => started_at.elapsed() >= deadline,
            _ => false,
        }
    }

    /// Returns `true` if there is data queued waiting to be flushed:
    /// - H2 control frames in the zero buffer (GOAWAY, SETTINGS ACK, etc.)
    /// - A partially-written stream or control frame (`expect_write`)
    /// - Encrypted TLS records in rustls's output buffer not yet flushed to TCP
    ///
    /// The TLS check is critical: `shutting_down()` uses this to prevent
    /// premature session close while response DATA is still in rustls's
    /// buffer (accepted by `socket_write_vectored` but not yet on the wire).
    ///
    /// Does NOT check per-stream `back.out`/`back.blocks`; use
    /// [`Self::has_pending_write_full`] on paths that must honour
    /// LIFECYCLE invariant 16 (e.g. shutdown-drain).
    pub fn has_pending_write(&self) -> bool {
        if self.peer_gone_after_final_goaway() {
            return false;
        }
        self.expect_write.is_some()
            || !self.zero.storage.is_empty()
            || self.socket.socket_wants_write()
    }

    /// Connection-level [`Self::has_pending_write`] extended with a per-stream
    /// back-buffer probe (LIFECYCLE §9 invariant 16). Used by shutdown-drain
    /// paths that must not close while any open stream still has outbound
    /// kawa bytes queued — a voluntary scheduler yield can leave `back.out`
    /// or `back.blocks` non-empty without `expect_write` being set.
    pub fn has_pending_write_full<L>(&self, context: &Context<L>) -> bool
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        self.has_pending_write() || any_stream_has_pending_back(&self.streams, &context.streams)
    }

    /// Flush the zero buffer to the socket, counting bytes as connection overhead.
    ///
    /// Returns `true` if the socket stalled (WouldBlock / zero-length write),
    /// meaning the caller should stop writing and wait for the next writable event.
    /// Returns `false` when the buffer has been fully drained.
    fn flush_zero_to_socket(&mut self) -> bool {
        while !self.zero.storage.is_empty() {
            let (size, status) = self.socket.socket_write(self.zero.storage.data());
            #[cfg(debug_assertions)]
            trace!(
                "{} flush_zero_to_socket: written={}, status={:?}, wants_write={}",
                log_context!(self),
                size,
                status,
                self.socket.socket_wants_write()
            );
            self.zero.storage.consume(size);
            self.position.count_bytes_out_counter(size);
            self.bytes.overhead_bout += size;
            if update_readiness_after_write(size, status, &mut self.readiness) {
                return true;
            }
        }
        // Reset buffer positions after draining. consume() advances start but
        // never resets it, so without clear() the next fill would panic.
        self.zero.storage.clear();
        false
    }

    /// Directly flush the zero buffer to the socket without going through
    /// the full writable() path. Used during shutdown when the event loop
    /// won't deliver new epoll events for this session (edge-triggered).
    pub fn flush_zero_buffer(&mut self) {
        if self.flush_zero_to_socket() {
            return;
        }
        self.expect_write = None;
        if self.socket.socket_wants_write() {
            let (_size, status) = self.socket.socket_write(&[]);
            let _ = update_readiness_after_write(0, status, &mut self.readiness);
        }
    }

    pub fn create_stream<L>(
        &mut self,
        stream_id: StreamId,
        context: &mut Context<L>,
    ) -> Option<GlobalStreamId>
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        // RFC 9113 §6.8: reject new streams on a draining connection
        if self.drain.draining {
            error!(
                "{} Rejecting new stream {} on draining connection",
                log_context!(self),
                stream_id
            );
            return None;
        }
        // Track the highest peer-initiated stream ID for GoAway frames
        // before any early return, so GoAway always reports the correct last stream.
        if stream_id > self.highest_peer_stream_id {
            self.highest_peer_stream_id = stream_id;
        }
        let global_stream_id = context.create_stream(
            Ulid::generate(),
            self.peer_settings.settings_initial_window_size,
        )?;
        self.last_stream_id = (stream_id + 2) & !1;
        self.streams.insert(stream_id, global_stream_id);
        self.stream_last_activity_at
            .insert(stream_id, Instant::now());
        Some(global_stream_id)
    }

    pub fn new_stream_id(&mut self) -> Option<StreamId> {
        let (issued, next) = next_stream_id(self.last_stream_id, self.position.is_client())?;
        self.last_stream_id = next;
        Some(issued)
    }

    /// Test-only setter: jump `last_stream_id` close to [`STREAM_ID_MAX`] so
    /// that the next call to [`Self::new_stream_id`] exhausts the 31-bit
    /// space. FIX-22 ("Stream-ID exhaustion disconnects backend gracefully")
    /// exercises the `None`-return branch — reaching it through normal API
    /// usage would require issuing ~2³¹ requests, which is not tractable in
    /// an E2E harness.
    #[cfg(any(test, feature = "e2e-hooks"))]
    pub fn __test_set_last_stream_id(&mut self, id: StreamId) {
        self.last_stream_id = id;
    }

    fn handle_frame<E, L>(
        &mut self,
        frame: Frame,
        wire_payload_len: u32,
        context: &mut Context<L>,
        endpoint: E,
    ) -> MuxResult
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        trace!("{} {:#?}", log_context!(self), frame);
        // Per-frame-type RX counter. Single chokepoint covers every H2 frame
        // type — adding a new `Frame::*` variant fails the build inside the
        // helper, keeping the metric breakdown in lock-step with RFC 9113 §6.
        count!(h2_frame_rx_metric_key(&frame), 1);
        match frame {
            Frame::Data(data) => self.handle_data_frame(data, wire_payload_len, context, endpoint),
            Frame::Headers(headers) => self.handle_headers_frame(headers, context, endpoint),
            Frame::PushPromise(_) => self.handle_push_promise_frame(),
            Frame::Priority(priority) => self.handle_priority_frame(priority, context, endpoint),
            Frame::RstStream(rst_stream) => {
                self.handle_rst_stream_frame(rst_stream, context, endpoint)
            }
            Frame::Settings(settings) => self.handle_settings_frame(settings, context),
            Frame::Ping(ping) => self.handle_ping_frame(ping),
            Frame::GoAway(goaway) => self.handle_goaway_frame(goaway, context, endpoint),
            Frame::WindowUpdate(wu) => self.handle_window_update_frame(wu, context, endpoint),
            Frame::PriorityUpdate(pu) => self.handle_priority_update_frame(pu),
            Frame::Continuation(_) => {
                // Unreachable: standalone CONTINUATION is rejected in
                // `handle_header_state` (RFC 9113 §6.10) and in-block
                // CONTINUATION is consumed by the inline header-parsing
                // path. Keep a defensive fallback that returns
                // PROTOCOL_ERROR rather than panicking in debug builds.
                self.attribute_bytes_to_overhead();
                warn!(
                    "{} CONTINUATION frames are handled inline during header parsing",
                    log_context!(self)
                );
                self.goaway(H2Error::ProtocolError)
            }
            // RFC 9113 §5.5: unknown frame types MUST be ignored and discarded.
            // The parser already consumed the payload; attribute the bytes
            // to connection-level overhead and continue.
            Frame::Unknown(raw) => {
                debug!(
                    "{} Ignoring unknown H2 frame type {}",
                    log_context!(self),
                    raw
                );
                self.attribute_bytes_to_overhead();
                MuxResult::Continue
            }
        }
    }

    /// RFC 9110 §8.6: Content-Length validation must be skipped for responses
    /// where the body is absent by definition:
    /// - Responses to HEAD requests (any status)
    /// - 1xx informational responses
    /// - 204 No Content
    /// - 304 Not Modified
    fn content_length_exempt(
        &self,
        context: &crate::protocol::kawa_h1::editor::HttpContext,
    ) -> bool {
        use crate::protocol::kawa_h1::parser::Method;
        // HEAD method responses (only relevant when reading backend responses)
        if self.position.is_client() && context.method == Some(Method::Head) {
            return true;
        }
        // 1xx, 204, 304 status codes
        if let Some(status) = context.status {
            if (100..200).contains(&status) || status == 204 || status == 304 {
                return true;
            }
        }
        false
    }

    fn handle_data_frame<E, L>(
        &mut self,
        data: parser::Data,
        wire_payload_len: u32,
        context: &mut Context<L>,
        mut endpoint: E,
    ) -> MuxResult
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        // CVE-2019-9518: track empty DATA frames (no payload, no END_STREAM)
        if data.payload.is_empty() && !data.end_stream {
            self.flood_detector.empty_data_count += 1;
            check_flood_or_return!(self);
        }
        let Some(global_stream_id) = self.streams.get(&data.stream_id).copied() else {
            // The stream was terminated while data was expected,
            // probably due to automatic answer for invalid/unauthorized access.
            // RFC 9113 §6.9: we MUST still account for the DATA payload in
            // connection-level flow control using the full wire length
            // (including pad-length byte and padding), otherwise the window
            // shrinks permanently and eventually stalls the connection.
            self.flow_control.received_bytes_since_update += wire_payload_len;
            let conn_threshold = self.connection_config.initial_connection_window / 2;
            if self.flow_control.received_bytes_since_update >= conn_threshold {
                let increment = self.flow_control.received_bytes_since_update;
                self.queue_window_update(0, increment);
                self.flow_control.received_bytes_since_update = 0;
                self.readiness.arm_writable();
            }
            self.attribute_bytes_to_overhead();
            return MuxResult::Continue;
        };
        let mut slice = data.payload;
        let stream = &mut context.streams[global_stream_id];
        // Unpadded application payload size — what is forwarded to the backend
        // and counted against Content-Length.
        let content_len = slice.len();
        // Full wire-payload size (includes pad-length byte and padding).
        // RFC 9113 §5.2: padding counts against flow-control windows.
        let wire_len = wire_payload_len as usize;
        let cl_exempt = self.content_length_exempt(&stream.context);

        // Extract declared content-length and update position-aware data counter
        let (data_received, declared_length) = {
            let parts = stream.split(&self.position);
            *parts.data_received += content_len;
            let total = *parts.data_received;
            let declared = match parts.rbuffer.body_size {
                kawa::BodySize::Length(n) => Some(n),
                _ => None,
            };
            (total, declared)
        };

        // RFC 9113 §6.9 + §5.2: credit connection-level flow control BEFORE any
        // early-return path. Malformed DATA still consumed the peer's send
        // window; without crediting it back, repeated bad streams permanently
        // shrink the connection window and stall unrelated streams that share
        // the same H2 connection. Stream-level credit can stay below — once we
        // RST the violating stream, its per-stream window is moot per
        // RFC 9113 §6.9 (the receiver discards further frames for the stream).
        let conn_threshold = self.connection_config.initial_connection_window / 2;
        self.flow_control.received_bytes_since_update += wire_payload_len;
        if self.flow_control.received_bytes_since_update >= conn_threshold {
            let increment = self.flow_control.received_bytes_since_update;
            self.queue_window_update(0, increment);
            self.flow_control.received_bytes_since_update = 0;
        }

        // RFC 9113 §8.1.1: if Content-Length is present, total DATA payload
        // must not exceed the declared length (check on every frame).
        // RFC 9110 §8.6: skip for HEAD/1xx/204/304 responses (body absent by definition).
        if !cl_exempt {
            if let Some(expected) = declared_length {
                if data_received > expected {
                    error!(
                        "{} Content-Length mismatch: received {} > declared {}",
                        log_context!(self),
                        data_received,
                        expected
                    );
                    // Pair WRITABLE arming with the queued connection-level
                    // WINDOW_UPDATE before returning; otherwise the credit sits
                    // until the next inbound frame on this connection.
                    if !self.flow_control.pending_window_updates.is_empty() {
                        self.readiness.arm_writable();
                    }
                    let result = self.reset_stream(
                        data.stream_id,
                        global_stream_id,
                        context,
                        endpoint,
                        H2Error::ProtocolError,
                    );
                    self.remove_dead_stream(data.stream_id, global_stream_id);
                    return result;
                }
            }
        }

        let stream = &mut context.streams[global_stream_id];
        self.attribute_bytes_to_stream(&mut stream.metrics);
        let stream_state = stream.state;
        let is_unlinked = matches!(stream_state, StreamState::Unlinked);
        let parts = stream.split(&self.position);
        let kawa = parts.rbuffer;
        self.position.count_bytes_in(parts.metrics, content_len);

        // Stream-level flow control (only if stream is still open).
        // Connection-level credit was already applied above the CL check so
        // malformed DATA cannot starve the connection window for other streams.
        if !data.end_stream {
            self.queue_window_update(data.stream_id, wire_payload_len);
        }

        // If we have pending updates, ensure we get a writable event.
        // Must use signal_pending_write() — not just interest.insert() — because
        // under edge-triggered epoll the WRITABLE event bit may have been consumed
        // by a previous write cycle. Without the event bit set, filter_interest()
        // returns 0 and the WINDOW_UPDATEs never get flushed, stalling the client.
        if !self.flow_control.pending_window_updates.is_empty() {
            self.readiness.arm_writable();
        }

        // Refresh per-stream idle timer on non-empty DATA.
        // Empty DATA frames (CVE-2019-9518 vector) must NOT reset the timer,
        // otherwise an attacker can keep a stream alive indefinitely with
        // zero-length frames while pinning a MAX_CONCURRENT_STREAMS slot.
        if content_len > 0 {
            if let Some(t) = self.stream_last_activity_at.get_mut(&data.stream_id) {
                *t = Instant::now();
            }
        }

        if is_unlinked {
            // Backend is gone but client is still sending DATA.
            // Discard the data (flow control updates were already
            // queued above) to prevent the buffer from filling up.
            kawa.storage.clear();
            if data.end_stream {
                kawa.parsing_phase = kawa::ParsingPhase::Terminated;
                self.mark_end_of_stream(stream);
            }
        } else {
            // Advance storage.head by the full wire payload length so the
            // next frame doesn't read stale pad-length+padding bytes.
            slice.start = slice.start.saturating_add(kawa.storage.head as u32);
            kawa.storage.head += wire_len;

            // Emit chunk framing for chunked transfer encoding (H2→H1 path).
            // H2 converter ignores ChunkHeader and end_chunk Flags, so this is safe for H2→H2.
            if kawa.body_size == kawa::BodySize::Chunked && content_len > 0 {
                let hex_len = {
                    let mut buf = Vec::with_capacity(16);
                    let _ = write!(buf, "{content_len:x}");
                    buf
                };
                kawa.push_block(kawa::Block::ChunkHeader(kawa::ChunkHeader {
                    length: kawa::Store::from_vec(hex_len),
                }));
            }

            kawa.push_block(kawa::Block::Chunk(kawa::Chunk {
                data: kawa::Store::Slice(slice),
            }));

            if kawa.body_size == kawa::BodySize::Chunked && content_len > 0 {
                kawa.push_block(kawa::Block::Flags(kawa::Flags {
                    end_body: false,
                    end_chunk: true,
                    end_header: false,
                    end_stream: false,
                }));
            }

            if data.end_stream {
                // RFC 9113 §8.1.1: on end_stream, total DATA must equal Content-Length.
                // RFC 9110 §8.6: skip for HEAD/1xx/204/304 responses.
                if !cl_exempt {
                    if let Some(expected) = declared_length {
                        if data_received != expected {
                            error!(
                                "{} Content-Length mismatch: received {} != declared {}",
                                log_context!(self),
                                data_received,
                                expected
                            );
                            let result = self.reset_stream(
                                data.stream_id,
                                global_stream_id,
                                context,
                                endpoint,
                                H2Error::ProtocolError,
                            );
                            self.remove_dead_stream(data.stream_id, global_stream_id);
                            return result;
                        }
                    }
                }
                let is_chunked = kawa.body_size == kawa::BodySize::Chunked;
                kawa.push_block(kawa::Block::Flags(kawa::Flags {
                    end_body: true,
                    end_chunk: is_chunked,
                    end_header: false,
                    end_stream: true,
                }));
                kawa.parsing_phase = kawa::ParsingPhase::Terminated;
                self.mark_end_of_stream(stream);
            }
            if let StreamState::Linked(token) = stream_state {
                // Mirror of h1.rs:361-368 for the H2-backend → H2-frontend
                // path: edge-triggered epoll will NOT re-fire for bytes we
                // just pushed into stream.back; the synthetic event is the
                // only wake path. LIFECYCLE invariant 15.
                endpoint.readiness_mut(token).arm_writable();
                incr!("h2.signal.writable.rearmed.peer_data");
            }
        }
        MuxResult::Continue
    }

    fn handle_headers_frame<E, L>(
        &mut self,
        headers: Headers,
        context: &mut Context<L>,
        mut endpoint: E,
    ) -> MuxResult
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        // HEADERS frames represent real application activity (new request
        // or response). Reset the timeout since the peer is actively
        // communicating, unlike control frames (PING, WINDOW_UPDATE).
        self.timeout_container.reset();
        if !headers.end_headers {
            // CVE-2024-27316: only initialize tracking on the very first HEADERS
            // fragment, not on re-entries from ContinuationFrame (which call
            // handle_frame(Frame::Headers) with the accumulated header block).
            if self.flood_detector.continuation_count == 0 {
                self.flood_detector.accumulated_header_size = headers.header_block_fragment.len;
            }
            debug!(
                "{} FRAGMENT: stream_id={}, len={}",
                log_context!(self),
                headers.stream_id,
                self.zero.storage.data().len()
            );
            self.state = H2State::ContinuationHeader(headers);
            return MuxResult::Continue;
        }
        // Header block is complete — reset CONTINUATION counters
        self.flood_detector.reset_continuation();
        // can this fail?
        let stream_id = headers.stream_id;
        let Some(global_stream_id) = self.streams.get(&stream_id).copied() else {
            error!(
                "{} Handling Headers frame with no attached stream {:#?}",
                log_context!(self),
                self
            );
            incr!("h2.headers_no_stream.error");
            self.attribute_bytes_to_overhead();
            return self.force_disconnect();
        };

        // Refresh per-stream idle timer on HEADERS (response headers or trailers
        // on an existing stream). Initial HEADERS that create the stream already
        // set the timestamp in create_stream().
        if let Some(t) = self.stream_last_activity_at.get_mut(&stream_id) {
            *t = Instant::now();
        }

        if let Some(priority) = &headers.priority {
            if self.prioriser.push_priority(stream_id, priority.clone()) {
                self.reset_stream(
                    stream_id,
                    global_stream_id,
                    context,
                    endpoint,
                    H2Error::ProtocolError,
                );
                self.remove_dead_stream(stream_id, global_stream_id);
                return MuxResult::Continue;
            }
        }

        let stream = &mut context.streams[global_stream_id];
        self.attribute_bytes_to_stream(&mut stream.metrics);
        let kawa = &mut self.zero;
        let buffer = headers.header_block_fragment.data(kawa.storage.buffer());
        let stream = &mut context.streams[global_stream_id];
        let parts = &mut stream.split(&self.position);
        let was_initial = parts.rbuffer.is_initial();
        let elide_x_real_ip = parts.context.elide_x_real_ip;
        let status = pkawa::handle_header(
            &mut self.decoder,
            &mut self.prioriser,
            stream_id,
            parts.rbuffer,
            buffer,
            headers.end_stream,
            parts.context,
            self.flood_detector.config.max_header_list_size,
            elide_x_real_ip,
        );
        kawa.storage.clear();
        if let Err((error, global)) = status {
            match self.position {
                Position::Client(..) => incr!("http.backend_parse_errors"),
                Position::Server => incr!("http.frontend_parse_errors"),
            }
            if global {
                error!(
                    "{} GOT GLOBAL ERROR WHILE PROCESSING HEADERS",
                    log_context!(self)
                );
                return self.goaway(error);
            } else {
                let result =
                    self.reset_stream(stream_id, global_stream_id, context, endpoint, error);
                self.remove_dead_stream(stream_id, global_stream_id);
                return result;
            }
        }
        if headers.end_stream {
            // RFC 9113 §8.1.1: when END_STREAM arrives via trailers,
            // validate that total DATA received matches Content-Length.
            // RFC 9110 §8.6: skip for HEAD/1xx/204/304 responses.
            if !was_initial && !self.content_length_exempt(&stream.context) {
                let parts = stream.split(&self.position);
                if let kawa::BodySize::Length(expected) = parts.rbuffer.body_size {
                    if *parts.data_received != expected {
                        error!(
                            "{} Content-Length mismatch on trailers: received {} != declared {}",
                            log_context!(self),
                            *parts.data_received,
                            expected
                        );
                        let result = self.reset_stream(
                            stream_id,
                            global_stream_id,
                            context,
                            endpoint,
                            H2Error::ProtocolError,
                        );
                        self.remove_dead_stream(stream_id, global_stream_id);
                        return result;
                    }
                }
            }
            self.mark_end_of_stream(stream);
        }
        if let StreamState::Linked(token) = stream.state {
            // Mirror of handle_data_frame's rearm. LIFECYCLE invariant 15.
            endpoint.readiness_mut(token).arm_writable();
            incr!("h2.signal.writable.rearmed.peer_headers");
        }
        // was_initial prevents trailers from triggering connection
        if was_initial && self.position.is_server() {
            incr!("http.requests");
            gauge_add!("http.active_requests", 1);
            stream.metrics.service_start();
            stream.request_counted = true;
            stream.state = StreamState::Link;
            context.pending_links.push_back(global_stream_id);
        }
        MuxResult::Continue
    }

    fn handle_push_promise_frame(&mut self) -> MuxResult {
        self.attribute_bytes_to_overhead();
        match self.position {
            Position::Client(..) => {
                // RFC 9113 §8.4: Server push is deprecated. Sozu never sends
                // SETTINGS_ENABLE_PUSH=1, so receiving PUSH_PROMISE is a protocol error.
                error!(
                    "{} Received PUSH_PROMISE but server push is not supported",
                    log_context!(self)
                );
                self.goaway(H2Error::ProtocolError)
            }
            Position::Server => {
                // Clients must never send PUSH_PROMISE (RFC 9113 §8.4)
                error!("{} Received PUSH_PROMISE from client", log_context!(self));
                self.goaway(H2Error::ProtocolError)
            }
        }
    }

    fn handle_priority_frame<E, L>(
        &mut self,
        priority: parser::Priority,
        context: &mut Context<L>,
        endpoint: E,
    ) -> MuxResult
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        if let Some(global_stream_id) = self.streams.get(&priority.stream_id).copied() {
            let stream = &mut context.streams[global_stream_id];
            self.attribute_bytes_to_stream(&mut stream.metrics);
        } else {
            self.attribute_bytes_to_overhead();
        }
        // Pass 3 Medium #4: standalone PRIORITY frames can arrive for any
        // peer-chosen stream ID. Accept only currently-open streams and a
        // small idle look-ahead window; everything else is dropped before
        // it can feed memory into the priority map.
        if self.prioriser.push_priority_guarded(
            priority.stream_id,
            priority.inner,
            self.last_stream_id,
            &self.streams,
        ) {
            if let Some(global_stream_id) = self.streams.get(&priority.stream_id).copied() {
                let result = self.reset_stream(
                    priority.stream_id,
                    global_stream_id,
                    context,
                    endpoint,
                    H2Error::ProtocolError,
                );
                self.remove_dead_stream(priority.stream_id, global_stream_id);
                return result;
            } else {
                error!(
                    "{} INVALID PRIORITY RECEIVED ON INVALID STREAM",
                    log_context!(self)
                );
                return self.goaway(H2Error::ProtocolError);
            }
        }
        MuxResult::Continue
    }

    /// RFC 9218 §7.1: PRIORITY_UPDATE reprioritizes an open or idle-soon
    /// stream at the connection level. Decodes the priority field value
    /// (same grammar as the `priority` request header, `parse_rfc9218_priority`)
    /// and pushes it into the `Prioriser` through the same guarded path used
    /// for standalone PRIORITY frames — the guard bounds memory against a
    /// client spamming PRIORITY_UPDATE for far-future stream IDs.
    ///
    /// Prioritized stream ID `0` is a connection-level `PROTOCOL_ERROR`
    /// (RFC 9218 §7.1). For any other ID that is not currently open or
    /// within the idle look-ahead budget, the update is silently dropped
    /// (matches the PRIORITY-frame guard semantics — no state change).
    fn handle_priority_update_frame(&mut self, pu: parser::PriorityUpdate) -> MuxResult {
        self.attribute_bytes_to_overhead();
        if pu.prioritized_stream_id == 0 {
            error!(
                "{} PRIORITY_UPDATE with prioritized_stream_id=0 (RFC 9218 §7.1)",
                log_context!(self)
            );
            return self.goaway(H2Error::ProtocolError);
        }
        let (urgency, incremental) = pkawa::parse_rfc9218_priority(&pu.priority_field_value);
        let (prev_urgency, _) = self.prioriser.get(&pu.prioritized_stream_id);
        trace!(
            "{} PRIORITY_UPDATE stream={} urgency={}->{} incremental={} rearmed_writable=true",
            log_context!(self),
            pu.prioritized_stream_id,
            prev_urgency,
            urgency,
            incremental
        );
        let _ = self.prioriser.push_priority_guarded(
            pu.prioritized_stream_id,
            parser::PriorityPart::Rfc9218 {
                urgency,
                incremental,
            },
            self.last_stream_id,
            &self.streams,
        );
        // LIFECYCLE invariant 15: reprioritisation only changes ordering for
        // the NEXT write pass. Under ET epoll, if finalize_write already
        // stripped WRITABLE, the scheduler won't re-run without a synthetic
        // wake — pair the interest insert with signal_pending_write.
        self.readiness.arm_writable();
        incr!("h2.signal.writable.rearmed.priority_update");
        MuxResult::Continue
    }

    fn handle_rst_stream_frame<E, L>(
        &mut self,
        rst_stream: parser::RstStream,
        context: &mut Context<L>,
        mut endpoint: E,
    ) -> MuxResult
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        // Per-error-code counter for the inbound RST. Emitted before the
        // flood-detector trip check so even a connection that gets terminated
        // by `handle_flood_violation` shows up in the per-code breakdown
        // (the dedicated `h2.flood.violation.rst_stream_*` series tracks the
        // mitigation event itself).
        count!(metric_for_rst_stream_received(rst_stream.error_code), 1);
        // CVE-2023-44487 Rapid Reset + CVE-2019-9514: track RST_STREAM rate.
        self.flood_detector.rst_stream_count += 1;
        check_flood_or_return!(self);
        // Additional CVE-2023-44487 mitigation: lifetime cap on RST_STREAM
        // frames received. The per-window counter above half-decays, so a
        // patient client can keep ~50 RST/s forever; a never-decaying
        // lifetime counter puts an absolute ceiling on that amplification.
        // Streams whose backend response has not yet started count toward a
        // much lower "abusive" ceiling — this is the signature Rapid Reset
        // pattern where the attacker pays one RST frame and we pay a
        // backend round-trip for each.
        //
        // "Response started" here means the Server has begun producing
        // response bytes (backend kawa buffer past its initial phase). For
        // the Client position the concept does not apply symmetrically
        // (RSTs received from the backend are rare and benign), so we
        // conservatively flag them as abusive too — lifetime cap still
        // dominates in practice.
        let response_started = match self.streams.get(&rst_stream.stream_id) {
            Some(global_stream_id) => {
                let stream = &context.streams[*global_stream_id];
                !stream.back.is_initial()
            }
            // Stream already gone (e.g. closed, not yet registered) —
            // treat as response-started to avoid over-counting benign
            // races as abusive.
            None => true,
        };
        if let Some(violation) = self.flood_detector.record_rst_lifetime(response_started) {
            return self.handle_flood_violation(violation);
        }
        // Rapid Reset signature (CVE-2023-44487): a RST that arrives before the
        // backend has begun answering. Emitted alongside the per-code counter
        // so the SOC can alert on the rate of pre-response RSTs without
        // having to differentiate by error code.
        if !response_started {
            count!("h2.rst_stream.received.pre_response_start", 1);
        }
        debug!(
            "{} RstStream({} -> {})",
            log_context!(self),
            rst_stream.error_code,
            H2Error::try_from(rst_stream.error_code).map_or("UNKNOWN_ERROR", |e| e.as_str())
        );
        // Compute totals before removing the stream from the map,
        // so the removed stream's bytes are included in the total.
        let rst_byte_totals = self.compute_stream_byte_totals(context);
        if let Some(global_stream_id) = self.streams.get(&rst_stream.stream_id).copied() {
            let stream = &mut context.streams[global_stream_id];
            self.attribute_bytes_to_stream(&mut stream.metrics);
            let linked_token = stream.linked_token();
            let (client_rtt, server_rtt) =
                Self::snapshot_rtts(&self.position, &self.socket, &endpoint, linked_token);
            if let Some(token) = linked_token {
                endpoint.end_stream(token, global_stream_id, context);
            }
            let stream = &mut context.streams[global_stream_id];
            match &self.position {
                // Inbound RST_STREAM on the backend side terminates the in-flight
                // request without going through Connection::end_stream (the normal
                // place where Backend.active_requests is decremented), so do the
                // bookkeeping explicitly here to avoid leaking load counters.
                Position::Client(_, backend, BackendStatus::Connected) => {
                    let mut backend_borrow = backend.borrow_mut();
                    backend_borrow.active_requests =
                        backend_borrow.active_requests.saturating_sub(1);
                }
                Position::Client(..) => {}
                Position::Server => {
                    self.distribute_overhead(&mut stream.metrics, rst_byte_totals);
                    // This is a special case, normally, all stream are terminated by the server
                    // when the last byte of the response is written. Here, the reset is requested
                    // on the server endpoint and immediately terminates, shortcutting the other path
                    stream.metrics.backend_stop();
                    stream.generate_access_log(
                        true,
                        Some("H2::ResetFrame"),
                        context.listener.clone(),
                        client_rtt,
                        server_rtt,
                    );
                    stream.state = StreamState::Recycle;
                }
            }
            // Retire from streams/prioriser/stream_last_activity_at and
            // invalidate expect_write/expect_read if they reference this gid.
            self.remove_dead_stream(rst_stream.stream_id, global_stream_id);
        } else {
            self.attribute_bytes_to_overhead();
        }
        MuxResult::Continue
    }

    fn handle_settings_frame<L>(
        &mut self,
        settings: parser::Settings,
        context: &mut Context<L>,
    ) -> MuxResult
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        if settings.ack {
            // RFC 9113 §6.5: SETTINGS ACK must have empty payload
            if !settings.settings.is_empty() {
                error!("{} SETTINGS ACK with non-empty payload", log_context!(self));
                return self.goaway(H2Error::FrameSizeError);
            }
            // RFC 9113 §6.5: peer acknowledged our SETTINGS — clear timeout
            self.settings_sent_at = None;
            // RFC 7541 §4.2: sync the decoder's max allowed table size with
            // what we advertised. Currently a no-op (settings don't change at
            // runtime), but guards against future runtime SETTINGS updates.
            self.decoder.set_max_allowed_table_size(
                self.local_settings.settings_header_table_size as usize,
            );
            self.attribute_bytes_to_overhead();
            return MuxResult::Continue;
        }
        // CVE-2019-9515: track SETTINGS frame rate
        self.flood_detector.settings_count += 1;
        self.flood_detector.total_settings_received_lifetime = self
            .flood_detector
            .total_settings_received_lifetime
            .saturating_add(1);
        check_flood_or_return!(self);
        for setting in settings.settings {
            let v = setting.value;
            let mut is_error = false;
            #[rustfmt::skip]
            match setting.identifier {
                parser::SETTINGS_HEADER_TABLE_SIZE => {
                    // Cap to the configured maximum — a malicious peer can
                    // advertise up to 4 GB to inflate HPACK encoder memory.
                    let cap = self.flood_detector.config.max_header_table_size;
                    let capped = v.min(cap);
                    self.peer_settings.settings_header_table_size = capped;
                    self.encoder.set_max_table_size(capped as usize);
                    // RFC 7541 §4.2 / §6.3: queue a dynamic-table-size-update
                    // HPACK directive for the next header block we emit.
                    // Without it, the peer's decoder keeps its previous (possibly
                    // larger) table cap and our encoder-side change is silent
                    // — conformance suites (h2spec `hpack/4.2`) will flag it.
                    self.pending_table_size_update = Some(capped);
                },
                parser::SETTINGS_ENABLE_PUSH       => { self.peer_settings.settings_enable_push = v == 1;             is_error |= v > 1 },
                parser::SETTINGS_MAX_CONCURRENT_STREAMS => { self.peer_settings.settings_max_concurrent_streams = v },
                parser::SETTINGS_INITIAL_WINDOW_SIZE    => { is_error |= self.update_initial_window_size(v, context) },
                parser::SETTINGS_MAX_FRAME_SIZE         => { self.peer_settings.settings_max_frame_size = v;           is_error |= !(MIN_MAX_FRAME_SIZE..MAX_MAX_FRAME_SIZE).contains(&v) },
                parser::SETTINGS_MAX_HEADER_LIST_SIZE   => { self.peer_settings.settings_max_header_list_size = v },
                parser::SETTINGS_ENABLE_CONNECT_PROTOCOL => { self.peer_settings.settings_enable_connect_protocol = v == 1; is_error |= v > 1 },
                parser::SETTINGS_NO_RFC7540_PRIORITIES   => { self.peer_settings.settings_no_rfc7540_priorities = v == 1;   is_error |= v > 1 },
                other => { warn!("Unknown setting_id: {}, we MUST ignore this", other); self.flood_detector.glitch_count += 1 },
            };
            if is_error {
                error!("{} INVALID SETTING", log_context!(self));
                return self.goaway(H2Error::ProtocolError);
            }
        }

        self.attribute_bytes_to_overhead();

        // Enlarge the connection-level receive window for backend H2
        // connections (Position::Client). The server side does this in
        // the ServerSettings writable path, but the client needs to do
        // it here after receiving the server's initial SETTINGS.
        if self.position.is_client()
            && self.flow_control.window <= DEFAULT_INITIAL_WINDOW_SIZE as i32
        {
            let increment = self
                .connection_config
                .initial_connection_window
                .saturating_sub(DEFAULT_INITIAL_WINDOW_SIZE);
            if increment > 0 {
                self.queue_window_update(0, increment);
            }
            // Do NOT increment flow_control.window here: sending our own
            // WINDOW_UPDATE enlarges the peer's send allowance, not ours.
            // Our send window is only updated by WINDOW_UPDATEs we receive
            // from the peer (RFC 9113 §6.9).
        }

        let kawa = &mut self.zero;
        let ack = &serializer::SETTINGS_ACKNOWLEDGEMENT;
        let buf = kawa.storage.space();
        if buf.len() < ack.len() {
            error!(
                "{} No space in zero buffer for SETTINGS ACK ({} available, {} needed)",
                log_context!(self),
                buf.len(),
                ack.len()
            );
            return self.force_disconnect();
        }
        buf[..ack.len()].copy_from_slice(ack);
        kawa.storage.fill(ack.len());

        self.readiness.interest.insert(Ready::WRITABLE);
        self.readiness.interest.remove(Ready::READABLE);
        self.expect_write = Some(H2StreamId::Zero);
        self.readiness.signal_pending_write();
        MuxResult::Continue
    }

    fn handle_ping_frame(&mut self, ping: parser::Ping) -> MuxResult {
        if ping.ack {
            self.attribute_bytes_to_overhead();
            return MuxResult::Continue;
        }
        // CVE-2019-9512: track non-ACK PING frame rate
        self.flood_detector.ping_count += 1;
        self.flood_detector.total_ping_received_lifetime = self
            .flood_detector
            .total_ping_received_lifetime
            .saturating_add(1);
        check_flood_or_return!(self);
        self.attribute_bytes_to_overhead();
        let kawa = &mut self.zero;
        let ping_response_size = serializer::PING_ACKNOWLEDGEMENT_HEADER.len() + 8;
        if kawa.storage.space().len() < ping_response_size {
            error!(
                "{} No space in zero buffer for PING response ({} available, {} needed)",
                log_context!(self),
                kawa.storage.space().len(),
                ping_response_size
            );
            return self.force_disconnect();
        }
        match serializer::gen_ping_acknowledgement(kawa.storage.space(), &ping.payload) {
            Ok((_, size)) => {
                kawa.storage.fill(size);
                incr!("h2.frames.tx.ping_ack");
            }
            Err(error) => {
                error!(
                    "{} Could not serialize PingFrame: {:?}",
                    log_context!(self),
                    error
                );
                return self.force_disconnect();
            }
        };
        self.readiness.interest.insert(Ready::WRITABLE);
        self.readiness.interest.remove(Ready::READABLE);
        self.expect_write = Some(H2StreamId::Zero);
        self.readiness.signal_pending_write();
        MuxResult::Continue
    }

    fn handle_goaway_frame<E, L>(
        &mut self,
        goaway: parser::GoAway,
        context: &mut Context<L>,
        mut endpoint: E,
    ) -> MuxResult
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        self.attribute_bytes_to_overhead();
        let error_name =
            H2Error::try_from(goaway.error_code).map_or("UNKNOWN_ERROR", |e| e.as_str());
        if goaway.error_code == H2Error::NoError as u32 {
            debug!(
                "{} Received GOAWAY: last_stream_id={}, error={}, debug_data={:?}",
                log_context!(self),
                goaway.last_stream_id,
                error_name,
                goaway.additional_debug_data
            );
        } else {
            // Peer-originated failure: no variant of H2Error from a peer
            // implies a sozu bug. Impact handling is separate (retry above
            // `last_stream_id`, RST_STREAM for consumed streams) and logs
            // its own details below, so the summary drops to `warn!`.
            warn!(
                "{} Received GOAWAY: last_stream_id={}, error={}, debug_data={:?}",
                log_context!(self),
                goaway.last_stream_id,
                error_name,
                goaway.additional_debug_data
            );
        }
        count!(metric_for_goaway_received(goaway.error_code), 1);
        // RFC 9113 §6.8: begin graceful drain.
        self.drain.draining = true;
        self.drain.peer_last_stream_id = Some(goaway.last_stream_id);

        // Streams with ID > last_stream_id were NOT processed by the peer.
        // Mark them for retry (StreamState::Link) so they can be retried
        // on a new connection.
        // IMPORTANT: do NOT call endpoint.end_stream() here — that would
        // remove the stream from the frontend's H2 stream map and send
        // RST_STREAM to the client, killing the request instead of retrying it.
        let mut retry_streams = Vec::new();
        for (&stream_id, &global_stream_id) in &self.streams {
            if stream_id > goaway.last_stream_id {
                retry_streams.push((stream_id, global_stream_id));
            }
        }
        for (stream_id, global_stream_id) in &retry_streams {
            // Remove from reverse index before transitioning away from Linked.
            if let StreamState::Linked(token) = context.streams[*global_stream_id].state {
                remove_backend_stream(&mut context.backend_streams, token, *global_stream_id);
            }
            let stream = &mut context.streams[*global_stream_id];
            if stream.front.consumed {
                // Request was already sent to this backend — we can't
                // replay it. Use the linked token's readiness (via endpoint)
                // so the RST_STREAM reaches the client.
                debug!(
                    "{} GOAWAY: stream {} already consumed, cannot retry",
                    log_context!(self),
                    stream_id
                );
                if let StreamState::Linked(token) = stream.state {
                    let front_readiness = endpoint.readiness_mut(token);
                    forcefully_terminate_answer(stream, front_readiness, H2Error::RefusedStream);
                } else {
                    warn!(
                        "{} GOAWAY: stream {} consumed but not Linked, cannot notify frontend",
                        log_context!(self),
                        stream_id
                    );
                }
            } else {
                stream.state = StreamState::Link;
                context.pending_links.push_back(*global_stream_id);
            }
            // Both retry (!consumed) and terminated (consumed) paths remove the
            // stream from self.streams without going through Connection::end_stream,
            // so decrement Backend.active_requests here to keep load metrics honest.
            if let Position::Client(_, backend, BackendStatus::Connected) = &self.position {
                let mut backend_borrow = backend.borrow_mut();
                backend_borrow.active_requests = backend_borrow.active_requests.saturating_sub(1);
            }
            // Retire from streams/prioriser/stream_last_activity_at and
            // invalidate expect_write/expect_read if they reference this gid.
            self.remove_dead_stream(*stream_id, *global_stream_id);
        }

        // If no active streams remain, close immediately
        if self.streams.is_empty() {
            return self.goaway(H2Error::NoError);
        }

        // Otherwise, let remaining streams (ID <= last_stream_id) complete.
        // The connection will be closed when all streams finish.
        MuxResult::Continue
    }

    fn handle_window_update_frame<E, L>(
        &mut self,
        wu: WindowUpdate,
        context: &mut Context<L>,
        endpoint: E,
    ) -> MuxResult
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        let stream_id = wu.stream_id;
        let increment = wu.increment;

        // RFC 9113 §6.9: increment of 0 MUST be treated as an error.
        // Connection-level (stream 0) -> connection error (GOAWAY).
        // Stream-level -> stream error (RST_STREAM).
        if increment == 0 {
            if stream_id == 0 {
                error!(
                    "{} WINDOW_UPDATE with zero increment on connection (stream 0)",
                    log_context!(self)
                );
                return self.goaway(H2Error::ProtocolError);
            } else {
                error!(
                    "{} WINDOW_UPDATE with zero increment on stream {}",
                    log_context!(self),
                    stream_id
                );
                if let Some(global_stream_id) = self.streams.get(&stream_id).copied() {
                    let result = self.reset_stream(
                        stream_id,
                        global_stream_id,
                        context,
                        endpoint,
                        H2Error::ProtocolError,
                    );
                    self.remove_dead_stream(stream_id, global_stream_id);
                    return result;
                }
                // Stream not in map (already closed) — treat as glitch
                self.flood_detector.glitch_count += 1;
                check_flood_or_return!(self);
                self.attribute_bytes_to_overhead();
                return MuxResult::Continue;
            }
        }

        // The parser masks the reserved bit (STREAM_ID_MASK), so increment <=
        // 2^31-1 and try_from always succeeds. Use try_from rather than `as` to
        // guard against a future parser change that drops the mask.
        let increment = i32::try_from(increment).unwrap_or(i32::MAX);
        if stream_id == 0 {
            // Count connection-level WINDOW_UPDATEs before touching the window
            // so a per-window flood stops us before we pay the arithmetic cost
            // on a million-frame burst. Zero-increment frames short-circuited
            // above, so every increment here is a legal-looking rate consumer.
            self.flood_detector.window_update_stream0_count = self
                .flood_detector
                .window_update_stream0_count
                .saturating_add(1);
            check_flood_or_return!(self);
            self.attribute_bytes_to_overhead();
            if let Some(window) = self.flow_control.window.checked_add(increment) {
                if self.flow_control.window <= 0 && window > 0 {
                    self.readiness.arm_writable();
                }
                self.flow_control.window = window;
                debug!(
                    "{} WINDOW_UPDATE received: stream=0 increment={} new_connection_window={}",
                    log_context!(self),
                    increment,
                    self.flow_control.window
                );
            } else {
                error!("{} INVALID WINDOW INCREMENT", log_context!(self));
                return self.goaway(H2Error::FlowControlError);
            }
        } else if let Some(global_stream_id) = self.streams.get(&stream_id).copied() {
            let stream = &mut context.streams[global_stream_id];
            self.attribute_bytes_to_stream(&mut stream.metrics);
            if let Some(window) = stream.window.checked_add(increment) {
                if stream.window <= 0 && window > 0 {
                    self.readiness.arm_writable();
                }
                stream.window = window;
                debug!(
                    "{} WINDOW_UPDATE received: stream={} increment={} new_stream_window={}",
                    log_context!(self),
                    stream_id,
                    increment,
                    stream.window
                );
            } else {
                let result = self.reset_stream(
                    stream_id,
                    global_stream_id,
                    context,
                    endpoint,
                    H2Error::FlowControlError,
                );
                self.remove_dead_stream(stream_id, global_stream_id);
                return result;
            }
        } else {
            self.attribute_bytes_to_overhead();
            trace!(
                "{} Ignoring window update on closed stream {}: {}",
                log_context!(self),
                stream_id,
                increment
            );
            // Pass 3 Low #5: WINDOW_UPDATE on a closed stream is legal
            // (RFC 9113 §6.9.1) but has no useful effect, so a peer that
            // keeps sending them is wasting our cycles. Count it as a
            // glitch so a flood contributes to `check_flood()` and can
            // eventually trigger ENHANCE_YOUR_CALM.
            self.flood_detector.glitch_count += 1;
            check_flood_or_return!(self);
        }
        MuxResult::Continue
    }

    fn update_initial_window_size<L>(&mut self, value: u32, context: &mut Context<L>) -> bool
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        if value > FLOW_CONTROL_MAX_WINDOW {
            return true;
        }
        let delta = match i32::try_from(
            value as i64 - self.peer_settings.settings_initial_window_size as i64,
        ) {
            Ok(d) => d,
            Err(_) => {
                error!("{} initial window size delta overflow", log_context!(self));
                return true;
            }
        };
        let mut open_window = false;
        // Only update windows for streams owned by this connection
        for &global_stream_id in self.streams.values() {
            let stream = &mut context.streams[global_stream_id];
            // RFC 9113 §6.9.2: changes to SETTINGS_INITIAL_WINDOW_SIZE can cause
            // stream windows to exceed 2^31-1, which is a flow control error.
            match stream.window.checked_add(delta) {
                Some(new_window) => {
                    open_window |= stream.window <= 0 && new_window > 0;
                    stream.window = new_window;
                }
                None => return true,
            }
        }
        trace!(
            "{} UPDATE INIT WINDOW: {} {} {:?}",
            log_context!(self),
            delta,
            open_window,
            self.readiness
        );
        if open_window {
            self.readiness.arm_writable();
        }
        self.peer_settings.settings_initial_window_size = value;
        false
    }

    pub fn force_disconnect(&mut self) -> MuxResult {
        self.state = H2State::Error;
        match &mut self.position {
            Position::Client(_, _, status) => {
                *status = BackendStatus::Disconnecting;
                self.readiness.event = Ready::HUP;
                debug!(
                    "{} H2 force_disconnect client: state={:?}, streams={}, expect_write={:?}, wants_write={}, readiness={:?}",
                    log_context!(self),
                    self.state,
                    self.streams.len(),
                    self.expect_write,
                    self.socket.socket_wants_write(),
                    self.readiness
                );
                MuxResult::Continue
            }
            Position::Server => {
                if self.peer_gone_after_final_goaway() {
                    return MuxResult::CloseSession;
                }
                // Don't disconnect immediately if rustls still has buffered TLS
                // records. Returning CloseSession here triggers shutdown(Write)
                // which sends FIN — but any TLS records still in rustls's buffer
                // (not yet flushed to the TCP send buffer) are lost, causing the
                // client to see "TLS decode error / unexpected eof".
                // Instead, keep WRITABLE interest and let the writable path flush.
                if self.socket.socket_wants_write() {
                    debug!(
                        "{} H2 force_disconnect delaying close: state={:?}, streams={}, expect_write={:?}, wants_write=true, readiness={:?}",
                        log_context!(self),
                        self.state,
                        self.streams.len(),
                        self.expect_write,
                        self.readiness
                    );
                    self.readiness.interest = Ready::WRITABLE | Ready::HUP | Ready::ERROR;
                    self.ensure_tls_flushed();
                    MuxResult::Continue
                } else {
                    debug!(
                        "{} H2 force_disconnect closing session: state={:?}, streams={}, expect_write={:?}, wants_write=false, readiness={:?}",
                        log_context!(self),
                        self.state,
                        self.streams.len(),
                        self.expect_write,
                        self.readiness
                    );
                    MuxResult::CloseSession
                }
            }
        }
    }

    pub fn close<E, L>(&mut self, context: &mut Context<L>, mut endpoint: E)
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        match self.position {
            Position::Client(_, _, BackendStatus::KeepAlive) => {
                error!(
                    "{} H2 connections do not use KeepAlive backend status",
                    log_context!(self)
                );
                return;
            }
            Position::Client(..) => {}
            Position::Server => {
                let tls_pending_before = self.socket.socket_wants_write();
                if !self.streams.is_empty() || tls_pending_before || self.expect_write.is_some() {
                    debug!(
                        "{} H2 close with active state: state={:?}, streams={}, expect_write={:?}, wants_write={}, readiness={:?}",
                        log_context!(self),
                        self.state,
                        self.streams.len(),
                        self.expect_write,
                        tls_pending_before,
                        self.readiness
                    );
                    for (stream_id, global_stream_id) in &self.streams {
                        let stream = &context.streams[*global_stream_id];
                        debug!(
                            "{}   close stream id={} gid={}: state={:?}, front_eos={}, back_eos={}, front_phase={:?}, back_phase={:?}, front_completed={}, back_completed={}",
                            log_context!(self),
                            stream_id,
                            global_stream_id,
                            stream.state,
                            stream.front_received_end_of_stream,
                            stream.back_received_end_of_stream,
                            stream.front.parsing_phase,
                            stream.back.parsing_phase,
                            stream.front.is_completed(),
                            stream.back.is_completed()
                        );
                    }
                }
                if !self.close_notify_sent {
                    trace!("{} H2 SENDING CLOSE NOTIFY", log_context!(self));
                }
                let (tls_pending_after, drain_rounds) =
                    drain_tls_close_notify(&mut self.socket, &mut self.close_notify_sent);
                if tls_pending_after {
                    // Severity tiering: key on stream-count + close-state, not
                    // peer-vs-operator. Composes with the send-side `H2Error`
                    // variant tier in `goaway()` — both rules demote benign
                    // paths and keep loss-bearing paths loud.
                    //
                    // - `streams != 0`           -> `error!`: live streams at
                    //   close time, response-byte loss is possible.
                    // - `streams == 0` AND state in {GoAway, Error}
                    //                             -> `warn!`: idle close after
                    //   a GOAWAY exchange (peer-initiated abort or our own
                    //   graceful drain). What's stranded is best-effort
                    //   GOAWAY/close_notify; no application data was queued.
                    // - `streams == 0` from any other state
                    //                             -> `error!`: unexpected
                    //   teardown path (no GOAWAY exchange) — keep loud so
                    //   unknown failure modes surface.
                    if !self.streams.is_empty() {
                        error!(
                            "{} TLS buffer NOT fully drained on close: \
                             pending_before={}, pending_after={}, drain_rounds={}, \
                             state={:?}, streams={}, expect_write={:?}, \
                             close_notify_sent={}, readiness={:?}",
                            log_context!(self),
                            tls_pending_before,
                            tls_pending_after,
                            drain_rounds,
                            self.state,
                            self.streams.len(),
                            self.expect_write,
                            self.close_notify_sent,
                            self.readiness
                        );
                    } else if matches!(self.state, H2State::GoAway | H2State::Error) {
                        warn!(
                            "{} TLS buffer NOT fully drained on close: \
                             pending_before={}, pending_after={}, drain_rounds={}, \
                             state={:?}, streams={}, expect_write={:?}, \
                             close_notify_sent={}, readiness={:?}",
                            log_context!(self),
                            tls_pending_before,
                            tls_pending_after,
                            drain_rounds,
                            self.state,
                            self.streams.len(),
                            self.expect_write,
                            self.close_notify_sent,
                            self.readiness
                        );
                    } else {
                        error!(
                            "{} TLS buffer NOT fully drained on close: \
                             pending_before={}, pending_after={}, drain_rounds={}, \
                             state={:?}, streams={}, expect_write={:?}, \
                             close_notify_sent={}, readiness={:?}",
                            log_context!(self),
                            tls_pending_before,
                            tls_pending_after,
                            drain_rounds,
                            self.state,
                            self.streams.len(),
                            self.expect_write,
                            self.close_notify_sent,
                            self.readiness
                        );
                    }
                }
                return;
            }
        }
        // reconnection is handled by the server for each stream separately
        for global_stream_id in self.streams.values() {
            trace!("{} end stream: {}", log_context!(self), global_stream_id);
            if let StreamState::Linked(token) = context.streams[*global_stream_id].state {
                endpoint.end_stream(token, *global_stream_id, context);
            }
        }
    }

    /// Reset a stream: tear down kawa state, emit `RST_STREAM` on the wire,
    /// and record MadeYouReset accounting.
    ///
    /// `wire_stream_id` is the on-wire `StreamId`; `stream_id` is the internal
    /// `GlobalStreamId` slot. Callers already carry both so we pass them
    /// explicitly rather than scanning `self.streams`. The wire id is threaded
    /// into [`Self::enqueue_rst`] which queues the frame for serialisation in
    /// [`Self::flush_pending_control_frames`] on the next writable tick —
    /// independent of whether the caller immediately evicts the slot via
    /// `remove_dead_stream` (which they usually do). This is what guarantees
    /// the RST reaches the peer for malformed HEADERS / flow-control /
    /// content-length violations flagged by h2spec 2.0.
    pub fn reset_stream<E, L>(
        &mut self,
        wire_stream_id: StreamId,
        stream_id: GlobalStreamId,
        context: &mut Context<L>,
        mut endpoint: E,
        error: H2Error,
    ) -> MuxResult
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        // Compute totals before taking mutable borrows on the target stream.
        let reset_byte_totals = self.compute_stream_byte_totals(context);
        context.unlink_stream(stream_id);
        let stream = &mut context.streams[stream_id];
        trace!(
            "{} reset H2 stream {}: {:#?}",
            log_context!(self),
            stream_id,
            stream.context
        );
        let old_state = std::mem::replace(&mut stream.state, StreamState::Unlinked);
        forcefully_terminate_answer(stream, &mut self.readiness, error);
        let linked_token = if let StreamState::Linked(token) = old_state {
            Some(token)
        } else {
            None
        };
        let (client_rtt, server_rtt) =
            Self::snapshot_rtts(&self.position, &self.socket, &endpoint, linked_token);
        if let Some(token) = linked_token {
            endpoint.end_stream(token, stream_id, context);
        }
        // Emit access log for server-side resets on streams that had active requests
        if self.position.is_server()
            && matches!(old_state, StreamState::Link | StreamState::Linked(_))
        {
            let stream = &mut context.streams[stream_id];
            self.distribute_overhead(&mut stream.metrics, reset_byte_totals);
            stream.metrics.backend_stop();
            stream.generate_access_log(
                true,
                Some("H2::Reset"),
                context.listener.clone(),
                client_rtt,
                server_rtt,
            );
            stream.metrics.reset();
        }
        // Queue the RST for wire emission. Independent of the owning stream
        // remaining in `self.streams` — callers typically follow this with
        // `remove_dead_stream`, which would otherwise evict the slot before
        // `write_streams` could run `kawa.prepare` against the converter.
        //
        // `enqueue_rst` performs every accounting side-effect at queue
        // time (per-error counter, global tx counter, CVE-2025-8671
        // MadeYouReset lifetime cap). Graceful `NoError` cancels —
        // stream recycle, propagated client-side cancel — are exempt
        // from the lifetime cap inside the accounting helper itself.
        if let Some(result) = self.enqueue_rst(wire_stream_id, error) {
            return result;
        }
        MuxResult::Continue
    }

    pub fn end_stream<L>(&mut self, stream_gid: GlobalStreamId, context: &mut Context<L>)
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        context.unlink_stream(stream_gid);
        let stream_context = context.http_context(stream_gid);
        trace!(
            "{} end H2 stream {}: {:#?}",
            log_context!(self),
            stream_gid,
            stream_context
        );
        match self.position {
            Position::Client(..) => {
                // Resolve the wire StreamId for this gid up front so the
                // subsequent cleanup does not hold an iterator borrow on
                // `self.streams` while also mutating it.
                let wire_stream_id = self
                    .streams
                    .iter()
                    .find_map(|(&sid, &gid)| (gid == stream_gid).then_some(sid));
                if let Some(id) = wire_stream_id {
                    // Only send RST_STREAM if the stream hasn't fully completed.
                    // If both request and response are terminated, the stream is
                    // already in "closed" state (RFC 9113 §5.1) — sending RST_STREAM
                    // on a closed stream would be a protocol error that could cause
                    // the H2 peer to close the entire connection.
                    let stream = &context.streams[stream_gid];
                    let fully_completed =
                        stream.back_received_end_of_stream && stream.front.is_terminated();
                    if !fully_completed && !self.rst_sent.contains(&id) {
                        let kawa = &mut self.zero;
                        let mut frame = [0; 13];
                        if let Ok((_, _size)) =
                            serializer::gen_rst_stream(&mut frame, id, H2Error::Cancel)
                        {
                            let buf = kawa.storage.space();
                            if buf.len() >= frame.len() {
                                buf[..frame.len()].copy_from_slice(&frame);
                                kawa.storage.fill(frame.len());
                                incr!("h2.frames.tx.rst_stream");
                                count!(metric_for_rst_stream_sent(H2Error::Cancel), 1);
                                self.readiness.arm_writable();
                                self.rst_sent.insert(id);
                            }
                        }
                    }
                    // Retire the stream and invalidate expect_write/expect_read
                    // if they still reference this gid — the slot may be popped
                    // by `shrink_trailing_recycle` on the next create_stream.
                    self.remove_dead_stream(id, stream_gid);
                    if context.streams[stream_gid].state != StreamState::Recycle {
                        context.streams[stream_gid].state = StreamState::Unlinked;
                    }
                    return;
                }
                error!(
                    "{} end_stream called for unknown global_stream_id {}",
                    log_context!(self),
                    stream_gid
                );
            }
            Position::Server => {
                let answers_rc = context.listener.borrow().get_answers().clone();
                let stream = &mut context.streams[stream_gid];
                match end_stream_decision(stream) {
                    EndStreamAction::ForwardTerminated => {
                        #[cfg(debug_assertions)]
                        context
                            .debug
                            .push(DebugEvent::Str(format!("Close terminated {stream_gid}")));
                        debug!(
                            "{} CLOSING H2 TERMINATED STREAM {} {:?}",
                            log_context!(self),
                            stream_gid,
                            stream
                        );
                        stream.state = StreamState::Unlinked;
                        self.readiness.arm_writable();
                        context.debug.set_interesting(true);
                    }
                    EndStreamAction::CloseDelimited => {
                        debug!(
                            "{} CLOSE DELIMITED H2 STREAM {} {:?}",
                            log_context!(self),
                            stream_gid,
                            stream
                        );
                        stream.back.push_block(kawa::Block::Flags(kawa::Flags {
                            end_body: true,
                            end_chunk: false,
                            end_header: false,
                            end_stream: true,
                        }));
                        stream.back.parsing_phase = kawa::ParsingPhase::Terminated;
                        stream.state = StreamState::Unlinked;
                        self.readiness.arm_writable();
                        context.debug.set_interesting(true);
                    }
                    EndStreamAction::ForwardUnterminated => {
                        #[cfg(debug_assertions)]
                        context
                            .debug
                            .push(DebugEvent::Str(format!("Close unterminated {stream_gid}")));
                        debug!(
                            "{} CLOSING H2 UNTERMINATED STREAM {} {:?}",
                            log_context!(self),
                            stream_gid,
                            stream
                        );
                        forcefully_terminate_answer(
                            stream,
                            &mut self.readiness,
                            H2Error::InternalError,
                        );
                        context.debug.set_interesting(true);
                    }
                    EndStreamAction::SendDefault(status) => {
                        #[cfg(debug_assertions)]
                        context.debug.push(DebugEvent::Str(format!(
                            "Can't retry, send {status} on {stream_gid}"
                        )));
                        let answers = answers_rc.borrow();
                        set_default_answer(stream, &mut self.readiness, status, &answers);
                    }
                    EndStreamAction::Reconnect => {
                        debug!("{} H2 RECONNECT", log_context!(self));
                        #[cfg(debug_assertions)]
                        context
                            .debug
                            .push(DebugEvent::Str(format!("Retry {stream_gid}")));
                        stream.state = StreamState::Link;
                        context.pending_links.push_back(stream_gid);
                    }
                }
            }
        }
    }

    pub fn start_stream<L>(&mut self, stream: GlobalStreamId, _context: &mut Context<L>) -> bool
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        // RFC 9113 §6.8: reject new streams on a draining connection
        if self.drain.draining {
            error!(
                "{} Cannot open new stream on draining connection (stream {})",
                log_context!(self),
                stream
            );
            return false;
        }
        // RFC 9113 §5.1.2: respect peer's max concurrent streams limit
        if self.streams.len() >= self.peer_settings.settings_max_concurrent_streams as usize {
            error!(
                "{} Cannot open new stream: active={} >= peer max_concurrent_streams={}",
                log_context!(self),
                self.streams.len(),
                self.peer_settings.settings_max_concurrent_streams
            );
            return false;
        }
        trace!(
            "{} start new H2 stream {} {:?}",
            log_context!(self),
            stream,
            self.readiness
        );
        let Some(stream_id) = self.new_stream_id() else {
            // Pass 4 Medium #5: the client-initiated stream-ID space
            // (31 bits, odd only) is exhausted. The backend is now useless
            // for new requests — gracefully drain it. Without this
            // transition, the Connection lingers in `Connected` state and
            // every subsequent request returns 503 because `start_stream`
            // keeps returning false.
            //
            // The session envelope is hoisted to a local because the
            // `match &mut self.position` below holds a mutable borrow on
            // `self.position`, and `log_context!(self)` reads that field
            // for its `position={...}` slot — calling the macro inside the
            // match arms would conflict with the active borrow. The
            // bidirectional regression guard in `lib/tests/log_layout.rs`
            // (and the matching scanner in `lib/build.rs`) recognises this
            // shape by scanning backward as well as forward from each log
            // call.
            let context = log_context!(self);
            match &mut self.position {
                Position::Client(cluster_id, backend, status) => {
                    let backend_addr = backend.borrow().address;
                    let cluster = cluster_id.clone();
                    info!(
                        "{} H2 backend stream IDs exhausted (cluster={}, backend={:?}) — draining",
                        context, cluster, backend_addr
                    );
                    *status = BackendStatus::Disconnecting;
                }
                Position::Server => {
                    error!(
                        "{} H2 server stream IDs exhausted — sending graceful GOAWAY",
                        context
                    );
                }
            }
            self.graceful_goaway();
            return false;
        };
        self.streams.insert(stream_id, stream);
        self.stream_last_activity_at
            .insert(stream_id, Instant::now());
        self.readiness.arm_writable();
        true
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use super::*;
    use crate::{pool::Pool, protocol::kawa_h1::editor::HttpContext};

    // ── H2FloodDetector ──────────────────────────────────────────────────

    #[test]
    fn test_flood_detector_no_flood_below_threshold() {
        let config = H2FloodConfig::default();
        let mut detector = H2FloodDetector::new(config);

        // All counters at zero -> no flood
        assert!(detector.check_flood().is_none());

        // Increment each counter to exactly the threshold (not exceeding)
        detector.rst_stream_count = config.max_rst_stream_per_window;
        detector.ping_count = config.max_ping_per_window;
        detector.settings_count = config.max_settings_per_window;
        detector.empty_data_count = config.max_empty_data_per_window;
        detector.continuation_count = config.max_continuation_frames;
        detector.glitch_count = config.max_glitch_count;
        // At threshold but not exceeding -> no flood
        assert!(detector.check_flood().is_none());
    }

    #[test]
    fn test_flood_detector_detects_rapid_reset() {
        let config = H2FloodConfig::default();
        let mut detector = H2FloodDetector::new(config);

        detector.rst_stream_count = config.max_rst_stream_per_window + 1;
        assert!(matches!(
            detector.check_flood(),
            Some(H2FloodViolation {
                error: H2Error::EnhanceYourCalm,
                ..
            })
        ));
    }

    #[test]
    fn test_flood_detector_detects_ping_flood() {
        let config = H2FloodConfig::default();
        let mut detector = H2FloodDetector::new(config);

        detector.ping_count = config.max_ping_per_window + 1;
        assert!(matches!(
            detector.check_flood(),
            Some(H2FloodViolation {
                error: H2Error::EnhanceYourCalm,
                ..
            })
        ));
    }

    #[test]
    fn test_flood_detector_detects_settings_flood() {
        let config = H2FloodConfig::default();
        let mut detector = H2FloodDetector::new(config);

        detector.settings_count = config.max_settings_per_window + 1;
        assert!(matches!(
            detector.check_flood(),
            Some(H2FloodViolation {
                error: H2Error::EnhanceYourCalm,
                ..
            })
        ));
    }

    #[test]
    fn test_flood_detector_detects_empty_data_flood() {
        let config = H2FloodConfig::default();
        let mut detector = H2FloodDetector::new(config);

        detector.empty_data_count = config.max_empty_data_per_window + 1;
        assert!(matches!(
            detector.check_flood(),
            Some(H2FloodViolation {
                error: H2Error::EnhanceYourCalm,
                ..
            })
        ));
    }

    #[test]
    fn test_flood_detector_detects_continuation_flood() {
        let config = H2FloodConfig::default();
        let mut detector = H2FloodDetector::new(config);

        detector.continuation_count = config.max_continuation_frames + 1;
        assert!(matches!(
            detector.check_flood(),
            Some(H2FloodViolation {
                error: H2Error::EnhanceYourCalm,
                ..
            })
        ));
    }

    #[test]
    fn test_flood_detector_detects_header_size_flood() {
        let config = H2FloodConfig::default();
        let mut detector = H2FloodDetector::new(config);

        detector.accumulated_header_size = MAX_HEADER_LIST_SIZE as u32 + 1;
        assert!(matches!(
            detector.check_flood(),
            Some(H2FloodViolation {
                error: H2Error::EnhanceYourCalm,
                ..
            })
        ));
    }

    #[test]
    fn test_flood_detector_detects_glitch_flood() {
        let config = H2FloodConfig::default();
        let mut detector = H2FloodDetector::new(config);

        detector.glitch_count = config.max_glitch_count + 1;
        assert!(matches!(
            detector.check_flood(),
            Some(H2FloodViolation {
                error: H2Error::EnhanceYourCalm,
                ..
            })
        ));
    }

    #[test]
    fn test_flood_detector_custom_thresholds() {
        let config = H2FloodConfig {
            max_rst_stream_per_window: 5,
            max_ping_per_window: 10,
            max_settings_per_window: 3,
            max_empty_data_per_window: 8,
            max_continuation_frames: 2,
            max_glitch_count: 15,
            ..H2FloodConfig::default()
        };
        let mut detector = H2FloodDetector::new(config);

        // Below custom threshold -> no flood
        detector.rst_stream_count = 5;
        assert!(detector.check_flood().is_none());

        // Above custom threshold -> flood
        detector.rst_stream_count = 6;
        assert!(matches!(
            detector.check_flood(),
            Some(H2FloodViolation {
                error: H2Error::EnhanceYourCalm,
                ..
            })
        ));
    }

    #[test]
    fn test_flood_detector_reset_continuation() {
        let config = H2FloodConfig::default();
        let mut detector = H2FloodDetector::new(config);

        detector.continuation_count = 15;
        detector.accumulated_header_size = 30000;

        detector.reset_continuation();

        assert_eq!(detector.continuation_count, 0);
        assert_eq!(detector.accumulated_header_size, 0);
    }

    #[test]
    fn test_flood_detector_half_decay_on_window_expiry() {
        let config = H2FloodConfig::default();
        let mut detector = H2FloodDetector::new(config);

        detector.rst_stream_count = 80;
        detector.ping_count = 60;
        detector.settings_count = 40;
        detector.empty_data_count = 20;
        detector.window_update_stream0_count = 90;
        detector.glitch_count = 50;

        // Force window expiry by setting window_start to the past
        detector.window_start = Instant::now() - FLOOD_WINDOW_DURATION;

        // check_flood calls maybe_reset_window which halves counters
        let _ = detector.check_flood();

        assert_eq!(detector.rst_stream_count, 40);
        assert_eq!(detector.ping_count, 30);
        assert_eq!(detector.settings_count, 20);
        assert_eq!(detector.empty_data_count, 10);
        assert_eq!(detector.window_update_stream0_count, 45);
        assert_eq!(detector.glitch_count, 25);
    }

    #[test]
    fn test_flood_detector_window_update_stream0_trips_at_threshold() {
        let config = H2FloodConfig {
            max_window_update_stream0_per_window: 5,
            ..H2FloodConfig::default()
        };
        let mut detector = H2FloodDetector::new(config);

        // At threshold — no flood yet (strict greater-than, matches existing counters).
        detector.window_update_stream0_count = 5;
        assert!(detector.check_flood().is_none());

        // Above threshold — flood with the correct violation reason + metric key.
        detector.window_update_stream0_count = 6;
        let violation = detector
            .check_flood()
            .expect("WINDOW_UPDATE stream-0 flood must trip above threshold");
        assert_eq!(violation.error, H2Error::EnhanceYourCalm);
        assert_eq!(violation.reason, "WINDOW_UPDATE stream 0");
        assert_eq!(
            violation.metric_key,
            "h2.flood.violation.window_update_stream0_window"
        );
        assert_eq!(violation.count, 6);
        assert_eq!(violation.threshold, 5);
    }

    #[test]
    fn test_flood_detector_window_update_stream0_honours_default() {
        // Default threshold must match the documented constant so operators
        // can reason about behaviour without reading code.
        let detector = H2FloodDetector::default();
        assert_eq!(
            detector.config.max_window_update_stream0_per_window,
            DEFAULT_MAX_WINDOW_UPDATE_STREAM0_PER_WINDOW
        );
        assert_eq!(detector.window_update_stream0_count, 0);
    }

    #[test]
    fn test_flood_detector_decay_prevents_flood() {
        let config = H2FloodConfig {
            max_rst_stream_per_window: 10,
            ..H2FloodConfig::default()
        };
        let mut detector = H2FloodDetector::new(config);

        // Set counter just above threshold
        detector.rst_stream_count = 12;

        // Without decay -> flood
        assert!(matches!(
            detector.check_flood(),
            Some(H2FloodViolation {
                error: H2Error::EnhanceYourCalm,
                ..
            })
        ));

        // Reset and simulate window expiry
        detector.rst_stream_count = 12;
        detector.window_start = Instant::now() - FLOOD_WINDOW_DURATION;

        // After decay: 12/2 = 6, which is below threshold 10 -> no flood
        assert!(detector.check_flood().is_none());
    }

    #[test]
    fn test_flood_detector_lifetime_rst_cap_triggers_enhance_your_calm() {
        // CVE-2023-44487 Rapid Reset: a patient attacker that stays under
        // the half-decaying per-window threshold must still be stopped by
        // the lifetime cap. Simulate a response-started RST (no abusive
        // counter bump) so only the lifetime ceiling is tested.
        let mut detector = H2FloodDetector::default();
        for _ in 0..DEFAULT_MAX_RST_STREAM_LIFETIME {
            assert!(detector.record_rst_lifetime(true).is_none());
        }
        assert_eq!(
            detector.total_rst_received_lifetime,
            DEFAULT_MAX_RST_STREAM_LIFETIME
        );
        assert_eq!(detector.total_abusive_rst_received_lifetime, 0);
        // Next RST crosses the ceiling.
        assert!(matches!(
            detector.record_rst_lifetime(true),
            Some(H2FloodViolation {
                error: H2Error::EnhanceYourCalm,
                ..
            })
        ));
    }

    #[test]
    fn test_flood_detector_abusive_rst_cap_triggers_first() {
        // Pre-response-start RSTs have a much lower ceiling; they trip
        // well before the generic lifetime cap.
        let mut detector = H2FloodDetector::default();
        for _ in 0..DEFAULT_MAX_RST_STREAM_ABUSIVE_LIFETIME {
            assert!(detector.record_rst_lifetime(false).is_none());
        }
        assert_eq!(
            detector.total_abusive_rst_received_lifetime,
            DEFAULT_MAX_RST_STREAM_ABUSIVE_LIFETIME
        );
        assert!(matches!(
            detector.record_rst_lifetime(false),
            Some(H2FloodViolation {
                error: H2Error::EnhanceYourCalm,
                ..
            })
        ));
    }

    #[test]
    fn test_flood_detector_emitted_rst_below_threshold_is_clean() {
        // Server may legitimately RST some streams (protocol errors,
        // client-side abuse caught by other mitigations). Staying at the
        // threshold must not trip the ceiling.
        let mut detector = H2FloodDetector::default();
        for _ in 0..DEFAULT_MAX_RST_STREAM_EMITTED_LIFETIME {
            assert!(detector.record_rst_emitted().is_none());
        }
        assert_eq!(
            detector.total_rst_streams_emitted_lifetime,
            DEFAULT_MAX_RST_STREAM_EMITTED_LIFETIME
        );
    }

    #[test]
    fn test_flood_detector_emitted_rst_cap_triggers_made_you_reset() {
        // CVE-2025-8671 MadeYouReset: unbounded server-emitted RST_STREAM is
        // a DoS vector equivalent to Rapid Reset with the emission direction
        // flipped. Crossing the ceiling must surface a EnhanceYourCalm
        // violation so the caller can GOAWAY.
        let mut detector = H2FloodDetector::default();
        for _ in 0..DEFAULT_MAX_RST_STREAM_EMITTED_LIFETIME {
            assert!(detector.record_rst_emitted().is_none());
        }
        let violation = detector
            .record_rst_emitted()
            .expect("emitting past the cap should produce a violation");
        assert!(matches!(
            violation,
            H2FloodViolation {
                error: H2Error::EnhanceYourCalm,
                reason: "MadeYouReset: lifetime server-emitted RST_STREAM",
                ..
            }
        ));
        assert_eq!(violation.count, DEFAULT_MAX_RST_STREAM_EMITTED_LIFETIME + 1);
        assert_eq!(violation.threshold, DEFAULT_MAX_RST_STREAM_EMITTED_LIFETIME);
    }

    #[test]
    fn test_flood_detector_emitted_rst_counter_does_not_decay() {
        // Unlike the windowed rst_stream_count, the emitted lifetime counter
        // is strictly monotonic — a patient attacker cannot reset it by
        // waiting out a window. maybe_reset_window must NOT touch it.
        let mut detector = H2FloodDetector::default();
        for _ in 0..10 {
            detector.record_rst_emitted();
        }
        detector.window_start = Instant::now() - FLOOD_WINDOW_DURATION;
        // Force a window reset through check_flood.
        let _ = detector.check_flood();
        assert_eq!(detector.total_rst_streams_emitted_lifetime, 10);
    }

    /// Every violation kind must carry a metric_key under the agreed
    /// `h2.flood.violation.*` namespace, and the keys must be unique. The
    /// statsd counter at `handle_flood_violation` reads `violation.metric_key`
    /// directly — drift between the construction site and the metric name
    /// would silently lose alerting on a CVE mitigation.
    #[test]
    fn test_flood_violation_metric_keys_are_unique_and_namespaced() {
        // Helper: run `record_rst_lifetime` until it trips, returning the metric_key.
        fn key_from_rst_lifetime(response_started: bool) -> &'static str {
            let mut detector = H2FloodDetector::default();
            loop {
                if let Some(v) = detector.record_rst_lifetime(response_started) {
                    return v.metric_key;
                }
            }
        }

        // Helper: run `record_rst_emitted` until it trips, returning the metric_key.
        fn key_from_rst_emitted() -> &'static str {
            let mut detector = H2FloodDetector::default();
            loop {
                if let Some(v) = detector.record_rst_emitted() {
                    return v.metric_key;
                }
            }
        }

        // Helper: drive a single `check_flood` counter past its threshold.
        fn key_from_check_flood(setup: impl FnOnce(&mut H2FloodDetector)) -> &'static str {
            let mut detector = H2FloodDetector::default();
            setup(&mut detector);
            detector
                .check_flood()
                .expect("setup should always trip a flood")
                .metric_key
        }

        let keys: [&'static str; 12] = [
            // Lifetime methods on the detector itself.
            key_from_rst_lifetime(true),
            key_from_rst_lifetime(false),
            key_from_rst_emitted(),
            // `check_flood` arms.
            key_from_check_flood(|d| d.rst_stream_count = u32::MAX),
            key_from_check_flood(|d| d.ping_count = u32::MAX),
            key_from_check_flood(|d| d.total_ping_received_lifetime = u32::MAX),
            key_from_check_flood(|d| d.settings_count = u32::MAX),
            key_from_check_flood(|d| d.total_settings_received_lifetime = u32::MAX),
            key_from_check_flood(|d| d.empty_data_count = u32::MAX),
            key_from_check_flood(|d| d.continuation_count = u32::MAX),
            key_from_check_flood(|d| d.accumulated_header_size = u32::MAX),
            key_from_check_flood(|d| d.glitch_count = u32::MAX),
        ];

        for key in keys {
            assert!(
                key.starts_with("h2.flood.violation."),
                "metric key {key} is missing the h2.flood.violation. prefix",
            );
        }
        let mut deduped = keys.to_vec();
        deduped.sort_unstable();
        deduped.dedup();
        assert_eq!(
            deduped.len(),
            keys.len(),
            "metric keys must be unique across violation kinds; collisions: {keys:?}",
        );
    }

    /// All four `metric_for_*` helpers must yield distinct, namespaced keys for
    /// every RFC 9113 §7 error code. The macro behind them uses `concat!`, so a
    /// new H2Error variant fails the build inside the macro — but a typo in
    /// the helper prefix would silently land. Walk every (direction × kind)
    /// pair and dedupe the set.
    /// `h2_frame_rx_metric_key` must yield a distinct `&'static str` per
    /// `Frame::*` variant. The single dispatch site in `handle_frame` reads
    /// from this helper, so a typo or duplicate would silently clobber the
    /// frame-mix dashboard. Asserting the literal set lets us compare against
    /// `doc/configure.md` and the RFC 9113 §6 frame catalogue without
    /// reconstructing every Frame variant in the test.
    #[test]
    fn test_h2_frame_rx_metric_keys_are_unique_and_namespaced() {
        // Update this list whenever a new Frame variant is added — the helper
        // match is also exhaustive, so the build will already break there
        // before anyone notices the test missing a key.
        let expected: [&'static str; 11] = [
            "h2.frames.rx.data",
            "h2.frames.rx.headers",
            "h2.frames.rx.push_promise",
            "h2.frames.rx.priority",
            "h2.frames.rx.rst_stream",
            "h2.frames.rx.settings",
            "h2.frames.rx.ping",
            "h2.frames.rx.goaway",
            "h2.frames.rx.window_update",
            "h2.frames.rx.continuation",
            "h2.frames.rx.unknown",
        ];

        for key in expected {
            assert!(
                key.starts_with("h2.frames.rx."),
                "metric key {key} is missing the h2.frames.rx. prefix",
            );
        }
        let mut deduped = expected.to_vec();
        deduped.sort_unstable();
        deduped.dedup();
        assert_eq!(
            deduped.len(),
            expected.len(),
            "frame-rx metric keys must be unique; collisions in: {expected:?}",
        );

        // Spot-check the helper for the one variant we can construct without
        // borrowing into a frame body — `Frame::Unknown(u8)` is just a tag.
        assert_eq!(
            h2_frame_rx_metric_key(&Frame::Unknown(42)),
            "h2.frames.rx.unknown",
        );
    }

    #[test]
    fn test_per_error_code_metric_keys_are_unique_and_namespaced() {
        const ALL_ERRORS: [H2Error; 14] = [
            H2Error::NoError,
            H2Error::ProtocolError,
            H2Error::InternalError,
            H2Error::FlowControlError,
            H2Error::SettingsTimeout,
            H2Error::StreamClosed,
            H2Error::FrameSizeError,
            H2Error::RefusedStream,
            H2Error::Cancel,
            H2Error::CompressionError,
            H2Error::ConnectError,
            H2Error::EnhanceYourCalm,
            H2Error::InadequateSecurity,
            H2Error::HTTP11Required,
        ];

        let mut keys: Vec<&'static str> = Vec::new();
        for error in ALL_ERRORS {
            let code = error as u32;
            keys.push(metric_for_goaway_sent(error));
            keys.push(metric_for_goaway_received(code));
            keys.push(metric_for_rst_stream_sent(error));
            keys.push(metric_for_rst_stream_received(code));
        }
        // …plus the four `unknown_error` fallbacks for codes outside RFC 9113 §7.
        let unknown_code = 0xff;
        assert!(H2Error::try_from(unknown_code).is_err());
        keys.push(metric_for_goaway_received(unknown_code));
        keys.push(metric_for_rst_stream_received(unknown_code));
        // …and the dedicated Rapid Reset signature counter.
        keys.push("h2.rst_stream.received.pre_response_start");

        for key in &keys {
            assert!(
                key.starts_with("h2.goaway.sent.")
                    || key.starts_with("h2.goaway.received.")
                    || key.starts_with("h2.rst_stream.sent.")
                    || key.starts_with("h2.rst_stream.received."),
                "metric key {key} does not match a known per-error-code namespace",
            );
        }
        let mut deduped = keys.clone();
        deduped.sort_unstable();
        deduped.dedup();
        assert_eq!(
            deduped.len(),
            keys.len(),
            "per-error-code metric keys must be unique; collisions in: {keys:?}",
        );
    }

    #[test]
    fn test_flood_detector_response_started_rst_not_abusive() {
        // When the backend response has begun, the RST is cheap for us
        // too — it only bumps the generic lifetime counter.
        let mut detector = H2FloodDetector::default();
        for _ in 0..(DEFAULT_MAX_RST_STREAM_ABUSIVE_LIFETIME + 100) {
            assert!(detector.record_rst_lifetime(true).is_none());
        }
        assert_eq!(detector.total_abusive_rst_received_lifetime, 0);
        assert_eq!(
            detector.total_rst_received_lifetime,
            DEFAULT_MAX_RST_STREAM_ABUSIVE_LIFETIME + 100
        );
    }

    #[test]
    fn test_flood_detector_default_matches_new_default() {
        let from_default = H2FloodDetector::default();
        let from_new = H2FloodDetector::new(H2FloodConfig::default());

        assert_eq!(from_default.rst_stream_count, from_new.rst_stream_count);
        assert_eq!(from_default.ping_count, from_new.ping_count);
        assert_eq!(from_default.settings_count, from_new.settings_count);
        assert_eq!(from_default.empty_data_count, from_new.empty_data_count);
        assert_eq!(from_default.continuation_count, from_new.continuation_count);
        assert_eq!(
            from_default.accumulated_header_size,
            from_new.accumulated_header_size
        );
        assert_eq!(from_default.glitch_count, from_new.glitch_count);
        assert_eq!(from_default.config, from_new.config);
    }

    // ── Prioriser ────────────────────────────────────────────────────────

    #[test]
    fn test_prioriser_defaults_for_unknown_stream() {
        let p = Prioriser::default();
        // Unknown stream -> RFC 9218 defaults: urgency 3, incremental false
        assert_eq!(p.get(&1), (3, false));
        assert_eq!(p.get(&999), (3, false));
    }

    #[test]
    fn test_prioriser_push_rfc9218_and_get() {
        let mut p = Prioriser::default();

        let invalid = p.push_priority(
            1,
            parser::PriorityPart::Rfc9218 {
                urgency: 0,
                incremental: true,
            },
        );
        assert!(!invalid);
        assert_eq!(p.get(&1), (0, true));

        let invalid = p.push_priority(
            3,
            parser::PriorityPart::Rfc9218 {
                urgency: 7,
                incremental: false,
            },
        );
        assert!(!invalid);
        assert_eq!(p.get(&3), (7, false));
    }

    #[test]
    fn test_prioriser_urgency_clamped_to_7() {
        let mut p = Prioriser::default();

        p.push_priority(
            1,
            parser::PriorityPart::Rfc9218 {
                urgency: 255,
                incremental: false,
            },
        );
        assert_eq!(p.get(&1), (7, false));
    }

    #[test]
    fn test_prioriser_update_priority() {
        let mut p = Prioriser::default();

        p.push_priority(
            1,
            parser::PriorityPart::Rfc9218 {
                urgency: 3,
                incremental: false,
            },
        );
        assert_eq!(p.get(&1), (3, false));

        // Update same stream
        p.push_priority(
            1,
            parser::PriorityPart::Rfc9218 {
                urgency: 1,
                incremental: true,
            },
        );
        assert_eq!(p.get(&1), (1, true));
    }

    #[test]
    fn test_prioriser_remove() {
        let mut p = Prioriser::default();

        p.push_priority(
            1,
            parser::PriorityPart::Rfc9218 {
                urgency: 0,
                incremental: true,
            },
        );
        assert_eq!(p.get(&1), (0, true));

        p.remove(&1);
        // After removal, falls back to defaults
        assert_eq!(p.get(&1), (3, false));
    }

    #[test]
    fn test_prioriser_rfc7540_self_dependency() {
        let mut p = Prioriser::default();

        // Self-dependency should return true (invalid)
        let invalid = p.push_priority(
            5,
            parser::PriorityPart::Rfc7540 {
                stream_dependency: parser::StreamDependency {
                    exclusive: false,
                    stream_id: 5, // same as stream_id
                },
                weight: 16,
            },
        );
        assert!(invalid);
    }

    #[test]
    fn test_prioriser_rfc7540_valid_dependency() {
        let mut p = Prioriser::default();

        // Non-self dependency is valid (but ignored for scheduling)
        let invalid = p.push_priority(
            5,
            parser::PriorityPart::Rfc7540 {
                stream_dependency: parser::StreamDependency {
                    exclusive: false,
                    stream_id: 3, // different stream
                },
                weight: 16,
            },
        );
        assert!(!invalid);
        // Still returns defaults since RFC 7540 priority is ignored
        assert_eq!(p.get(&5), (3, false));
    }

    #[test]
    fn test_prioriser_max_entries_cap() {
        let mut p = Prioriser::default();

        // Fill up to MAX_PRIORITIES
        for i in 0..MAX_PRIORITIES as u32 {
            let stream_id = i * 2 + 1; // odd stream IDs
            p.push_priority(
                stream_id,
                parser::PriorityPart::Rfc9218 {
                    urgency: (i % 8) as u8,
                    incremental: false,
                },
            );
        }

        // Next insert for a new stream should be silently rejected
        let next_id = (MAX_PRIORITIES as u32) * 2 + 1;
        let invalid = p.push_priority(
            next_id,
            parser::PriorityPart::Rfc9218 {
                urgency: 0,
                incremental: true,
            },
        );
        assert!(!invalid); // not a protocol error, just silently dropped
        assert_eq!(p.get(&next_id), (3, false)); // defaults, not stored
    }

    #[test]
    fn test_prioriser_update_existing_at_cap() {
        let mut p = Prioriser::default();

        // Fill to cap
        for i in 0..MAX_PRIORITIES as u32 {
            p.push_priority(
                i * 2 + 1,
                parser::PriorityPart::Rfc9218 {
                    urgency: 3,
                    incremental: false,
                },
            );
        }

        // Updating an existing entry should still work even at cap
        p.push_priority(
            1,
            parser::PriorityPart::Rfc9218 {
                urgency: 0,
                incremental: true,
            },
        );
        assert_eq!(p.get(&1), (0, true));
    }

    #[test]
    fn test_prioriser_guarded_accepts_open_stream() {
        let mut p = Prioriser::default();
        let mut open: HashMap<StreamId, GlobalStreamId> = HashMap::new();
        open.insert(3, 0);
        let invalid = p.push_priority_guarded(
            3,
            parser::PriorityPart::Rfc9218 {
                urgency: 1,
                incremental: false,
            },
            7,
            &open,
        );
        assert!(!invalid);
        assert_eq!(p.get(&3), (1, false));
    }

    #[test]
    fn test_prioriser_guarded_accepts_idle_lookahead() {
        let mut p = Prioriser::default();
        let open: HashMap<StreamId, GlobalStreamId> = HashMap::new();
        // Just ahead of last_stream_id, within PRIORITY_IDLE_LOOKAHEAD.
        let invalid = p.push_priority_guarded(
            105,
            parser::PriorityPart::Rfc9218 {
                urgency: 2,
                incremental: true,
            },
            99,
            &open,
        );
        assert!(!invalid);
        assert_eq!(p.get(&105), (2, true));
    }

    #[test]
    fn test_prioriser_guarded_drops_far_future_stream() {
        let mut p = Prioriser::default();
        let open: HashMap<StreamId, GlobalStreamId> = HashMap::new();
        // Beyond the 64-slot lookahead window.
        let invalid = p.push_priority_guarded(
            1_000_001,
            parser::PriorityPart::Rfc9218 {
                urgency: 0,
                incremental: false,
            },
            3,
            &open,
        );
        assert!(!invalid); // not a protocol error, just dropped
        // Default priority returned — no entry stored.
        assert_eq!(p.get(&1_000_001), (DEFAULT_URGENCY, false));
    }

    #[test]
    fn test_prioriser_guarded_drops_closed_past_stream() {
        let mut p = Prioriser::default();
        let open: HashMap<StreamId, GlobalStreamId> = HashMap::new();
        // Past the counter and not open = already closed. Drop.
        let invalid = p.push_priority_guarded(
            3,
            parser::PriorityPart::Rfc9218 {
                urgency: 5,
                incremental: false,
            },
            99,
            &open,
        );
        assert!(!invalid);
        assert_eq!(p.get(&3), (DEFAULT_URGENCY, false));
    }

    #[test]
    fn test_prioriser_guarded_cannot_flood_with_far_ids() {
        // Previously an attacker could pack MAX_PRIORITIES entries by picking
        // far-future stream IDs. The guard rejects them before the cap helps.
        let mut p = Prioriser::default();
        let open: HashMap<StreamId, GlobalStreamId> = HashMap::new();
        for delta in 10_000..(10_000 + MAX_PRIORITIES as u32) {
            p.push_priority_guarded(
                delta,
                parser::PriorityPart::Rfc9218 {
                    urgency: 0,
                    incremental: false,
                },
                0,
                &open,
            );
        }
        assert_eq!(p.priorities.len(), 0);
    }

    // ── RFC 9218 §4 round-robin rotation ───────────────────────────────

    /// Helper: mark `stream_id` as (urgency, incremental) in the map.
    fn set_prio(p: &mut Prioriser, stream_id: StreamId, urgency: u8, incremental: bool) {
        p.push_priority(
            stream_id,
            parser::PriorityPart::Rfc9218 {
                urgency,
                incremental,
            },
        );
    }

    #[test]
    fn test_apply_incremental_rotation_all_non_incremental_is_noop() {
        // Non-incremental streams keep the existing (urgency, stream_id) sort.
        let mut p = Prioriser::default();
        set_prio(&mut p, 1, 3, false);
        set_prio(&mut p, 3, 3, false);
        set_prio(&mut p, 5, 3, false);

        let mut buf = vec![1u32, 3, 5];
        let count = p.apply_incremental_rotation(&mut buf);
        assert_eq!(count, 0);
        assert_eq!(buf, vec![1, 3, 5]);
    }

    #[test]
    fn test_apply_incremental_rotation_moves_incremental_to_tail() {
        // Within a same-urgency bucket non-incremental must come before
        // incremental, each subrange staying ascending.
        let mut p = Prioriser::default();
        set_prio(&mut p, 1, 3, true);
        set_prio(&mut p, 3, 3, false);
        set_prio(&mut p, 5, 3, true);
        set_prio(&mut p, 7, 3, false);

        let mut buf = vec![1u32, 3, 5, 7];
        let count = p.apply_incremental_rotation(&mut buf);
        assert_eq!(count, 2);
        // Non-incremental first (3, 7), then incremental (1, 5) — ascending
        // within each subrange before the cursor rotation.
        assert_eq!(buf, vec![3, 7, 1, 5]);
    }

    #[test]
    fn test_apply_incremental_rotation_respects_urgency_buckets() {
        // Different urgency buckets must not be mixed.
        let mut p = Prioriser::default();
        set_prio(&mut p, 1, 0, true); // urgent incremental
        set_prio(&mut p, 3, 3, false); // default non-incremental
        set_prio(&mut p, 5, 3, true); // default incremental
        set_prio(&mut p, 7, 5, false); // low-priority non-incremental

        // Input is pre-sorted by (urgency, id) as the scheduler does.
        let mut buf = vec![1u32, 3, 5, 7];
        let count = p.apply_incremental_rotation(&mut buf);
        assert_eq!(count, 2);
        // Bucket 0: [1] (alone, stays). Bucket 3: [3] non-inc, [5] inc.
        // Bucket 5: [7] alone. Cross-bucket order is preserved.
        assert_eq!(buf, vec![1, 3, 5, 7]);
    }

    #[test]
    fn test_apply_incremental_rotation_rotates_by_cursor() {
        // Three same-urgency incremental streams: cursor advancement shifts
        // the bucket so the next pass starts after the previously fired ID.
        let mut p = Prioriser::default();
        set_prio(&mut p, 1, 3, true);
        set_prio(&mut p, 3, 3, true);
        set_prio(&mut p, 5, 3, true);

        let base = vec![1u32, 3, 5];

        // Pass 1: cursor is 0 (initial), so order stays 1, 3, 5.
        let mut buf = base.clone();
        assert_eq!(p.apply_incremental_rotation(&mut buf), 3);
        assert_eq!(buf, vec![1, 3, 5]);
        p.advance_incremental_cursor(Some(1));

        // Pass 2: cursor is 1, rotate so 3 comes first.
        let mut buf = base.clone();
        assert_eq!(p.apply_incremental_rotation(&mut buf), 3);
        assert_eq!(buf, vec![3, 5, 1]);
        p.advance_incremental_cursor(Some(3));

        // Pass 3: cursor is 3, rotate so 5 comes first.
        let mut buf = base.clone();
        assert_eq!(p.apply_incremental_rotation(&mut buf), 3);
        assert_eq!(buf, vec![5, 1, 3]);
        p.advance_incremental_cursor(Some(5));

        // Pass 4: cursor is 5 (largest in bucket), wrap to 1.
        let mut buf = base;
        assert_eq!(p.apply_incremental_rotation(&mut buf), 3);
        assert_eq!(buf, vec![1, 3, 5]);
    }

    #[test]
    fn test_apply_incremental_rotation_cursor_unknown_id() {
        // Cursor points at an ID no longer active (stream completed). Rotation
        // should still start from the smallest ID greater than the cursor.
        let mut p = Prioriser::default();
        set_prio(&mut p, 3, 3, true);
        set_prio(&mut p, 5, 3, true);
        set_prio(&mut p, 7, 3, true);
        p.advance_incremental_cursor(Some(4)); // 4 is not in the bucket

        let mut buf = vec![3u32, 5, 7];
        assert_eq!(p.apply_incremental_rotation(&mut buf), 3);
        assert_eq!(buf, vec![5, 7, 3]);
    }

    #[test]
    fn test_apply_incremental_rotation_single_stream_buckets() {
        // Single-stream buckets are a degenerate fast path: no reordering.
        let mut p = Prioriser::default();
        set_prio(&mut p, 1, 1, true);
        set_prio(&mut p, 3, 2, false);
        set_prio(&mut p, 5, 3, true);

        let mut buf = vec![1u32, 3, 5];
        let count = p.apply_incremental_rotation(&mut buf);
        assert_eq!(count, 2);
        assert_eq!(buf, vec![1, 3, 5]);
    }

    #[test]
    fn test_advance_incremental_cursor_none_is_noop() {
        // If no incremental stream fires (only non-incremental served), the
        // cursor must stay put so fairness is preserved for the next pass.
        let mut p = Prioriser::default();
        p.advance_incremental_cursor(Some(5));
        p.advance_incremental_cursor(None);
        assert_eq!(p.incremental_cursor, 5);
    }

    #[test]
    fn test_apply_incremental_rotation_mixed_bucket_with_cursor() {
        // Same-urgency bucket with a mix: non-inc served first in ascending
        // order, then the incremental tail rotated by cursor.
        let mut p = Prioriser::default();
        set_prio(&mut p, 1, 3, true);
        set_prio(&mut p, 3, 3, false);
        set_prio(&mut p, 5, 3, true);
        set_prio(&mut p, 7, 3, false);
        set_prio(&mut p, 9, 3, true);
        p.advance_incremental_cursor(Some(5));

        let mut buf = vec![1u32, 3, 5, 7, 9];
        let count = p.apply_incremental_rotation(&mut buf);
        assert_eq!(count, 3);
        // Non-inc (3, 7) first, then incremental rotated: cursor 5 means
        // next-after-5 = 9, then 1, then 5 (wrap).
        assert_eq!(buf, vec![3, 7, 9, 1, 5]);
    }

    // ── H2FlowControl ───────────────────────────────────────────────────

    #[test]
    fn test_flow_control_initial_state() {
        let fc = H2FlowControl {
            window: DEFAULT_INITIAL_WINDOW_SIZE as i32,
            received_bytes_since_update: 0,
            pending_window_updates: HashMap::new(),
        };
        assert_eq!(fc.window, 65535);
        assert_eq!(fc.received_bytes_since_update, 0);
        assert!(fc.pending_window_updates.is_empty());
    }

    #[test]
    fn test_flow_control_window_update_coalescing() {
        let mut updates: HashMap<u32, u32> = HashMap::new();

        // First update for stream 1
        updates.insert(1, 1000);
        assert_eq!(*updates.get(&1).unwrap(), 1000);

        // Coalesce second update for same stream
        if let Some(existing) = updates.get_mut(&1) {
            *existing = existing.saturating_add(500).min(i32::MAX as u32);
        }
        assert_eq!(*updates.get(&1).unwrap(), 1500);

        // Different stream gets its own entry
        updates.insert(3, 2000);
        assert_eq!(updates.len(), 2);
        assert_eq!(*updates.get(&3).unwrap(), 2000);
    }

    #[test]
    fn test_flow_control_window_update_saturation() {
        let mut updates: HashMap<u32, u32> = HashMap::new();

        // Insert near max and coalesce — should saturate to i32::MAX
        let max_increment = i32::MAX as u32;
        updates.insert(1, max_increment - 100);
        if let Some(existing) = updates.get_mut(&1) {
            *existing = existing.saturating_add(200).min(max_increment);
        }
        assert_eq!(*updates.get(&1).unwrap(), max_increment);
    }

    #[test]
    fn test_flow_control_connection_window_can_go_negative() {
        // RFC 9113 §6.9.2: connection-level window can go negative
        let mut fc = H2FlowControl {
            window: 100,
            received_bytes_since_update: 0,
            pending_window_updates: HashMap::new(),
        };

        // Simulate consuming more than available
        fc.window -= 200;
        assert_eq!(fc.window, -100);
    }

    // ── H2FloodConfig ───────────────────────────────────────────────────

    #[test]
    fn test_flood_config_default_values() {
        let config = H2FloodConfig::default();
        assert_eq!(config.max_rst_stream_per_window, 100);
        assert_eq!(config.max_ping_per_window, 100);
        assert_eq!(config.max_settings_per_window, 50);
        assert_eq!(config.max_empty_data_per_window, 100);
        assert_eq!(config.max_continuation_frames, 20);
        assert_eq!(config.max_glitch_count, 100);
        assert_eq!(config.max_rst_stream_lifetime, 10_000);
        assert_eq!(config.max_rst_stream_abusive_lifetime, 50);
        assert_eq!(config.max_header_list_size, MAX_HEADER_LIST_SIZE as u32);
    }

    // ── distribute_overhead ─────────────────────────────────────────────

    #[test]
    fn test_distribute_overhead_proportional() {
        let mut metrics = SessionMetrics::new(None);
        let mut overhead_bin = 1000;
        let mut overhead_bout = 500;

        // Stream transferred 60% of total bytes (not last stream)
        distribute_overhead(
            &mut metrics,
            &mut overhead_bin,
            &mut overhead_bout,
            (600, 300),  // stream_bytes
            (1000, 500), // total_bytes
            2,           // active_streams
            false,       // is_last_stream
        );

        assert_eq!(metrics.bin, 600); // 60% of 1000
        assert_eq!(metrics.bout, 300); // 60% of 500
        assert_eq!(overhead_bin, 400); // 1000 - 600
        assert_eq!(overhead_bout, 200); // 500 - 300
    }

    #[test]
    fn test_distribute_overhead_even_split_when_no_bytes() {
        let mut metrics = SessionMetrics::new(None);
        let mut overhead_bin = 100;
        let mut overhead_bout = 200;

        // No bytes transferred -> even distribution (not last stream)
        distribute_overhead(
            &mut metrics,
            &mut overhead_bin,
            &mut overhead_bout,
            (0, 0), // stream_bytes
            (0, 0), // total_bytes
            4,      // active_streams
            false,  // is_last_stream
        );

        assert_eq!(metrics.bin, 25); // 100 / 4
        assert_eq!(metrics.bout, 50); // 200 / 4
        assert_eq!(overhead_bin, 75);
        assert_eq!(overhead_bout, 150);
    }

    #[test]
    fn test_distribute_overhead_clamps_to_remaining() {
        let mut metrics = SessionMetrics::new(None);
        let mut overhead_bin = 10;
        let mut overhead_bout = 10;

        // Stream claims 100% of bytes but overhead is small (last stream)
        distribute_overhead(
            &mut metrics,
            &mut overhead_bin,
            &mut overhead_bout,
            (1000, 1000), // stream_bytes
            (1000, 1000), // total_bytes
            1,            // active_streams
            true,         // is_last_stream
        );

        assert_eq!(metrics.bin, 10);
        assert_eq!(metrics.bout, 10);
        assert_eq!(overhead_bin, 0);
        assert_eq!(overhead_bout, 0);
    }

    #[test]
    fn test_distribute_overhead_zero_active_streams() {
        let mut metrics = SessionMetrics::new(None);
        let mut overhead_bin = 100;
        let mut overhead_bout = 100;

        // 0 active streams (edge case) — last stream gets all remainder
        distribute_overhead(
            &mut metrics,
            &mut overhead_bin,
            &mut overhead_bout,
            (0, 0),
            (0, 0),
            0,
            true,
        );

        assert_eq!(metrics.bin, 100); // last stream gets all remaining
        assert_eq!(metrics.bout, 100);
        assert_eq!(overhead_bin, 0);
        assert_eq!(overhead_bout, 0);
    }

    #[test]
    fn test_distribute_overhead_last_stream_gets_remainder() {
        let mut metrics1 = SessionMetrics::new(None);
        let mut metrics2 = SessionMetrics::new(None);
        let mut overhead_bin = 120;
        let mut overhead_bout = 120;

        // First stream (not last): gets proportional share
        distribute_overhead(
            &mut metrics1,
            &mut overhead_bin,
            &mut overhead_bout,
            (100, 100), // stream_bytes
            (300, 300), // total_bytes
            3,          // active_streams
            false,      // is_last_stream
        );

        let remaining_bin = overhead_bin;
        let remaining_bout = overhead_bout;

        // Last stream: gets ALL remaining overhead (no rounding loss)
        distribute_overhead(
            &mut metrics2,
            &mut overhead_bin,
            &mut overhead_bout,
            (100, 100), // stream_bytes
            (300, 300), // total_bytes
            3,          // active_streams
            true,       // is_last_stream
        );

        assert_eq!(metrics2.bin, remaining_bin);
        assert_eq!(metrics2.bout, remaining_bout);
        assert_eq!(overhead_bin, 0, "no remainder bytes should be lost");
        assert_eq!(overhead_bout, 0, "no remainder bytes should be lost");
    }

    // ── H2FlowControl (additional edge cases) ─────────────────────────

    #[test]
    fn test_flow_control_queue_window_update_cap() {
        // Verify DEFAULT_MAX_PENDING_WINDOW_UPDATES reflects 1 + 4*MAX_CONCURRENT_STREAMS
        assert_eq!(DEFAULT_MAX_PENDING_WINDOW_UPDATES, 1 + 100 * 4);

        // Simulate queue reaching capacity
        let cap = DEFAULT_MAX_PENDING_WINDOW_UPDATES;
        let mut updates: HashMap<u32, u32> = HashMap::new();
        for i in 0..cap as u32 {
            updates.insert(i, 1000);
        }
        assert_eq!(updates.len(), cap);

        // A new stream ID beyond capacity should be rejected
        let next_stream = cap as u32;
        let at_cap = updates.len() >= cap;
        assert!(at_cap);
        assert!(!updates.contains_key(&next_stream));

        // Verify custom max_concurrent_streams produces proportional cap
        let custom_cap = 1 + 500_usize * 4;
        assert_eq!(custom_cap, 2001);
    }

    #[test]
    fn test_h2_connection_config_defaults() {
        let config = H2ConnectionConfig::default();
        assert_eq!(config.initial_connection_window, ENLARGED_CONNECTION_WINDOW);
        assert_eq!(
            config.max_concurrent_streams,
            DEFAULT_MAX_CONCURRENT_STREAMS
        );
        assert_eq!(config.stream_shrink_ratio, 2);
    }

    #[test]
    fn test_h2_connection_config_clamp_window_lower_bound() {
        // Below minimum: clamped to DEFAULT_INITIAL_WINDOW_SIZE (65535)
        let config = H2ConnectionConfig::new(100, 100, 2);
        assert_eq!(
            config.initial_connection_window,
            DEFAULT_INITIAL_WINDOW_SIZE
        );
    }

    #[test]
    fn test_h2_connection_config_clamp_window_upper_bound() {
        // Above maximum: clamped to FLOW_CONTROL_MAX_WINDOW (2^31-1)
        let config = H2ConnectionConfig::new(u32::MAX, 100, 2);
        assert_eq!(config.initial_connection_window, FLOW_CONTROL_MAX_WINDOW);
    }

    #[test]
    fn test_h2_connection_config_clamp_window_exact_minimum() {
        // Exactly minimum: no clamping, no zero-increment WINDOW_UPDATE risk
        let config = H2ConnectionConfig::new(DEFAULT_INITIAL_WINDOW_SIZE, 100, 2);
        assert_eq!(
            config.initial_connection_window,
            DEFAULT_INITIAL_WINDOW_SIZE
        );
        // Increment to send would be 0 — the code guards this with `if increment > 0`
        let increment = config
            .initial_connection_window
            .saturating_sub(DEFAULT_INITIAL_WINDOW_SIZE);
        assert_eq!(increment, 0);
    }

    #[test]
    fn test_h2_connection_config_clamp_shrink_ratio() {
        // Below minimum: clamped to 2 (1 would defeat recycling)
        let config = H2ConnectionConfig::new(ENLARGED_CONNECTION_WINDOW, 100, 0);
        assert_eq!(config.stream_shrink_ratio, 2);
        let config = H2ConnectionConfig::new(ENLARGED_CONNECTION_WINDOW, 100, 1);
        assert_eq!(config.stream_shrink_ratio, 2);
    }

    #[test]
    fn test_h2_connection_config_clamp_concurrent_streams() {
        let config = H2ConnectionConfig::new(ENLARGED_CONNECTION_WINDOW, 0, 2);
        assert_eq!(config.max_concurrent_streams, 1);
    }

    #[test]
    fn test_h2_connection_config_from_optional_uses_defaults() {
        let config = H2ConnectionConfig::from_optional(None, None, None);
        let defaults = H2ConnectionConfig::default();
        assert_eq!(config, defaults);
    }

    #[test]
    fn test_h2_connection_config_from_optional_overrides() {
        let config = H2ConnectionConfig::from_optional(Some(2_000_000), Some(500), Some(4));
        assert_eq!(config.initial_connection_window, 2_000_000);
        assert_eq!(config.max_concurrent_streams, 500);
        assert_eq!(config.stream_shrink_ratio, 4);
    }

    #[test]
    fn test_flow_control_window_settings_change_negative() {
        // RFC 9113 §6.9.2: A change to SETTINGS_INITIAL_WINDOW_SIZE can cause
        // the flow-control window to become negative.
        let mut fc = H2FlowControl {
            window: 100,
            received_bytes_since_update: 0,
            pending_window_updates: HashMap::new(),
        };

        // Simulate SETTINGS_INITIAL_WINDOW_SIZE reduction:
        // old_initial = 65535, new_initial = 10 => delta = 10 - 65535 = -65525
        let old_initial: i32 = DEFAULT_INITIAL_WINDOW_SIZE as i32;
        let new_initial: i32 = 10;
        let delta = new_initial - old_initial; // -65525
        fc.window += delta;

        assert!(
            fc.window < 0,
            "Window must be able to go negative after settings change"
        );
        assert_eq!(fc.window, 100 + (10 - 65535));
    }

    #[test]
    fn test_flow_control_coalesce_saturates_at_max_increment() {
        let max_increment = i32::MAX as u32;
        let mut updates: HashMap<u32, u32> = HashMap::new();

        // Insert at max and try to coalesce more
        updates.insert(1, max_increment);
        if let Some(existing) = updates.get_mut(&1) {
            *existing = existing.saturating_add(1000).min(max_increment);
        }
        assert_eq!(*updates.get(&1).unwrap(), max_increment);
    }

    // ── H2FloodConfig (additional) ───────────────────────────────────

    #[test]
    fn test_flood_config_default_matches_constants() {
        let config = H2FloodConfig::default();
        assert_eq!(
            config.max_rst_stream_per_window,
            DEFAULT_MAX_RST_STREAM_PER_WINDOW
        );
        assert_eq!(config.max_ping_per_window, DEFAULT_MAX_PING_PER_WINDOW);
        assert_eq!(
            config.max_settings_per_window,
            DEFAULT_MAX_SETTINGS_PER_WINDOW
        );
        assert_eq!(
            config.max_empty_data_per_window,
            DEFAULT_MAX_EMPTY_DATA_PER_WINDOW
        );
        assert_eq!(
            config.max_continuation_frames,
            DEFAULT_MAX_CONTINUATION_FRAMES
        );
        assert_eq!(config.max_glitch_count, DEFAULT_MAX_GLITCH_COUNT);
    }

    #[test]
    fn test_flood_config_equality() {
        let config_a = H2FloodConfig::default();
        let config_b = H2FloodConfig::default();
        assert_eq!(config_a, config_b);

        let config_c = H2FloodConfig {
            max_rst_stream_per_window: 1,
            ..H2FloodConfig::default()
        };
        assert_ne!(config_a, config_c);
    }

    // ── distribute_overhead (additional edge cases) ───────────────────

    #[test]
    fn test_distribute_overhead_asymmetric_in_out() {
        let mut metrics = SessionMetrics::new(None);
        let mut overhead_bin = 1000;
        let mut overhead_bout = 1000;

        // Stream transferred 100% inbound, 0% outbound (not last stream)
        distribute_overhead(
            &mut metrics,
            &mut overhead_bin,
            &mut overhead_bout,
            (500, 0),   // stream_bytes
            (500, 100), // total_bytes
            2,          // active_streams
            false,      // is_last_stream
        );

        assert_eq!(metrics.bin, 1000); // 100% of inbound overhead
        assert_eq!(metrics.bout, 0); // 0% of outbound overhead
        assert_eq!(overhead_bin, 0);
        assert_eq!(overhead_bout, 1000);
    }

    #[test]
    fn test_distribute_overhead_many_streams_accumulate() {
        let mut metrics = SessionMetrics::new(None);
        let mut overhead_bin = 120;
        let mut overhead_bout = 120;

        // Three equal streams, each calling distribute_overhead.
        // With is_last_stream on the third call, the last stream gets all
        // remaining overhead, so no rounding loss occurs.
        //   call 1: 120 * 100/300 = 40 -> remaining 80
        //   call 2:  80 * 100/300 = 26 -> remaining 54
        //   call 3: last stream gets all remaining = 54
        // Total distributed: 40 + 26 + 54 = 120 (no loss)
        for i in 0..3 {
            distribute_overhead(
                &mut metrics,
                &mut overhead_bin,
                &mut overhead_bout,
                (100, 100), // stream_bytes
                (300, 300), // total_bytes
                3,          // active_streams
                i == 2,     // is_last_stream on final call
            );
        }

        assert_eq!(metrics.bin, 120);
        assert_eq!(metrics.bout, 120);
        // No rounding residual — last stream absorbed the remainder
        assert_eq!(overhead_bin, 0);
        assert_eq!(overhead_bout, 0);
    }

    // ── Hex chunk formatting ────────────────────────────────────────────

    /// Verify that the Vec<u8> + write!() hex formatting used in
    /// handle_data_frame produces output identical to format!("{:x}").
    #[test]
    fn test_hex_chunk_length_formatting() {
        use std::io::Write as _;

        let cases: &[(usize, &[u8])] = &[
            (1, b"1"),
            (15, b"f"),
            (16, b"10"),
            (255, b"ff"),
            (256, b"100"),
            (4096, b"1000"),
            (65535, b"ffff"),
            (65536, b"10000"),
        ];

        for &(payload_len, expected) in cases {
            let mut buf = Vec::with_capacity(16);
            let _ = write!(buf, "{payload_len:x}");
            assert_eq!(
                buf, expected,
                "hex formatting mismatch for payload_len={payload_len}"
            );
        }

        // usize::MAX tested separately to avoid temporary lifetime issue
        let max_expected = format!("{:x}", usize::MAX);
        let mut buf = Vec::with_capacity(16);
        let _ = write!(buf, "{:x}", usize::MAX);
        assert_eq!(buf, max_expected.as_bytes());
    }

    // ── Stream-ID allocation / exhaustion ──────────────────────────────────

    /// A fresh client connection starts with `last_stream_id == 0`. The first
    /// call MUST issue stream `1` (odd, RFC 9113 §5.1.1) and advance the
    /// watermark to `2`.
    #[test]
    fn test_next_stream_id_client_first_allocation() {
        let (issued, next) = next_stream_id(0, true).expect("fresh client must allocate");
        assert_eq!(issued, 1);
        assert_eq!(next, 2);
    }

    /// Client allocation yields strictly increasing odd identifiers
    /// (1, 3, 5, ...) as required by RFC 9113 §5.1.1.
    #[test]
    fn test_next_stream_id_client_sequence_is_odd_and_monotonic() {
        let mut last = 0u32;
        let mut issued_ids = Vec::with_capacity(8);
        for _ in 0..8 {
            let (id, next) = next_stream_id(last, true).expect("unexhausted");
            assert_eq!(id & 1, 1, "client stream ids must be odd (RFC 9113 §5.1.1)");
            assert!(issued_ids.last().is_none_or(|prev: &u32| id > *prev));
            issued_ids.push(id);
            last = next;
        }
        assert_eq!(issued_ids, vec![1, 3, 5, 7, 9, 11, 13, 15]);
    }

    /// Server-side allocation yields even identifiers. The helper
    /// convention is `watermark - 2` for server, `watermark - 1` for client,
    /// so both sides share the same monotonically-increasing even watermark.
    /// Sōzu never server-pushes, but the helper must be symmetric so push
    /// could be enabled without a regression.
    #[test]
    fn test_next_stream_id_server_is_even() {
        // `last = 2` means the most recent allocation advanced the watermark
        // to 2; server then issues `2 - 2 = 0`. This is an artefact of the
        // shared watermark and only matters in tests — server never uses it.
        let (issued, next) = next_stream_id(2, false).expect("server allocation");
        assert_eq!(issued & 1, 0, "server stream ids must be even");
        assert_eq!(next, 4);
        assert_eq!(issued, 2);

        let (issued, next) = next_stream_id(next, false).expect("second slot");
        assert_eq!(issued, 4);
        assert_eq!(issued & 1, 0);
        assert_eq!(next, 6);
    }

    /// The last client-issuable odd stream ID is `STREAM_ID_MAX = 0x7FFF_FFFF`.
    /// To issue it the watermark must advance to `STREAM_ID_MAX + 1 = 2³¹`;
    /// the caller therefore supplies `last = STREAM_ID_MAX - 1 = 0x7FFF_FFFE`.
    /// That call MUST succeed and return the max ID; the post-call watermark
    /// sits at `2³¹`, which is the sentinel that makes the next call fail.
    #[test]
    fn test_next_stream_id_client_final_slot_allocates() {
        let last = STREAM_ID_MAX - 1;
        let (issued, next) = next_stream_id(last, true).expect("final slot still allocates");
        assert_eq!(issued, STREAM_ID_MAX);
        assert_eq!(next, STREAM_ID_MAX + 1);
        // And the very next call MUST refuse rather than wrap.
        assert!(next_stream_id(next, true).is_none());
    }

    /// Exhaustion case: once the client has issued stream ID `STREAM_ID_MAX`,
    /// the watermark sits at `STREAM_ID_MAX + 1`. The next request MUST return
    /// `None` — without this guard the helper would issue `STREAM_ID_MAX + 2`
    /// (wrapped down to an even id), which would (a) use the reserved
    /// high bit and (b) violate the odd-parity invariant for client streams.
    #[test]
    fn test_next_stream_id_client_exhausted_returns_none() {
        let last = STREAM_ID_MAX + 1;
        assert!(next_stream_id(last, true).is_none());
    }

    /// Exhaustion via `checked_add` saturation: defence in depth in case a
    /// caller jumps `last_stream_id` close to `u32::MAX`. The helper must
    /// not panic nor overflow — it must return `None`.
    #[test]
    fn test_next_stream_id_saturates_near_u32_max() {
        assert!(next_stream_id(u32::MAX, true).is_none());
        assert!(next_stream_id(u32::MAX - 1, true).is_none());
    }

    /// Server-side exhaustion: same guard, even-parity identifier space.
    #[test]
    fn test_next_stream_id_server_exhausted_returns_none() {
        let last = STREAM_ID_MAX + 1;
        assert!(next_stream_id(last, false).is_none());
    }

    /// Regression guard: the helper must never issue a stream ID that
    /// exceeds `STREAM_ID_MAX` for either side, no matter where the
    /// watermark sits. This walks every value in a neighbourhood of the
    /// boundary to rule out off-by-one errors.
    #[test]
    fn test_next_stream_id_never_exceeds_stream_id_max() {
        for last in (STREAM_ID_MAX - 4)..=(STREAM_ID_MAX + 4) {
            for is_client in [true, false] {
                if let Some((issued, next)) = next_stream_id(last, is_client) {
                    assert!(
                        issued <= STREAM_ID_MAX,
                        "issued id {issued} exceeds STREAM_ID_MAX (last={last}, is_client={is_client})"
                    );
                    // `next` is the post-allocation watermark and may sit at
                    // STREAM_ID_MAX + 1 — the very next call must then return None.
                    if next > STREAM_ID_MAX {
                        assert!(
                            next_stream_id(next, is_client).is_none(),
                            "second call after final slot must report exhaustion"
                        );
                    }
                }
            }
        }
    }

    /// The helper's `is_client` flag must cleanly split the ID space so that
    /// a client and a server peered on the same connection cannot collide.
    /// Given the same `last_stream_id`, the two parities must differ by 1.
    #[test]
    fn test_next_stream_id_client_server_parities_disjoint() {
        for last in [0u32, 2, 4, 10, 100, 1_000_000, STREAM_ID_MAX - 3] {
            let (client_id, _) = next_stream_id(last, true).unwrap();
            let (server_id, _) = next_stream_id(last, false).unwrap();
            assert_eq!(client_id & 1, 1);
            assert_eq!(server_id & 1, 0);
            assert_eq!(client_id.abs_diff(server_id), 1);
        }
    }

    // ── LIFECYCLE §9 invariant 16: any_stream_id_matches ─────────────────
    //
    // Covers the iteration dispatch used by `any_stream_has_pending_back`.
    // Testing the probe directly against a synthetic closure keeps the
    // tests independent of the full `Stream` fixture (which requires a
    // `Pool` and a fully-built `HttpContext`).

    #[test]
    fn test_any_stream_id_matches_empty_map_is_false() {
        let streams: HashMap<StreamId, GlobalStreamId> = HashMap::new();
        assert!(!any_stream_id_matches(&streams, |_| true));
    }

    #[test]
    fn test_any_stream_id_matches_all_probe_false_is_false() {
        let mut streams: HashMap<StreamId, GlobalStreamId> = HashMap::new();
        streams.insert(1, 0);
        streams.insert(3, 1);
        streams.insert(5, 2);
        assert!(!any_stream_id_matches(&streams, |_| false));
    }

    #[test]
    fn test_any_stream_id_matches_any_probe_true_is_true() {
        let mut streams: HashMap<StreamId, GlobalStreamId> = HashMap::new();
        streams.insert(1, 0);
        streams.insert(3, 1);
        streams.insert(5, 2);
        // Probe is true only for GlobalStreamId == 1 (i.e. StreamId 3).
        assert!(any_stream_id_matches(&streams, |gid| gid == 1));
    }

    #[test]
    fn test_any_stream_id_matches_single_entry() {
        let mut streams: HashMap<StreamId, GlobalStreamId> = HashMap::new();
        streams.insert(42, 7);
        assert!(any_stream_id_matches(&streams, |gid| gid == 7));
        assert!(!any_stream_id_matches(&streams, |gid| gid == 8));
    }

    #[test]
    fn test_any_stream_id_matches_short_circuits() {
        let mut streams: HashMap<StreamId, GlobalStreamId> = HashMap::new();
        streams.insert(1, 0);
        streams.insert(3, 1);
        streams.insert(5, 2);
        streams.insert(7, 3);
        let mut calls = 0usize;
        let result = any_stream_id_matches(&streams, |_| {
            calls += 1;
            true
        });
        assert!(result);
        // `Iterator::any` short-circuits on the first `true` — so the probe
        // must fire at most once in this construction.
        assert_eq!(calls, 1);
    }

    // ── LIFECYCLE §9 invariant 16: any_stream_has_pending_back ───────────

    /// Build a minimal `Stream` for invariant-16 probing. Uses the pool
    /// plumbing so `back.blocks` / `back.out` exist; every other field is
    /// default-valued because the predicate only reads the back buffer.
    fn make_stream_for_invariant_16(pool: &Rc<RefCell<Pool>>, session_ulid: Ulid) -> Stream {
        let http_ctx = HttpContext {
            keep_alive_backend: true,
            keep_alive_frontend: true,
            sticky_session_found: None,
            method: None,
            authority: None,
            path: None,
            status: None,
            reason: None,
            user_agent: None,
            x_request_id: None,
            xff_chain: None,
            #[cfg(feature = "opentelemetry")]
            otel: None,
            closing: false,
            session_id: session_ulid,
            id: Ulid::generate(),
            backend_id: None,
            cluster_id: None,
            protocol: Protocol::HTTPS,
            public_address: "127.0.0.1:0".parse().unwrap(),
            session_address: None,
            sticky_name: String::new(),
            sticky_session: None,
            backend_address: None,
            tls_server_name: None,
            strict_sni_binding: false,
            elide_x_real_ip: false,
            send_x_real_ip: false,
            tls_version: None,
            tls_cipher: None,
            tls_alpn: None,
            sozu_id_header: String::from("Sozu-Id"),
            redirect_location: None,
            www_authenticate: None,
            original_authority: None,
            headers_response: Vec::new(),
            retry_after_seconds: None,
            frontend_redirect_template: None,
            redirect_status: None,
        };
        Stream::new(Rc::downgrade(pool), http_ctx, 65_535)
            .expect("pool should have capacity for two buffers")
    }

    fn make_pool_for_invariant_16() -> Rc<RefCell<Pool>> {
        // Two buffer slots per stream (front + back), ten stream slots is
        // plenty for the tests below.
        Rc::new(RefCell::new(Pool::with_capacity(4, 20, 16_384)))
    }

    #[test]
    fn test_any_stream_has_pending_back_empty_map_is_false() {
        let pool = make_pool_for_invariant_16();
        let ulid = Ulid::generate();
        let streams_map: HashMap<StreamId, GlobalStreamId> = HashMap::new();
        let context_streams = vec![make_stream_for_invariant_16(&pool, ulid)];
        assert!(!any_stream_has_pending_back(&streams_map, &context_streams));
    }

    #[test]
    fn test_any_stream_has_pending_back_all_drained_is_false() {
        let pool = make_pool_for_invariant_16();
        let ulid = Ulid::generate();
        let context_streams = vec![
            make_stream_for_invariant_16(&pool, ulid),
            make_stream_for_invariant_16(&pool, ulid),
        ];
        let mut streams_map: HashMap<StreamId, GlobalStreamId> = HashMap::new();
        streams_map.insert(1, 0);
        streams_map.insert(3, 1);
        // Both freshly-built streams have empty back.out and back.blocks
        // (Kawa::new starts with empty deques).
        assert!(!any_stream_has_pending_back(&streams_map, &context_streams));
    }

    #[test]
    fn test_any_stream_has_pending_back_unknown_gid_is_false() {
        // LIFECYCLE invariant 16 defence-in-depth: an unknown
        // `GlobalStreamId` during a stream-removal race must not panic;
        // `.get()` must short-circuit to `false`.
        let pool = make_pool_for_invariant_16();
        let ulid = Ulid::generate();
        let context_streams = vec![make_stream_for_invariant_16(&pool, ulid)];
        let mut streams_map: HashMap<StreamId, GlobalStreamId> = HashMap::new();
        // GlobalStreamId 42 is out of range for the 1-element slice above.
        streams_map.insert(7, 42);
        assert!(!any_stream_has_pending_back(&streams_map, &context_streams));
    }

    #[test]
    fn test_any_stream_has_pending_back_with_pending_blocks_is_true() {
        let pool = make_pool_for_invariant_16();
        let ulid = Ulid::generate();
        let mut stream = make_stream_for_invariant_16(&pool, ulid);
        // Push one dummy block — any Block variant is fine; the predicate
        // only checks `blocks.is_empty()`.
        stream.back.blocks.push_back(kawa::Block::StatusLine);
        let mut streams_map: HashMap<StreamId, GlobalStreamId> = HashMap::new();
        streams_map.insert(1, 0);
        assert!(any_stream_has_pending_back(&streams_map, &[stream]));
    }

    #[test]
    fn test_any_stream_has_pending_back_with_pending_out_is_true() {
        let pool = make_pool_for_invariant_16();
        let ulid = Ulid::generate();
        let mut stream = make_stream_for_invariant_16(&pool, ulid);
        // Non-empty out buffer with no blocks.
        stream
            .back
            .out
            .push_back(kawa::OutBlock::Store(kawa::Store::Static(b"partial frame")));
        let mut streams_map: HashMap<StreamId, GlobalStreamId> = HashMap::new();
        streams_map.insert(1, 0);
        assert!(any_stream_has_pending_back(&streams_map, &[stream]));
    }

    // ── Fix D: ready_incremental_by_urgency mid-pass consistency ─────────
    //
    // The full RED is in e2e and currently #[ignore]'d (timing-sensitive).
    // The scalar logic below pins the saturating_sub + bucket-scoped
    // decrement contract the scheduler at h2.rs:2412-2414 + h2.rs:2481
    // relies on: a same-urgency transition-to-ineligible MUST drop the
    // per-bucket count by exactly 1 and never underflow the u64.

    fn make_bucket(counts: &[(u8, usize)]) -> HashMap<u8, usize> {
        counts.iter().copied().collect()
    }

    #[test]
    fn ready_incremental_bucket_decrement_reduces_same_urgency_only() {
        let mut map = make_bucket(&[(1, 3), (3, 2)]);
        let urgency: u8 = 1;
        let is_incremental = true;
        // Simulate a stream in urgency=1 going ineligible mid-pass.
        if is_incremental {
            if let Some(c) = map.get_mut(&urgency) {
                *c = c.saturating_sub(1);
            }
        }
        assert_eq!(map.get(&1), Some(&2), "urgency-1 bucket must drop to 2");
        assert_eq!(map.get(&3), Some(&2), "urgency-3 bucket untouched");
    }

    #[test]
    fn ready_incremental_bucket_decrement_saturates_at_zero() {
        let mut map = make_bucket(&[(0, 0)]);
        let urgency: u8 = 0;
        if let Some(c) = map.get_mut(&urgency) {
            *c = c.saturating_sub(1);
        }
        assert_eq!(map.get(&0), Some(&0), "saturating_sub must not underflow");
    }

    #[test]
    fn ready_incremental_bucket_decrement_skipped_for_non_incremental() {
        let mut map = make_bucket(&[(1, 3)]);
        let is_incremental = false;
        if is_incremental {
            if let Some(c) = map.get_mut(&1) {
                *c = c.saturating_sub(1);
            }
        }
        assert_eq!(
            map.get(&1),
            Some(&3),
            "non-incremental transitions must not touch the bucket"
        );
    }

    // ── enqueue_rst: queue / dedupe / counter / arm invariants ───────────
    //
    // `enqueue_rst_into` is the free-function primitive shared by all three
    // RST push sites (DATA-on-closed, refuse_stream_and_discard,
    // reset_stream). The method delegates; the invariants live here.

    #[test]
    fn test_enqueue_rst_into_populates_queue_and_dedupe() {
        let mut pending: Vec<(StreamId, H2Error)> = Vec::new();
        let mut total: usize = 0;
        let mut sent: HashSet<StreamId> = HashSet::new();
        let mut readiness = Readiness::new();

        let first = enqueue_rst_into(
            &mut pending,
            &mut total,
            &mut sent,
            &mut readiness,
            5,
            H2Error::ProtocolError,
        );
        assert!(first, "first call must report freshly_queued = true");
        // Second call for the same stream must be a no-op AND return
        // false so accounting in `Self::enqueue_rst` skips this case.
        let second = enqueue_rst_into(
            &mut pending,
            &mut total,
            &mut sent,
            &mut readiness,
            5,
            H2Error::InternalError,
        );
        assert!(
            !second,
            "second call for same stream must return freshly_queued = false"
        );

        assert_eq!(pending.len(), 1, "dedupe must collapse to a single entry");
        assert_eq!(
            pending[0],
            (5, H2Error::ProtocolError),
            "the first error wins — second push is ignored"
        );
        assert_eq!(total, 1, "queued-cap counter must bump exactly once");
        assert!(sent.contains(&5), "rst_sent must record the id");
    }

    #[test]
    fn test_enqueue_rst_into_bumps_total_for_distinct_ids() {
        let mut pending: Vec<(StreamId, H2Error)> = Vec::new();
        let mut total: usize = 0;
        let mut sent: HashSet<StreamId> = HashSet::new();
        let mut readiness = Readiness::new();

        for sid in [1u32, 3, 5, 7] {
            enqueue_rst_into(
                &mut pending,
                &mut total,
                &mut sent,
                &mut readiness,
                sid,
                H2Error::ProtocolError,
            );
        }

        assert_eq!(pending.len(), 4);
        assert_eq!(total, 4);
        assert_eq!(sent.len(), 4);
    }

    #[test]
    fn test_enqueue_rst_into_arms_writable_in_invariant_15_form() {
        let mut pending: Vec<(StreamId, H2Error)> = Vec::new();
        let mut total: usize = 0;
        let mut sent: HashSet<StreamId> = HashSet::new();
        let mut readiness = Readiness::new();

        // Precondition: no WRITABLE bits set.
        assert!(!readiness.interest.is_writable());
        assert!(!readiness.event.is_writable());

        enqueue_rst_into(
            &mut pending,
            &mut total,
            &mut sent,
            &mut readiness,
            9,
            H2Error::FlowControlError,
        );

        // Postcondition: invariant-15 — both `interest` and `event` WRITABLE
        // are raised so the next tick runs `writable()` under edge-triggered
        // epoll.
        assert!(
            readiness.interest.is_writable(),
            "arm_writable must raise the interest bit"
        );
        assert!(
            readiness.event.is_writable(),
            "arm_writable must raise the event bit (edge-triggered epoll)"
        );
    }

    #[test]
    fn test_enqueue_rst_into_dedupe_does_not_rearm_writable() {
        // Dedupe is a pure short-circuit: if the stream id is already in
        // `rst_sent`, we do not touch the readiness. This matters because
        // a re-entrant reset_stream call during a cascading error path
        // would otherwise re-raise WRITABLE unnecessarily — harmless but
        // noisy in metrics.
        let mut pending: Vec<(StreamId, H2Error)> = Vec::new();
        let mut total: usize = 0;
        let mut sent: HashSet<StreamId> = HashSet::new();
        sent.insert(11);
        let mut readiness = Readiness::new();

        enqueue_rst_into(
            &mut pending,
            &mut total,
            &mut sent,
            &mut readiness,
            11,
            H2Error::ProtocolError,
        );

        assert!(
            pending.is_empty(),
            "already-sent ids must not queue a second frame"
        );
        assert_eq!(total, 0);
        assert!(!readiness.interest.is_writable());
        assert!(!readiness.event.is_writable());
    }

    // ── forcefully_terminate_answer arms WRITABLE for ET epoll ───────────
    //
    // Gap A in the h2spec diagnosis: the pre-fix code set `interest` but
    // never raised `event`, so `filter_interest() = event & interest` was
    // zero and `writable()` was never scheduled. This test pins the fix.

    #[test]
    fn test_forcefully_terminate_answer_arms_event_and_interest() {
        let pool = make_pool_for_invariant_16();
        let ulid = Ulid::generate();
        let mut stream = make_stream_for_invariant_16(&pool, ulid);
        let mut readiness = Readiness::new();

        assert!(!readiness.interest.is_writable());
        assert!(!readiness.event.is_writable());

        forcefully_terminate_answer(&mut stream, &mut readiness, H2Error::ProtocolError);

        assert!(
            readiness.interest.is_writable(),
            "forcefully_terminate_answer must set the WRITABLE interest bit"
        );
        assert!(
            readiness.event.is_writable(),
            "forcefully_terminate_answer must set the WRITABLE event bit — \
             without this, filter_interest() = 0 under edge-triggered epoll \
             and writable() is never scheduled (h2spec Gap A)"
        );
    }
}
