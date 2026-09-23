//! H2 mux connection wrapper (RFC 9113).
//!
//! Owns wire-side connection state: HPACK encoder/decoder, peer settings,
//! flow window, GOAWAY/RST attribution, and orchestrates the
//! [`h2_flood_detector::H2FloodDetector`] backing the CVE-2023-44487 /
//! CVE-2024-27316 / CVE-2025-8671 mitigations. Stream
//! storage lives in the sibling `Context<L>` (`mux/mod.rs`); this module is
//! the canonical home for the edge-trigger discipline — paths that queue
//! bytes for a later event-loop pass must arm writable / signal pending
//! write (cf. `arm_writable()` at the deferred-control-frame sites and
//! `lib/src/lib.rs:1006`-`1010`).

use std::{
    cmp::min,
    collections::HashMap,
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

use crate::metrics::names;
use crate::{
    L7ListenerHandler, ListenerHandler, Protocol, Readiness, SessionMetrics,
    protocol::mux::{
        BackendStatus, Context, DebugEvent, DebugHistory, Endpoint, GenericHttpStream,
        GlobalStreamId, MuxResult, Position, Stream, StreamId, StreamState, converter,
        forcefully_terminate_answer,
        h2_close::{self, CloseAction, FinalizeAction, TlsFlushPhase},
        h2_control_tx,
        h2_drain::{self, GracefulDrainDecision},
        h2_flood_detector::{self, H2FloodConfig, H2FloodViolation},
        h2_flow_control, h2_header_reassembly, h2_scheduler, h2_stream_table, h2_transmit,
        hpack_state,
        parser::{self, Frame, FrameHeader, FrameType, H2Error, Headers, WindowUpdate},
        pkawa, remove_backend_stream, serializer, set_default_answer,
        shared::{EndStreamAction, drain_tls_close_notify, end_stream_decision},
        update_readiness_after_read, update_readiness_after_write,
    },
    socket::{SocketHandler, SocketResult, stats::socket_rtt},
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
/// - `peer` — peer address via [`SocketHandler::peer_addr`](crate::socket::SocketHandler::peer_addr):
///   the snapshot the handler cached, not a live `getpeername(2)`. It therefore
///   survives the peer's RST (which is when these lines are read) and, on a
///   PROXY-protocol frontend, names the advertised client rather than the load
///   balancer — matching the `HTTPS`/`HTTP` line for the same request id
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
            peer = $self.peer_address,
            position = $self.position,
            state = $self.state,
            streams = $self.stream_table.len(),
            last_peer_id = $self.stream_table.highest_peer_stream_id(),
            window = $self.flow_control.window(),
            draining = $self.drain.draining(),
            total_rst_streams_emitted_lifetime = $self.flood_detector.total_rst_streams_emitted_lifetime(),
            total_rst_received_lifetime = $self.flood_detector.total_rst_received_lifetime(),
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
            peer = $self.peer_address,
            position = $self.position,
            state = $self.state,
            streams = $self.stream_table.len(),
            last_peer_id = $self.stream_table.highest_peer_stream_id(),
            window = $self.flow_control.window(),
            draining = $self.drain.draining(),
            total_rst_streams_emitted_lifetime = $self.flood_detector.total_rst_streams_emitted_lifetime(),
            total_rst_received_lifetime = $self.flood_detector.total_rst_received_lifetime(),
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

/// `if let Some(violation) = self.flood_detector.check_flood(self.now) { return self.handle_flood_violation(violation); }`
/// pattern wrapped as a single statement. Pure dispatch — the actual flood
/// thresholds and counters live inside `H2FloodDetector::check_flood` and
/// `ConnectionH2::handle_flood_violation`, which the macro does not touch.
/// Use this at every per-frame counter bump site so the wrapper stays
/// uniform and a future grep for "flood-check forgot to return" finds zero.
macro_rules! check_flood_or_return {
    ($self:expr) => {
        if let Some(violation) = $self.flood_detector.check_flood($self.now) {
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
    // Post-conditions (RFC 9113 §5.1.1):
    // - the issued id fits the 31-bit space;
    // - the returned watermark is strictly greater than the id we issued, so a
    //   subsequent call cannot re-issue or regress;
    // - role-parity: client ids are odd, server ids even. This holds ONLY when
    //   `last_stream_id` is an even watermark, which is the helper's documented
    //   contract and what production always maintains (`create_stream` rounds to
    //   `(stream_id + 2) & !1`; the connection initialises it to 0). The unit
    //   tests deliberately feed odd `last` values at the saturation boundary, so
    //   the parity check is gated on the watermark being even — a parity slip
    //   from an *even* watermark would let two roles collide on one id.
    debug_assert!(
        issued <= STREAM_ID_MAX,
        "issued stream id must fit the 31-bit space"
    );
    debug_assert!(
        next > issued,
        "the next watermark must advance strictly past the issued id"
    );
    debug_assert!(
        last_stream_id & 1 != 0 || (issued & 1 == 1) == is_client,
        "from an even watermark, client ids must be odd and server ids even (RFC 9113 §5.1.1)"
    );
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
//
// The CVE-tagged threshold defaults, `H2FloodConfig`, `H2FloodViolation` and
// `H2FloodDetector` itself moved into `h2_flood_detector.rs`, behind the same
// closed-API shape `hpack_state.rs`, `h2_flow_control.rs` and
// `h2_stream_table.rs` established in the three prior extraction steps.
// `MAX_HEADER_LIST_SIZE` stays here: `converter.rs` and `pkawa.rs` reference
// it directly (`h2::MAX_HEADER_LIST_SIZE`) as a general HPACK encode/decode
// safety ceiling, independent of any one connection's configured
// `H2FloodConfig::max_header_list_size` — moving it would widen this
// extraction into two unrelated modules for no benefit.

/// Maximum accumulated header block size across CONTINUATION frames (64KB).
/// Also the compile-time default for `H2FloodConfig::max_header_list_size`
/// (`h2_flood_detector.rs`).
pub(super) const MAX_HEADER_LIST_SIZE: usize = 65536;
/// Cumulative outbound progress (bytes) a window-stalled stream must drain to
/// clear its flow-control-stall deadline (M2 cumulative-stall budget). Below
/// this, a `WINDOW_UPDATE(+1)` drip that trickles a few bytes per idle period
/// cannot keep the slot alive: the deadline ages out and the reaper
/// RST(CANCEL)s the stream. Chosen as one max H2 DATA frame payload (16 KiB) —
/// a legitimate slow-but-steady transfer drains at least one frame per idle
/// period at any realistic bandwidth, while a drip attacker grants far less. A
/// `const`, not a config knob: `h2_stream_idle_timeout_seconds` is already the
/// operator dial for slow-link tolerance, and coupling a second knob invites
/// misconfiguration (high floor + low deadline = mass false reaps).
const FC_STALL_CLEAR_FLOOR: usize = 16 * 1024;
/// RFC 9113 §6.5.2: the size accounted against `SETTINGS_MAX_HEADER_LIST_SIZE`
/// is the uncompressed name + value octets PLUS a 32-octet overhead per field.
/// The per-field overhead is what bounds the field count under a fixed byte
/// budget — omitting it lets a peer materialize ~33× more fields than intended.
pub(super) const HEADER_FIELD_SIZE_OVERHEAD: usize = 32;

/// RFC 9113 §5.1.2: threshold of `REFUSED_STREAM` emissions per
/// [`BACKPRESSURE_WINDOW_DURATION`] that triggers back-pressure — at this
/// point we halve the advertised `SETTINGS_MAX_CONCURRENT_STREAMS` so the
/// peer throttles its request rate instead of paying the RST round-trip for
/// every new stream.
const BACKPRESSURE_REFUSAL_THRESHOLD: u32 = 50;
/// Sliding window used to detect refusal bursts for SETTINGS back-pressure.
const BACKPRESSURE_WINDOW_DURATION: std::time::Duration = std::time::Duration::from_secs(60);

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
        let config = Self {
            initial_connection_window: clamped_window,
            max_concurrent_streams: clamped_streams,
            stream_shrink_ratio: clamped_ratio,
        };
        // Post-conditions matching the documented clamp ranges. The window must
        // stay within RFC 9113 §6.9's [65535, 2^31-1] (a window outside this
        // band desynchronises flow control with the peer); max_concurrent_streams
        // must be >= 1 (zero would refuse every stream); shrink_ratio must be
        // >= 2 (1 defeats slot recycling, the whole point of the knob).
        debug_assert!(
            (DEFAULT_INITIAL_WINDOW_SIZE..=FLOW_CONTROL_MAX_WINDOW)
                .contains(&config.initial_connection_window),
            "clamped connection window must lie within RFC 9113 §6.9 bounds"
        );
        debug_assert!(
            config.max_concurrent_streams >= 1,
            "clamped max_concurrent_streams must be >= 1"
        );
        debug_assert!(
            config.stream_shrink_ratio >= 2,
            "clamped stream_shrink_ratio must be >= 2 to keep slot recycling effective"
        );
        config
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
/// A free function rather than a method: the seven `test_distribute_overhead_*`
/// cases drive this arithmetic directly, with no `ConnectionH2` fixture, the
/// same reason [`any_stream_id_matches`] is split out.
/// [`ConnectionH2::distribute_overhead`] is the `&mut self` wrapper the reset
/// paths use. [`ConnectionH2::try_recycle_server_stream`] calls this function
/// directly instead, which is a spelling choice and not a constraint: the
/// wrapper would credit the same shares at that site, because it reads
/// `self.stream_table.streams().len()` at call time — which is the same count,
/// the pass retiring nothing until after its loop — and its `len() <= 1`
/// cannot differ from the `len() == 1` spelled out there while the stream
/// being retired is still in the map.
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
    } else if let Some(share) = (*overhead_bin * stream_bytes.0).checked_div(total_bytes.0) {
        // Clamp to remaining overhead — integer division rounding across multiple
        // streams can cause accumulated shares to exceed the total.
        share.min(*overhead_bin)
    } else {
        // No stream has transferred any inbound bytes — fall back to even split.
        *overhead_bin / active_streams.max(1)
    };
    let share_out = if is_last_stream {
        *overhead_bout
    } else if let Some(share) = (*overhead_bout * stream_bytes.1).checked_div(total_bytes.1) {
        share.min(*overhead_bout)
    } else {
        // No stream has transferred any outbound bytes — fall back to even split.
        *overhead_bout / active_streams.max(1)
    };
    // Pre-condition: a stream can never be credited more overhead than remains
    // in the pool — otherwise the `*overhead_b* -= share_*` below underflows
    // (usize wraps to a huge value, corrupting connection-overhead accounting).
    // Every branch above either takes the whole pool (last stream) or `.min`s
    // against it, so this must hold.
    debug_assert!(
        share_in <= *overhead_bin,
        "overhead-in share must not exceed the remaining overhead pool"
    );
    debug_assert!(
        share_out <= *overhead_bout,
        "overhead-out share must not exceed the remaining overhead pool"
    );
    let before_bin = *overhead_bin;
    let before_bout = *overhead_bout;
    metrics.bin += share_in;
    metrics.bout += share_out;
    *overhead_bin -= share_in;
    *overhead_bout -= share_out;
    // Post-condition: the pool shrinks by exactly the credited share (overhead
    // is conserved, neither created nor lost). The last stream drains it to 0.
    debug_assert_eq!(
        *overhead_bin,
        before_bin - share_in,
        "overhead-in pool must decrease by exactly the credited share"
    );
    debug_assert_eq!(
        *overhead_bout,
        before_bout - share_out,
        "overhead-out pool must decrease by exactly the credited share"
    );
    debug_assert!(
        !is_last_stream || (*overhead_bin == 0 && *overhead_bout == 0),
        "the last stream must drain the overhead pool to zero (no lost remainder)"
    );
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

/// True when a stream still has response/upload bytes that could be put on the
/// wire — headers/body in flight, or a terminated-but-not-fully-flushed buffer.
/// Deliberately EXCLUDES `is_error()`/`rst_sent`: that disjunct is specific to
/// the priority-eligibility and write-loop gates (`write_streams`) and must stay
/// inline there; this 2-clause helper backs ONLY the window-stall arm.
fn has_sendable_response(kawa: &GenericHttpStream) -> bool {
    kawa.is_main_phase() || (kawa.is_terminated() && !kawa.is_completed())
}

/// Outcome of the M2 cumulative-stall budget decision for one `write_streams`
/// pass on a window-stalled stream. Extracted from the `write_streams` arm so
/// the budget logic is unit-testable without a full `ConnectionH2` fixture
/// (mirrors the [`h2_stream_table::H2StreamTable::collect_timed_out`] extraction).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FcStallAction {
    /// Clear both the deadline (`stream_fc_stalled_since`) and the progress
    /// accumulator (`stream_fc_stalled_progress`) for this stream.
    Clear,
    /// Ensure the deadline is armed (WITHOUT refreshing an existing `Instant`)
    /// and set the progress accumulator to `progress`.
    Arm { progress: usize },
}

/// Decide what to do with a stream's flow-control-stall deadline + cumulative
/// progress accumulator on one write pass (M2 cumulative-stall budget).
///
/// - A genuinely open send window (`!outbound_window_blocked`) is a real
///   un-stall → [`FcStallAction::Clear`].
/// - While the window stays blocked, accumulate this pass's outbound drain
///   (`consumed`, clamped to `>= 0`) onto `prior_progress`. Once the cumulative
///   total reaches [`FC_STALL_CLEAR_FLOOR`] (a full DATA frame of real delivery)
///   → `Clear`; otherwise `Arm` with the running total. A `WINDOW_UPDATE(+1)`
///   drip adds ~1 byte/pass and never reaches the floor, so the deadline keeps
///   aging and the reaper eventually fires.
fn fc_stall_budget_decision(
    outbound_window_blocked: bool,
    consumed: i32,
    prior_progress: Option<usize>,
) -> FcStallAction {
    if !outbound_window_blocked {
        return FcStallAction::Clear;
    }
    let progressed = prior_progress
        .unwrap_or(0)
        .saturating_add(consumed.max(0) as usize);
    if progressed >= FC_STALL_CLEAR_FLOOR {
        FcStallAction::Clear
    } else {
        FcStallAction::Arm {
            progress: progressed,
        }
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

/// Byte accounting for connection overhead attribution.
pub struct H2ByteAccounting {
    /// Bytes read on the zero stream not yet attributed to a stream.
    pub zero_bytes_read: usize,
    /// Overhead bytes received (connection-level frames).
    pub overhead_bin: usize,
    /// Overhead bytes sent (connection-level frames).
    pub overhead_bout: usize,
}

pub struct ConnectionH2<Front: SocketHandler> {
    /// Connection/session ULID propagated from the parent [`Mux`]. Used to
    /// stamp the session slot of the `[session req cluster backend]` log
    /// prefix emitted by this module's `log_context!` / `log_context_stream!`
    /// macros.
    pub session_ulid: Ulid,
    /// HPACK decoder/encoder pair and their reusable scratch buffers
    /// (`converter_buf`, `lowercase_buf`, `cookie_buf`), encapsulated so
    /// nothing outside `hpack_state.rs` can reach the raw fields — see
    /// [`hpack_state::HpackState`]. The pass-ordering buffer that used to sit
    /// beside them belongs to [`h2_scheduler::H2Scheduler`].
    hpack: hpack_state::HpackState,
    pub last_stream_id: StreamId,
    pub local_settings: H2Settings,
    pub peer_settings: H2Settings,
    pub position: Position,
    /// RFC 9218 priority state and the per-write-pass scheduling decision
    /// (stream order, the same-urgency ready-incremental census, the
    /// round-robin cursor), encapsulated so nothing outside
    /// `h2_scheduler.rs` can reach the raw fields — see
    /// [`h2_scheduler::H2Scheduler`].
    scheduler: h2_scheduler::H2Scheduler,
    pub readiness: Readiness,
    /// Peer address of this connection, captured once at construction from
    /// [`SocketHandler::peer_addr`](crate::socket::SocketHandler::peer_addr)
    /// and never re-read.
    ///
    /// This is the `peer=` slot of every line `log_context!` /
    /// `log_context_stream!` render, and those two macros expand at 113
    /// production callsites in this file — so sourcing the slot from
    /// `self.socket` made *every logging method* socket-coupled, including
    /// the ones that touch no I/O at all.
    ///
    /// Both production handlers *prefer* a cached address and fall back to
    /// a live `getpeername(2)` when they have none — `SessionTcpStream` and
    /// `FrontRustls` each answer
    /// `self.configured_peer.or_else(|| self.stream.peer_addr().ok())`
    /// (`socket.rs`). **That fallback arm is reachable**, it is pinned by its
    /// own tests, and `FrontRustls`' arm carries a comment saying so and
    /// naming the test that fails without it. So this snapshot is not
    /// unconditionally the same value a per-line call would have produced,
    /// and nothing here may be read as licence to delete those arms.
    ///
    /// It is at least as good on every path, which is the actual argument.
    /// The snapshot is taken after the handshake, so wherever the fallback
    /// would have answered, it answers here too — and it then survives the
    /// peer's reset, where a later live lookup returns `ENOTCONN` and renders
    /// `peer=None` on exactly the error lines an operator reads during an
    /// incident. On the direct-HTTPS frontend route, where `HttpsSession::new`
    /// seeds `peer_address` with a best-effort `peer_addr().ok()`, a line
    /// emitted after a reset now renders `peer=Some(addr)` where it used to
    /// render `peer=None`. That is a rendered-line change, and it is
    /// `df52d83d`'s intent applied one level further in.
    ///
    /// No production caller reaches [`SocketHandler::peer_addr`] on a bare
    /// `mio::net::TcpStream`. The impl exists and `MioTcpStream` is
    /// instantiated in production (`TcpStateMachine`, `tcp.rs`), but the TCP
    /// proxy reads its peer through mio's inherent method; only tests call
    /// the trait method on a bare stream.
    peer_address: Option<std::net::SocketAddr>,
    pub socket: Front,
    pub state: H2State,
    /// Wire `StreamId -> GlobalStreamId` map, `expect_read`/`expect_write`,
    /// `highest_peer_stream_id`, `rst_sent`, and the per-stream
    /// activity/flow-control-stall caches, encapsulated so nothing outside
    /// `h2_stream_table.rs` can reach the raw fields — see
    /// [`h2_stream_table::H2StreamTable`].
    stream_table: h2_stream_table::H2StreamTable,
    /// Configured idle timeout for this connection. The core never arms a
    /// wheel entry itself: it publishes the next instant it wants to be called
    /// back at through [`ConnectionH2::poll_timeout`], and the embedder — the
    /// `Mux` adapter — owns the `TimeoutContainer` that reflects it onto the
    /// real timer. See `LIFECYCLE.md` §7.7.
    pub timeout_duration: Duration,
    /// Next instant this connection wants `timeout()` called at, or `None`
    /// when it wants no timer at all. Always derived from
    /// [`ConnectionH2::now`], never from a fresh clock read (invariant 20).
    pub(super) timeout_deadline: Option<Instant>,
    /// Connection-level flow control state (send window, receive tracking,
    /// pending updates), encapsulated so nothing outside `h2_flow_control.rs`
    /// can reach the raw fields — see [`h2_flow_control::H2FlowControl`].
    flow_control: h2_flow_control::H2FlowControl,
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
    /// RFC 9113 §6.8 double-GOAWAY drain bookkeeping, encapsulated so
    /// nothing outside `h2_drain.rs` can reach the raw fields — see
    /// [`h2_drain::H2DrainState`].
    pub(super) drain: h2_drain::H2DrainState,
    /// Control-frame write scratch (WINDOW_UPDATE, RST_STREAM, GOAWAY,
    /// SETTINGS) and the read landing zone for every stream-0 frame's own
    /// header/payload bytes for the duration of ONE frame's read. It no
    /// longer doubles as a HEADERS+CONTINUATION reassembly buffer — that
    /// role moved to [`Self::header_reassembly`]; see that field's doc and
    /// `h2_header_reassembly.rs` for why, and LIFECYCLE.md invariant 24 for
    /// the bugs the split closes.
    pub zero: GenericHttpStream,
    /// Owned accumulator for an in-progress HEADERS+CONTINUATION field
    /// block (`H2State::ContinuationHeader`/`ContinuationFrame`). Separated
    /// from [`Self::zero`] so a control-frame flush that clears and reuses
    /// `zero.storage` has no way to name — and therefore cannot corrupt —
    /// the bytes reassembled so far. See `h2_header_reassembly.rs` and
    /// LIFECYCLE.md invariant 24.
    header_reassembly: h2_header_reassembly::HeaderBlockAccumulator,
    /// Byte accounting for connection overhead attribution.
    pub bytes: H2ByteAccounting,
    /// CVE-mitigation flood/abuse counters (Rapid Reset, MadeYouReset,
    /// CONTINUATION, Ping, Settings floods), encapsulated so nothing outside
    /// `h2_flood_detector.rs` can reach the raw fields — see
    /// [`h2_flood_detector::H2FloodDetector`].
    flood_detector: h2_flood_detector::H2FloodDetector,
    /// RFC 9113 §6.5: timestamp when we sent SETTINGS and are awaiting ACK.
    /// If the peer does not ACK within SETTINGS_ACK_TIMEOUT, we send GOAWAY
    /// with SettingsTimeout error.
    pub settings_sent_at: Option<Instant>,
    /// Queued proxy-emitted RST_STREAM frames and the never-decaying
    /// lifetime counter behind the CVE-2025-8671 MadeYouReset cap,
    /// encapsulated so nothing outside `h2_control_tx.rs` can reach the raw
    /// fields — see [`h2_control_tx::H2ControlTx`]. Frames are queued while
    /// refusing streams during `readable()`; the write happens in the
    /// writable preamble so it cannot conflict with `zero.storage`'s use for
    /// frame-payload discard.
    control_tx: h2_control_tx::H2ControlTx,
    /// Set by [`Self::refuse_stream_and_discard`], consumed once by the
    /// `H2State::Discard` arm of [`Self::handle_read`]. Carries enough of the
    /// refused frame's shape to still hand the connection-level HPACK
    /// decoder a complete field block before its bytes are dropped — RFC
    /// 9113 §4.3: field-compression state is scoped to the connection, not
    /// the stream. See `LIFECYCLE.md`'s Discard section.
    discarded_field_block: Option<DiscardedFieldBlock>,
    /// True once we've asked rustls to emit TLS close_notify for this frontend.
    close_notify_sent: bool,
    /// Per-listener H2 connection tuning (window size, max streams, shrink ratio).
    pub connection_config: H2ConnectionConfig,
    /// Maximum pending WINDOW_UPDATE entries before dropping.
    /// Derived from `connection_config.max_concurrent_streams` at construction.
    max_pending_window_updates: usize,
    /// Ready incremental streams observed on the last completed write pass,
    /// summed across urgency buckets (RFC 9218 §4). Sampled at the END of
    /// `write_streams` from [`h2_scheduler::ReadyIncrementalCensus::ready_total`],
    /// where the pass census is final: the entry call to
    /// [`Self::gauge_connection_state`] runs *before* that census exists, so
    /// sampling there would publish the previous pass's value.
    ///
    /// Carried as connection state rather than emitted inline because the
    /// aggregate it feeds must be a signed delta, not an absolute set — see
    /// [`Self::gauge_connection_state`].
    ready_incremental_streams: usize,
    /// Last `(connection_window, active_streams, pending_window_updates,
    /// ready_incremental_streams)` snapshot emitted by
    /// [`Self::gauge_connection_state`]. The snapshot represents this
    /// connection's *contribution* to the four aggregate gauges; each call
    /// emits the signed delta against this snapshot via [`gauge_add!`] so the
    /// gauges sum across connections.
    ///
    /// Stays `None` until the first emission. [`Drop`] applies the negative of
    /// this snapshot so the connection's contribution is always rebalanced to
    /// zero on teardown — independent of which close path runs.
    last_gauge_snapshot: Option<(usize, usize, usize, usize)>,
    /// Per-stream idle cap. Streams with no activity for longer than this are
    /// RST_STREAM(CANCEL)'d by [`Self::cancel_timed_out_streams`]. Compared
    /// against `stream_table`'s per-stream activity/flow-control-stall
    /// caches — see [`h2_stream_table::H2StreamTable::collect_timed_out`].
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
    /// Clock snapshot for the pass currently executing — this connection's
    /// mirror of [`Context::now`](super::Context::now).
    ///
    /// Every time-based decision in this module reads this field. The only
    /// code below [`Mux`](super::Mux) that samples the real clock is
    /// [`Self::new`], which takes one sample to seed this field,
    /// `refuse_window_start` and the flood detector's window — a connection
    /// is constructed outside any pass, so there is no snapshot to inherit.
    /// `h2_flood_detector::H2FloodDetector` carries no clock-sampling
    /// exception of its own any more: its former test-only `impl Default`
    /// was removed in the same changeset that extracted it, so
    /// [`Self::new`] is the sole place under `ConnectionH2` that reads
    /// the system clock.
    ///
    /// Assigned from `context.now` at each public entry point
    /// ([`Self::readable`], [`Self::writable`],
    /// [`Self::cancel_timed_out_streams`], [`Self::start_stream`]) and from
    /// the `now` parameter of [`Self::graceful_goaway`]. `Mux::shutting_down`
    /// reaches it indirectly: it refreshes `Context::now`, and the
    /// `readable()` call inside `drive_frontend_shutdown_io` mirrors that into
    /// this field before the forced-close deadline is evaluated.
    ///
    /// A field rather than a threaded parameter because the read sites are
    /// unreachable from a `context`: [`Self::handle_ping_frame`] takes no
    /// context at all, and the ten `check_flood_or_return!` sites are spread
    /// across six frame handlers. The macro reads `$self.now`, so those ten
    /// call sites stay as they were.
    pub(super) now: Instant,
}
impl<Front: SocketHandler> std::fmt::Debug for ConnectionH2<Front> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionH2")
            .field("position", &self.position)
            .field("state", &self.state)
            .field("expect", &self.stream_table.expect_read())
            .field("readiness", &self.readiness)
            .field("local_settings", &self.local_settings)
            .field("peer_settings", &self.peer_settings)
            .field("socket", &self.socket.socket_ref())
            .field("streams", self.stream_table.streams())
            .field("zero", &self.zero.storage.meter(20))
            .field(
                "header_reassembly_in_progress",
                &self.header_reassembly.is_in_progress(),
            )
            .field("header_reassembly_len", &self.header_reassembly.len())
            .field("window", &self.flow_control.window())
            .field(
                "total_rst_streams_queued",
                &self.control_tx.lifetime_queued(),
            )
            .finish()
    }
}

/// Symmetric tear-down for the four aggregate gauges
/// [`ConnectionH2::gauge_connection_state`] feeds — the three
/// `h2.connection.*` metrics and `h2.streams.ready_incremental.by_urgency`:
/// whatever positive contribution this connection made is subtracted back out
/// when the connection is dropped.
///
/// Using `Drop` (rather than wiring decrements into every close path —
/// `graceful_goaway`, `force_disconnect`, `handle_goaway_frame`, `Mux::close`,
/// stream-id exhaustion, panic-unwind) is what guarantees the gauge is
/// arithmetically symmetric regardless of which path teardown took. Past
/// underflow incidents (commits ff401b54, aadb3fa4) were increment/decrement
/// asymmetries. `ff401b54` carried both shapes at once: a `-1` for streams that
/// never ran the `+1`, AND a `-1` that ran twice on one stream (100-Continue,
/// and an H2 reset followed by close). `aadb3fa4` was the doubled `-1` alone,
/// let through by an early return. `Drop` closes both shapes — it is the sole
/// decrement site, and taking `last_gauge_snapshot` makes a second call a
/// no-op.
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

/// What [`ConnectionH2::poll_read_target`] wants its caller to do before it
/// may call [`ConnectionH2::handle_read`].
///
/// The three variants are the three shapes one frontend read pass takes, and
/// [`Self::Skip`] is deliberately not folded into a `Fill { amount: 0 }`: that
/// would have the caller issue a zero-length read, and
/// `update_readiness_after_read` reads its `(0, SocketResult::Continue)`
/// answer as "nothing arrived, stop" — so every frame carrying no payload (an
/// empty SETTINGS, an empty DATA, a SETTINGS ACK) would stop being parsed at
/// all. The variant is what stops a caller from having to know that.
#[derive(Debug, Clone, Copy)]
pub(super) enum H2ReadTarget {
    /// The core ended the pass by itself: a SETTINGS-ACK timeout, nothing owed
    /// by the peer, or no room left for what is owed. No read, and no
    /// [`ConnectionH2::handle_read`] — this is the pass's result.
    Done(MuxResult),
    /// Every byte the core is waiting for already sits in `stream_id`'s
    /// buffer, because the frame in flight carries a zero-length payload.
    /// Perform no read and answer with [`H2ReadOutcome::Skipped`].
    Skip(H2StreamId),
    /// Read into the space [`read_space`] returns for `stream_id`, which is
    /// exactly `amount` bytes and never less than one, then answer with
    /// [`H2ReadOutcome::Filled`].
    Fill {
        stream_id: H2StreamId,
        amount: usize,
    },
}

/// What the caller of [`ConnectionH2::poll_read_target`] actually did, handed
/// back to [`ConnectionH2::handle_read`]. Each variant answers the
/// [`H2ReadTarget`] of the same name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum H2ReadOutcome {
    /// Answers [`H2ReadTarget::Skip`]: no read was performed.
    Skipped,
    /// Answers [`H2ReadTarget::Fill`]: `amount` bytes of space were offered,
    /// and the socket returned `size` bytes with `status`.
    ///
    /// `amount` is echoed back rather than re-read from `expect_read` inside
    /// [`ConnectionH2::handle_read`], so the byte debt the core settles is the
    /// one it actually offered and not whatever the table happens to hold by
    /// then.
    Filled {
        amount: usize,
        size: usize,
        status: SocketResult,
    },
}

/// The buffer an [`H2ReadTarget`] names, resolved to the `Kawa` owning it.
///
/// [`H2StreamId::Zero`] is the connection-level scratch every control frame
/// and every header block is read into; [`H2StreamId::Other`] is one
/// application stream's *read* buffer, which [`Stream::split`] reports as the
/// front buffer at [`Position::Server`] and the back buffer at
/// [`Position::Client`].
///
/// Taking `zero` and `streams` apart instead of a whole `&mut ConnectionH2`
/// and `&mut Context` is what makes the split compile: `ConnectionH2::socket`
/// and `Context::debug` stay borrowable while the buffer this returns is live,
/// which a method returning a borrow of all of `*self` would forbid.
fn read_buffer<'a>(
    zero: &'a mut GenericHttpStream,
    streams: &'a mut [Stream],
    position: &Position,
    stream_id: H2StreamId,
) -> &'a mut GenericHttpStream {
    match stream_id {
        H2StreamId::Zero => zero,
        H2StreamId::Other {
            gid: global_stream_id,
            ..
        } => streams[global_stream_id].split(position).rbuffer,
    }
}

/// The exact space [`H2ReadTarget::Fill`] offers its caller: `amount` bytes of
/// [`read_buffer`]'s free storage, never the whole of it.
///
/// The cap is load-bearing. `expect_read` is a byte debt for *one* frame, and
/// [`ConnectionH2::handle_read`] parses everything the read appended as that
/// frame's body — so a caller reading past `amount` would fold the next
/// frame's header into this frame's payload.
fn read_space<'a>(
    zero: &'a mut GenericHttpStream,
    streams: &'a mut [Stream],
    position: &Position,
    stream_id: H2StreamId,
    amount: usize,
) -> &'a mut [u8] {
    &mut read_buffer(zero, streams, position, stream_id)
        .storage
        .space()[..amount]
}

/// What [`ConnectionH2::refuse_stream_and_discard`] hands the `H2State::Discard`
/// arm of [`ConnectionH2::handle_read`] to locate a refused stream's HPACK field
/// block, so the connection decoder can still be advanced before the bytes are
/// dropped (RFC 9113 §4.3).
///
/// RFC 9113 §6.10: only a HEADERS frame carries PADDED/PRIORITY — a
/// CONTINUATION never does. `New` therefore keeps the whole flag byte so the
/// fragment can be located inside the raw payload the same way
/// [`parser::headers_frame`] would. `Continuation` already knows everything
/// accumulated from every *prior* frame in the block (copied out eagerly,
/// see the call site in `handle_continuation_header_state` for why it cannot
/// be a byte offset into `zero.storage` instead) and only needs this frame's
/// own END_HEADERS bit.
#[derive(Debug)]
enum DiscardedFieldBlock {
    /// A brand-new stream's initiating HEADERS frame was refused whole
    /// (drain, MAX_CONCURRENT_STREAMS, or buffer-pool exhaustion).
    New { flags: u8 },
    /// A later CONTINUATION frame in an in-progress header block was refused
    /// (CVE-2024-27316 flood mitigation). `prior_fragment` is every field-block
    /// byte accumulated before this (about to be refused) frame's own payload,
    /// which is still unread at capture time and gets appended once it lands
    /// in `zero.storage` at Discard time.
    Continuation {
        prior_fragment: Vec<u8>,
        end_headers: bool,
    },
}

/// Decode a refused stream's HPACK field block into `decoder` so the
/// connection-level dynamic table stays in sync with the peer's encoder (RFC
/// 9113 §4.3) — no header pair is kept, the stream itself was already
/// refused and its headers are never used. `payload` is the just-read bytes
/// sitting in `zero.storage` at the moment `H2State::Discard` is reached
/// (this frame's own payload only — see [`DiscardedFieldBlock::Continuation`]
/// for how any earlier frames' bytes are threaded in).
///
/// A free function rather than a `ConnectionH2` method: the caller in the
/// `H2State::Discard` arm of [`ConnectionH2::handle_read`] already holds
/// `zero.storage` borrowed as `kawa`, and a method needing the whole
/// `&mut self` would conflict with that borrow. Taking `decoder` and
/// `payload` as disjoint parameters keeps the borrow legal.
fn decode_discarded_field_block(
    decoder: &mut loona_hpack::Decoder<'static>,
    payload: &[u8],
    discarded: DiscardedFieldBlock,
) -> Result<(), H2Error> {
    match discarded {
        DiscardedFieldBlock::New { flags } => {
            // Reparse using the same grammar as an accepted HEADERS frame
            // (RFC 9113 §6.2): [Pad Length][Stream Dependency+Weight][field
            // block fragment][padding]. `payload` is exactly this frame's
            // whole wire payload — `refuse_stream_and_discard` was handed
            // `header.payload_len` before any padding/priority stripping —
            // so the field block has to be located the same way
            // `parser::headers_frame` locates it for an accepted stream.
            let header = FrameHeader {
                payload_len: payload.len() as u32,
                frame_type: FrameType::Headers,
                flags,
                stream_id: 0,
            };
            let (_, frame) =
                parser::headers_frame(payload, &header).map_err(|_| H2Error::ProtocolError)?;
            let Frame::Headers(headers) = frame else {
                unreachable!("parser::headers_frame always yields Frame::Headers")
            };
            // Correction: when END_HEADERS is not set, the block continues
            // on a CONTINUATION frame that (per `handle_header_state`) can
            // now only arrive as a standalone frame and is therefore
            // rejected as a connection error of type PROTOCOL_ERROR before
            // any further bytes of this block are read. There is nothing
            // to decode yet, and decoding a fragment that ends mid-integer
            // or mid-Huffman-string would misreport COMPRESSION_ERROR in
            // place of that PROTOCOL_ERROR.
            if !headers.end_headers {
                return Ok(());
            }
            let fragment = headers
                .header_block_fragment
                .data_opt(payload)
                .ok_or(H2Error::InternalError)?;
            decoder
                .decode_with_cb(fragment, |_, _| {})
                .map_err(|_| H2Error::CompressionError)
        }
        DiscardedFieldBlock::Continuation {
            mut prior_fragment,
            end_headers,
        } => {
            // Same END_HEADERS gate as above, evaluated on this trailing
            // CONTINUATION frame's own flags rather than the (still false)
            // flag the originating HEADERS frame carried.
            if !end_headers {
                return Ok(());
            }
            prior_fragment.extend_from_slice(payload);
            decoder
                .decode_with_cb(&prior_fragment, |_, _| {})
                .map_err(|_| H2Error::CompressionError)
        }
    }
}

impl<Front: SocketHandler> ConnectionH2<Front> {
    fn frontend_hung_up_while_draining(&self) -> bool {
        matches!(self.position, Position::Server)
            && self.drain.draining()
            && (self.readiness.event.is_hup() || self.readiness.event.is_error())
    }

    /// Once the final GOAWAY has been queued and all streams/control frames are
    /// gone, a peer-side HUP/ERR means any remaining rustls backlog is no
    /// longer deliverable. Waiting on `socket_wants_write()` in that state can
    /// deadlock shutdown forever because GOAWAY disables further frame reads.
    fn peer_gone_after_final_goaway(&self) -> bool {
        self.frontend_hung_up_while_draining()
            && matches!(self.state, H2State::GoAway | H2State::Error)
            && self.stream_table.streams().is_empty()
            && self.stream_table.expect_write().is_none()
            && self.zero.storage.is_empty()
    }

    /// The next instant this connection wants its embedder to call `timeout()`
    /// at, or `None` for "no timer". The adapter reflects this onto the real
    /// wheel; nothing in this module touches `crate::timer`.
    pub(super) fn poll_timeout(&self) -> Option<Instant> {
        self.timeout_deadline
    }

    /// Push the deadline one full [`Self::timeout_duration`] out from the
    /// connection's clock snapshot. Replaces the old
    /// `TimeoutContainer::reset()` at the same three call sites, and reads
    /// `self.now` rather than the real clock (invariant 20).
    pub(super) fn arm_timeout(&mut self) {
        self.timeout_deadline = self.now.checked_add(self.timeout_duration);
    }

    /// Ask for no timer at all. Replaces `TimeoutContainer::cancel()`.
    pub(super) fn clear_timeout(&mut self) {
        self.timeout_deadline = None;
    }

    /// Adopt a new configured duration and re-arm from `now`.
    ///
    /// Mirrors `TimeoutContainer::set_duration`, which likewise cancelled and
    /// re-armed rather than keeping the old deadline. `now` is the adapter's
    /// snapshot: this is called from `Mux::ready`, outside the entry points
    /// that mirror it into `self.now`.
    pub(super) fn set_timeout_duration(&mut self, duration: Duration, now: Instant) {
        self.timeout_duration = duration;
        self.timeout_deadline = now.checked_add(duration);
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
        timeout_duration: std::time::Duration,
        expect_read: Option<(H2StreamId, usize)>,
        readiness_interest: sozu_command::ready::Ready,
    ) -> Option<Self> {
        let buffer = pool
            .upgrade()
            .and_then(|pool| pool.borrow_mut().checkout())?;
        // The one clock sample this module takes outside `Mux`'s sampling
        // points. A connection is constructed at accept / backend-connect
        // time, outside any `ready()` pass, so there is no snapshot to
        // inherit; everything downstream of here reads `self.now`, which
        // `Mux` refreshes from its next pass onward.
        let now = Instant::now();
        let local_settings = H2Settings {
            settings_max_concurrent_streams: connection_config.max_concurrent_streams,
            ..H2Settings::default()
        };
        // RFC 7541 §4.2: enforce SETTINGS_HEADER_TABLE_SIZE as the upper bound
        // for dynamic table size updates from the peer
        let hpack =
            hpack_state::HpackState::new(local_settings.settings_header_table_size as usize);
        Some(ConnectionH2 {
            session_ulid,
            hpack,
            stream_table: h2_stream_table::H2StreamTable::new(expect_read),
            last_stream_id: 0,
            local_settings,
            peer_settings: H2Settings::default(),
            position,
            scheduler: h2_scheduler::H2Scheduler::default(),
            readiness: crate::Readiness {
                interest: readiness_interest,
                event: Ready::EMPTY,
            },
            // The one read of `peer_addr()` on this connection's whole
            // lifetime. Taken before `socket` is moved into the struct.
            peer_address: socket.peer_addr(),
            socket,
            state: H2State::ClientPreface,
            timeout_duration,
            // Armed from construction, exactly as the old `TimeoutContainer`
            // was: the frontend arrived already armed from the handshake state,
            // and `Router::connect` armed a fresh backend with the connect
            // timeout right after registering its socket. The adapter reflects
            // this onto the wheel on its next reschedule.
            timeout_deadline: now.checked_add(timeout_duration),
            flow_control: h2_flow_control::H2FlowControl::new(DEFAULT_INITIAL_WINDOW_SIZE as i32),
            pending_table_size_update: None,
            drain: h2_drain::H2DrainState::new(graceful_shutdown_deadline),
            zero: kawa::Kawa::new(kawa::Kind::Request, kawa::Buffer::new(buffer)),
            header_reassembly: h2_header_reassembly::HeaderBlockAccumulator::new(),
            bytes: H2ByteAccounting {
                zero_bytes_read: 0,
                overhead_bin: 0,
                overhead_bout: 0,
            },
            flood_detector: h2_flood_detector::H2FloodDetector::new(flood_config, now),
            settings_sent_at: None,
            control_tx: h2_control_tx::H2ControlTx::new(),
            discarded_field_block: None,
            close_notify_sent: false,
            max_pending_window_updates: 1 + connection_config.max_concurrent_streams as usize * 4,
            connection_config,
            ready_incremental_streams: 0,
            last_gauge_snapshot: None,
            stream_idle_timeout,
            refuse_count_window: 0,
            refuse_window_start: now,
            mcs_backpressure_applied: false,
            now,
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
        self.stream_table
            .set_expect_read(Some((H2StreamId::Zero, 9)));
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
                } else if let Some(global_stream_id) = self.stream_table.streams().get(&stream_id) {
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
                        // `graceful_goaway` marks the connection draining
                        // through `H2DrainState::begin_graceful_drain`, then
                        // sends an initial GOAWAY with last_stream_id =
                        // STREAM_ID_MAX (so in-flight requests are still
                        // accepted); *new* peer streams must still be refused.
                        // Without this check, a peer racing the drain
                        // window could open arbitrary new streams between
                        // the initial and final GOAWAY emission.
                        if self.drain.draining() {
                            self.stream_table.observe_peer_stream_id(stream_id);
                            return self.refuse_stream_and_discard(
                                stream_id,
                                H2Error::RefusedStream,
                                header.payload_len,
                                DiscardedFieldBlock::New {
                                    flags: header.flags,
                                },
                            );
                        }
                        if self.stream_table.len()
                            >= self.local_settings.settings_max_concurrent_streams as usize
                        {
                            error!(
                                "{} MAX CONCURRENT STREAMS: limit={}, current={}",
                                log_context!(self),
                                self.local_settings.settings_max_concurrent_streams,
                                self.stream_table.len()
                            );
                            // RFC 9113 §6.8: update highest_peer_stream_id BEFORE
                            // queueing RST_STREAM so GOAWAY reports the correct
                            // last_stream_id if the connection closes later.
                            self.stream_table.observe_peer_stream_id(stream_id);
                            return self.refuse_stream_and_discard(
                                stream_id,
                                H2Error::RefusedStream,
                                header.payload_len,
                                DiscardedFieldBlock::New {
                                    flags: header.flags,
                                },
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
                                self.stream_table.observe_peer_stream_id(stream_id);
                                return self.refuse_stream_and_discard(
                                    stream_id,
                                    H2Error::RefusedStream,
                                    header.payload_len,
                                    DiscardedFieldBlock::New {
                                        flags: header.flags,
                                    },
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
                            header.stream_id <= self.stream_table.highest_peer_stream_id()
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
                                    self.flood_detector.record_glitch();
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
                                    self.flood_detector.record_glitch();
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
                    self.stream_table.streams()
                );
                self.stream_table
                    .set_expect_read(Some((read_stream, header.payload_len as usize)));
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
                // The 9-byte CONTINUATION frame header has now been fully
                // parsed from `zero.storage`; nothing about it needs to
                // survive the transition below. Earlier revisions kept the
                // buffer's trailing bytes alive with a manual
                // `storage.end -= 9` so the next payload read would land
                // contiguously after the accumulated fragment inside
                // `zero.storage` itself — that trick existed only to serve
                // the old design where the fragment lived here. It now
                // lives in `self.header_reassembly` (h2_header_reassembly.rs),
                // so there is nothing left to stay contiguous with.
                self.zero.storage.clear();
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
                self.flood_detector.record_continuation_frame(payload_len);
                check_flood_or_return!(self);
                // RFC 9113 §10.5.1: reject header blocks that cannot be
                // buffered. Previously we silently removed READABLE interest
                // when amount > available_space, stalling the connection.
                // If the payload still fits in our zero buffer we can refuse
                // just this stream (RST_STREAM + drain); if not, the
                // connection can no longer decode header blocks safely and we
                // escalate to GOAWAY(EnhanceYourCalm).
                if self.flood_detector.accumulated_header_size()
                    > self.flood_detector.config().max_header_list_size()
                {
                    error!(
                        "{} CONTINUATION accumulated header size {} exceeds {}",
                        log_context!(self),
                        self.flood_detector.accumulated_header_size(),
                        self.flood_detector.config().max_header_list_size()
                    );
                    if (payload_len as usize) > self.zero.storage.available_space() {
                        return self.goaway(H2Error::EnhanceYourCalm);
                    }
                    // Remove the already-created stream slot before refusing,
                    // so it does not leak against MAX_CONCURRENT_STREAMS. Route
                    // through `remove_dead_stream` so the expect_write/read
                    // invariant (§LIFECYCLE.md 5.4) holds on this path too.
                    if let Some(global_stream_id) = self.stream_table.get(stream_id) {
                        self.remove_dead_stream(stream_id, global_stream_id);
                    }
                    // The field-block bytes accumulated by every *prior*
                    // frame in this block already live in
                    // `self.header_reassembly`, decoupled from
                    // `zero.storage`'s own bookkeeping — no eager copy is
                    // needed here the way the old single-buffer design
                    // required (the buffer this comment used to warn about
                    // reuse of is `zero.storage`; that reuse now targets an
                    // empty, unrelated scratch region, not this fragment).
                    let prior_fragment = self.header_reassembly.finish();
                    return self.refuse_stream_and_discard(
                        stream_id,
                        H2Error::RefusedStream,
                        payload_len,
                        DiscardedFieldBlock::Continuation {
                            prior_fragment,
                            end_headers: flags & parser::FLAG_END_HEADERS != 0,
                        },
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
                self.stream_table
                    .set_expect_read(Some((H2StreamId::Zero, payload_len as usize)));
                let mut headers = headers.clone();
                headers.end_headers = flags & parser::FLAG_END_HEADERS != 0;
                // `header_block_fragment`'s window is no longer extended
                // here: it described an offset into `zero.storage`, which
                // this CONTINUATION frame's payload has not been read into
                // yet. `(H2State::ContinuationFrame(headers), _)` folds this
                // frame's payload into `self.header_reassembly` once it has
                // actually been read, and `handle_headers_frame` reads the
                // accumulator directly rather than through this field.
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

    /// Ask the core what it wants read from the socket, so the caller can
    /// perform that read and report it back through [`Self::handle_read`].
    ///
    /// This is the read half of the same **two-call protocol** the write half
    /// already uses: `h2_transmit::gather` borrows the bytes to send, the
    /// caller writes them, `h2_transmit::confirm` is told how many landed.
    /// Here the core names the buffer it wants filled, the caller
    /// performs the read, and [`Self::handle_read`] is told how many bytes
    /// arrived and with what status.
    ///
    /// **This is not `AsyncRead::poll_read`.** Nothing here is a future,
    /// `context` is this module's [`Context`] and not a `task::Context`, and
    /// `lib/` holds no asynchronous function. The name follows
    /// `UdpManager::poll_output` (`protocol/udp/manager.rs`), this
    /// repository's existing spelling for "ask the core what it has for its
    /// caller", the way [`Self::handle_read`] follows that module's
    /// `UdpManager::handle_input`.
    ///
    /// Everything up to and including the decision of *which* buffer needs
    /// *how many* bytes lives on this side of the split, because two of the
    /// three answers are produced by that prelude: a SETTINGS-ACK timeout and
    /// an idle `expect_read` both end the pass with no read at all. A poll
    /// that could not say "nothing" would not be the core's answer.
    pub(super) fn poll_read_target<E, L>(
        &mut self,
        context: &mut Context<L>,
        endpoint: &mut E,
    ) -> H2ReadTarget
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        // Entry point: adopt the mux's snapshot for this pass.
        self.now = context.now;
        self.prune_inactive_streams_while_closing(context);
        // Pass 4 Medium #3: per-stream idle guard. Slow-multiplex Slowloris
        // sends one byte or a control frame per stream just often enough to
        // reset the connection-level timer; per-stream deadlines catch it.
        self.cancel_timed_out_streams(context, endpoint);

        // RFC 9113 §6.5: check if peer has timed out on SETTINGS ACK
        if let Some(sent_at) = self.settings_sent_at
            && self.now.saturating_duration_since(sent_at) >= SETTINGS_ACK_TIMEOUT
        {
            warn!(
                "{} SETTINGS ACK timeout: no SETTINGS ACK observed within {:?}",
                log_context!(self),
                SETTINGS_ACK_TIMEOUT
            );
            return H2ReadTarget::Done(self.goaway(H2Error::SettingsTimeout));
        }

        // Don't reset the timeout unconditionally here. Only application data
        // (DATA/HEADERS frames) should reset the timeout. H2 control frames
        // (PING, WINDOW_UPDATE, SETTINGS) must NOT reset it, otherwise a peer
        // sending periodic PINGs prevents timeout detection on stuck sessions.
        // The timeout is reset:
        // - Below, when reading DATA payload (H2StreamId::Other)
        // - In handle_frame(), when processing HEADERS frames
        let Some((stream_id, amount)) = self.stream_table.expect_read() else {
            self.readiness.event.remove(Ready::READABLE);
            return H2ReadTarget::Done(MuxResult::Continue);
        };
        match stream_id {
            H2StreamId::Zero => {}
            H2StreamId::Other { .. } => {
                // Reading DATA frame payload for an application stream.
                // This is real application activity — reset the timeout.
                self.arm_timeout();
            }
        }
        let kawa = read_buffer(
            &mut self.zero,
            &mut context.streams,
            &self.position,
            stream_id,
        );
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
                return H2ReadTarget::Done(MuxResult::Continue);
            }
            H2ReadTarget::Fill { stream_id, amount }
        } else {
            self.stream_table.set_expect_read(None);
            H2ReadTarget::Skip(stream_id)
        }
    }

    /// Tell the core what the caller's read actually did, then let it consume
    /// the bytes and dispatch the frame they completed.
    ///
    /// The second half of [`Self::poll_read_target`]'s protocol. `stream_id`
    /// is the one the matching [`H2ReadTarget`] named, and `outcome` answers
    /// that same variant: [`H2ReadOutcome::Skipped`] for
    /// [`H2ReadTarget::Skip`], [`H2ReadOutcome::Filled`] for
    /// [`H2ReadTarget::Fill`].
    ///
    /// `read_buffer` is called a second time here rather than carried across
    /// the split: `StreamParts` borrows `context`, so a buffer held across the
    /// caller's read would pin `context.debug` and `Self::handle_frame`'s whole
    /// `&mut Context` with it. Re-deriving it from a `Copy` [`H2StreamId`] is
    /// a match and a field projection.
    fn handle_read<E, L>(
        &mut self,
        context: &mut Context<L>,
        endpoint: E,
        stream_id: H2StreamId,
        outcome: H2ReadOutcome,
    ) -> MuxResult
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        let kawa = read_buffer(
            &mut self.zero,
            &mut context.streams,
            &self.position,
            stream_id,
        );
        match outcome {
            H2ReadOutcome::Skipped => {}
            H2ReadOutcome::Filled {
                amount,
                size,
                status,
            } => {
                let did = match stream_id {
                    H2StreamId::Zero => usize::MAX,
                    H2StreamId::Other {
                        gid: global_stream_id,
                        ..
                    } => global_stream_id,
                };
                context.debug.push(DebugEvent::SocketIO(0, did, size));
                kawa.storage.fill(size);
                self.position.count_bytes_in_counter(size);
                self.bytes.zero_bytes_read += size;
                if update_readiness_after_read(size, status, &mut self.readiness) {
                    if matches!(self.position, Position::Server)
                        && self.drain.draining()
                        && matches!(status, SocketResult::Closed | SocketResult::Error)
                    {
                        // During graceful drain, a frontend EOF/HUP means no
                        // further frame headers or payload bytes can arrive.
                        // Keeping expect_read here strands the connection in
                        // Header/Frame forever even after the peer is gone.
                        self.stream_table.set_expect_read(None);
                    }
                    return MuxResult::Continue;
                } else if size == amount {
                    self.stream_table.set_expect_read(None);
                } else {
                    self.stream_table
                        .set_expect_read(Some((stream_id, amount - size)));
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
            }
        }
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
                let i = kawa.storage.data();
                trace!("{} DISCARDING: {:?}", log_context!(self), i);
                // RFC 9113 §4.3: HPACK field-compression state is scoped to
                // the connection, not the stream. `i` is this refused
                // frame's own just-read payload; decode it (folding in any
                // earlier frames' bytes for a CONTINUATION refusal — see
                // `DiscardedFieldBlock`) before it is dropped, so our
                // decoder does not fall behind the peer's encoder.
                if let Some(discarded) = self.discarded_field_block.take()
                    && let Err(error) =
                        decode_discarded_field_block(self.hpack.decoder_mut(), i, discarded)
                {
                    error!(
                        "{} discarded stream's HPACK field block failed to decode: {:?}",
                        log_context!(self),
                        error
                    );
                    return self.goaway(error);
                }
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
                        self.stream_table
                            .set_expect_read(Some((H2StreamId::Zero, payload_len as usize)));
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
                        incr!(names::h2::FRAMES_TX_SETTINGS);
                        // RFC 9113 §6.5: start tracking SETTINGS ACK timeout
                        self.settings_sent_at = Some(self.now);
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
                self.stream_table.set_expect_write(Some(H2StreamId::Zero));
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
                        self.stream_table
                            .set_expect_read(Some((H2StreamId::Zero, payload_len as usize)));
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
                    // Free `zero.storage` for the next frame header read. A
                    // HEADERS frame used to be special-cased here
                    // (`head = end`, preserving its payload bytes in place
                    // so a following CONTINUATION could extend them
                    // in-buffer) — the reassembly accumulator that needed
                    // now lives in `self.header_reassembly` instead (see
                    // `handle_headers_frame`), copied out before this read
                    // cycle ends, so every zero-stream frame gets the same
                    // treatment.
                    kawa.storage.end = kawa.storage.head;
                }
                self.expect_header();
                return self.handle_frame(frame, wire_payload_len, context, endpoint);
            }
            (H2State::ContinuationFrame(headers), _) => {
                // This CONTINUATION frame's payload has just finished being
                // read into `zero.storage` (the generic stream-0 read
                // dispatch above, shared by every zero-stream frame type).
                // Move it into the owned reassembly accumulator and free
                // `zero.storage` immediately — from this point until the
                // next CONTINUATION's bytes start arriving, `zero.storage`
                // holds nothing belonging to this header block. See
                // `h2_header_reassembly.rs` and LIFECYCLE.md invariant 24.
                self.header_reassembly.append(kawa.storage.data());
                kawa.storage.clear();
                let headers = headers.clone();
                self.expect_header();
                return self.handle_frame(Frame::Headers(headers), 0, context, endpoint);
            }
        }
        MuxResult::Continue
    }

    /// Drive one frontend read pass.
    ///
    /// The core lives in [`Self::poll_read_target`] and [`Self::handle_read`];
    /// this function is the caller that sits between them, and its
    /// `self.socket.socket_read` is the only socket touch on the whole H2 read
    /// path. Keeping it *here* rather than inside the core is the point of the
    /// split: a later change can move this body next to the socket without
    /// reopening the frame state machine, exactly as
    /// `h2_transmit::gather`/`confirm` already bracket the vectored write.
    pub fn readable<E, L>(&mut self, context: &mut Context<L>, mut endpoint: E) -> MuxResult
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        match self.poll_read_target(context, &mut endpoint) {
            H2ReadTarget::Done(result) => result,
            H2ReadTarget::Skip(stream_id) => {
                self.handle_read(context, endpoint, stream_id, H2ReadOutcome::Skipped)
            }
            H2ReadTarget::Fill { stream_id, amount } => {
                let space = read_space(
                    &mut self.zero,
                    &mut context.streams,
                    &self.position,
                    stream_id,
                    amount,
                );
                let (size, status) = self.socket.socket_read(space);
                self.handle_read(
                    context,
                    endpoint,
                    stream_id,
                    H2ReadOutcome::Filled {
                        amount,
                        size,
                        status,
                    },
                )
            }
        }
    }

    /// Update the H2 connection-level *aggregate* gauges with this connection's
    /// current contribution, expressed as a signed delta against the last
    /// snapshot we emitted.
    ///
    /// The four metrics are emitted via [`gauge_add!`] (lifecycle deltas) so
    /// that the dashboard sees the **sum across all live H2 connections**:
    ///
    /// - `h2.connection.window_bytes` — sum of available connection-level
    ///   send-window bytes. Negative per-connection windows clamp to 0 so the
    ///   aggregate represents only available capacity, not deficit.
    /// - `h2.connection.active_streams` — sum of in-flight streams across
    ///   every H2 connection.
    /// - `h2.connection.pending_window_updates` — sum of queued (un-flushed)
    ///   per-stream WINDOW_UPDATE entries across every H2 connection.
    /// - `h2.streams.ready_incremental.by_urgency` — sum of
    ///   [`Self::ready_incremental_streams`] across every H2 connection.
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
            self.flow_control.window().max(0) as usize,
            self.stream_table.streams().len(),
            self.flow_control.pending_window_updates_len(),
            self.ready_incremental_streams,
        );
        if self.last_gauge_snapshot == Some(snapshot) {
            return;
        }
        let prev = self.last_gauge_snapshot.unwrap_or((0, 0, 0, 0));
        // Diff in i64 — usize cannot represent the negative side of the delta.
        let dw = snapshot.0 as i64 - prev.0 as i64;
        let ds = snapshot.1 as i64 - prev.1 as i64;
        let du = snapshot.2 as i64 - prev.2 as i64;
        let dr = snapshot.3 as i64 - prev.3 as i64;
        if dw != 0 {
            gauge_add!(names::h2::CONNECTION_WINDOW_BYTES, dw);
        }
        if ds != 0 {
            gauge_add!(names::h2::CONNECTION_ACTIVE_STREAMS, ds);
        }
        if du != 0 {
            gauge_add!(names::h2::CONNECTION_PENDING_WINDOW_UPDATES, du);
        }
        if dr != 0 {
            gauge_add!(names::h2::STREAMS_READY_INCREMENTAL_BY_URGENCY, dr);
        }
        self.last_gauge_snapshot = Some(snapshot);
    }

    /// Subtract this connection's contribution from the four aggregate gauges
    /// [`Self::gauge_connection_state`] feeds. Idempotent: clears
    /// `last_gauge_snapshot` so a second call (or a [`Drop`] on top of an
    /// explicit reset) is a no-op.
    ///
    /// Pairs with every prior call to [`Self::gauge_connection_state`]; called
    /// from [`Drop`] so the symmetry is guaranteed regardless of the close
    /// path.
    fn release_connection_gauges(&mut self) {
        if let Some((w, s, u, r)) = self.last_gauge_snapshot.take() {
            if w != 0 {
                gauge_add!(names::h2::CONNECTION_WINDOW_BYTES, -(w as i64));
            }
            if s != 0 {
                gauge_add!(names::h2::CONNECTION_ACTIVE_STREAMS, -(s as i64));
            }
            if u != 0 {
                gauge_add!(names::h2::CONNECTION_PENDING_WINDOW_UPDATES, -(u as i64));
            }
            if r != 0 {
                gauge_add!(names::h2::STREAMS_READY_INCREMENTAL_BY_URGENCY, -(r as i64));
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
    /// The [`converter::H2BlockConverter`] is scoped to a single
    /// `kawa.prepare` call rather than to the whole per-stream loop, so the
    /// `&mut self.hpack` borrow it takes for the connection's HPACK encoder
    /// never spans the loop. Every `&self` / `&mut self` method is therefore
    /// callable inside the loop body; what remains deferred to after it —
    /// RST accounting and stream retirement — is deferred for its own
    /// ordering reasons, documented at each site.
    /// [`converter::H2ConverterPass`] carries the scratch buffers and the
    /// RFC 7541 §6.3 size-update signal from one stream's `prepare` to the
    /// next by moving them, so the narrower scope costs no copy.
    fn write_streams<E, L>(&mut self, context: &mut Context<L>, mut endpoint: E) -> MuxResult
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        self.arm_timeout();
        // Pre-compute byte totals for proportional overhead distribution.
        let byte_totals = self.compute_stream_byte_totals(context);
        let mut io_slices: Vec<IoSlice<'static>> = Vec::new();

        if let Some(
            write_stream @ H2StreamId::Other {
                id: stream_id,
                gid: global_stream_id,
            },
        ) = self.stream_table.expect_write()
        {
            let stream = &mut context.streams[global_stream_id];
            let stream_state = stream.state;
            let parts = stream.split(&self.position);
            let kawa = parts.wbuffer;
            // Resume path: if the same stream is parked waiting for buffer
            // space (expect_read matches write_stream), pass the amount so
            // flush_stream_out can re-enable READABLE as soon as we drain.
            let cross_read_amount = match self.stream_table.expect_read() {
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
                self.stream_table.touch_activity(stream_id, self.now);
                // Clear the flow-control-stall deadline ONLY when the effective
                // send window is genuinely open — that alone is a real un-stall.
                // A window-stalled stream can flush a `WINDOW_UPDATE(+1)`-drip
                // byte HERE via socket-backpressure resume; clearing on that
                // would reset the deadline at 1-byte granularity and re-open the
                // drip the M2 cumulative-stall budget closes. While still blocked,
                // leave the deadline (and its progress accumulator) for the main
                // write loop's budget to govern — keeping the two maps in lockstep.
                if min(*parts.window, self.flow_control.window()) > 0 {
                    self.stream_table.clear_fc_stall(stream_id);
                }
            }
            if outcome == FlushOutcome::Stalled {
                return MuxResult::Continue;
            }
            self.stream_table.set_expect_write(None);
            if (kawa.is_terminated() || kawa.is_error())
                && kawa.is_completed()
                && !Self::handle_1xx_reset(kawa, stream_state, &mut endpoint)
            {
                let (client_rtt, server_rtt) = self.snapshot_rtts(&endpoint, stream.linked_token());

                if let Some((dead_id, token)) = self.try_recycle_server_stream(
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
        // The converter is built for ONE `kawa.prepare` call at a time (see
        // [`converter::H2ConverterPass`]), so its encoder borrow never spans
        // the per-stream loop and every `&self` / `&mut self` method stays
        // callable inside it. The pass carries the three reusable scratch
        // buffers — moved, never copied — plus the RFC 7541 §6.3 pending
        // size-update, so the first header block of this pass prepends the
        // signal and no later one repeats it. We clear the connection-side
        // mirror only AFTER the pass confirms emission via
        // `pass.size_update_emitted()`, so a DATA-only write pass (no header
        // block) does not drop the signal.
        let mut pass = converter::H2ConverterPass::new(
            self.peer_settings.settings_max_frame_size as usize,
            scheme,
            // When this connection is a backend client we are writing
            // toward the upstream backend — flow-control stalls in that
            // direction are scoped to `backend.flow_control.paused` (in
            // addition to the existing direction-agnostic
            // `h2.flow_control_stall`).
            self.position.is_client(),
            self.hpack.take_converter_buf(),
            self.hpack.take_lowercase_buf(),
            self.hpack.take_cookie_buf(),
            self.pending_table_size_update,
        );
        // The whole RFC 9218 ordering decision — ascending urgency, stream
        // id for stability, incremental streams to the tail of their bucket,
        // that tail rotated by the round-robin cursor — plus the pass's
        // same-urgency ready-peer census, belongs to
        // [`h2_scheduler::H2Scheduler`]. It runs over state that module
        // owns; the ONE fact it cannot own is whether a stream has anything
        // to send, which it takes as this closure. The closure is called for
        // incremental streams only, so it costs exactly what the inline
        // census cost. `order` is returned by value for the same reason the
        // scratch buffers are moved into the converter pass: the per-stream
        // loop below re-borrows the encoder out of `self.hpack` for every
        // eligible stream, so no borrow of a connection field may span it.
        let is_server = matches!(self.position, Position::Server);
        let (order, mut census) =
            self.scheduler
                .begin_pass(self.stream_table.streams().keys().copied(), |stream_id| {
                    let Some(&gid) = self.stream_table.streams().get(&stream_id) else {
                        return false;
                    };
                    let wbuffer = if is_server {
                        &context.streams[gid].back
                    } else {
                        &context.streams[gid].front
                    };
                    wbuffer.is_main_phase()
                        || (wbuffer.is_terminated() && !wbuffer.is_completed())
                        || (wbuffer.is_error() && !self.stream_table.rst_sent_contains(stream_id))
                });

        trace!(
            "{} PRIORITIES: {:?} (incremental_count={}, per_bucket={:?})",
            log_context!(self),
            order,
            census.incremental_count(),
            census.ready_buckets()
        );
        let mut socket_write = false;
        // Total outbound bytes emitted across all stream flushes this pass —
        // `finalize_write` uses this to distinguish a voluntary scheduler
        // yield (progress + pending back-buffer, LIFECYCLE §9 invariant 16)
        // from a no-progress wait state (e.g. flow-control starvation).
        let mut total_bytes_written: usize = 0;
        // Collect every fresh RST_STREAM emitted via the converter
        // (`initialize` chokepoint or the HPACK over-budget abort path)
        // so we can run `account_emitted_rst` for each one AFTER the loop.
        // This is an ORDERING requirement, not a borrow workaround: a
        // MadeYouReset cap trip makes `account_emitted_rst` return a GOAWAY
        // result that ends the pass, and every stream in the pass's
        // `order` must get its write before that preemption.
        let mut freshly_emitted_rsts: Vec<H2Error> = Vec::new();
        'outer: for &stream_id in &order {
            let Some(&global_stream_id) = self.stream_table.streams().get(&stream_id) else {
                error!(
                    "{} stream_id {} from sorted keys missing in streams map",
                    log_context!(self),
                    stream_id
                );
                continue;
            };
            let (urgency, is_incremental) = self.scheduler.priority(&stream_id);
            let stream = &mut context.streams[global_stream_id];
            let stream_state = stream.state;
            let parts = stream.split(&self.position);
            let kawa = parts.wbuffer;
            // Hoisted out of the gate below so the post-flush flow-control-stall
            // classification can see how many flow-control bytes this pass moved.
            let mut consumed: i32 = 0;
            if kawa.is_main_phase()
                || (kawa.is_terminated() && !kawa.is_completed())
                || (kawa.is_error() && !self.stream_table.rst_sent_contains(stream_id))
            {
                let window = min(*parts.window, self.flow_control.window());
                // Same-urgency-bucket ready-peer count (Tier 3a, LIFECYCLE §9
                // invariant 17). The converter skips the yield when there is
                // no peer in the same bucket to interleave with — prevents
                // the `finalize_write` WRITABLE-withdrawal strand (see
                // `test_h2_solo_incremental_drains_fully`). A connection-wide
                // count would wrongly yield for a solo incremental stream
                // when another urgency bucket happens to contain an
                // incremental peer.
                let incremental_peer_count = census.incremental_peer_count(urgency);
                // Track RST_STREAM dedup: if kawa is in error state, the converter
                // will generate a RST_STREAM frame via `initialize`. Mark it so we
                // don't send a duplicate on the next writable cycle.
                if kawa.is_error() {
                    let freshly_rst = self.stream_table.rst_sent_mut().insert(stream_id);
                    // LIFECYCLE §9 invariant 17: any transition to ineligible
                    // mid-pass MUST leave the pass census so later streams in
                    // the same 'outer iteration see the live count, not the
                    // snapshot. Missing this costs one voluntary yield per
                    // same-urgency peer that trails the RST.
                    if freshly_rst {
                        census.note_ineligible(urgency, is_incremental);
                    }
                    // Account for the RST that `initialize` is about to emit
                    // for this stream. Without this the MadeYouReset lifetime
                    // cap is evadable: any path that flips `parsing_phase` to
                    // Error before reaching this gate (oversized inbound
                    // trailers, malformed bodies, etc.) would land an
                    // unaccounted RST on the wire. The accounting call is
                    // deferred to after the loop so a cap trip cannot
                    // preempt the remaining streams' writes; see
                    // `freshly_emitted_rsts` above.
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
                //
                // The snapshot is **drained** via `mem::take` so the
                // injection runs exactly once per response. Without
                // this, a re-entry of `write_streams` for the same
                // stream (multi-frame body, flow-control yield, or
                // RFC 9218 same-urgency round-robin) would re-call
                // `apply_response_header_edits` after `kawa.prepare`
                // had already consumed the `Block::Flags{end_header}`
                // anchor — the helper falls back to
                // `kawa.blocks.len()` and appends the edit AFTER all
                // remaining DATA blocks. The next prepare cycle then
                // encodes that orphan `Block::Header` into
                // `H2BlockConverter.out` with no closing
                // `Block::Flags{end_header}` to flush it as a HEADERS
                // frame, and `H2BlockConverter::finalize` trips the
                // "out buffer not empty (38 bytes remaining), clearing"
                // defense-in-depth log on every re-entry. 38 bytes is
                // the static-table HPACK encoding of a typical HSTS
                // header, which is how the symptom surfaces in
                // production once the listener-default HSTS reaches a
                // non-trivial share of frontends.
                if matches!(self.position, super::Position::Server)
                    && !parts.context.headers_response.is_empty()
                {
                    let edits = std::mem::take(&mut parts.context.headers_response);
                    super::shared::apply_response_header_edits(kawa, &edits);
                }
                // One converter for exactly this `prepare` call. It borrows
                // the encoder out of `self.hpack`; keeping that borrow no
                // longer than the call is what leaves the rest of this loop
                // body free to take `&self` / `&mut self`. The scratch and
                // the RFC 7541 §6.3 signal are moved in here and moved back
                // out by `reclaim` below — no buffer is copied.
                //
                // RFC 9218 §4: `incremental_mode` makes an incremental
                // stream yield the converter after a single DATA frame so
                // same-urgency peers interleave; a non-scheduled caller
                // (the resume path above, the converter's own unit tests)
                // keeps the sequential semantics with `false`.
                let mut converter = pass.converter(
                    self.hpack.encoder_mut(),
                    stream_id,
                    window,
                    is_incremental,
                    incremental_peer_count,
                );
                kawa.prepare(&mut converter);
                consumed = window - pass.reclaim(converter);
                // The pre-prepare gate above only inserts into
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
                let freshly_rst_post_prepare =
                    kawa.is_error() && self.stream_table.rst_sent_mut().insert(stream_id);
                if freshly_rst_post_prepare {
                    // Deferred to after the loop; same reason as the
                    // pre-prepare collector above.
                    freshly_emitted_rsts.push(rst_error_from_kawa(kawa));
                    census.note_ineligible(urgency, is_incremental);
                }
                *parts.window = parts.window.saturating_sub(consumed);
                self.flow_control.consume_send_window(consumed);
                census.note_fired(stream_id, is_incremental, consumed);
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
            // inbound-only refreshes in `handle_data_frame` (non-empty DATA)
            // and `handle_headers_frame` never fire while the peer is idle.
            if stream_bytes > 0 {
                self.stream_table.touch_activity(stream_id, self.now);
            }
            // Arm/age the dedicated flow-control-stall deadline that catches a
            // window-stalled stream — a buffered RESPONSE to a slow frontend
            // (`Position::Server`) OR a buffered request UPLOAD to a slow H2
            // backend (`Position::Client`): window-stall reaping is bidirectional
            // by design (M4), so there is no position gate here. Set only when the
            // stream holds sendable buffered data it cannot send because its
            // effective send window is exhausted; unlike `stream_last_activity_at`
            // it is NEVER refreshed by inbound DATA/HEADERS, so a peer dribbling
            // 1-byte DATA cannot keep it warm.
            //
            // M2 cumulative-stall budget: a genuinely OPEN window clears the
            // deadline immediately (real un-stall). While the window stays
            // blocked, accumulate this pass's outbound drain; only cumulative
            // progress reaching `FC_STALL_CLEAR_FLOOR` (a full frame of real
            // delivery) clears it. A `WINDOW_UPDATE(+1)` drip drains ~1 byte/pass
            // straight back to a zero window, so it never reaches the floor — the
            // deadline ages out and `cancel_timed_out_streams` RST(CANCEL)s the
            // slot-pinning stream after `stream_idle_timeout`.
            let outbound_window_blocked = has_sendable_response(kawa)
                && min(*parts.window, self.flow_control.window()) <= 0
                && (!kawa.blocks.is_empty() || !kawa.out.is_empty());
            match fc_stall_budget_decision(
                outbound_window_blocked,
                consumed,
                self.stream_table.fc_stall_progress(stream_id),
            ) {
                FcStallAction::Clear => {
                    self.stream_table.clear_fc_stall(stream_id);
                }
                FcStallAction::Arm { progress } => {
                    self.stream_table
                        .arm_fc_stall(stream_id, self.now, progress);
                }
            }
            total_bytes_written = total_bytes_written.saturating_add(stream_bytes);
            if outcome == FlushOutcome::Stalled {
                self.stream_table.set_expect_write(Some(H2StreamId::Other {
                    id: stream_id,
                    gid: global_stream_id,
                }));
                break 'outer;
            }
            self.stream_table.set_expect_write(None);
            if (kawa.is_terminated() || kawa.is_error())
                && kawa.is_completed()
                && !Self::handle_1xx_reset(kawa, stream_state, &mut endpoint)
            {
                let close_frontend =
                    matches!(self.position, Position::Server) && !parts.context.keep_alive_frontend;
                let (client_rtt, server_rtt) = self.snapshot_rtts(&endpoint, stream.linked_token());

                if let Some((dead_id, token)) = self.try_recycle_server_stream(
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
                    // LIFECYCLE §9 invariant 17: leave the census INSIDE
                    // 'outer so later iterations see the reduced count. The
                    // post-loop retirement at remove_dead_stream is too late.
                    census.note_ineligible(urgency, is_incremental);
                }
            }
        }
        // Sample the pass's final bucket totals. Publication is deferred to
        // the `gauge_connection_state` call below because the value must
        // reach the gauge in the SAME pass that computed it — the entry call
        // at the top of `write_streams` runs before the pass census exists,
        // so sampling there would publish the previous pass's value.
        self.ready_incremental_streams = census.ready_total();
        // RFC 7541 §6.3: clear our mirror of the pending size-update only
        // AFTER the pass confirmed the signal reached a header block. A
        // DATA-only pass leaves `size_update_emitted` as `false` so the
        // signal stays queued for the next pass with a header block.
        if pass.size_update_emitted() {
            self.pending_table_size_update = None;
        }
        // End the pass and take its three reusable buffers back. They are
        // moved, not copied: the pass never owned an allocation of its own.
        let (converter_out, lowercase_buf, cookie_buf) = pass.into_buffers();
        // Publish `ready_incremental_streams` (and any window/stream drift the
        // pass produced) before the two early returns below, so no pass
        // samples without emitting.
        self.gauge_connection_state();
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
        self.hpack.put_converter_buf(converter_out);
        self.hpack.put_lowercase_buf(lowercase_buf);
        self.hpack.put_cookie_buf(cookie_buf);
        self.hpack.shrink_converter_buffers();
        // RFC 9218 §4: end the pass — take the order buffer back and commit
        // the round-robin cursor so the next writable cycle begins with the
        // stream immediately after the one we fired first this pass
        // (LIFECYCLE §9 invariant 26). Placed here, after the deferred RST
        // accounting above, so a MadeYouReset cap trip that returns a GOAWAY
        // early skips both exactly as it did before the extraction.
        self.scheduler.end_pass(order, census);
        let mut close_frontend_after_completed_stream = false;
        for (dead_id, global_stream_id, token, close_frontend) in completed_streams {
            // Retirement is deferred out of the loop on purpose, and this is
            // an ORDERING requirement rather than a borrow workaround:
            // `try_recycle_server_stream` passes `is_last_stream` as
            // `streams().len() == 1`, and that branch hands the WHOLE
            // remaining connection overhead pool to one stream instead of a
            // proportional share. Retiring inline would let a later completer
            // of the same pass read `len() == 1` while other streams are still
            // live, and drain the pool early. (`active_streams` is passed
            // too, but it is only the even-split divisor of the fallback
            // branch, reached while the connection-wide byte total is still
            // zero — on every other pass the divisor is that total.)
            // Retiring here also runs
            // before `endpoint.end_stream()` can trigger teardown and observe
            // a stale `Recycle` entry in `self.stream_table.streams()`.
            self.remove_dead_stream(dead_id, global_stream_id);
            close_frontend_after_completed_stream |= close_frontend;
            if let Some(token) = token {
                remove_backend_stream(&mut context.backend_streams, token, global_stream_id);
                endpoint.end_stream(token, global_stream_id, context);
            }
        }
        if close_frontend_after_completed_stream && !self.drain.draining() {
            return if self.stream_table.streams().is_empty() {
                self.goaway(H2Error::NoError)
            } else {
                self.graceful_goaway(self.now)
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
            // Gather / write / confirm. The gather borrows `kawa.storage`
            // and hands back descriptors with an extended lifetime; `confirm`
            // discharges that obligation before the consume. Both halves and
            // the `unsafe` between them live in `h2_transmit`.
            let offered = h2_transmit::gather(kawa, io_slices);
            let (size, status) = socket.socket_write_vectored(io_slices);
            debug_assert!(
                size <= offered,
                "the socket reported {size} bytes written for an offer of {offered}"
            );
            debug.push(DebugEvent::SocketIO(debug_site, global_stream_id, size));
            h2_transmit::confirm(kawa, io_slices, size);
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
    /// **Invariant**: `stream_table` (wire map, `rst_sent`,
    /// `stream_last_activity_at`, `stream_fc_stalled_since`,
    /// `stream_fc_stalled_progress`, `expect_read`/`expect_write`) and
    /// `prioriser` MUST be emptied of `stream_id` here. `prioriser` is the
    /// only per-stream cache not folded into [`h2_stream_table::H2StreamTable`]
    /// — this module deliberately does not have the RFC 9218 priority map,
    /// same boundary [`h2_flow_control`]'s doc draws for per-stream send
    /// window state. `H2StreamTable::remove` asserts its own five caches are
    /// clean as a postcondition; forgetting `prioriser` here would still
    /// cause unbounded memory growth on long-lived connections with many
    /// cancelled streams, so it stays a second, explicit call.
    fn remove_dead_stream(&mut self, stream_id: StreamId, global_stream_id: GlobalStreamId) {
        if self.stream_table.remove(stream_id, global_stream_id)
            == h2_stream_table::RemoveOutcome::NotPresent
        {
            error!(
                "{} dead stream_id {} missing from streams map",
                log_context!(self),
                stream_id
            );
        }
        self.scheduler.remove_stream(&stream_id);
    }

    /// Drop stream-id mappings for streams that never became active before a
    /// connection-level close. This happens on incomplete/oversized header
    /// blocks: the stream slot is created on the initial HEADERS frame, then a
    /// GOAWAY closes the connection before the request is fully materialized.
    fn prune_inactive_streams_while_closing<L>(&mut self, context: &mut Context<L>)
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        if !self.drain.draining() || !matches!(self.state, H2State::GoAway | H2State::Error) {
            return;
        }

        let stale_streams = self
            .stream_table
            .streams()
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
        if self.drain.draining() && self.stream_table.streams().is_empty() {
            return self.graceful_goaway(self.now);
        }

        // The flush triple and the readiness policy behind it are one
        // decision, taken in `h2_close` where it is enumerable without a
        // socket. Why it is a sibling of `CloseAction` rather than four more
        // of its variants, why the conditional middle flush is an INPUT
        // instead of an `if` kept here, and why the invariant-16 probe is
        // passed as a closure: `h2_close::finalize_action`.
        // Bound rather than matched inline: the invariant-16 probe below
        // borrows `self.stream_table` and `context.streams`, and a `match`
        // keeps its scrutinee's temporaries alive for every arm — including
        // the arms that need `&mut self.readiness` and `&mut context.debug`.
        let action = h2_close::finalize_action(
            TlsFlushPhase::BeforeFlush,
            self.socket.socket_wants_write(),
            socket_write,
            self.stream_table.expect_write().is_some(),
            bytes_written_this_pass > 0,
            || any_stream_has_pending_back(self.stream_table.streams(), &context.streams),
            self.control_tx.has_pending() || !self.flow_control.pending_window_updates_is_empty(),
        );
        match action {
            // A parked `expect_write` owns the next tick: no bit moves.
            FinalizeAction::Parked => return MuxResult::Continue,
            // LIFECYCLE §9 invariant 16: a voluntary scheduler yield left
            // stranded bytes in a stream's `back.out`/`back.blocks` after a
            // pass that made forward progress. Retaining `Ready::WRITABLE` is
            // the ABSENCE of the withdrawal below, so this arm changes no bit
            // — it only narrates.
            FinalizeAction::RetainPendingBack => {
                #[cfg(debug_assertions)]
                context.debug.push(DebugEvent::Str(
                    "finalize_write: invariant 16 retained WRITABLE (pending back-buffer)"
                        .to_owned(),
                ));
                return MuxResult::Continue;
            }
            // Control-frame liveness: `flush_pending_control_frames` is gated
            // on `expect_write.is_none()`, so when a prior partial write
            // deferred the flush the RST / WINDOW_UPDATE queues stay non-empty
            // after `expect_write` finally drains. Without this rearm the next
            // tick would drop `Ready::WRITABLE` and the queued RST would stall
            // until an unrelated event re-triggered writable — which is
            // exactly the scenario h2spec trips by sending back-to-back
            // malformed streams.
            FinalizeAction::ArmControlQueue => {
                #[cfg(debug_assertions)]
                context.debug.push(DebugEvent::Str(
                    "finalize_write: retained WRITABLE (control queue non-empty)".to_owned(),
                ));
                self.readiness.arm_writable();
                incr!(names::h2::SIGNAL_WRITABLE_REARMED_CONTROL_QUEUE);
                return MuxResult::Continue;
            }
            // We wrote everything.
            FinalizeAction::Quiesce => {
                #[cfg(debug_assertions)]
                context.debug.push(DebugEvent::Str(format!(
                    "Wrote everything: {:?}",
                    self.stream_table.streams()
                )));
                self.readiness.interest.remove(Ready::WRITABLE);
                return MuxResult::Continue;
            }
            // The two TLS answers differ only in the step performed here, and
            // both fall through to the single post-flush query below.
            FinalizeAction::Flush => {
                self.socket.socket_write(&[]);
            }
            FinalizeAction::SkipFlush => {}
            // Named rather than `other =>`: a wildcard arm would turn a new
            // `FinalizeAction` variant into a release-mode panic on the proxy
            // write path instead of a compile error.
            action @ (FinalizeAction::ReArm | FinalizeAction::Settled) => {
                unreachable!("BeforeFlush yielded {action:?}")
            }
        }

        // Edge-triggered epoll: the second query is not a repeat of the first.
        // It is the only way this site learns whether the flush landed —
        // `socket_write(&[])`'s `(size, status)` is discarded — so it re-arms
        // WRITABLE when rustls still holds encrypted data. The readiness
        // policy above is deliberately NOT reopened here: once records were
        // found pending, this pass leaves the bits to the next one. The
        // degenerate arguments are the shape `goaway_close_action`'s own
        // post-flush call already uses: the inputs the flush could not change
        // are not re-read.
        let action = h2_close::finalize_action(
            TlsFlushPhase::AfterFlush,
            self.socket.socket_wants_write(),
            false,
            false,
            false,
            || false,
            false,
        );
        match action {
            // `ensure_tls_flushed` asks a third time rather than re-arming
            // outright, which is redundant — nothing mutates the socket
            // between the two — and deliberate: it keeps every post-decision
            // TLS re-arm in this file spelled the same way, as the GoAway and
            // Error arms of `writable` already do.
            FinalizeAction::ReArm => self.ensure_tls_flushed(),
            FinalizeAction::Settled => {}
            action @ (FinalizeAction::Flush
            | FinalizeAction::SkipFlush
            | FinalizeAction::Parked
            | FinalizeAction::RetainPendingBack
            | FinalizeAction::ArmControlQueue
            | FinalizeAction::Quiesce) => {
                unreachable!("AfterFlush yielded {action:?}")
            }
        }
        MuxResult::Continue
    }

    /// True while a HEADERS/CONTINUATION field block is being reassembled
    /// (spanning one or more `readable()` passes, by protocol design — a
    /// legitimate large header block routinely splits across frames).
    ///
    /// `self.zero` no longer doubles as the reassembly buffer — that role
    /// moved to `self.header_reassembly` (`h2_header_reassembly.rs`), which
    /// no write-side code can reach. That closes the #1396/#1397/#1401
    /// class outright: the ACCUMULATED, multi-frame history of a block is
    /// no longer representable as bytes a control-frame flush can touch, by
    /// construction, not by convention.
    ///
    /// What THIS flag still guards is narrower but still real: a single
    /// CONTINUATION frame's payload can be *mid-flight* in `zero.storage` —
    /// a `socket_read()` that has only partially filled this one frame —
    /// when a write pass runs in the same event-loop sweep (`mod.rs`'s
    /// inner loop dispatches frontend `readable()` then `writable()`
    /// together). Losing those bytes would still corrupt an otherwise-
    /// completable block. Checked at every site that clears or reuses
    /// `zero.storage`'s write-scratch role: the frontend-hung-up-while-
    /// draining, WINDOW_UPDATE-drain and RST_STREAM-drain stages and the
    /// deferred-initial-GOAWAY-readiness check, all four in
    /// [`Self::flush_pending_control_frames`]; plus [`Self::graceful_goaway`]'s
    /// own defer-or-send decision and [`Self::flush_zero_buffer`]'s no-op
    /// guard — six call sites in total. All six were already present
    /// except the frontend-hung-up-while-draining one, added in the review
    /// that found #1423's premise wrong: that stage used to assume
    /// `Ready::HUP` means no further bytes can ever arrive, which mio's own
    /// `is_read_closed()` documentation contradicts (a TCP half-close can
    /// still have unread data queued) — see that stage's own comment and
    /// `h2_header_reassembly.rs`'s module doc for the full account.
    ///
    /// The write side already protects the mirror case — while a zero-buffer
    /// write is stalled (`expect_write == Some(Zero)`), READABLE interest is
    /// explicitly disabled (see the comment a few lines below) so a fresh
    /// frame read cannot clobber the pending write. This is the missing
    /// other half of that same invariant.
    fn header_block_reassembly_in_progress(&self) -> bool {
        matches!(
            self.state,
            H2State::ContinuationHeader(_) | H2State::ContinuationFrame(_)
        )
    }

    /// Flush pending control frames (zero-buffer resume, WINDOW_UPDATEs, RST_STREAMs)
    /// before entering the main writable state machine.
    ///
    /// Returns `Some(result)` if the caller should return early (e.g. socket would
    /// block, GOAWAY triggered), or `None` if writable() should proceed normally.
    fn flush_pending_control_frames(&mut self) -> Option<MuxResult> {
        // CORRECTION (review of e1c3c2fb, B2): this stage used to clear
        // `zero.storage` unconditionally here, on the theory that
        // `frontend_hung_up_while_draining()` firing meant "no further
        // bytes can ever arrive". That is false: `Ready::HUP` is
        // `is_read_closed() || is_write_closed()`
        // (`command/src/ready.rs`), and mio documents `is_read_closed()` as
        // true not only on a full close but also when "the peer stream has
        // shutdown the write half of its socket" (TCP half-close) — a
        // FIN with data the peer already sent still sitting in the kernel
        // receive queue, unread. `drive_frontend_shutdown_io` (`mod.rs`)
        // force-calls `readable()` for H2 on every `shutting_down()` poll,
        // so a CONTINUATION frame split across TCP segments can have its
        // partial payload wiped here — mid-flight, not stale — one
        // `readable()` pass before the rest of it would have been read.
        // `self.header_reassembly` still protects the ACCUMULATED history
        // of a block from every other write-side site (see
        // `h2_header_reassembly.rs`); what this stage could still corrupt
        // is the same narrower "one frame still mid-`socket_read()`" window
        // the three sibling stages below already guard — so it now shares
        // their guard instead of being the one stage that does not.
        if self.frontend_hung_up_while_draining() {
            self.stream_table.set_expect_write(None);
            if !self.header_block_reassembly_in_progress() {
                self.zero.storage.clear();
            }
            self.flow_control.clear_pending_window_updates();
            self.control_tx.clear_pending();
        }

        // RFC 9113 §6.5: check if peer has timed out on SETTINGS ACK
        if let Some(sent_at) = self.settings_sent_at
            && self.now.saturating_duration_since(sent_at) >= SETTINGS_ACK_TIMEOUT
        {
            warn!(
                "{} SETTINGS ACK timeout: no SETTINGS ACK observed within {:?}",
                log_context!(self),
                SETTINGS_ACK_TIMEOUT
            );
            return Some(self.goaway(H2Error::SettingsTimeout));
        }

        // Stage — resume zero-buffer flush.
        // If a previous write was partial, finish it before serialising any
        // new control frames. Don't reset the timeout for control frame
        // writes (SETTINGS ACK, PING response, WINDOW_UPDATE) — only
        // application-data writes should reset it.
        if let Some(H2StreamId::Zero) = self.stream_table.expect_write() {
            if self.flush_zero_to_socket() {
                self.ensure_tls_flushed();
                return Some(MuxResult::Continue);
            }
            // When H2StreamId::Zero is used to write, READABLE is disabled —
            // re-enable it now that the flush is complete.
            self.readiness.interest.insert(Ready::READABLE);
            self.stream_table.set_expect_write(None);
        }

        // Stage — send a deferred initial GOAWAY.
        // `graceful_goaway` could not serialize into `self.zero.storage`
        // while `header_block_reassembly_in_progress()` was true — see its
        // comment and `send_initial_goaway`. Send it now that reassembly has
        // completed; if it is still in progress this call, leave the flag
        // set and retry on a later pass (nothing is lost: `graceful_goaway`
        // already armed WRITABLE, and completing the reassembly re-enters
        // this function via the next writable() call in the same
        // readable()/writable() sweep). The readiness/reassembly check stays
        // here, computed by this caller: `H2DrainState` has neither
        // `self.stream_table` nor `self.state` to compute it itself — see
        // `h2_drain`'s module doc.
        let ready_to_flush_initial_goaway = self.stream_table.expect_write().is_none()
            && !self.header_block_reassembly_in_progress();
        if self
            .drain
            .take_deferred_initial_goaway(ready_to_flush_initial_goaway)
        {
            return Some(self.send_initial_goaway());
        }

        // Stage — drain pending WINDOW_UPDATE frames.
        // Serialize and flush them inline to avoid extra event loop
        // iterations that could cause response data to be sent before
        // subsequent frames are validated.
        //
        // Deferred while a header block is being reassembled: this stage
        // clears and reuses `self.zero.storage`, which right now holds the
        // bytes accumulated from an earlier HEADERS/CONTINUATION frame on
        // some (possibly different) stream, awaiting its own CONTINUATION.
        // The queued WINDOW_UPDATEs stay queued — WRITABLE was already
        // armed when they were enqueued (`Self::queue_window_update`), so
        // the next `writable()` call after the block completes drains them
        // normally; nothing is lost, only delayed.
        if !self.flow_control.pending_window_updates_is_empty()
            && self.stream_table.expect_write().is_none()
            && !self.header_block_reassembly_in_progress()
        {
            let kawa = &mut self.zero;
            kawa.storage.clear();
            let buf = kawa.storage.space();
            // Emission order is the deterministic ascending stream_id order
            // `H2FlowControl` guarantees — see its module doc — not
            // insertion/arrival order.
            let (offset, frames_written) = self.flow_control.drain_window_updates_into(buf);
            if frames_written > 0 {
                count!(names::h2::FRAMES_TX_WINDOW_UPDATE, frames_written as i64);
            }
            if offset > 0 {
                kawa.storage.fill(offset);
                if self.flush_zero_to_socket() {
                    self.stream_table.set_expect_write(Some(H2StreamId::Zero));
                    // Edge-triggered epoll: ensure pending TLS data gets flushed
                    if self.socket.socket_wants_write() {
                        self.readiness.event.insert(Ready::WRITABLE);
                    }
                    return Some(MuxResult::Continue);
                }
            }
        }

        // Stage — RST_STREAM cap check + drain.
        // Check the lifetime total (not just pending queue length) because
        // writable() drains the queue between readable() calls, so the
        // pending count alone may never reach the cap even under sustained
        // misbehavior.
        if !matches!(self.state, H2State::GoAway | H2State::Error)
            && self.control_tx.lifetime_cap_reached()
        {
            error!(
                "{} total RST_STREAM count {} exceeds cap {}, sending GOAWAY(ENHANCE_YOUR_CALM)",
                log_context!(self),
                self.control_tx.lifetime_queued(),
                h2_control_tx::MAX_PENDING_RST_STREAMS
            );
            return Some(self.goaway(H2Error::EnhanceYourCalm));
        }

        // Flush pending RST_STREAM frames (queued when refusing streams).
        // Accounting happens at queue-time inside `Self::enqueue_rst`, so
        // this drain only serialises and flushes — no metric/flood calls
        // here would double-count.
        //
        // Deferred while a header block is being reassembled, for the same
        // `self.zero.storage` reuse reason as the WINDOW_UPDATE stage above
        // — `Self::enqueue_rst` already arms WRITABLE, so this is a delay,
        // not a drop.
        if self.control_tx.has_pending()
            && self.stream_table.expect_write().is_none()
            && !self.header_block_reassembly_in_progress()
        {
            let kawa = &mut self.zero;
            kawa.storage.clear();
            let buf = kawa.storage.space();
            // Emission order is queue order, which is arrival order over
            // already-deduped stream ids — see `h2_control_tx`'s module doc.
            let (offset, _frames_written) = self.control_tx.drain_rst_streams_into(buf);
            if offset > 0 {
                kawa.storage.fill(offset);
                if self.flush_zero_to_socket() {
                    self.stream_table.set_expect_write(Some(H2StreamId::Zero));
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
        // Entry point: adopt the mux's snapshot for this pass.
        self.now = context.now;
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
                // The preamble above already attempted this pass's flush, so
                // this arm reads the post-flush answer and has no `Flush` of
                // its own — see `h2_close`'s module doc.
                match h2_close::error_close_action(self.socket.socket_wants_write()) {
                    CloseAction::ReArmAndContinue => {
                        self.ensure_tls_flushed();
                        MuxResult::Continue
                    }
                    CloseAction::CloseSession => MuxResult::CloseSession,
                    // Named rather than `other =>`: a wildcard arm would turn a
                    // new `CloseAction` variant into a release-mode panic in the
                    // proxy write path instead of a compile error.
                    action @ (CloseAction::Flush | CloseAction::Disconnect) => {
                        unreachable!("error_close_action yielded {action:?}")
                    }
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
                // Response DATA frames may still sit in rustls's output
                // buffer — accepted by socket_write_vectored during
                // write_streams() but not yet flushed to TCP. Under TCP
                // backpressure (HAProxy chain) this is the primary truncation
                // vector, so the decision lives in `h2_close`, exhaustively
                // unit-tested there rather than inline here. Why the two
                // `socket_wants_write()` queries are two DIFFERENT questions,
                // and why the flush between them stays inside this call: see
                // `h2_close::TlsFlushPhase`.
                match h2_close::goaway_close_action(
                    TlsFlushPhase::BeforeFlush,
                    self.peer_gone_after_final_goaway(),
                    self.socket.socket_wants_write(),
                ) {
                    CloseAction::CloseSession => return MuxResult::CloseSession,
                    CloseAction::Flush => {
                        self.socket.socket_write(&[]);
                        match h2_close::goaway_close_action(
                            TlsFlushPhase::AfterFlush,
                            false,
                            self.socket.socket_wants_write(),
                        ) {
                            CloseAction::ReArmAndContinue => {
                                self.ensure_tls_flushed();
                                return MuxResult::Continue;
                            }
                            CloseAction::Disconnect => {}
                            action @ (CloseAction::CloseSession | CloseAction::Flush) => {
                                unreachable!("AfterFlush yielded {action:?}")
                            }
                        }
                    }
                    CloseAction::Disconnect => {}
                    action @ CloseAction::ReArmAndContinue => {
                        unreachable!("BeforeFlush yielded {action:?}")
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
                        incr!(names::h2::FRAMES_TX_SETTINGS);
                        // RFC 9113 §6.5: start tracking SETTINGS ACK timeout
                        self.settings_sent_at = Some(self.now);
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
                self.stream_table.set_expect_write(Some(H2StreamId::Zero));
                MuxResult::Continue
            }
            (H2State::ClientSettings, Position::Client(..)) => {
                trace!("{} Sent preface and settings", log_context!(self));
                self.state = H2State::ServerSettings;
                self.stream_table
                    .set_expect_read(Some((H2StreamId::Zero, 9)));
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
    fn snapshot_rtts<E: Endpoint>(
        &self,
        endpoint: &E,
        linked_token: Option<mio::Token>,
    ) -> (Option<Duration>, Option<Duration>) {
        if !self.position.is_server() {
            return (None, None);
        }
        (
            socket_rtt(self.socket.socket_ref()),
            linked_token.and_then(|t| endpoint.peer_rtt(t)),
        )
    }

    /// Try to recycle a completed server-side stream by distributing overhead,
    /// generating access logs, and transitioning the stream to `Recycle` state.
    ///
    /// Returns `Some((stream_id, Option<token>))` if the stream was recycled, so the
    /// caller can add `stream_id` to the dead-streams list and call `endpoint.end_stream()`
    /// if a token was returned. Returns `None` if recycling was deferred or not applicable.
    ///
    /// `client_rtt`/`server_rtt` are snapshotted by the caller — which must
    /// do so BEFORE `endpoint.end_stream(...)`, see [`Self::snapshot_rtts`] —
    /// and forwarded into the access log. `stream` stays a parameter rather
    /// than being looked up here because the caller already holds it as
    /// `&mut context.streams[global_stream_id]`.
    #[allow(clippy::too_many_arguments)]
    fn try_recycle_server_stream<L>(
        &mut self,
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
        match self.position {
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
                let active_streams = self.stream_table.streams().len();
                distribute_overhead(
                    &mut stream.metrics,
                    &mut self.bytes.overhead_bin,
                    &mut self.bytes.overhead_bout,
                    stream_bytes,
                    byte_totals,
                    active_streams,
                    active_streams == 1,
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
    /// Callers must distribute overhead *before* calling this: it resets
    /// `stream.metrics`, so a share credited afterwards would be discarded.
    fn complete_server_stream<L>(
        stream: &mut crate::protocol::mux::Stream,
        listener: std::rc::Rc<std::cell::RefCell<L>>,
        client_rtt: Option<Duration>,
        server_rtt: Option<Duration>,
    ) -> Option<mio::Token>
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        incr!(names::http::E2E_H2);
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
        for &gid in self.stream_table.streams().values() {
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
            self.stream_table.streams().len(),
            self.stream_table.streams().len() <= 1,
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
    /// RFC 9113 §6.9.1: window size increment MUST be 1..2^31-1 (0x7FFFFFFF);
    /// `increment == 0` is a legal no-op to *send* and queues nothing (see
    /// `h2_flow_control::H2FlowControl::queue_window_update`).
    ///
    /// Always signals pending write so callers don't have to remember the
    /// edge-triggered epoll invariant (see memory feedback_epollet_signal_pending_write):
    /// under ET epoll a queued WINDOW_UPDATE without a live WRITABLE event bit
    /// is invisible to filter_interest() and will never get flushed.
    fn queue_window_update(&mut self, stream_id: u32, increment: u32) {
        match self.flow_control.queue_window_update(
            stream_id,
            increment,
            self.max_pending_window_updates,
        ) {
            h2_flow_control::QueueWindowUpdateOutcome::Coalesced { old, new } => {
                trace!(
                    "{} WINDOW_UPDATE coalesced: stream={} old={} new={}",
                    log_context!(self),
                    stream_id,
                    old,
                    new
                );
            }
            h2_flow_control::QueueWindowUpdateOutcome::Inserted { increment } => {
                trace!(
                    "{} WINDOW_UPDATE queued: stream={} increment={}",
                    log_context!(self),
                    stream_id,
                    increment
                );
            }
            h2_flow_control::QueueWindowUpdateOutcome::Dropped => {
                error!(
                    "{} WINDOW_UPDATE dropped: queue full ({} entries), stream={} increment={}",
                    log_context!(self),
                    self.max_pending_window_updates,
                    stream_id,
                    increment
                );
                incr!(names::h2::WINDOW_UPDATE_DROPPED);
            }
            // Zero increment: nothing was queued, nothing to log.
            h2_flow_control::QueueWindowUpdateOutcome::Noop => {}
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
        )) = self.stream_table.expect_read()
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
        // Entry point: `Mux::timeout` reaches this directly, without going
        // through `readable`, so adopt the mux's snapshot here too.
        self.now = context.now;
        // Per-connection scratch Vecs (`converter_buf`, `lowercase_buf`,
        // `cookie_buf`, and the scheduler's own pass-order buffer) grow to a
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
        self.hpack.reclaim_idle_buffers(SCRATCH_BUF_RETAIN);
        self.scheduler.reclaim_idle_buffer(SCRATCH_BUF_RETAIN);

        if self.stream_table.is_empty()
            || (self.stream_table.activity_is_empty() && self.stream_table.fc_stall_is_empty())
        {
            return;
        }
        let now = self.now;
        let deadline = self.stream_idle_timeout;
        // Two independent per-stream guards reap on the same deadline — see
        // `H2StreamTable::collect_timed_out`. The flow-control-stall guard
        // (`stream_fc_stalled_since`) closes the HTTP/2 window-stall vector that
        // the bidirectional liveness guard (`stream_last_activity_at`) misses,
        // because an inbound DATA drip keeps the liveness timer warm while the
        // response stays window-blocked.
        let timed_out = self.stream_table.collect_timed_out(now, deadline);
        if timed_out.is_empty() {
            return;
        }
        for (sid, reason) in timed_out {
            info!(
                "{} H2 stream {} exceeded {:?} ({}), cancelling",
                log_context!(self),
                sid,
                deadline,
                reason
            );
            // M1: break reaps down by guard so a window-stall reap (a DoS
            // mitigation) is distinguishable from an ordinary idle reap on a
            // dashboard. M2: a window-stall reap whose stream dribbled some
            // outbound progress (`acc > 0`) below the floor is specifically a
            // stall-budget reap — the `WINDOW_UPDATE`-drip vector the budget
            // closes — counted as a subset. Read the accumulator BEFORE
            // `remove_dead_stream` evicts it below.
            match reason {
                "H2::WindowStall" => {
                    count!(names::h2::STREAMS_REAPED_WINDOW_STALL, 1);
                    if matches!(self.stream_table.fc_stall_progress(sid), Some(acc) if acc > 0) {
                        count!(names::h2::STREAMS_REAPED_STALL_BUDGET, 1);
                    }
                }
                "H2::IdleTimeout" => count!(names::h2::STREAMS_REAPED_IDLE_TIMEOUT, 1),
                other => debug!("{} unexpected reap reason {}", log_context!(self), other),
            }
            // Route through the canonical chokepoint so dedupe (rst_sent),
            // queued-cap accounting (`H2ControlTx`'s MadeYouReset lifetime
            // counter against MAX_PENDING_RST_STREAMS), and edge-triggered-epoll arming
            // (Readiness::arm_writable) all stay consistent — see LIFECYCLE
            // §8.2. The previous direct push bypassed all three: a peer
            // that opens 200 streams and lets them all idle past
            // stream_idle_timeout could push past the queued cap silently
            // (no GOAWAY(ENHANCE_YOUR_CALM) escalation), a double-cancel
            // pass would grow the pending queue instead of short-
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
            if let Some(global_stream_id) = self.stream_table.get(sid) {
                {
                    let stream = &mut context.streams[global_stream_id];
                    self.attribute_bytes_to_stream(&mut stream.metrics);
                }
                // Check if stream is linked to a backend — borrow must be scoped
                // so end_stream can take &mut context.
                let linked_token = context.streams[global_stream_id].linked_token();
                let (client_rtt, server_rtt) = self.snapshot_rtts(&*endpoint, linked_token);
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
                            Some(reason),
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
    /// Delegates the queueing itself to [`h2_control_tx::H2ControlTx::enqueue_rst`],
    /// which owns the four invariants (dedupe via `rst_sent`, MadeYouReset
    /// queued cap, the per-insert queue bound, edge-triggered-epoll arm via
    /// [`Readiness::arm_writable`]) and covers them with unit tests that need no
    /// `ConnectionH2` fixture. What stays here is the accounting, because a
    /// lifetime-cap trip converts to a connection-wide GOAWAY only this type
    /// can return.
    ///
    /// Two of the call paths that reach here are never inspected by
    /// [`Self::check_invariants`], whose only production caller is
    /// [`Self::handle_frame`]:
    ///
    /// * `cancel_timed_out_streams`, which queues one RST per timed-out stream
    ///   in a single sweep and, from `Mux::timeout` against a silent peer,
    ///   runs when `handle_frame` does not run at all;
    /// * the DATA-on-closed-stream reset, which sits in
    ///   [`Self::handle_header_state`] — `handle_read` returns into that helper
    ///   directly and that branch never reaches `handle_frame`.
    ///
    /// The second path is separately rate-limited: it is preceded by
    /// `H2FloodDetector::record_glitch` + `check_flood_or_return!`, whose
    /// `DEFAULT_MAX_GLITCH_COUNT` (100) bounds it inside one flood window, and
    /// the window can only half-decay when `Mux::ready_inner` resamples
    /// `context.now` on a new outer iteration — which is strictly after an
    /// inner iteration that already ran `writable()`, since `arm_writable`
    /// raises both the WRITABLE interest and its event bit. So it cannot
    /// accumulate across windows without a `flush_pending_control_frames`
    /// pass in between. The per-insert bound inside
    /// [`h2_control_tx::H2ControlTx::enqueue_rst`] is what makes that reasoning
    /// unnecessary for correctness.
    fn enqueue_rst(&mut self, wire_stream_id: StreamId, error: H2Error) -> Option<MuxResult> {
        let outcome = self.control_tx.enqueue_rst(
            self.stream_table.rst_sent_mut(),
            &mut self.readiness,
            wire_stream_id,
            error,
        );
        // Account ONLY when a new RST actually entered the queue.
        // Calling `enqueue_rst` for a stream that already has a queued
        // (or already-flushed) RST is the dedup short-circuit — counting
        // those would inflate `h2.frames.tx.rst_stream` /
        // `h2.rst_stream.sent.*` and trip the CVE-2025-8671 MadeYouReset
        // lifetime cap on frames that never reached the wire. An
        // at-capacity refusal is accounted the same way, and for the same
        // reason: that frame never reaches the wire either.
        //
        // Account at queue-time, not at drain-time. Doing it later in
        // `flush_pending_control_frames` would double-count any RST that
        // a re-entrant call (DATA on a closed stream we already RSTed)
        // tried to enqueue — and missing it at queue-time leaves
        // `cancel_timed_out_streams` / `refuse_stream_and_discard` /
        // DATA-on-closed-stream paths bypassing the lifetime cap
        // (security review LISA-001 on commit `da845c71`).
        match outcome {
            h2_control_tx::EnqueueRstOutcome::Queued => self.account_emitted_rst(error),
            h2_control_tx::EnqueueRstOutcome::Deduped => None,
            // Drop + metric + contextual log, never a panic on the release
            // path. No GOAWAY is raised here: reaching the cap implies
            // `total_rst_streams_queued >= MAX_PENDING_RST_STREAMS`, which
            // `flush_pending_control_frames` escalates to
            // `GOAWAY(ENHANCE_YOUR_CALM)` before its drain loop for as long as
            // the connection is not already in `H2State::GoAway`/`Error` — and
            // once it is, the peer already holds a GOAWAY and `writable()`
            // force-disconnects. See `EnqueueRstOutcome::Dropped`. Raising a
            // second one from here would re-enter `goaway()` once per
            // remaining reaped stream, clobbering `self.zero` and inflating
            // `h2.goaway.sent.*`.
            h2_control_tx::EnqueueRstOutcome::Dropped => {
                error!(
                    "{} RST_STREAM dropped: pending queue already at capacity ({}), stream={} error={:?}",
                    log_context!(self),
                    h2_control_tx::MAX_PENDING_RST_STREAMS,
                    wire_stream_id,
                    error
                );
                incr!(names::h2::RST_STREAM_DROPPED);
                None
            }
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
    ///     after the loop, so a lifetime-cap trip cannot preempt the
    ///     writes of the streams that follow.
    ///
    /// Returning `Some(MuxResult)` means the caller MUST short-circuit
    /// with that result — the flood detector tripped its lifetime cap
    /// and converted to a connection-wide GOAWAY.
    fn account_emitted_rst(&mut self, error: H2Error) -> Option<MuxResult> {
        incr!(names::h2::FRAMES_TX_RST_STREAM);
        count!(metric_for_rst_stream_sent(error), 1);
        if !matches!(error, H2Error::NoError)
            && let Some(violation) = self.flood_detector.record_rst_emitted()
        {
            return Some(self.handle_flood_violation(violation));
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
    ///
    /// `discarded` is stashed in [`Self::discarded_field_block`] for the
    /// `H2State::Discard` arm of [`Self::handle_read`] to consume — see
    /// [`DiscardedFieldBlock`] for why the HPACK field block cannot simply be
    /// dropped with the rest of the payload.
    fn refuse_stream_and_discard(
        &mut self,
        stream_id: StreamId,
        error: H2Error,
        payload_len: u32,
        discarded: DiscardedFieldBlock,
    ) -> MuxResult {
        if let Some(result) = self.enqueue_rst(stream_id, error) {
            return result;
        }
        self.state = H2State::Discard;
        self.stream_table
            .set_expect_read(Some((H2StreamId::Zero, payload_len as usize)));
        self.discarded_field_block = Some(discarded);
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
        if self.now.saturating_duration_since(self.refuse_window_start)
            >= BACKPRESSURE_WINDOW_DURATION
        {
            self.refuse_count_window = 0;
            self.refuse_window_start = self.now;
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
    /// compare `self.stream_table.streams().len()` against this reduced cap, so the peer
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
        // A final/error GOAWAY supersedes any advisory initial GOAWAY that
        // `graceful_goaway` deferred: this frame carries a real
        // `last_stream_id` and `expect_read` is dropped below, so no further
        // readable() will ever complete the reassembly that deferred it.
        // Sending the stale advisory afterward would only be a redundant,
        // less informative GOAWAY.
        self.drain.enter_final_goaway();
        self.stream_table.set_expect_read(None);
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
        match serializer::gen_goaway(
            kawa.storage.space(),
            self.stream_table.highest_peer_stream_id(),
            error,
        ) {
            Ok((_, size)) => {
                kawa.storage.fill(size);
                incr!(names::h2::FRAMES_TX_GOAWAY);
                self.state = H2State::GoAway;
                self.stream_table.set_expect_write(Some(H2StreamId::Zero));
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
    ///
    /// `now` is the caller's clock snapshot and is what arms the forced-close
    /// budget. It is a parameter rather than a read of [`Self::now`] because
    /// the caller that matters — `Mux::shutting_down` — runs outside
    /// `ready()` and therefore outside the pass that last refreshed the
    /// mirror. In-module callers pass `self.now`.
    pub fn graceful_goaway(&mut self, now: Instant) -> MuxResult {
        // `self.zero.storage` is also the read-side accumulation buffer for
        // an in-flight HEADERS/CONTINUATION field block
        // (`header_block_reassembly_in_progress`, a few lines above the
        // WINDOW_UPDATE stage of `flush_pending_control_frames`): clearing it
        // here to serialize the GOAWAY would destroy that reassembly out from
        // under it, directly contradicting the "existing streams should
        // continue reading" promise below. `H2DrainState::begin_graceful_drain`
        // decides whether to defer for exactly that reason, given this
        // caller-computed check — the module has no `H2State` of its own to
        // read it with.
        let reassembly_in_progress = self.header_block_reassembly_in_progress();
        match self.drain.begin_graceful_drain(now, reassembly_in_progress) {
            // Second GOAWAY: send with the real last_stream_id.
            GracefulDrainDecision::AlreadyDraining => self.goaway(H2Error::NoError),
            // Defer, the same way `flush_pending_control_frames` already
            // defers its WINDOW_UPDATE and RST_STREAM drains:
            // `flush_pending_control_frames` sends the deferred GOAWAY via
            // `send_initial_goaway` as soon as reassembly completes.
            GracefulDrainDecision::DeferInitial => {
                debug!(
                    "{} GOAWAY (graceful, initial) deferred: header block reassembly in progress",
                    log_context!(self)
                );
                // Ensure a writable() pass happens even if nothing else would
                // arm it, so the deferred GOAWAY is not left waiting on an
                // unrelated event.
                self.readiness.arm_writable();
                MuxResult::Continue
            }
            // First GOAWAY, no reassembly in progress: advertise MAX stream
            // ID so the peer knows we are draining but does not yet know the
            // cutoff. This gives in-flight requests a chance to arrive
            // before we commit to a final last_stream_id.
            GracefulDrainDecision::SendInitial => self.send_initial_goaway(),
        }
    }

    /// Serializes and queues the first, advisory GOAWAY
    /// (`NO_ERROR`, `last_stream_id = STREAM_ID_MAX`) of a graceful drain
    /// into `self.zero.storage`.
    ///
    /// Split out of [`Self::graceful_goaway`] so
    /// [`Self::flush_pending_control_frames`] can call it once an in-flight
    /// header block finishes reassembling, for the case where
    /// `graceful_goaway` had to defer it. Callers must already have checked
    /// `!self.header_block_reassembly_in_progress()`.
    fn send_initial_goaway(&mut self) -> MuxResult {
        // Keep expect_read as-is: existing streams should continue reading
        // data during the drain window opened by the initial GOAWAY. Only
        // the final GOAWAY (via `goaway()`) removes READABLE.
        let kawa = &mut self.zero;
        kawa.storage.clear();
        debug!(
            "{} GOAWAY (graceful, initial): last_stream_id=0x7FFFFFFF",
            log_context!(self)
        );
        // The initial GOAWAY sends NO_ERROR on the wire — count it under
        // the same per-code key as the final GOAWAY. The downstream alert
        // that wants to distinguish drain from termination compares
        // against the `h2.goaway.sent.no_error` rate (drain) vs the other
        // variants (termination on error).
        count!(metric_for_goaway_sent(H2Error::NoError), 1);

        match serializer::gen_goaway(kawa.storage.space(), STREAM_ID_MAX, H2Error::NoError) {
            Ok((_, size)) => {
                kawa.storage.fill(size);
                incr!(names::h2::FRAMES_TX_GOAWAY);
                // Stay in the current state so the connection can continue processing
                // existing streams. The final GOAWAY will transition to GoAway state.
                // Keep READABLE so in-flight request bodies can still be received
                // during the drain window. Only remove READABLE in the final GOAWAY
                // (via `goaway()`).
                self.stream_table.set_expect_write(Some(H2StreamId::Zero));
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
        self.drain.deadline_elapsed(self.now)
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
        self.stream_table.expect_write().is_some()
            || !self.zero.storage.is_empty()
            || self.socket.socket_wants_write()
    }

    /// True when the reaper has queued control frames (`RST_STREAM`) into
    /// [`h2_control_tx::H2ControlTx`] that have not yet been serialized. Kept SEPARATE
    /// from [`Self::has_pending_write`] because that probe gates connection close
    /// (the `mod.rs` close-gating sites) and must NOT treat a queued RST as a
    /// reason to keep the connection open; this probe is consulted ONLY by the
    /// `MuxState::timeout` flush gate to push a silent-peer `RST_STREAM(CANCEL)`
    /// onto the wire before the connection closes.
    pub fn has_pending_control_write(&self) -> bool {
        self.control_tx.has_pending()
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
        self.has_pending_write()
            || any_stream_has_pending_back(self.stream_table.streams(), &context.streams)
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
    ///
    /// No-op while `header_block_reassembly_in_progress()`: `self.zero` is
    /// also the read-side accumulation buffer for an in-flight
    /// HEADERS/CONTINUATION field block, and `Mux::shutting_down` calls this
    /// unconditionally right after `graceful_goaway` — including when
    /// `graceful_goaway` deferred its GOAWAY for that exact reason and left
    /// nothing queued. `expect_write == Some(H2StreamId::Zero)` (the only
    /// case with legitimate bytes to flush here) already cannot coexist with
    /// reassembly in progress — READABLE is disabled for the whole time a
    /// zero-buffer write is stalled — so skipping is always safe and never
    /// drops a real write.
    pub fn flush_zero_buffer(&mut self) {
        if self.header_block_reassembly_in_progress() {
            return;
        }
        if self.flush_zero_to_socket() {
            return;
        }
        self.stream_table.set_expect_write(None);
        if self.socket.socket_wants_write() {
            let (_size, status) = self.socket.socket_write(&[]);
            let _ = update_readiness_after_write(0, status, &mut self.readiness);
        }
    }

    /// Number of streams currently tracked in the wire map. `stream_table` is
    /// private to this module, so this is the sole accessor for `router.rs`'s
    /// H2-connection-reuse heuristic (picks the non-draining H2 backend
    /// connection with the fewest active streams).
    pub fn stream_count(&self) -> usize {
        self.stream_table.len()
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
        if self.drain.draining() {
            error!(
                "{} Rejecting new stream {} on draining connection",
                log_context!(self),
                stream_id
            );
            return None;
        }
        let streams_before = self.stream_table.len();
        // Track the highest peer-initiated stream ID for GoAway frames
        // before any early return, so GoAway always reports the correct last
        // stream. `observe_peer_stream_id` asserts the monotonic-non-decreasing
        // property itself.
        self.stream_table.observe_peer_stream_id(stream_id);
        let global_stream_id = context.create_stream(
            Ulid::generate(),
            self.peer_settings.settings_initial_window_size,
        )?;
        self.last_stream_id = (stream_id + 2) & !1;
        self.stream_table
            .register(stream_id, global_stream_id, self.now);
        // Post-conditions: the stream is now reachable in both indices (see
        // `H2StreamTable::register`'s own postconditions), the active count
        // grew by exactly one (the id was not already present —
        // `handle_header_state` rejects re-used ids), and `last_stream_id` is
        // the even watermark just past this id so `new_stream_id` never collides.
        debug_assert_eq!(
            self.stream_table.len(),
            streams_before + 1,
            "create_stream must add exactly one stream (id must not pre-exist)"
        );
        debug_assert!(
            self.last_stream_id > stream_id && self.last_stream_id & 1 == 0,
            "last_stream_id watermark must be the even value strictly above stream_id"
        );
        Some(global_stream_id)
    }

    pub fn new_stream_id(&mut self) -> Option<StreamId> {
        let watermark_before = self.last_stream_id;
        let (issued, next) = next_stream_id(self.last_stream_id, self.position.is_client())?;
        self.last_stream_id = next;
        // Post-conditions: the locally-issued id has the parity of our role and
        // the watermark advanced strictly (so the next allocation cannot reuse
        // this id). `next_stream_id` already asserts parity vs `is_client`; here
        // we re-assert against `self.position` and the watermark monotonicity.
        debug_assert_eq!(
            issued & 1 == 1,
            self.position.is_client(),
            "locally-issued stream id parity must match our role"
        );
        debug_assert!(
            self.last_stream_id > watermark_before,
            "issuing a stream id must advance the watermark"
        );
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

    /// Test-only setter: map `stream_id -> gid` on the wire WITHOUT arming
    /// the per-stream liveness timer `stream_table.register` otherwise always
    /// pairs it with. `mod::tests::shutting_down_refreshes_the_snapshot_so_the_drain_budget_expires`
    /// needs exactly this artificial state, to force `cancel_timed_out_streams`
    /// down its early-return path (empty activity map) rather than race an
    /// RST_STREAM into that test's unrelated drain-budget assertion.
    #[cfg(any(test, feature = "e2e-hooks"))]
    pub fn __test_insert_wire_mapping_only(&mut self, stream_id: StreamId, gid: GlobalStreamId) {
        self.stream_table
            .__test_insert_wire_mapping_only(stream_id, gid);
    }

    /// Cross-field invariant sweep for the H2 connection state machine,
    /// asserted as a run-to-completion post-condition at the end of every
    /// frame-handling pass (see the call in [`Self::handle_frame`]).
    ///
    /// These are relationships between *separate* fields that no single setter
    /// can guarantee on its own — exactly the class of bug TigerStyle's
    /// `check_invariants` targets. Each one is cheap (counter compares + a few
    /// `HashMap` membership probes); the whole function is `#[cfg(debug_assertions)]`
    /// and compiles out of release entirely.
    ///
    /// Encoded invariants:
    /// 1. **Stream-id watermark parity**: locally-issued ids never exceed
    ///    `STREAM_ID_MAX`; `last_stream_id` stays the even watermark (it is
    ///    rounded to `(id + 2) & !1` and initialised to 0).
    /// 2. **Per-stream caches are subsets of the live stream set**:
    ///    `stream_last_activity_at` is keyed only by currently-tracked stream
    ///    ids — a leak here would let a removed stream keep an idle timer and
    ///    mis-fire `cancel_timed_out_streams`. (`rst_sent` is intentionally NOT
    ///    a subset: a queued RST for an already-removed stream is legal.)
    /// 3. **RST queue accounting** is checked by
    ///    [`h2_control_tx::H2ControlTx::check_invariants`] as a post-condition
    ///    of its own mutating methods, so it is not restated here — a second
    ///    copy of a rule drifts from the first.
    /// 4. **Pending WINDOW_UPDATE bound**: the coalescing map never exceeds the
    ///    per-connection cap derived from `max_concurrent_streams`.
    /// 5. **Drain/state coupling**: a terminal `GoAway`/`Error` state implies the
    ///    connection is draining (`goaway()` sets both); the converse need not
    ///    hold (graceful drain stays in a live state).
    #[cfg(debug_assertions)]
    fn check_invariants<L>(&self, context: &Context<L>)
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        // (1) Watermark parity and bound.
        debug_assert!(
            self.last_stream_id & 1 == 0,
            "last_stream_id must stay an even watermark, got {}",
            self.last_stream_id
        );

        // (2) Per-stream caches are subsets of the live stream set, and every
        // mapping points at a valid context slot.
        debug_assert!(
            self.stream_table
                .stream_last_activity_at()
                .keys()
                .all(|id| self.stream_table.streams().contains_key(id)),
            "stream_last_activity_at must only track currently-open stream ids"
        );
        debug_assert!(
            self.stream_table
                .streams()
                .values()
                .all(|&gid| gid < context.streams.len()),
            "every stream mapping must point at a valid context slot"
        );

        // (4) Pending WINDOW_UPDATE coalescing map bound.
        debug_assert!(
            self.flow_control.pending_window_updates_len() <= self.max_pending_window_updates,
            "pending WINDOW_UPDATE map must stay within its per-connection cap"
        );

        // (5) Drain/state coupling: terminal states imply draining.
        debug_assert!(
            !matches!(self.state, H2State::GoAway | H2State::Error) || self.drain.draining(),
            "GoAway/Error state must imply the connection is draining"
        );
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
        let result = match frame {
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
        };
        // Run-to-completion post-condition: the connection-level cross-field
        // invariants must hold after every frame is dispatched, on success and
        // on the protocol-error paths alike.
        #[cfg(debug_assertions)]
        self.check_invariants(context);
        result
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
        if let Some(status) = context.status
            && ((100..200).contains(&status) || status == 204 || status == 304)
        {
            return true;
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
            self.flood_detector.record_empty_data_frame();
            check_flood_or_return!(self);
        }
        let Some(global_stream_id) = self.stream_table.get(data.stream_id) else {
            // The stream was terminated while data was expected,
            // probably due to automatic answer for invalid/unauthorized access.
            // RFC 9113 §6.9: we MUST still account for the DATA payload in
            // connection-level flow control using the full wire length
            // (including pad-length byte and padding), otherwise the window
            // shrinks permanently and eventually stalls the connection.
            let conn_threshold = self.connection_config.initial_connection_window / 2;
            if let Some(increment) = self
                .flow_control
                .account_received_bytes(wire_payload_len, conn_threshold)
            {
                self.queue_window_update(0, increment);
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
        if let Some(increment) = self
            .flow_control
            .account_received_bytes(wire_payload_len, conn_threshold)
        {
            self.queue_window_update(0, increment);
        }

        // RFC 9113 §8.1.1: if Content-Length is present, total DATA payload
        // must not exceed the declared length (check on every frame).
        // RFC 9110 §8.6: skip for HEAD/1xx/204/304 responses (body absent by definition).
        if !cl_exempt
            && let Some(expected) = declared_length
            && data_received > expected
        {
            error!(
                "{} Content-Length mismatch: received {} > declared {}",
                log_context!(self),
                data_received,
                expected
            );
            // Pair WRITABLE arming with the queued connection-level
            // WINDOW_UPDATE before returning; otherwise the credit sits
            // until the next inbound frame on this connection.
            if !self.flow_control.pending_window_updates_is_empty() {
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
        if !self.flow_control.pending_window_updates_is_empty() {
            self.readiness.arm_writable();
        }

        // Refresh per-stream idle timer on non-empty DATA.
        // Empty DATA frames (CVE-2019-9518 vector) must NOT reset the timer,
        // otherwise an attacker can keep a stream alive indefinitely with
        // zero-length frames while pinning a MAX_CONCURRENT_STREAMS slot.
        if content_len > 0 {
            self.stream_table.touch_activity(data.stream_id, self.now);
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
                if !cl_exempt
                    && let Some(expected) = declared_length
                    && data_received != expected
                {
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
                incr!(names::h2::SIGNAL_WRITABLE_REARMED_PEER_DATA);
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
        self.arm_timeout();
        if !headers.end_headers {
            // CVE-2024-27316: only initialize tracking on the very first HEADERS
            // fragment, not on re-entries from ContinuationFrame (which call
            // handle_frame(Frame::Headers) with the accumulated header block).
            self.flood_detector
                .begin_header_block_if_new(headers.header_block_fragment.len);
            if !self.header_reassembly.is_in_progress() {
                // First HEADERS frame of this block: copy its fragment out of
                // `zero.storage` now, before the caller's `expect_header()`
                // starts overwriting it with the next frame's bytes. A
                // re-entry from `(H2State::ContinuationFrame(headers), _)`
                // already appended THIS frame's own payload to the
                // accumulator before calling back in here (see that match
                // arm in `handle_read()`), so there is nothing left to copy.
                //
                // `data_opt` (bounds-checked), not `data` (panics on OOB):
                // `header_block_fragment` is network-facing-derived — the
                // parser computes it correctly by construction, so this
                // should never actually be out of bounds, but "no panic on
                // network-facing input" (CLAUDE.md) applies regardless, and
                // the CVE-2024-27316 abort path below this one used to make
                // exactly this check before this step replaced its own
                // buffer access with `HeaderBlockAccumulator::finish()`
                // (owned bytes, no slicing left to bound-check there).
                let Some(fragment) = headers
                    .header_block_fragment
                    .data_opt(self.zero.storage.buffer())
                else {
                    error!(
                        "{} header_block_fragment out of bounds of zero.storage",
                        log_context!(self)
                    );
                    return self.goaway(H2Error::InternalError);
                };
                self.header_reassembly.begin(fragment);
                self.zero.storage.clear();
            }
            debug!(
                "{} FRAGMENT: stream_id={}, accumulated_len={}",
                log_context!(self),
                headers.stream_id,
                self.header_reassembly.len()
            );
            self.state = H2State::ContinuationHeader(headers);
            return MuxResult::Continue;
        }
        // Header block is complete — reset CONTINUATION counters
        self.flood_detector.reset_continuation();
        // can this fail?
        let stream_id = headers.stream_id;
        let Some(global_stream_id) = self.stream_table.get(stream_id) else {
            error!(
                "{} Handling Headers frame with no attached stream {:#?}",
                log_context!(self),
                self
            );
            incr!(names::h2::HEADERS_NO_STREAM_ERROR);
            self.attribute_bytes_to_overhead();
            return self.force_disconnect();
        };

        // Refresh per-stream idle timer on HEADERS (response headers or trailers
        // on an existing stream). Initial HEADERS that create the stream already
        // set the timestamp in create_stream().
        self.stream_table.touch_activity(stream_id, self.now);

        if let Some(priority) = &headers.priority
            && self.scheduler.push_priority(stream_id, priority.clone())
        {
            // This HEADERS frame's own block just completed (single-frame,
            // or the final CONTINUATION of a multi-frame sequence) — but
            // we're aborting before ever reaching the `finish()` call below.
            // If a CONTINUATION sequence fed it, `header_reassembly` is
            // still `is_in_progress() == true`; retire it here or it leaks
            // that flag into the NEXT HEADERS frame processed on this
            // connection, which may belong to a completely different
            // stream — that frame's own "read from zero.storage" fast path
            // would then be skipped in favour of these stale, already-
            // discarded bytes (sozu-proxy/sozu review of e1c3c2fb, B1).
            // Regression:
            // `a_priority_self_dependency_reset_does_not_leak_the_reassembly_accumulator`.
            if self.header_reassembly.is_in_progress() {
                self.header_reassembly.finish();
            }
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

        let stream = &mut context.streams[global_stream_id];
        self.attribute_bytes_to_stream(&mut stream.metrics);
        // The field-block bytes live in one of two places depending on how
        // we got here: the owned reassembly accumulator when a CONTINUATION
        // sequence fed this HEADERS (`header_reassembly.is_in_progress()`),
        // or straight out of `zero.storage` for the single-frame fast path
        // — the common case, which never touches the accumulator at all.
        // See `h2_header_reassembly.rs`.
        let buffer: &[u8] = if self.header_reassembly.is_in_progress() {
            self.header_reassembly.data()
        } else {
            // Invariant: only reachable when nothing is accumulating for
            // THIS stream's own block. Every early-return path above this
            // point that could have left a CONTINUATION-fed reassembly
            // `is_in_progress()` now retires it first (see the priority
            // self-dependency branch above — B1 in the review of e1c3c2fb
            // was exactly a return that skipped this and leaked the flag
            // into the next HEADERS frame's decode).
            debug_assert!(!self.header_reassembly.is_in_progress());
            headers
                .header_block_fragment
                .data(self.zero.storage.buffer())
        };
        let stream = &mut context.streams[global_stream_id];
        let parts = &mut stream.split(&self.position);
        let was_initial = parts.rbuffer.is_initial();
        let elide_x_real_ip = parts.context.elide_x_real_ip;
        let status = pkawa::handle_header(
            self.hpack.decoder_mut(),
            self.scheduler.prioriser_mut(),
            stream_id,
            parts.rbuffer,
            buffer,
            headers.end_stream,
            parts.context,
            self.flood_detector.config().max_header_list_size(),
            self.flood_detector.config().max_header_fields(),
            elide_x_real_ip,
        );
        if self.header_reassembly.is_in_progress() {
            self.header_reassembly.finish();
        }
        self.zero.storage.clear();
        if let Err((error, global)) = status {
            match self.position {
                Position::Client(..) => incr!(names::http::BACKEND_PARSE_ERRORS),
                Position::Server => incr!(names::http::FRONTEND_PARSE_ERRORS),
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
                if let kawa::BodySize::Length(expected) = parts.rbuffer.body_size
                    && *parts.data_received != expected
                {
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
            self.mark_end_of_stream(stream);
        }
        if let StreamState::Linked(token) = stream.state {
            // Mirror of handle_data_frame's rearm. LIFECYCLE invariant 15.
            endpoint.readiness_mut(token).arm_writable();
            incr!(names::h2::SIGNAL_WRITABLE_REARMED_PEER_HEADERS);
        }
        // was_initial prevents trailers from triggering connection
        if was_initial && self.position.is_server() {
            incr!(names::http::REQUESTS);
            gauge_add!(names::http::ACTIVE_REQUESTS, 1);
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
        if let Some(global_stream_id) = self
            .stream_table
            .streams()
            .get(&priority.stream_id)
            .copied()
        {
            let stream = &mut context.streams[global_stream_id];
            self.attribute_bytes_to_stream(&mut stream.metrics);
        } else {
            self.attribute_bytes_to_overhead();
        }
        // Pass 3 Medium #4: standalone PRIORITY frames can arrive for any
        // peer-chosen stream ID. Accept only currently-open streams and a
        // small idle look-ahead window; everything else is dropped before
        // it can feed memory into the priority map.
        if self.scheduler.push_priority_guarded(
            priority.stream_id,
            priority.inner,
            self.last_stream_id,
            self.stream_table.streams(),
        ) {
            if let Some(global_stream_id) = self
                .stream_table
                .streams()
                .get(&priority.stream_id)
                .copied()
            {
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
        let (prev_urgency, _) = self.scheduler.priority(&pu.prioritized_stream_id);
        trace!(
            "{} PRIORITY_UPDATE stream={} urgency={}->{} incremental={} rearmed_writable=true",
            log_context!(self),
            pu.prioritized_stream_id,
            prev_urgency,
            urgency,
            incremental
        );
        let _ = self.scheduler.push_priority_guarded(
            pu.prioritized_stream_id,
            parser::PriorityPart::Rfc9218 {
                urgency,
                incremental,
            },
            self.last_stream_id,
            self.stream_table.streams(),
        );
        // LIFECYCLE invariant 15: reprioritisation only changes ordering for
        // the NEXT write pass. Under ET epoll, if finalize_write already
        // stripped WRITABLE, the scheduler won't re-run without a synthetic
        // wake — pair the interest insert with signal_pending_write.
        self.readiness.arm_writable();
        incr!(names::h2::SIGNAL_WRITABLE_REARMED_PRIORITY_UPDATE);
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
        self.flood_detector.record_rst_stream_window();
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
        let response_started = match self.stream_table.streams().get(&rst_stream.stream_id) {
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
            count!(names::h2::RST_STREAM_RECEIVED_PRE_RESPONSE_START, 1);
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
        if let Some(global_stream_id) = self
            .stream_table
            .streams()
            .get(&rst_stream.stream_id)
            .copied()
        {
            let stream = &mut context.streams[global_stream_id];
            self.attribute_bytes_to_stream(&mut stream.metrics);
            let linked_token = stream.linked_token();
            let (client_rtt, server_rtt) = self.snapshot_rtts(&endpoint, linked_token);
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
            self.hpack.set_decoder_max_allowed_table_size(
                self.local_settings.settings_header_table_size as usize,
            );
            self.attribute_bytes_to_overhead();
            return MuxResult::Continue;
        }
        // CVE-2019-9515: track SETTINGS frame rate
        self.flood_detector.record_settings_frame();
        check_flood_or_return!(self);
        for setting in settings.settings {
            let v = setting.value;
            let mut is_error = false;
            #[rustfmt::skip]
            match setting.identifier {
                parser::SETTINGS_HEADER_TABLE_SIZE => {
                    // Cap to the configured maximum — a malicious peer can
                    // advertise up to 4 GB to inflate HPACK encoder memory.
                    let cap = self.flood_detector.config().max_header_table_size();
                    let capped = v.min(cap);
                    self.peer_settings.settings_header_table_size = capped;
                    self.hpack.set_encoder_max_table_size(capped as usize);
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
                other => { warn!("Unknown setting_id: {}, we MUST ignore this", other); self.flood_detector.record_glitch() },
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
            && self.flow_control.window() <= DEFAULT_INITIAL_WINDOW_SIZE as i32
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
        self.stream_table.set_expect_write(Some(H2StreamId::Zero));
        self.readiness.signal_pending_write();
        MuxResult::Continue
    }

    fn handle_ping_frame(&mut self, ping: parser::Ping) -> MuxResult {
        if ping.ack {
            self.attribute_bytes_to_overhead();
            return MuxResult::Continue;
        }
        // CVE-2019-9512: track non-ACK PING frame rate
        self.flood_detector.record_ping_frame();
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
                incr!(names::h2::FRAMES_TX_PING_ACK);
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
        self.stream_table.set_expect_write(Some(H2StreamId::Zero));
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
        self.drain.observe_peer_goaway(goaway.last_stream_id);
        let peer_last_stream_id = self
            .drain
            .peer_last_stream_id()
            .expect("observe_peer_goaway just recorded this");

        // Streams with ID > last_stream_id were NOT processed by the peer.
        // Mark them for retry (StreamState::Link) so they can be retried
        // on a new connection.
        // IMPORTANT: do NOT call endpoint.end_stream() here — that would
        // remove the stream from the frontend's H2 stream map and send
        // RST_STREAM to the client, killing the request instead of retrying it.
        let mut retry_streams = Vec::new();
        for (&stream_id, &global_stream_id) in self.stream_table.streams() {
            if stream_id > peer_last_stream_id {
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
        if self.stream_table.streams().is_empty() {
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
                if let Some(global_stream_id) = self.stream_table.get(stream_id) {
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
                self.flood_detector.record_glitch();
                check_flood_or_return!(self);
                self.attribute_bytes_to_overhead();
                return MuxResult::Continue;
            }
        }

        // The parser masks the reserved bit (STREAM_ID_MASK), so increment <=
        // 2^31-1 and try_from always succeeds. Use try_from rather than `as` to
        // guard against a future parser change that drops the mask.
        let increment = i32::try_from(increment).unwrap_or(i32::MAX);
        // RFC 9113 §6.9: a non-zero WINDOW_UPDATE increment is in [1, 2^31-1].
        // Zero was short-circuited above; this asserts the masked value is a
        // legal positive increment before we add it to a window.
        debug_assert!(
            increment > 0,
            "WINDOW_UPDATE increment must be strictly positive at this point (zero handled above)"
        );
        if stream_id == 0 {
            // Count connection-level WINDOW_UPDATEs before touching the window
            // so a per-window flood stops us before we pay the arithmetic cost
            // on a million-frame burst. Zero-increment frames short-circuited
            // above, so every increment here is a legal-looking rate consumer.
            self.flood_detector.record_window_update_stream0();
            check_flood_or_return!(self);
            self.attribute_bytes_to_overhead();
            // Window arithmetic + its replenish-invariant asserts live on
            // `H2FlowControl` now (see its module doc); this arm keeps the
            // frame-dispatch orchestration — flood accounting, logging,
            // GOAWAY — which needs `self` (flood_detector, readiness,
            // log_context!) that the flow-control module deliberately does
            // not have.
            match self.flow_control.apply_window_update(increment) {
                h2_flow_control::ApplyWindowUpdateOutcome::Applied {
                    new_window,
                    should_arm_writable,
                } => {
                    if should_arm_writable {
                        self.readiness.arm_writable();
                    }
                    debug!(
                        "{} WINDOW_UPDATE received: stream=0 increment={} new_connection_window={}",
                        log_context!(self),
                        increment,
                        new_window
                    );
                }
                h2_flow_control::ApplyWindowUpdateOutcome::Overflow => {
                    error!("{} INVALID WINDOW INCREMENT", log_context!(self));
                    return self.goaway(H2Error::FlowControlError);
                }
            }
        } else if let Some(global_stream_id) = self.stream_table.get(stream_id) {
            let stream = &mut context.streams[global_stream_id];
            self.attribute_bytes_to_stream(&mut stream.metrics);
            let stream_window_before = stream.window;
            if let Some(window) = stream.window.checked_add(increment) {
                if stream.window <= 0 && window > 0 {
                    self.readiness.arm_writable();
                }
                stream.window = window;
                // Same replenish invariant as the connection window, applied to
                // the per-stream send window (RFC 9113 §6.9.1). Overflow past
                // 2^31-1 is rejected by `checked_add` and handled as a
                // FLOW_CONTROL_ERROR RST_STREAM below.
                debug_assert_eq!(
                    stream.window,
                    stream_window_before + increment,
                    "stream window must increase by exactly the increment"
                );
                debug_assert!(
                    stream.window > stream_window_before,
                    "a positive WINDOW_UPDATE must strictly grow the stream window"
                );
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
            self.flood_detector.record_glitch();
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
        for &global_stream_id in self.stream_table.streams().values() {
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
                    self.stream_table.streams().len(),
                    self.stream_table.expect_write(),
                    self.socket.socket_wants_write(),
                    self.readiness
                );
                MuxResult::Continue
            }
            Position::Server => {
                // Don't disconnect immediately if rustls still has buffered TLS
                // records. Returning CloseSession here triggers shutdown(Write)
                // which sends FIN — but any TLS records still in rustls's buffer
                // (not yet flushed to the TCP send buffer) are lost, causing the
                // client to see "TLS decode error / unexpected eof".
                // Instead, keep WRITABLE interest and let the writable path flush.
                // The decision itself is `h2_close::force_disconnect_action`,
                // exhaustively unit-tested there.
                //
                // The answer is read ONCE and reported by both log lines. A
                // literal `wants_write=` in either arm is a second copy of a
                // fact the socket already owns, and the closing arm is reached
                // with records still pending whenever the peer is gone — an
                // operator diagnosing a truncation under HAProxy chaining would
                // read the opposite of the socket's state.
                let tls_wants_write = self.socket.socket_wants_write();
                if h2_close::force_disconnect_action(
                    self.peer_gone_after_final_goaway(),
                    tls_wants_write,
                ) == CloseAction::ReArmAndContinue
                {
                    debug!(
                        "{} H2 force_disconnect delaying close: state={:?}, streams={}, expect_write={:?}, wants_write={}, readiness={:?}",
                        log_context!(self),
                        self.state,
                        self.stream_table.streams().len(),
                        self.stream_table.expect_write(),
                        tls_wants_write,
                        self.readiness
                    );
                    self.readiness.interest = Ready::WRITABLE | Ready::HUP | Ready::ERROR;
                    self.ensure_tls_flushed();
                    MuxResult::Continue
                } else {
                    debug!(
                        "{} H2 force_disconnect closing session: state={:?}, streams={}, expect_write={:?}, wants_write={}, readiness={:?}",
                        log_context!(self),
                        self.state,
                        self.stream_table.streams().len(),
                        self.stream_table.expect_write(),
                        tls_wants_write,
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
                if !self.stream_table.streams().is_empty()
                    || tls_pending_before
                    || self.stream_table.expect_write().is_some()
                {
                    debug!(
                        "{} H2 close with active state: state={:?}, streams={}, expect_write={:?}, wants_write={}, readiness={:?}",
                        log_context!(self),
                        self.state,
                        self.stream_table.streams().len(),
                        self.stream_table.expect_write(),
                        tls_pending_before,
                        self.readiness
                    );
                    for (stream_id, global_stream_id) in self.stream_table.streams() {
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
                    if !self.stream_table.streams().is_empty() {
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
                            self.stream_table.streams().len(),
                            self.stream_table.expect_write(),
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
                            self.stream_table.streams().len(),
                            self.stream_table.expect_write(),
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
                            self.stream_table.streams().len(),
                            self.stream_table.expect_write(),
                            self.close_notify_sent,
                            self.readiness
                        );
                    }
                }
                return;
            }
        }
        // reconnection is handled by the server for each stream separately
        for global_stream_id in self.stream_table.streams().values() {
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
        let (client_rtt, server_rtt) = self.snapshot_rtts(&endpoint, linked_token);
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
                    .stream_table
                    .streams()
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
                    if !fully_completed && !self.stream_table.rst_sent_contains(id) {
                        let kawa = &mut self.zero;
                        let mut frame = [0; 13];
                        if let Ok((_, _size)) =
                            serializer::gen_rst_stream(&mut frame, id, H2Error::Cancel)
                        {
                            let buf = kawa.storage.space();
                            if buf.len() >= frame.len() {
                                buf[..frame.len()].copy_from_slice(&frame);
                                kawa.storage.fill(frame.len());
                                incr!(names::h2::FRAMES_TX_RST_STREAM);
                                count!(metric_for_rst_stream_sent(H2Error::Cancel), 1);
                                self.readiness.arm_writable();
                                self.stream_table.rst_sent_mut().insert(id);
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
                    EndStreamAction::ReplayOnFreshBackend => {
                        // Reachable only through an H1 BACKEND behind this H2
                        // frontend: `reused_from_pool` — and so the captured
                        // request — is set by `ConnectionH1::start_stream`
                        // alone, and an H2 backend never fills it. The action
                        // is handled here rather than merged into
                        // `SendDefault` so the H2 frontend re-routes the same
                        // stale-upstream race the H1 frontend does, with the
                        // same bytes on the wire.
                        match stream.queue_upstream_replay() {
                            Some(len) => {
                                debug!(
                                    "{} H2 REPLAY {} request bytes on a fresh backend",
                                    log_context!(self),
                                    len
                                );
                                incr!(
                                    names::backend::RETRY_STALE_UPSTREAM,
                                    stream.context.cluster_id.as_deref(),
                                    stream.context.backend_id.as_deref()
                                );
                                stream.state = StreamState::Link;
                                context.pending_links.push_back(stream_gid);
                            }
                            None => {
                                error!(
                                    "{} replay selected with no captured request",
                                    log_context!(self)
                                );
                                let answers = answers_rc.borrow();
                                set_default_answer(stream, &mut self.readiness, 502, &answers);
                            }
                        }
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

    pub fn start_stream<L>(&mut self, stream: GlobalStreamId, context: &mut Context<L>) -> bool
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        // Entry point: a backend connection reaches this from the router
        // without necessarily having had `readable`/`writable` called on it
        // this pass, so adopt the mux's snapshot before arming the new
        // stream's liveness deadline below.
        self.now = context.now;
        // RFC 9113 §6.8: reject new streams on a draining connection
        if self.drain.draining() {
            error!(
                "{} Cannot open new stream on draining connection (stream {})",
                log_context!(self),
                stream
            );
            return false;
        }
        // RFC 9113 §5.1.2: respect peer's max concurrent streams limit
        if self.stream_table.len() >= self.peer_settings.settings_max_concurrent_streams as usize {
            error!(
                "{} Cannot open new stream: active={} >= peer max_concurrent_streams={}",
                log_context!(self),
                self.stream_table.len(),
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
            self.graceful_goaway(self.now);
            return false;
        };
        self.stream_table.register(stream_id, stream, self.now);
        self.readiness.arm_writable();
        true
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use super::*;
    use crate::{
        pool::Pool,
        protocol::{
            kawa_h1::editor::HttpContext,
            mux::{
                connection::EndpointClient,
                router::Router,
                test_support::{connected_socket, test_context},
            },
        },
    };

    // ── H2FloodDetector / H2FloodViolation ─────────────────────────────
    //
    // H2FloodDetector's own unit tests (threshold trips, half-decay,
    // lifetime ceilings, metric-key uniqueness) moved to
    // `h2_flood_detector.rs`'s `#[cfg(test)] mod tests` alongside the type —
    // its fields are private to that module now, matching `HpackState`,
    // `H2FlowControl` and `H2StreamTable`. The old test comparing
    // default-constructed vs. explicitly-constructed detectors did not
    // move: it compared `H2FloodDetector::default()` against
    // `H2FloodDetector::new(...)`, and this extraction removed `impl Default
    // for H2FloodDetector` entirely (LIFECYCLE.md invariant 20's last
    // self-sampling-clock exception), so there is no second constructor left
    // for it to compare against.

    // ── Prioriser / scheduler ───────────────────────────────────────────
    //
    // `Prioriser`'s own unit tests (defaults, the RFC 9218 urgency clamp,
    // the `MAX_PRIORITIES` cap, the standalone-PRIORITY acceptance filter,
    // the same-urgency incremental rotation) moved to `h2_scheduler.rs`'s
    // `#[cfg(test)] mod tests` alongside the type — its fields are private
    // to that module now, matching `HpackState`, `H2FlowControl`,
    // `H2StreamTable` and `H2FloodDetector`. The pass-scoped scheduling
    // tests that used to re-implement `ready_incremental_by_urgency`'s
    // arithmetic inline in the test body moved there too, and now drive
    // `ReadyIncrementalCensus`'s production methods instead of a copy of
    // them.

    // ── H2FlowControl ───────────────────────────────────────────────────
    //
    // H2FlowControl's own unit tests (initial state, coalescing, saturation,
    // negative window, the ordering-determinism regression test) moved to
    // `h2_flow_control.rs`'s `#[cfg(test)] mod tests` alongside the type —
    // its fields are private to that module now, matching `HpackState`.

    // ── H2FloodConfig ───────────────────────────────────────────────────
    //
    // Moved to `h2_flood_detector.rs`'s `#[cfg(test)] mod tests` alongside
    // `H2FloodConfig`, `H2FloodViolation` and `H2FloodDetector`.

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

    // The old settings-change-negative-window test was removed here, not
    // moved: its docstring premise was wrong. `update_initial_window_size`
    // (below) only ever adjusts *per-stream* windows on a
    // SETTINGS_INITIAL_WINDOW_SIZE change, never the connection-level
    // `flow_control.window` that test constructed and mutated directly — so
    // it never exercised a real code path. `h2_flow_control.rs`'s test
    // `consume_send_window_saturates_at_i32_min_without_wrapping` covers a
    // genuinely different, previously-untested boundary of the same
    // `consume_send_window` API (the `saturating_sub` floor at `i32::MIN`);
    // it does not restore the deleted test's coverage, because the deleted
    // test covered nothing real.

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
    //
    // Moved to `h2_flood_detector.rs`'s `#[cfg(test)] mod tests`
    // (`test_flood_config_default_matches_constants`, `test_flood_config_equality`).

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

    // ── cumulative-stall budget decision (fc_stall_budget_decision) ──

    #[test]
    fn test_fc_stall_budget_open_window_always_clears() {
        // A genuinely open send window is a real un-stall, regardless of prior
        // accumulated progress or this pass's drain.
        assert_eq!(
            fc_stall_budget_decision(false, 0, None),
            FcStallAction::Clear
        );
        assert_eq!(
            fc_stall_budget_decision(false, 1, Some(5)),
            FcStallAction::Clear
        );
        assert_eq!(
            fc_stall_budget_decision(false, i32::MAX, Some(FC_STALL_CLEAR_FLOOR)),
            FcStallAction::Clear
        );
    }

    #[test]
    fn test_fc_stall_budget_blocked_arms_and_accumulates() {
        // First blocked pass arms with this pass's drain.
        assert_eq!(
            fc_stall_budget_decision(true, 1, None),
            FcStallAction::Arm { progress: 1 }
        );
        // A blocked pass with no drain keeps the accumulator unchanged, so the
        // deadline keeps aging (a window-0 stall makes consumed == 0).
        assert_eq!(
            fc_stall_budget_decision(true, 0, Some(42)),
            FcStallAction::Arm { progress: 42 }
        );
        // Negative `consumed` is clamped to 0 (defensive; converter.window only
        // shrinks, so consumed is >= 0 in practice).
        assert_eq!(
            fc_stall_budget_decision(true, -10, Some(7)),
            FcStallAction::Arm { progress: 7 }
        );
    }

    #[test]
    fn test_fc_stall_budget_floor_clears() {
        // Reaching the floor in a single pass (a full DATA frame of real
        // delivery) clears the deadline.
        assert_eq!(
            fc_stall_budget_decision(true, FC_STALL_CLEAR_FLOOR as i32, None),
            FcStallAction::Clear
        );
        // Exactly one byte below the floor still arms.
        assert_eq!(
            fc_stall_budget_decision(true, (FC_STALL_CLEAR_FLOOR - 1) as i32, None),
            FcStallAction::Arm {
                progress: FC_STALL_CLEAR_FLOOR - 1
            }
        );
        // Prior progress plus this pass crossing the floor clears.
        assert_eq!(
            fc_stall_budget_decision(true, 1, Some(FC_STALL_CLEAR_FLOOR - 1)),
            FcStallAction::Clear
        );
    }

    #[test]
    fn test_fc_stall_budget_wu_drip_ages_until_floor() {
        // The WINDOW_UPDATE(+1) closure: a 1-byte-per-pass drip must keep the
        // deadline armed (aging) for the whole run up to the floor and only
        // clear on the pass that reaches it — so a drip granting < floor bytes
        // per idle period is reaped, never kept alive. This is the unit-level
        // proof that the budget closes the WINDOW_UPDATE-drip vector.
        let mut progress: Option<usize> = None;
        for pass in 1..FC_STALL_CLEAR_FLOOR {
            match fc_stall_budget_decision(true, 1, progress) {
                FcStallAction::Arm { progress: p } => {
                    assert_eq!(p, pass, "drip accumulator off at pass {pass}");
                    progress = Some(p);
                }
                FcStallAction::Clear => panic!("drip cleared the deadline early at pass {pass}"),
            }
        }
        // The pass that reaches the floor finally clears.
        assert_eq!(
            fc_stall_budget_decision(true, 1, progress),
            FcStallAction::Clear
        );
    }

    // ── flow-control-stall reaper union (h2_stream_table::collect_timed_out) ──
    //
    // The two-guard union (bidirectional-silence + flow-control-stall),
    // its dedup, its rst_sent/untracked filtering, and its determinism
    // (ascending stream_id order — the regression test for this
    // extraction's fix) are now unit-tested directly against
    // `H2StreamTable::collect_timed_out` in `h2_stream_table.rs`'s own test
    // module, without needing a full `ConnectionH2` fixture.

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
            tls_cert_names: None,
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
            tags: None,
            access_log_message: None,
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

    // ── Mid-pass ready-incremental census consistency ───────────────────
    //
    // The three scalar bucket-decrement tests moved to `h2_scheduler.rs`.
    // They were named after `ready_incremental_by_urgency`, the local
    // `HashMap` this step replaced with `ReadyIncrementalCensus`; no symbol
    // of that name exists anywhere in the tree any more.
    // They used to re-implement the `saturating_sub` + bucket-scoped
    // decrement inline in the test body — a copy of the scheduler's
    // arithmetic, which could not catch the scheduler getting it wrong.
    // They now call `ReadyIncrementalCensus::note_ineligible` and
    // `::incremental_peer_count`, which is the code `write_streams` runs.

    // ── h2.streams.ready_incremental.by_urgency aggregate ────────────────

    /// Read the process-local `h2.streams.ready_incremental.by_urgency` gauge,
    /// treating an absent key as 0. `dump_local_proxy_metrics` is a
    /// non-draining filter over the proxy `MetricsMap`, so repeated reads are
    /// side-effect free and the key is the raw metric name.
    fn ready_incremental_gauge() -> i64 {
        use sozu_command::proto::command::filtered_metrics::Inner;
        crate::metrics::METRICS.with(|metrics| {
            metrics
                .borrow_mut()
                .dump_local_proxy_metrics()
                .get(names::h2::STREAMS_READY_INCREMENTAL_BY_URGENCY)
                .and_then(|fm| fm.inner.as_ref())
                .and_then(|inner| match inner {
                    Inner::Gauge(v) => Some(*v as i64),
                    _ => None,
                })
                .unwrap_or(0)
        })
    }

    /// `h2.streams.ready_incremental.by_urgency` is an aggregate across every
    /// live H2 connection, so a connection must hand back exactly what it
    /// contributed, on every close path. [`Drop`] is the single decrement
    /// site; this pins the round trip.
    ///
    /// Before this was folded into `last_gauge_snapshot`, the metric was an
    /// absolute `gauge!` set from inside `write_streams`: connection B
    /// overwrote connection A's value, nothing was ever released, and the
    /// dashboard read "whatever the last writer wrote" instead of a sum.
    ///
    /// `METRICS` is a thread-local, so this asserts the DELTA around one
    /// connection's lifetime — robust to any starting value.
    ///
    /// To SEE THIS RED: delete the
    /// `if r != 0 { gauge_add!(names::h2::STREAMS_READY_INCREMENTAL_BY_URGENCY, -(r as i64)); }`
    /// arm from [`ConnectionH2::release_connection_gauges`]. The live-delta
    /// assertion still passes; the post-drop one fails with `left: 3, right: 0`
    /// — the connection's contribution outliving the connection, which is the
    /// aggregate drifting upward by 3 for the rest of the worker's life.
    #[test]
    fn dropping_a_connection_releases_its_ready_incremental_contribution() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(2, 4, 16384)));

        // A non-blocking connect to a (likely unused) loopback port returns a
        // real socket handle immediately, regardless of whether the connection
        // completes; nothing below reads or writes it.
        let socket = mio::net::TcpStream::connect(
            format!("127.0.0.1:{}", crate::testing::provide_port())
                .parse()
                .expect("loopback address must parse"),
        )
        .expect("mio connect must return a socket handle");

        let before = ready_incremental_gauge();

        let mut connection = ConnectionH2::new(
            Ulid::generate(),
            socket,
            Position::Server,
            Rc::downgrade(&pool),
            H2FloodConfig::default(),
            H2ConnectionConfig::default(),
            Duration::from_secs(30),
            None,
            Duration::from_secs(30),
            Some((H2StreamId::Zero, CLIENT_PREFACE_SIZE)),
            Ready::READABLE | Ready::HUP | Ready::ERROR,
        )
        .expect("a pool with free buffers must yield an H2 connection");

        // What the tail of `write_streams` writes once the pass's bucket map
        // is final, published by the `gauge_connection_state` call that
        // follows it.
        connection.ready_incremental_streams = 3;
        connection.gauge_connection_state();

        assert_eq!(
            ready_incremental_gauge() - before,
            3,
            "gauge_connection_state must add this connection's ready-incremental \
             count to the aggregate as a signed delta"
        );

        drop(connection);

        assert_eq!(
            ready_incremental_gauge(),
            before,
            "Drop must subtract exactly the contribution the connection made, \
             returning the aggregate to its prior value"
        );
    }

    // ── Clock hygiene: every deadline reads `ConnectionH2::now` ──────────
    //
    // `Mux` is the only clock sampler; the H2 core reads the snapshot it
    // mirrors into `ConnectionH2::now`. A reintroduced `Instant::now()` in
    // the core would still "work" in production — it would just be reading a
    // second clock — so these two tests advance ONLY the connection's
    // snapshot and prove the decision moves with it. Neither sleeps.

    // ── TLS backpressure: the GoAway truncation vector (#1454) ──────────
    //
    // Until this harness existed, NO test anywhere instantiated a
    // `ConnectionH2` over a handler whose `socket_wants_write()` could return
    // `true`. `mio::net::TcpStream` and `SessionTcpStream` both take the
    // trait's `false` default and only `FrontRustls` overrides it, so every
    // branch that asks "does rustls still hold records?" was statically dead
    // in the test suite — including the one whose own comment calls it the
    // primary truncation vector. These are the first tests that branch class
    // has ever had against a handler that answers `true`.

    /// A `SocketHandler` that models rustls-over-a-blocked-kernel: it holds
    /// `pending` records, and each empty-buffer flush drains `drain_per_flush`
    /// of them. `drain_per_flush = 0` is a kernel that accepts nothing, which
    /// is the backpressure case; a positive value is a kernel that takes them.
    ///
    /// Modelling the state rather than scripting an answer sequence is
    /// deliberate: the close path queries `socket_wants_write()` a number of
    /// times that depends on which branches it takes, so a positional script
    /// would pin the query COUNT instead of the behaviour and would have to be
    /// rewritten by anyone who added a query.
    struct BackpressuredTlsSocket {
        stream: mio::net::TcpStream,
        pending: std::cell::Cell<usize>,
        drain_per_flush: usize,
        flushes: std::cell::Cell<usize>,
        /// Scripted `(size, status)` answers for `socket_write_vectored`,
        /// consumed front to back. An EMPTY script — the default, and what
        /// every test written before this field had — delegates to the real
        /// loopback socket, so those tests are byte-for-byte unaffected.
        ///
        /// The record model above cannot express the pair this scripts.
        /// `pending`/`drain_per_flush` answer "does rustls still hold
        /// records?"; they say nothing about what a *vectored* write
        /// reported, because that method delegated to the kernel and the
        /// kernel only ever answers `(0, WouldBlock)` when it blocks.
        /// `FrontRustls` does not: `socket_write_vectored` accumulates
        /// `buffered_size` from `session.writer().write(..)` — plaintext
        /// rustls took off the caller's hands — and separately sets
        /// `can_write = false` when `write_tls` hits `WouldBlock` against the
        /// kernel (`socket.rs`). The two are independent, so
        /// `(size > 0, WouldBlock)` is a shape ONLY the TLS handler returns.
        /// That is why the script lives on this fixture rather than on a
        /// second one: it is the same handler answering the same question the
        /// record model already answers, one layer down.
        vectored_script: std::collections::VecDeque<(usize, SocketResult)>,
        /// How many times `socket_write_vectored` was called, whether the
        /// answer came from the script or from the delegated loopback socket.
        ///
        /// Counting BOTH is what makes this a falsifiable witness. Counting
        /// only the scripted answers bounds the total by the script's length,
        /// so an assertion that the loop did NOT go round again could only
        /// ever deviate downwards and could never fail — and the extra
        /// delegated rounds are exactly what such an assertion has to be able
        /// to see.
        vectored_calls: usize,
    }

    impl BackpressuredTlsSocket {
        fn new(stream: mio::net::TcpStream, pending: usize, drain_per_flush: usize) -> Self {
            Self {
                stream,
                pending: std::cell::Cell::new(pending),
                drain_per_flush,
                flushes: std::cell::Cell::new(0),
                vectored_script: std::collections::VecDeque::new(),
                vectored_calls: 0,
            }
        }

        /// Script the `(size, status)` pairs `socket_write_vectored` returns.
        ///
        /// A builder rather than two more `new` parameters, so the three tests
        /// that predate the script keep calling `new` unchanged. Each `size`
        /// is a CAP, clamped to what the gather actually offered: a scripted
        /// count above the offer would trip `flush_stream_out`'s own
        /// `debug_assert!(size <= offered)`, and reddening through a
        /// production assertion is the tree noticing rather than the test.
        fn with_vectored_script(
            mut self,
            script: impl IntoIterator<Item = (usize, SocketResult)>,
        ) -> Self {
            self.vectored_script = script.into_iter().collect();
            self
        }
    }

    impl SocketHandler for BackpressuredTlsSocket {
        fn socket_read(&mut self, buf: &mut [u8]) -> (usize, SocketResult) {
            self.stream.socket_read(buf)
        }

        fn socket_write(&mut self, buf: &[u8]) -> (usize, SocketResult) {
            if buf.is_empty() {
                self.flushes.set(self.flushes.get() + 1);
                let drained = self.drain_per_flush.min(self.pending.get());
                self.pending.set(self.pending.get() - drained);
                return (0, SocketResult::Continue);
            }
            self.stream.socket_write(buf)
        }

        fn socket_write_vectored(&mut self, bufs: &[IoSlice]) -> (usize, SocketResult) {
            self.vectored_calls += 1;
            match self.vectored_script.pop_front() {
                Some((cap, status)) => {
                    let offered: usize = bufs.iter().map(|slice| slice.len()).sum();
                    (cap.min(offered), status)
                }
                None => self.stream.socket_write_vectored(bufs),
            }
        }

        /// The override that makes this harness worth having.
        fn socket_wants_write(&self) -> bool {
            self.pending.get() > 0
        }

        fn socket_ref(&self) -> &mio::net::TcpStream {
            &self.stream
        }

        fn socket_mut(&mut self) -> &mut mio::net::TcpStream {
            &mut self.stream
        }

        fn peer_addr(&self) -> Option<std::net::SocketAddr> {
            mio::net::TcpStream::peer_addr(&self.stream).ok()
        }

        fn protocol(&self) -> crate::socket::TransportProtocol {
            crate::socket::TransportProtocol::Tls1_3
        }

        fn read_error(&self) {}

        fn write_error(&self) {}
    }

    /// `state` is a parameter rather than a second copy of this fixture: the
    /// close decision reads it as `H2State::GoAway` and the write-pass
    /// finalization reads it as `H2State::Header`, and both need the same
    /// handler underneath.
    fn connection_with_backpressure(
        pool: &Rc<RefCell<Pool>>,
        pending: usize,
        drain_per_flush: usize,
        state: H2State,
    ) -> (ConnectionH2<BackpressuredTlsSocket>, std::net::TcpStream) {
        let (socket, peer) = connected_socket();
        let mut connection = ConnectionH2::new(
            Ulid::generate(),
            BackpressuredTlsSocket::new(socket, pending, drain_per_flush),
            Position::Server,
            Rc::downgrade(pool),
            H2FloodConfig::default(),
            H2ConnectionConfig::default(),
            Duration::from_secs(30),
            None,
            Duration::from_secs(30),
            Some((H2StreamId::Zero, CLIENT_PREFACE_SIZE)),
            Ready::WRITABLE | Ready::HUP | Ready::ERROR,
        )
        .expect("a pool with free buffers must yield an H2 connection");
        connection.state = state;
        (connection, peer)
    }

    /// Records that survive the flush hold the connection open.
    ///
    /// This is the branch whose own comment calls it the primary truncation
    /// vector: closing here sends FIN and destroys the records rustls is still
    /// holding, which the client reads as a truncated response.
    ///
    /// TO SEE THIS RED: delete the `self.ensure_tls_flushed();` call in the
    /// `CloseAction::ReArmAndContinue` arm of `ConnectionH2::writable`'s
    /// `H2State::GoAway` branch. The WRITABLE *event* bit is then never
    /// re-signalled and this test fails on its OWN assertion, `the WRITABLE
    /// event must be re-signalled so the event loop retries the flush`. The
    /// recipe deliberately does not swap `TlsFlushPhase::AfterFlush` for
    /// `BeforeFlush`: that reddens through the production `unreachable!`, which
    /// is someone else's assertion and would pass whatever this test claimed.
    #[test]
    fn a_flush_that_does_not_drain_keeps_the_connection_open() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(2, 4, 16384)));
        // A kernel that accepts nothing: every flush leaves the records.
        let (mut connection, _peer) = connection_with_backpressure(&pool, 2, 0, H2State::GoAway);
        let mut context = test_context(&pool);
        let mut router = Router::new(Duration::from_secs(30), Duration::from_secs(30));

        // Premise: the handler really does report buffered records. Without
        // this the assertions below would pass against a handler taking the
        // trait's `false` default, which is exactly the blind spot #1454 names.
        assert!(
            connection.socket.socket_wants_write(),
            "premise: this harness must report buffered TLS records"
        );

        let result = connection.writable(&mut context, EndpointClient(&mut router));

        assert!(
            matches!(result, MuxResult::Continue),
            "records still buffered: the session must stay open, got {result:?}"
        );
        // `Continue` alone does not discriminate: `force_disconnect`'s server
        // arm also returns it for a live peer with records pending. The re-arm
        // path returns from the GoAway arm BEFORE reaching `force_disconnect`,
        // so the state is still `GoAway`; the fall-through would have gone
        // through `force_disconnect`, which sets `H2State::Error` first.
        assert!(
            matches!(connection.state, H2State::GoAway),
            "the GoAway arm must re-arm and return, not fall through to \
             force_disconnect, got {:?}",
            connection.state
        );
        // Not `readiness.interest`: that bit is this fixture's own argument to
        // `ConnectionH2::new` and nothing on this path clears it, so asserting
        // on it would assert on the harness. The edge-triggered re-arm is the
        // EVENT bit, set by `ensure_tls_flushed` -> `signal_pending_write`.
        assert!(
            connection.readiness.event.is_writable(),
            "the WRITABLE event must be re-signalled so the event loop retries \
             the flush, got {:?}",
            connection.readiness
        );
        // Not `socket_wants_write()`: with `drain_per_flush = 0` that can never
        // change, so it is a tautology of the harness no production edit can
        // falsify. The flush COUNT is falsifiable — a GoAway arm that skipped
        // its own flush would leave it at 1.
        assert!(
            connection.socket.flushes.get() >= 2,
            "the preamble and the GoAway arm must each attempt a flush, got {}",
            connection.socket.flushes.get()
        );
    }

    /// A flush the kernel accepts closes within ONE `writable()` call.
    ///
    /// This is the tick-count assertion. The pre-image asked its second
    /// question inline, immediately after the flush; a split that returned
    /// `Continue` and waited to be called again would still deliver the bytes,
    /// but would double the latency of every close under backpressure and
    /// would strand the connection against a peer that never re-arms.
    ///
    /// TO SEE THIS RED: in the `H2State::GoAway` arm, add
    /// `return MuxResult::Continue;` immediately after the
    /// `self.socket.socket_write(&[]);` inside the `CloseAction::Flush` body —
    /// the deferred shape, which still performs the flush. The first call then
    /// returns `Continue` and the final assertion fails with `a drained flush
    /// must reach the disconnect in the SAME writable call`. Dropping the flush
    /// as well would redden the premise at the top of the test instead, with a
    /// different message.
    #[test]
    fn a_flush_that_succeeds_closes_within_one_writable_call() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(2, 4, 16384)));
        // Two records, one drained per flush: the preamble takes one and the
        // GoAway arm's own flush takes the other, so the post-flush query is
        // the first one that can answer `false`.
        let (mut connection, _peer) = connection_with_backpressure(&pool, 2, 1, H2State::GoAway);
        let mut context = test_context(&pool);
        let mut router = Router::new(Duration::from_secs(30), Duration::from_secs(30));

        assert!(
            connection.socket.socket_wants_write(),
            "premise: this harness must report buffered TLS records"
        );

        let result = connection.writable(&mut context, EndpointClient(&mut router));

        assert!(
            !connection.socket.socket_wants_write(),
            "premise: both flushes landed, so nothing is pending any more"
        );
        assert!(
            connection.socket.flushes.get() >= 2,
            "the preamble and the GoAway arm must each attempt a flush, got {}",
            connection.socket.flushes.get()
        );
        assert!(
            !matches!(result, MuxResult::Continue),
            "a drained flush must reach the disconnect in the SAME writable \
             call, not defer it to another tick: got {result:?}"
        );
    }

    // ── The write pass's own flush triple (`finalize_write`) ────────────
    //
    // `ConnectionH2::finalize_write` ends every write pass with the same
    // query / flush / query shape the close sites use, and its decision now
    // lives beside theirs in `h2_close::finalize_action`. The two tests below
    // are the CALLER half — that `h2.rs` performs the step each answer names
    // and wires the inputs to the right parameters. The answers themselves are
    // enumerated exhaustively in `h2_close`'s own tables, which no socket can
    // reach: `SkipFlush`, `Parked` and `RetainPendingBack` are covered there
    // only, because reaching them through `writable()` needs a stream carrying
    // response bytes through the scheduler and none of this module's fixtures
    // builds one. Same boundary `h2_close`'s `force_disconnect` re-arm branch
    // already sits on.

    /// The pass flushes once of its own and re-arms when records survive it.
    ///
    /// Three records, one drained per flush: `writable`'s preamble takes the
    /// first, `finalize_write`'s own flush takes the second, and the third is
    /// still pending when the post-flush query runs — so the re-arm branch is
    /// reached with the socket genuinely still holding data, not by a fixture
    /// that can only answer one way.
    ///
    /// TO SEE THIS RED, either half independently:
    /// (a) empty the `FinalizeAction::Flush` arm of `finalize_write` (the
    ///     `SkipFlush` behaviour, which is what a caller that dropped the
    ///     `socket_write` input would do for every pass). The flush count
    ///     assertion fails with `finalize_write must attempt exactly one
    ///     empty-buffer flush of its own`.
    /// (b) empty the `FinalizeAction::ReArm` arm. The event assertion fails
    ///     with `the WRITABLE event must be re-signalled`.
    /// Neither recipe reddens through a production `unreachable!`.
    #[test]
    fn a_finalized_write_pass_flushes_once_and_re_arms_while_records_survive() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(2, 4, 16384)));
        let (mut connection, _peer) = connection_with_backpressure(&pool, 3, 1, H2State::Header);
        let mut context = test_context(&pool);
        let mut router = Router::new(Duration::from_secs(30), Duration::from_secs(30));

        assert!(
            connection.socket.socket_wants_write(),
            "premise: this harness must report buffered TLS records"
        );
        // Not the interest bit: `signal_pending_write` touches `event` only,
        // and this fixture hands `Ready::WRITABLE` to `ConnectionH2::new` as
        // its INTEREST, so an assertion written against interest would be
        // reading the fixture's own argument back and could not see the
        // re-arm at all.
        assert!(
            !connection.readiness.event.is_writable(),
            "premise: the WRITABLE event must start clear so the re-arm below              is the only thing that can set it, got {:?}",
            connection.readiness
        );

        let result = connection.writable(&mut context, EndpointClient(&mut router));

        assert!(
            matches!(result, MuxResult::Continue),
            "a write pass with records still buffered continues, got {result:?}"
        );
        assert_eq!(
            connection.socket.flushes.get(),
            2,
            "finalize_write must attempt exactly one empty-buffer flush of its              own on top of the preamble's"
        );
        assert!(
            connection.socket.socket_wants_write(),
            "premise: one record must survive both flushes, or the post-flush              query could only answer one way"
        );
        assert!(
            connection.readiness.event.is_writable(),
            "the WRITABLE event must be re-signalled so the event loop retries              the flush, got {:?}",
            connection.readiness
        );
    }

    /// A pass that owes nothing withdraws `Ready::WRITABLE` interest.
    ///
    /// This is the other side of the same decision: no TLS records (a plain
    /// `mio::net::TcpStream` takes `socket_wants_write()`'s `false` default),
    /// no parked `expect_write`, no bytes written, no queued control frame.
    /// Forward progress must come from an external trigger, so the connection
    /// relinquishes the bit rather than busy-spinning against the dispatcher.
    ///
    /// TO SEE THIS RED: delete `self.readiness.interest.remove(Ready::WRITABLE);`
    /// from the `FinalizeAction::Quiesce` arm of `finalize_write`, leaving its
    /// debug narration in place. The final assertion fails with `a pass that
    /// owes nothing must relinquish WRITABLE interest`. The interest bit is
    /// asserted here rather than the event bit because withdrawal is what this
    /// branch does, and it is set by the test rather than by the fixture —
    /// `test_h2_connection` arms `READABLE | HUP | ERROR` only.
    #[test]
    fn a_write_pass_that_owes_nothing_withdraws_writable_interest() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(2, 4, 16384)));
        let (mut connection, _peer) = test_h2_connection(&pool, None);
        let mut context = test_context(&pool);
        let mut router = Router::new(Duration::from_secs(30), Duration::from_secs(30));

        connection.state = H2State::Header;
        connection.readiness.interest.insert(Ready::WRITABLE);

        assert!(
            !connection.socket.socket_wants_write(),
            "premise: a plain TcpStream holds no TLS records, so the readiness              policy is reached at all"
        );
        assert!(
            connection.stream_table.expect_write().is_none(),
            "premise: no parked partial write, or the policy is suppressed"
        );

        let result = connection.writable(&mut context, EndpointClient(&mut router));

        assert!(
            matches!(result, MuxResult::Continue),
            "an empty write pass continues, got {result:?}"
        );
        assert!(
            !connection.readiness.interest.is_writable(),
            "a pass that owes nothing must relinquish WRITABLE interest, got              {:?}",
            connection.readiness
        );
    }

    // ── The vectored write loop: a partial write that reports WouldBlock ──
    //
    // First, which shape each neighbour targets, because #1454 asks for that
    // and the shapes are near-identical. `h2.rs` has FOUR `socket_write(&[])`
    // sites. Three are the `query / socket_write(&[]) / query` TRIPLE, where
    // the middle call discards both `size` and `status` and only the second
    // query says whether the flush landed:
    //
    //   - `finalize_write`, the BeforeFlush/AfterFlush pair — covered by
    //     `a_finalized_write_pass_flushes_once_and_re_arms_while_records_survive`.
    //   - `writable`'s `H2State::GoAway` arm — covered by
    //     `a_flush_that_does_not_drain_keeps_the_connection_open` and
    //     `a_flush_that_succeeds_closes_within_one_writable_call`.
    //   - `writable`'s preamble paired with the `H2State::Error` arm's
    //     `error_close_action` query. Only the preamble half is covered (the
    //     two GoAway tests assert its flush); the second query is read solely
    //     in the `(H2State::Error, Position::Server)` arm, which no fixture
    //     enters. **Uncovered.**
    //
    // The fourth, in `flush_zero_buffer`, is NOT a triple: it keeps the
    // `status` the flush returned instead of re-querying. Also uncovered.
    //
    // The tests below target none of those. They target **the vectored loop
    // the triples bracket** — `flush_stream_out`'s `while !kawa.out.is_empty()`
    // — so naming a triple for them would be naming the wrong shape.
    //
    // What had never run against production code: a `socket_write_vectored`
    // that moves bytes AND reports `WouldBlock`. `update_readiness` (`mux/mod.rs`)
    // classifies a pass as stalled **iff `size == 0`** — `status` only clears
    // the event bit — so `size > 0 && status == WouldBlock` is NOT a stall and
    // the loop must go round again. A write machine that treated
    // `status != Continue` as a pass terminator would drop that second write
    // and truncate the response, and would pass every test that existed.
    //
    // Why no test could see it. `BackpressuredTlsSocket::socket_write_vectored`
    // delegated to a real loopback socket, and a kernel that blocks answers
    // `(0, WouldBlock)` — never `(size > 0, WouldBlock)`. Only `FrontRustls`
    // returns that pair, and it does so structurally: `buffered_size` counts
    // plaintext `session.writer().write(..)` accepted, while `can_write` goes
    // false when `write_tls` blocks against the kernel. Two independent
    // quantities, one return value.
    //
    // `h2_transmit`'s `qc_partial_writes_preserve_the_byte_stream` does drive
    // partial writes, but through `drive`, a loop written in its own test
    // module that calls neither `flush_stream_out` nor
    // `update_readiness_after_write`; its `WritePlan` carries an accept COUNT
    // and no `SocketResult` at all. A mirror cannot constrain the thing it
    // mirrors, and that plan cannot express this pair in the first place.

    /// A server-side `Stream` whose response buffer already holds `blocks` as
    /// queued output, ready for `flush_stream_out` to drain.
    ///
    /// `Store::Static` rather than a slice into `kawa.storage`: a partial
    /// `consume` on a static store re-queues the untaken tail
    /// (`Store::Static(&data[amount..])`), which is exactly the partial-write
    /// bookkeeping under test, and it needs no fill of the pooled buffer.
    fn response_stream_with_out_blocks(
        pool: &Rc<RefCell<Pool>>,
        blocks: &[&'static [u8]],
    ) -> Stream {
        let mut stream = make_stream_for_invariant_16(pool, Ulid::generate());
        for block in blocks {
            stream
                .back
                .out
                .push_back(kawa::OutBlock::Store(kawa::Store::Static(block)));
        }
        stream
    }

    /// Drive `flush_stream_out` over `blocks` with `script` as the socket's
    /// answers, returning what the loop did.
    ///
    /// Calls the production associated function directly. That is deliberate:
    /// reaching this loop through `writable()` needs a stream carrying
    /// response bytes through the scheduler, which is the boundary the tests
    /// above already sit on — and driving it directly is what lets the
    /// `(size, status)` pair be chosen rather than negotiated with a kernel.
    fn drive_flush_stream_out(
        pool: &Rc<RefCell<Pool>>,
        blocks: &[&'static [u8]],
        script: &[(usize, SocketResult)],
    ) -> (FlushOutcome, usize, usize, bool, Readiness) {
        let (raw_socket, _peer) = connected_socket();
        let mut socket = BackpressuredTlsSocket::new(raw_socket, 0, 0)
            .with_vectored_script(script.iter().copied());
        let mut stream = response_stream_with_out_blocks(pool, blocks);
        let position = Position::Server;
        let parts = stream.split(&position);
        let mut readiness = Readiness {
            event: Ready::WRITABLE,
            interest: Ready::WRITABLE | Ready::HUP | Ready::ERROR,
        };
        let mut debug = DebugHistory::default();
        let mut io_slices: Vec<IoSlice<'static>> = Vec::new();
        let mut bytes_written = 0usize;

        let outcome = ConnectionH2::<BackpressuredTlsSocket>::flush_stream_out(
            &mut socket,
            parts.wbuffer,
            parts.metrics,
            &position,
            &mut readiness,
            &mut debug,
            3,
            0,
            None,
            None,
            &mut io_slices,
            Some(&mut bytes_written),
        );

        let out_is_empty = parts.wbuffer.out.is_empty();
        (
            outcome,
            bytes_written,
            socket.vectored_calls,
            out_is_empty,
            readiness,
        )
    }

    /// Bytes the two blocks below carry, and the whole response as far as
    /// this stream is concerned.
    const FIRST_BLOCK: &[u8] = b"HTTP/2 response prefix";
    const SECOND_BLOCK: &[u8] = b" and its continuation";
    const TOTAL_QUEUED: usize = FIRST_BLOCK.len() + SECOND_BLOCK.len();

    /// A write that moved bytes but reported `WouldBlock` does NOT end the
    /// pass: the loop goes round again and delivers the rest.
    ///
    /// This is the truncation vector of #1454 at the vectored loop — not at
    /// any of the three `socket_wants_write()` triples. `FrontRustls` returns
    /// `(buffered_size, WouldBlock)` with `buffered_size > 0` whenever rustls
    /// took plaintext off our hands while the kernel was full, and
    /// `update_readiness` deliberately calls that NOT stalled. A machine that
    /// terminated the pass on `status != Continue` would leave
    /// `SECOND_BLOCK` queued and send the response short.
    ///
    /// TO SEE THIS RED: in `flush_stream_out`, change
    /// `if update_readiness_after_write(size, status, readiness) {`
    /// to
    /// `if update_readiness_after_write(size, status, readiness)
    ///      || !matches!(status, SocketResult::Continue) {`
    /// — the pass-terminator shape the `write_streams` inversion could
    /// introduce. Measured on that mutation: this test fails on its own first
    /// assertion, `a vectored write that moved bytes but reported WouldBlock
    /// must not end the pass`, with `left: 5, right: 43`, and it is the ONLY
    /// failure in the 1083-test `sozu-lib` suite — the other 1082, including
    /// the three `BackpressuredTlsSocket` tests above and both controls below,
    /// pass. No production `debug_assert!` fires: every scripted size is
    /// clamped to the gather's offer.
    #[test]
    fn a_partial_write_reporting_would_block_continues_the_pass() {
        let pool = make_pool_for_invariant_16();
        // Round 1 accepts 5 of the 43 offered and reports WouldBlock; round 2
        // takes everything still queued and reports Continue.
        // The event bit is deliberately not read here: `update_readiness`
        // removes WRITABLE on the WouldBlock round and the Continue round does
        // not put it back, so its final value is identical under the correct
        // and the broken machine. An assertion on it could not fail.
        let (outcome, bytes_written, vectored_calls, out_is_empty, _readiness) =
            drive_flush_stream_out(
                &pool,
                &[FIRST_BLOCK, SECOND_BLOCK],
                &[
                    (5, SocketResult::WouldBlock),
                    (TOTAL_QUEUED, SocketResult::Continue),
                ],
            );

        assert_eq!(
            bytes_written, TOTAL_QUEUED,
            "a vectored write that moved bytes but reported WouldBlock must \
             not end the pass: the loop must go round again and deliver the \
             remaining bytes, or the response is truncated"
        );
        assert_eq!(
            vectored_calls, 2,
            "the loop must issue a SECOND socket_write_vectored after the \
             partial write, not settle for the first"
        );
        assert!(
            out_is_empty,
            "every queued block must be consumed once the pass drains"
        );
        assert!(
            matches!(outcome, FlushOutcome::Drained),
            "a queue emptied within the pass is Drained, got {outcome:?}"
        );
    }

    /// A write the socket takes whole, in one round, reporting `Continue`.
    ///
    /// **This test does not discriminate, and that is its job.** It is the
    /// `status == Continue` path — the one every existing test already takes,
    /// because a real loopback socket with room answers exactly this. It
    /// passes against the correct loop AND against the pass-terminator
    /// mutation in the test above, since a terminator keyed on
    /// `status != Continue` never fires here. Kept as the standing
    /// demonstration that coverage of this shape proves nothing about the
    /// truncation vector: the suite was full of it and #1454 was open anyway.
    #[test]
    fn a_fully_accepted_write_drains_in_one_round() {
        let pool = make_pool_for_invariant_16();
        let (outcome, bytes_written, vectored_calls, out_is_empty, _readiness) =
            drive_flush_stream_out(
                &pool,
                &[FIRST_BLOCK, SECOND_BLOCK],
                &[(TOTAL_QUEUED, SocketResult::Continue)],
            );

        assert_eq!(
            bytes_written, TOTAL_QUEUED,
            "a socket that takes the whole offer delivers the whole queue"
        );
        assert_eq!(
            vectored_calls, 1,
            "nothing is left to write, so the loop must not go round again"
        );
        assert!(out_is_empty, "the queue must be empty after a full accept");
        assert!(
            matches!(outcome, FlushOutcome::Drained),
            "a fully accepted offer is Drained, got {outcome:?}"
        );
    }

    /// A write that moves NO bytes stalls the pass and drops WRITABLE.
    ///
    /// The other side of `update_readiness`'s single decision: `size == 0` is
    /// the stall, whatever the status. Together with the partial-write test
    /// above this pins that the discriminator is the SIZE and not the status —
    /// both rounds there reported a non-`Continue` status, and only this one
    /// stops.
    ///
    /// Like `a_fully_accepted_write_drains_in_one_round`, this passes under
    /// the pass-terminator mutation too: `(0, WouldBlock)` stalls either way.
    /// The size arm is the covered one; the partial arm was not.
    ///
    /// TO SEE THIS RED: in `update_readiness` (`mux/mod.rs`), change the
    /// `else` arm's trailing `true` to `false`, so no size ever reports a
    /// stall. Measured: the first assertion fails with `a write that moved no
    /// bytes must stall the pass, got Drained`. It does not hang — the script
    /// here is a single round, so the loop re-enters, finds the script spent,
    /// delegates to the live loopback socket and drains.
    #[test]
    fn a_write_that_moves_no_bytes_stalls_the_pass() {
        let pool = make_pool_for_invariant_16();
        let (outcome, bytes_written, vectored_calls, out_is_empty, readiness) =
            drive_flush_stream_out(
                &pool,
                &[FIRST_BLOCK, SECOND_BLOCK],
                &[(0, SocketResult::WouldBlock)],
            );

        assert!(
            matches!(outcome, FlushOutcome::Stalled),
            "a write that moved no bytes must stall the pass, got {outcome:?}"
        );
        assert_eq!(
            vectored_calls, 1,
            "a stalled pass must not retry the socket within the same pass"
        );
        assert_eq!(bytes_written, 0, "a stalled write delivered nothing");
        assert!(
            !out_is_empty,
            "the undelivered blocks must stay queued for the next pass, or \
             they are lost"
        );
        assert!(
            !readiness.event.is_writable(),
            "a stalled pass must clear the WRITABLE event so the pass is not \
             re-entered before the socket says it can take more, got {readiness:?}"
        );
    }

    // ── force_disconnect's server arm with records pending ──────────────
    //
    // `h2_close::force_disconnect_action` is exhaustively tabled over its two
    // booleans in that module. Nothing called `ConnectionH2::force_disconnect`
    // itself, so the CALLER half — the `socket_wants_write()` query that feeds
    // the table and the wiring of `ReArmAndContinue` to interest, flush and
    // return value — had no coverage. #1454 asks for this site by name.
    //
    // Shape targeted: a SINGLE query, not a triple. `force_disconnect` reads
    // the answer once into `tls_wants_write`, feeds the table and reports the
    // same value from both debug arms; there is no `socket_write(&[])` here
    // and so no second question to reconstruct.

    /// A server connection whose TLS records are still buffered re-arms
    /// instead of closing the session.
    ///
    /// `MuxResult::CloseSession` triggers `shutdown(Write)`, which sends FIN
    /// and destroys whatever rustls is still holding — the client reads a
    /// truncated response. So the live-peer, records-pending row of
    /// `force_disconnect_action` must keep WRITABLE and let the writable path
    /// flush.
    ///
    /// TO SEE THIS RED: in `force_disconnect`'s `Position::Server` arm, pass
    /// `false` in place of `tls_wants_write` to `force_disconnect_action`.
    /// Measured: this test fails on its own first assertion, `a server still
    /// holding TLS records must not close: CloseSession sends FIN and destroys
    /// them, got CloseSession`, while `h2_close`'s own
    /// `force_disconnect_arm_is_exhaustive` stays green — the table is not the
    /// caller, which is why this test exists. Passing `true` instead would NOT
    /// redden this test; catching that constant ON THIS DIRECT PATH is what
    /// the sibling below is for — it is not the suite's only witness against
    /// `true`, and its own doc says which other test is.
    ///
    /// The interest assertion has its own recipe: delete
    /// `self.readiness.interest = Ready::WRITABLE | Ready::HUP | Ready::ERROR;`
    /// from the same arm. Measured: `the re-arm must restore WRITABLE interest
    /// so the writable path is called again to flush`, `left: Hup, right:
    /// Writable | Error | Hup`. That recipe only bites because the test
    /// withdraws the bit first — asserting it against the fixture's own
    /// constructor argument passed under this very mutation.
    #[test]
    fn force_disconnect_re_arms_while_tls_records_are_pending() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(2, 4, 16384)));
        // A kernel that accepts nothing, so the records outlive any flush.
        let (mut connection, _peer) = connection_with_backpressure(&pool, 2, 0, H2State::GoAway);

        assert!(
            connection.socket.socket_wants_write(),
            "premise: this harness must report buffered TLS records"
        );
        assert!(
            !connection.peer_gone_after_final_goaway(),
            "premise: the peer must still be live, or the table short-circuits \
             to CloseSession on the first boolean and the record count is never \
             read"
        );
        // Withdraw WRITABLE interest first. `connection_with_backpressure`
        // hands `WRITABLE | HUP | ERROR` to `ConnectionH2::new` as the
        // connection's INTEREST, so asserting on it untouched would be reading
        // this fixture's own argument back and would hold even if the re-arm
        // arm never assigned it. Starting from `HUP` alone makes the
        // assignment the only thing that can restore the bit.
        connection.readiness.interest = Ready::HUP;
        assert!(
            !connection.readiness.event.is_writable(),
            "premise: the WRITABLE event must start clear, so the signal below \
             is the only thing that can set it, got {:?}",
            connection.readiness
        );

        let result = connection.force_disconnect();

        assert!(
            matches!(result, MuxResult::Continue),
            "a server still holding TLS records must not close: CloseSession \
             sends FIN and destroys them, got {result:?}"
        );
        assert_eq!(
            connection.readiness.interest,
            Ready::WRITABLE | Ready::HUP | Ready::ERROR,
            "the re-arm must restore WRITABLE interest so the writable path is \
             called again to flush, got {:?}",
            connection.readiness
        );
        assert!(
            connection.readiness.event.is_writable(),
            "the re-arm must signal the edge-triggered WRITABLE event, or the \
             event loop never revisits this connection, got {:?}",
            connection.readiness
        );
    }

    /// The same call with nothing buffered closes the session.
    ///
    /// The other row of the table. It does NOT rest on being the only thing
    /// that catches a hard-wired `true`: the suite already caught that
    /// constant without it. Measured on that mutation, `1081 passed; 2
    /// failed` — this test AND
    /// `a_flush_that_succeeds_closes_within_one_writable_call` above, which
    /// predates it and reaches `force_disconnect` through `writable()`'s
    /// GoAway fall-through, failing with `a drained flush must reach the
    /// disconnect in the SAME writable call, not defer it to another tick:
    /// got Continue`.
    ///
    /// What this test adds is the PATH, not the count: it calls
    /// `ConnectionH2::force_disconnect` directly, so a red here names the
    /// query under test, where the fall-through route arrives through a write
    /// pass and a state machine that can each shift for reasons of their own.
    ///
    /// TO SEE THIS RED: pass `true` in place of `tls_wants_write` to
    /// `force_disconnect_action` in `force_disconnect`'s `Position::Server`
    /// arm. Measured: this test fails with `a server holding nothing must
    /// close rather than wait for a flush that has nothing to flush, got
    /// Continue`, while the sibling above stays green. The mirror constant
    /// `false` is measured at `1082 passed; 1 failed`, and that one failure is
    /// the sibling above — so neither constant can replace the query, and the
    /// sibling is the suite's only witness against `false`.
    #[test]
    fn force_disconnect_closes_when_no_tls_records_are_pending() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(2, 4, 16384)));
        // Same fixture, zero records: the query answers `false`.
        let (mut connection, _peer) = connection_with_backpressure(&pool, 0, 0, H2State::GoAway);

        assert!(
            !connection.socket.socket_wants_write(),
            "premise: nothing buffered, so the table reads its second boolean \
             as false"
        );
        assert!(
            !connection.peer_gone_after_final_goaway(),
            "premise: the peer must still be live, so the close comes from the \
             record count and not from the first boolean"
        );

        let result = connection.force_disconnect();

        assert!(
            matches!(result, MuxResult::CloseSession),
            "a server holding nothing must close rather than wait for a flush \
             that has nothing to flush, got {result:?}"
        );
    }

    /// Build a bare server-side `ConnectionH2` for tests that only exercise
    /// connection-level bookkeeping. The socket is never read or written.
    ///
    /// Returns the accepted peer alongside it; keep it alive for the duration
    /// of the test so the socket stays connected and reads answer `WouldBlock`
    /// instead of `ECONNREFUSED`.
    fn test_h2_connection(
        pool: &Rc<RefCell<Pool>>,
        graceful_shutdown_deadline: Option<Duration>,
    ) -> (ConnectionH2<mio::net::TcpStream>, std::net::TcpStream) {
        let (socket, peer) = connected_socket();
        let connection = ConnectionH2::new(
            Ulid::generate(),
            socket,
            Position::Server,
            Rc::downgrade(pool),
            H2FloodConfig::default(),
            H2ConnectionConfig::default(),
            Duration::from_secs(30),
            graceful_shutdown_deadline,
            Duration::from_secs(30),
            Some((H2StreamId::Zero, CLIENT_PREFACE_SIZE)),
            Ready::READABLE | Ready::HUP | Ready::ERROR,
        )
        .expect("a pool with free buffers must yield an H2 connection");
        (connection, peer)
    }

    /// The graceful-shutdown budget is armed from the `now` handed to
    /// `graceful_goaway` and evaluated against `ConnectionH2::now` — not
    /// against the real clock at either end.
    ///
    /// `armed_at` is deliberately 100 s in the future relative to the
    /// connection's construction snapshot. That gap is what makes the test
    /// discriminating: a `graceful_goaway` that armed `started_at` from
    /// `Instant::now()` would put it ~100 s in the PAST relative to
    /// `armed_at`, and the "not yet elapsed" assertion would trip.
    ///
    /// To SEE THIS RED: restore either half of the old behaviour.
    /// (a) `self.started_at = Some(Instant::now());` in
    ///     `h2_drain::H2DrainState::begin_graceful_drain` — the `assert_eq!`
    ///     on `drain.__test_started_at()` fails: ``assertion `left == right`
    ///     failed: graceful_goaway must arm the budget from its `now`
    ///     parameter / left: Some(Instant { tv_sec: 107343, .. }) / right:
    ///     Some(Instant { tv_sec: 107443, .. })`` — the two differing by
    ///     exactly the 100 s offset above.
    /// (b) `started_at.elapsed() >= deadline` in
    ///     `h2_drain::H2DrainState::deadline_elapsed` — the LAST assertion
    ///     fails with `advancing only the connection snapshot past the
    ///     budget must force close`, because no real time passes in this
    ///     test, so the forced-close budget never expires however far the
    ///     connection's clock is moved.
    #[test]
    fn graceful_shutdown_deadline_is_evaluated_against_the_connection_snapshot() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(2, 4, 16384)));
        let deadline = Duration::from_secs(5);
        let (mut connection, _peer) = test_h2_connection(&pool, Some(deadline));

        // Nothing armed yet: `started_at` is None, so the budget cannot expire.
        assert!(
            !connection.graceful_shutdown_deadline_elapsed(),
            "an unarmed drain must never report its budget as elapsed"
        );

        let armed_at = connection.now + Duration::from_secs(100);
        connection.graceful_goaway(armed_at);
        assert_eq!(
            connection.drain.__test_started_at(),
            Some(armed_at),
            "graceful_goaway must arm the budget from its `now` parameter"
        );

        // One millisecond short of the budget, measured on the connection's
        // clock. Real elapsed time here is microseconds.
        connection.now = armed_at + deadline - Duration::from_millis(1);
        assert!(
            !connection.graceful_shutdown_deadline_elapsed(),
            "the budget must not expire before `started_at + deadline` on the \
             connection's own clock"
        );

        connection.now = armed_at + deadline;
        assert!(
            connection.graceful_shutdown_deadline_elapsed(),
            "advancing only the connection snapshot past the budget must force close"
        );
    }

    /// The RFC 9113 §5.1.2 back-pressure window is driven by
    /// `ConnectionH2::now`, so a refusal burst is weighed against the window
    /// that was open when the pass started — it cannot decay mid-pass.
    ///
    /// To SEE THIS RED: restore
    /// `if self.refuse_window_start.elapsed() >= BACKPRESSURE_WINDOW_DURATION`
    /// in `record_refusal_for_backpressure`. The window then never rolls over
    /// (no real time passes) and the 50th refusal lands in the same window as
    /// the first 49, so the `refuse_count_window` assertion fails first:
    /// ``assertion `left == right` failed: a refusal in a new window must
    /// restart the count, not top up the old one / left: 50 / right: 1``. The
    /// two assertions after it would also fail — `mcs_backpressure_applied`
    /// becomes true and `settings_max_concurrent_streams` is halved off a burst
    /// that actually spanned two windows.
    #[test]
    fn backpressure_window_rolls_over_on_the_connection_snapshot() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(2, 4, 16384)));
        let (mut connection, _peer) = test_h2_connection(&pool, None);
        let advertised = connection.local_settings.settings_max_concurrent_streams;

        // One short of the burst threshold, all inside the first window.
        for _ in 0..BACKPRESSURE_REFUSAL_THRESHOLD - 1 {
            connection.record_refusal_for_backpressure();
        }
        assert_eq!(
            connection.refuse_count_window,
            BACKPRESSURE_REFUSAL_THRESHOLD - 1
        );
        assert!(
            !connection.mcs_backpressure_applied,
            "back-pressure must not apply below the refusal threshold"
        );

        // Cross into the next window on the connection's clock alone.
        connection.now += BACKPRESSURE_WINDOW_DURATION;
        connection.record_refusal_for_backpressure();

        assert_eq!(
            connection.refuse_count_window, 1,
            "a refusal in a new window must restart the count, not top up the old one"
        );
        assert!(
            !connection.mcs_backpressure_applied,
            "two refusals spread across two windows are not a burst"
        );
        assert_eq!(
            connection.local_settings.settings_max_concurrent_streams, advertised,
            "MAX_CONCURRENT_STREAMS must be untouched when no burst occurred"
        );
    }

    /// The RFC 9113 §6.5 SETTINGS-ACK deadline is evaluated against
    /// `ConnectionH2::now`. `flush_pending_control_frames` is the `writable`
    /// half of the pair; the `poll_read_target` half at `h2.rs` shares the predicate.
    ///
    /// To SEE THIS RED: restore `sent_at.elapsed() >= SETTINGS_ACK_TIMEOUT` in
    /// `flush_pending_control_frames`. No real time passes in this test, so the
    /// deadline never fires however far the connection's clock is advanced, and
    /// the third assertion fails with `advancing only the connection snapshot
    /// to the deadline must emit GOAWAY`.
    #[test]
    fn settings_ack_deadline_is_evaluated_against_the_connection_snapshot() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(2, 4, 16384)));
        let (mut connection, _peer) = test_h2_connection(&pool, None);

        let sent_at = connection.now;
        connection.settings_sent_at = Some(sent_at);

        // One millisecond short of the budget on the connection's clock.
        connection.now = sent_at + SETTINGS_ACK_TIMEOUT - Duration::from_millis(1);
        assert!(
            connection.flush_pending_control_frames().is_none(),
            "the SETTINGS-ACK deadline must not fire before SETTINGS_ACK_TIMEOUT \
             on the connection's own clock"
        );
        assert_eq!(
            connection.settings_sent_at,
            Some(sent_at),
            "a deadline that has not fired must leave the ACK timer armed"
        );

        // At the budget: GOAWAY(SETTINGS_TIMEOUT).
        connection.now = sent_at + SETTINGS_ACK_TIMEOUT;
        assert!(
            connection.flush_pending_control_frames().is_some(),
            "advancing only the connection snapshot to the deadline must emit GOAWAY"
        );
        assert_eq!(
            connection.settings_sent_at, None,
            "goaway must disarm the ACK timer so the timeout cannot re-fire"
        );
    }

    /// The per-stream liveness guard reaps against `ConnectionH2::now`, and
    /// `cancel_timed_out_streams` adopts `context.now` on entry — this pins
    /// both the entry-point mirror and the deadline arithmetic in
    /// `H2StreamTable::collect_timed_out`.
    ///
    /// To SEE THIS RED: restore `let now = Instant::now();` in
    /// `cancel_timed_out_streams` (in place of `let now = self.now;`). The
    /// injected instant is then ignored, no stream is ever old enough to reap
    /// because no real time passes, and the reap assertion fails with
    /// `advancing only the mux snapshot past the idle deadline must reap the
    /// stream`. Deleting the `self.now = context.now;` entry-point assignment
    /// turns the same assertion red, with the same message, for the same reason.
    #[test]
    fn per_stream_liveness_reaping_is_evaluated_against_the_connection_snapshot() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(2, 4, 16384)));
        let (mut connection, _peer) = test_h2_connection(&pool, None);
        let mut context = test_context(&pool);
        let mut router = Router::new(Duration::from_secs(30), Duration::from_secs(30));

        let idle_timeout = connection.stream_idle_timeout;
        let armed_at = connection.now;
        let gid = context
            .create_stream(Ulid::generate(), 1 << 16)
            .expect("test context must create a stream");
        connection.stream_table.register(1, gid, armed_at);

        // Exactly at the deadline: the predicate is a strict `>`, so the stream
        // survives.
        context.now = armed_at + idle_timeout;
        connection.cancel_timed_out_streams(&mut context, &mut EndpointClient(&mut router));
        assert!(
            connection.control_tx.pending().is_empty(),
            "a stream exactly at its idle deadline must not be reaped"
        );

        // One millisecond past it: reaped with RST_STREAM(CANCEL).
        context.now = armed_at + idle_timeout + Duration::from_millis(1);
        connection.cancel_timed_out_streams(&mut context, &mut EndpointClient(&mut router));
        assert!(
            !connection.control_tx.pending().is_empty(),
            "advancing only the mux snapshot past the idle deadline must reap the stream"
        );
        assert_eq!(
            connection.control_tx.pending()[0],
            (1, H2Error::Cancel),
            "the reaper must queue RST_STREAM(CANCEL) for the timed-out stream"
        );
    }

    /// A mass idle-timeout reap is the only path that queues more than one
    /// RST_STREAM between two `flush_pending_control_frames` passes:
    /// `cancel_timed_out_streams` walks the whole timed-out set and calls
    /// `enqueue_rst` for every entry inside ONE pass. The cap therefore has to
    /// live at the insert, not at the drain — the drain-side
    /// `total_rst_streams_queued >= MAX_PENDING_RST_STREAMS` short-circuit
    /// bounds what is *written*, and says nothing about how large the queue got
    /// before anyone looked. See sozu-proxy/sozu#1413.
    ///
    /// Reaching >200 concurrently-tracked streams in one sweep needs an
    /// operator-raised `max_concurrent_streams` (default 100, clamped at
    /// `MAX_SAFE_CONCURRENT_STREAMS` = 10 000) — that setting bounds the live
    /// set the reaper walks, not the queue, which holds what every caller
    /// queued since the last successful drain. This test registers the wire
    /// ids on the stream table directly, exactly as its sibling
    /// `per_stream_liveness_reaping_is_evaluated_against_the_connection_snapshot`
    /// does, because the accept path is not what is under test here.
    ///
    /// `check_invariants` is asserted explicitly because the reaper is also
    /// reachable from `Mux::timeout` against a fully silent peer — the very
    /// scenario that produces a mass reap — where `handle_frame`, the only
    /// production caller of `check_invariants`, never runs at all. The reaper
    /// is not the only insert path outside that caller: the
    /// DATA-on-closed-stream reset sits in `handle_header_state`, which
    /// `handle_read` returns into without ever reaching `handle_frame`. That one
    /// is rate-limited by `record_glitch` + `check_flood_or_return!` and
    /// cannot by itself fill the queue — see `ConnectionH2::enqueue_rst`.
    ///
    /// To SEE THIS RED: delete the `pending_before >= self.max_pending` guard
    /// at the top of `H2ControlTx::enqueue_rst` (`h2_control_tx.rs`), then run
    /// `cargo test -p sozu-lib --locked mass_reap` (one positional filter —
    /// cargo rejects a second one). Every reaped stream is then queued
    /// unconditionally and `H2ControlTx::check_invariants` — the post-condition
    /// every mutating method on that type runs — panics inside the sweep with
    /// `pending RST queue must stay within its hard cap (escalates at the
    /// cap)`. That clause moved to the module with this commit and is NOT
    /// restated in `ConnectionH2::check_invariants`, so the explicit call
    /// below no longer covers the queue bound — the assertion at the end of
    /// this test is what pins it from the connection's side.
    #[test]
    fn mass_reap_keeps_the_pending_rst_queue_within_its_hard_cap() {
        // `Stream::new` checks out two buffers per stream and the connection
        // itself holds one for its zero buffer, so the ceiling has to clear
        // `2 * REAPED` with room to spare or `create_stream` hands back
        // `None` — which is how the first draft of this test failed.
        const REAPED: usize = h2_control_tx::MAX_PENDING_RST_STREAMS + 32;
        let pool = Rc::new(RefCell::new(Pool::with_capacity(2, 2 * REAPED + 4, 16384)));
        let (mut connection, _peer) = test_h2_connection(&pool, None);
        let mut context = test_context(&pool);
        let mut router = Router::new(Duration::from_secs(30), Duration::from_secs(30));

        let idle_timeout = connection.stream_idle_timeout;
        let armed_at = connection.now;
        for i in 0..REAPED {
            let gid = context
                .create_stream(Ulid::generate(), 1 << 16)
                .expect("test context must create a stream");
            // RFC 9113 §5.1.1: client-initiated stream ids are odd.
            connection
                .stream_table
                .register(2 * i as StreamId + 1, gid, armed_at);
        }

        // One sweep, every stream past its deadline: the entire set is reaped
        // and RST-queued with no intervening flush.
        context.now = armed_at + idle_timeout + Duration::from_millis(1);
        connection.cancel_timed_out_streams(&mut context, &mut EndpointClient(&mut router));

        #[cfg(debug_assertions)]
        connection.check_invariants(&context);
        assert!(
            connection.control_tx.pending().len() <= h2_control_tx::MAX_PENDING_RST_STREAMS,
            "a mass reap must stop queueing at the cap, got {} entries",
            connection.control_tx.pending().len()
        );
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

    // ── peer-address snapshot in the MUX-H2 log envelope ─────────────────
    //
    // `log_context!` / `log_context_stream!` render `peer=` from
    // `SocketHandler::peer_addr`, which is a snapshot the handler cached, not a
    // live `getpeername(2)`. Both tests below make the two answers differ on
    // purpose: the socket is genuinely connected to a loopback listener, so the
    // live lookup succeeds and would print `127.0.0.1:<port>`; only a macro
    // reading the snapshot can print `10.0.0.42:12345`.
    //
    // That gap is the PROXY-protocol symptom in miniature — the frontend's real
    // peer is the load balancer while the cache holds the advertised client —
    // and the same read is what keeps the slot populated after the peer's RST,
    // when `getpeername(2)` answers ENOTCONN and the live lookup collapses to
    // `None` on exactly the error lines an operator is reading.

    /// A live, established loopback connection plus the address
    /// `getpeername(2)` reports for it. The listener is returned so the
    /// connection stays up for the whole test: these tests assert the snapshot
    /// wins even while the live lookup is perfectly healthy, so a half-dead
    /// socket would weaken them rather than strengthen them.
    fn connected_loopback_stream() -> (
        std::net::TcpListener,
        mio::net::TcpStream,
        std::net::SocketAddr,
    ) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("test listener must bind to a loopback port");
        let live_peer = listener
            .local_addr()
            .expect("test listener must report its local address");
        let stream =
            std::net::TcpStream::connect(live_peer).expect("loopback connect must complete");
        stream
            .set_nonblocking(true)
            .expect("mio requires a nonblocking stream");
        (listener, mio::net::TcpStream::from_std(stream), live_peer)
    }

    /// Address the handler caches. Deliberately non-loopback so it cannot
    /// collide with whatever ephemeral port the live lookup would report.
    const CACHED_PEER: &str = "10.0.0.42:12345";

    /// Build a server-side H2 connection whose frontend handler caches
    /// [`CACHED_PEER`] while its socket is really connected to `live_peer`.
    fn connection_with_cached_peer(
        pool: &Rc<RefCell<Pool>>,
        stream: mio::net::TcpStream,
    ) -> ConnectionH2<crate::socket::SessionTcpStream> {
        let session_ulid = Ulid::generate();
        let socket = crate::socket::SessionTcpStream::new(
            stream,
            session_ulid,
            Some(
                CACHED_PEER
                    .parse()
                    .expect("the cached peer literal must parse"),
            ),
        );
        ConnectionH2::new(
            session_ulid,
            socket,
            Position::Server,
            Rc::downgrade(pool),
            H2FloodConfig::default(),
            H2ConnectionConfig::default(),
            Duration::from_secs(30),
            None,
            Duration::from_secs(30),
            Some((H2StreamId::Zero, CLIENT_PREFACE_SIZE)),
            Ready::READABLE | Ready::HUP | Ready::ERROR,
        )
        .expect("a pool with free buffers must yield an H2 connection")
    }

    /// To SEE THIS RED: in `log_context!` (h2.rs), put
    /// `peer = $self.socket.socket_ref().peer_addr().ok(),` back in place of
    /// `peer = $self.peer_address,`. The rendered line then carries the
    /// loopback address the socket is really connected to, so the first
    /// assertion fails on the missing cached address.
    ///
    /// Substituting the pre-snapshot `peer = $self.socket.peer_addr(),` does
    /// NOT turn this red, and that is worth stating: for a handler that
    /// already caches, reading the cache and reading a snapshot taken from
    /// that cache agree by construction. What the snapshot changed is WHICH
    /// object is read on a log line, not what comes back — so the call-count
    /// property, not this one, is what distinguishes them. It is pinned by
    /// [`log_context_reads_the_peer_address_once_per_connection`].
    #[test]
    fn log_context_renders_the_cached_peer_not_a_live_lookup() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(2, 4, 16384)));
        let (_listener, stream, live_peer) = connected_loopback_stream();
        let connection = connection_with_cached_peer(&pool, stream);

        // Premise of the test: the live lookup is healthy and disagrees with the
        // cache. Without this the assertions below could pass for the wrong
        // reason (both answers happening to be the same address).
        assert_eq!(
            connection.socket.socket_ref().peer_addr().ok(),
            Some(live_peer),
            "the test socket must be genuinely connected, so a live lookup succeeds"
        );

        let rendered = log_context!(connection);

        assert!(
            rendered.contains(&format!("peer=Some({CACHED_PEER})")),
            "MUX-H2 context must render the cached peer: {rendered}"
        );
        assert!(
            !rendered.contains(&live_peer.to_string()),
            "MUX-H2 context must not fall back to the live getpeername(2) answer: {rendered}"
        );
    }

    /// To SEE THIS RED: in `log_context_stream!` (h2.rs), put
    /// `peer = $self.socket.socket_ref().peer_addr().ok(),` back in place of
    /// `peer = $self.peer_address,`. The per-stream envelope then
    /// diverges from the connection envelope, which is worse than either being
    /// wrong alone: the same session renders two different peers depending on
    /// whether a stream happened to be in scope at the callsite.
    #[test]
    fn log_context_stream_renders_the_cached_peer_not_a_live_lookup() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 20, 16_384)));
        let (_listener, stream, live_peer) = connected_loopback_stream();
        let connection = connection_with_cached_peer(&pool, stream);
        let mux_stream = make_stream_for_invariant_16(&pool, connection.session_ulid);
        let http_context = &mux_stream.context;

        assert_eq!(
            connection.socket.socket_ref().peer_addr().ok(),
            Some(live_peer),
            "the test socket must be genuinely connected, so a live lookup succeeds"
        );

        let rendered = log_context_stream!(connection, http_context);

        assert!(
            rendered.contains(&format!("peer=Some({CACHED_PEER})")),
            "per-stream MUX-H2 context must render the cached peer: {rendered}"
        );
        assert!(
            !rendered.contains(&live_peer.to_string()),
            "per-stream MUX-H2 context must not fall back to the live \
             getpeername(2) answer: {rendered}"
        );
    }

    /// A [`SocketHandler`] that counts how often it is asked for the peer
    /// address, delegating every other method to a real loopback stream.
    ///
    /// The two tests above pin WHICH address the log prefix renders. Neither
    /// can see how many times the connection reaches into the socket to get
    /// it, because both production handlers answer from a cache and so give
    /// the same answer however often they are asked. This handler makes the
    /// count observable.
    struct PeerAddrCountingSocket {
        stream: mio::net::TcpStream,
        peer: std::net::SocketAddr,
        calls: Rc<std::cell::Cell<usize>>,
    }

    impl SocketHandler for PeerAddrCountingSocket {
        fn socket_read(&mut self, buf: &mut [u8]) -> (usize, SocketResult) {
            self.stream.socket_read(buf)
        }

        fn socket_write(&mut self, buf: &[u8]) -> (usize, SocketResult) {
            self.stream.socket_write(buf)
        }

        fn socket_write_vectored(&mut self, bufs: &[IoSlice]) -> (usize, SocketResult) {
            self.stream.socket_write_vectored(bufs)
        }

        fn socket_ref(&self) -> &mio::net::TcpStream {
            &self.stream
        }

        fn socket_mut(&mut self) -> &mut mio::net::TcpStream {
            &mut self.stream
        }

        fn peer_addr(&self) -> Option<std::net::SocketAddr> {
            self.calls.set(self.calls.get() + 1);
            Some(self.peer)
        }

        fn protocol(&self) -> crate::socket::TransportProtocol {
            crate::socket::TransportProtocol::Tcp
        }

        fn read_error(&self) {}

        fn write_error(&self) {}
    }

    /// The `peer=` slot is read from the socket exactly ONCE per connection —
    /// at construction — however many log lines the connection renders.
    ///
    /// This is the property that makes `peer_address` worth having. It is not
    /// about which address appears (the two tests above own that); it is about
    /// `log_context!` no longer being a socket operation. The macro expands at
    /// 113 production callsites in this file, so before the snapshot every
    /// method that logged was `Front`-coupled whether or not it did any I/O —
    /// which is 113 of the ~140 total `Front` touch points in `h2.rs`.
    ///
    /// TO SEE THIS RED: in `log_context!` (h2.rs), put
    /// `peer = $self.socket.peer_addr(),` back in place of
    /// `peer = $self.peer_address,`. The count then rises by one per rendered
    /// line and the final assertion fails with
    /// `the peer address must be read once, at construction, not once per log line:
    /// left: 4, right: 1`.
    #[test]
    fn log_context_reads_the_peer_address_once_per_connection() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(2, 4, 16384)));
        let (_listener, stream, _live_peer) = connected_loopback_stream();
        let calls = Rc::new(std::cell::Cell::new(0usize));
        let session_ulid = Ulid::generate();
        let socket = PeerAddrCountingSocket {
            stream,
            peer: CACHED_PEER
                .parse()
                .expect("the cached peer literal must parse"),
            calls: Rc::clone(&calls),
        };
        let connection = ConnectionH2::new(
            session_ulid,
            socket,
            Position::Server,
            Rc::downgrade(&pool),
            H2FloodConfig::default(),
            H2ConnectionConfig::default(),
            Duration::from_secs(30),
            None,
            Duration::from_secs(30),
            Some((H2StreamId::Zero, CLIENT_PREFACE_SIZE)),
            Ready::READABLE | Ready::HUP | Ready::ERROR,
        )
        .expect("a pool with free buffers must yield an H2 connection");

        assert_eq!(
            calls.get(),
            1,
            "construction takes exactly one peer_addr() snapshot"
        );

        // Premise: the rendered lines really do carry the address, so a zero
        // count below would mean the slot went missing rather than that it
        // became free.
        for _ in 0..3 {
            let rendered = log_context!(connection);
            assert!(
                rendered.contains(&format!("peer=Some({CACHED_PEER})")),
                "each rendered line must still carry the peer: {rendered}"
            );
        }

        assert_eq!(
            calls.get(),
            1,
            "the peer address must be read once, at construction, not once per log line"
        );
    }

    // ── The read-side two-call protocol (poll_read_target / handle_read) ──

    /// `read_space` hands out the buffer the core named, and hands out the
    /// stream's READ buffer — the one `Stream::split` labels `rbuffer` for the
    /// connection's position — not its write buffer.
    ///
    /// This is the only genuinely new logic in the read-side inversion: every
    /// other line moved. Both halves are pinned because the two failure modes
    /// differ — offering the wrong *kawa* corrupts the peer's request while
    /// the response is parsed as H2 payload, offering the wrong *stream*
    /// writes one stream's DATA into another's.
    ///
    /// To SEE THIS RED: in `read_buffer` (h2.rs), return
    /// `streams[global_stream_id].split(position).wbuffer` in place of
    /// `.rbuffer`.
    #[test]
    fn read_space_offers_the_read_buffer_of_the_stream_the_core_named() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 20, 16_384)));
        let (mut connection, _peer) = test_h2_connection(&pool, None);
        let mut context = test_context(&pool);

        let gid = context
            .create_stream(Ulid::generate(), 1 << 16)
            .expect("test context must create a stream");

        // Premise: the two per-stream buffers really are distinct allocations,
        // so a pointer comparison below can tell them apart at all.
        let front = context.streams[gid].front.storage.space().as_ptr();
        let back = context.streams[gid].back.storage.space().as_ptr();
        let zero = connection.zero.storage.space().as_ptr();
        assert_ne!(
            front, back,
            "a stream's request and response buffers must be distinct allocations"
        );

        let offered = read_space(
            &mut connection.zero,
            &mut context.streams,
            &connection.position,
            H2StreamId::Zero,
            9,
        );
        assert_eq!(
            offered.as_ptr(),
            zero,
            "H2StreamId::Zero must offer the connection-level scratch buffer"
        );
        assert_eq!(
            offered.len(),
            9,
            "the offer is capped at the byte debt, never the whole free space"
        );

        let offered = read_space(
            &mut connection.zero,
            &mut context.streams,
            &connection.position,
            H2StreamId::Other { id: 1, gid },
            7,
        );
        assert_eq!(
            offered.as_ptr(),
            front,
            "a server position must offer the stream's request (front) buffer"
        );
        assert_ne!(
            offered.as_ptr(),
            back,
            "the offered buffer must not be the stream's response (back) buffer"
        );
        assert_eq!(
            offered.len(),
            7,
            "the offer is capped at the byte debt, never the whole free space"
        );
    }

    /// A frame whose payload is zero bytes long owes the socket nothing, and
    /// the core says so with its own variant rather than an `amount` of 0.
    ///
    /// That distinction is load-bearing, not cosmetic. A caller handed
    /// `Fill { amount: 0 }` would issue a zero-length read, and
    /// `update_readiness_after_read(0, SocketResult::Continue, ..)` returns
    /// `true` for it — "nothing arrived, stop" — so `handle_read` would return
    /// before the frame state machine ever ran, and every empty SETTINGS,
    /// empty DATA and SETTINGS ACK would stop being parsed.
    ///
    /// To SEE THIS RED: in `poll_read_target` (h2.rs), replace the trailing
    /// `if amount > 0 { .. } else { .. }` with its `Fill` half alone —
    /// keep the `available_space` guard and end the function with
    /// `H2ReadTarget::Fill { stream_id, amount }`, dropping the
    /// `set_expect_read(None)` / `H2ReadTarget::Skip(stream_id)` branch.
    #[test]
    fn poll_read_target_skips_the_read_when_the_frame_carries_no_payload() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(2, 4, 16384)));
        let (mut connection, _peer) = test_h2_connection(&pool, None);
        let mut context = test_context(&pool);
        let mut router = Router::new(Duration::from_secs(30), Duration::from_secs(30));

        connection.state = H2State::Header;
        connection
            .stream_table
            .set_expect_read(Some((H2StreamId::Zero, 0)));

        let target = connection.poll_read_target(&mut context, &mut EndpointClient(&mut router));
        assert!(
            matches!(target, H2ReadTarget::Skip(H2StreamId::Zero)),
            "a zero-length payload must be a Skip, not a zero-length Fill: {target:?}"
        );
        assert!(
            connection.stream_table.expect_read().is_none(),
            "the settled byte debt must be cleared before the frame is dispatched"
        );
    }

    /// The core is told how many bytes arrived, and it subtracts exactly that
    /// many from the debt it offered — a short read leaves the remainder owed,
    /// it does not restart the frame.
    ///
    /// 6 is neither of the two numbers the test hands in, so this cannot pass
    /// by echoing the fixture: `amount` is 9 and `size` is 3.
    ///
    /// To SEE THIS RED: in `handle_read` (h2.rs), pass `amount` in place of
    /// `amount - size` to `set_expect_read`.
    #[test]
    fn handle_read_subtracts_the_byte_count_the_caller_reported() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(2, 4, 16384)));
        let (mut connection, _peer) = test_h2_connection(&pool, None);
        let mut context = test_context(&pool);
        let mut router = Router::new(Duration::from_secs(30), Duration::from_secs(30));

        // Not ClientPreface: a short read there runs the early-preface check
        // instead, which is a different branch with its own coverage.
        connection.state = H2State::Header;
        connection
            .stream_table
            .set_expect_read(Some((H2StreamId::Zero, 9)));

        let result = connection.handle_read(
            &mut context,
            EndpointClient(&mut router),
            H2StreamId::Zero,
            H2ReadOutcome::Filled {
                amount: 9,
                size: 3,
                status: SocketResult::Continue,
            },
        );

        assert!(
            matches!(result, MuxResult::Continue),
            "a short read keeps the connection running: {result:?}"
        );
        assert_eq!(
            connection.stream_table.expect_read(),
            Some((H2StreamId::Zero, 6)),
            "a 3-byte answer to a 9-byte offer must leave 6 bytes owed"
        );
        assert_eq!(
            connection.zero.storage.data().len(),
            3,
            "the core must consume exactly the bytes the caller reported"
        );
    }

    /// A read that returned nothing clears the READABLE **event** and leaves
    /// the READABLE **interest** alone, so the connection is re-armed for the
    /// next epoll wake-up instead of being taken off the loop.
    ///
    /// The interest half is asserted on purpose: `signal_pending_write` and
    /// this path both touch `Readiness.event` only, and an assertion written
    /// against `interest` cannot see either of them move.
    ///
    /// To SEE THIS RED: in `handle_read` (h2.rs), call
    /// `update_readiness_after_write` in place of `update_readiness_after_read`
    /// — the WRITABLE bit is cleared instead and the READABLE event survives.
    #[test]
    fn handle_read_clears_the_readable_event_not_the_interest_when_nothing_arrived() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(2, 4, 16384)));
        let (mut connection, _peer) = test_h2_connection(&pool, None);
        let mut context = test_context(&pool);
        let mut router = Router::new(Duration::from_secs(30), Duration::from_secs(30));

        connection.state = H2State::Header;
        connection
            .stream_table
            .set_expect_read(Some((H2StreamId::Zero, 9)));
        connection.readiness.event = Ready::READABLE;
        connection.readiness.interest = Ready::READABLE | Ready::HUP | Ready::ERROR;

        let result = connection.handle_read(
            &mut context,
            EndpointClient(&mut router),
            H2StreamId::Zero,
            H2ReadOutcome::Filled {
                amount: 9,
                size: 0,
                status: SocketResult::WouldBlock,
            },
        );

        assert!(
            matches!(result, MuxResult::Continue),
            "an empty read is not a close: {result:?}"
        );
        assert!(
            !connection.readiness.event.is_readable(),
            "an empty read must clear the READABLE event"
        );
        assert!(
            connection.readiness.interest.is_readable(),
            "an empty read must NOT drop the READABLE interest"
        );
        assert_eq!(
            connection.stream_table.expect_read(),
            Some((H2StreamId::Zero, 9)),
            "an empty read settles no part of the byte debt"
        );
    }

    // ── RFC 9113 §4.3: a refused stream must not desynchronise HPACK ──────
    //
    // Field compression state is scoped to the whole connection, not to a
    // stream. When `handle_header_state` refuses a stream — during a
    // graceful drain, over MAX_CONCURRENT_STREAMS, or on buffer-pool
    // exhaustion — the HEADERS payload it drops is an HPACK field block the
    // peer's encoder has *already* applied to its own dynamic table. Dropping
    // it without decoding leaves our decoder permanently behind the peer's
    // encoder, and every later header block on the surviving connection then
    // resolves the wrong dynamic entry or fails outright.

    /// To SEE THIS RED: in the `(H2State::Discard, _)` arm of
    /// [`ConnectionH2::handle_read`], remove the `if let Some(discarded) =
    /// self.discarded_field_block.take() && ...` block that calls
    /// [`decode_discarded_field_block`], restoring the unconditional
    /// `kawa.storage.clear()`. The peer's second block is then a one-byte
    /// reference to dynamic index 62 that our decoder has never been told
    /// about, and the decode fails with `HeaderIndexOutOfBounds`. Verified
    /// 2026-09-21 (see this commit's message for the exact failure output).
    #[test]
    fn a_refused_stream_keeps_the_hpack_decoder_in_sync() {
        use std::io::Write;

        let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 20, 16_384)));
        let (mut connection, mut peer) = test_h2_connection(&pool, None);
        let mut context = test_context(&pool);
        let mut router = Router::new(Duration::from_secs(30), Duration::from_secs(30));

        // Past the preface/SETTINGS handshake, waiting on a frame header, and
        // draining — so the next client-initiated stream is refused rather
        // than created. Drain is only the cheapest of the three refusal
        // triggers; MAX_CONCURRENT_STREAMS and pool exhaustion reach the same
        // `refuse_stream_and_discard` call.
        connection.state = H2State::Header;
        connection
            .stream_table
            .set_expect_read(Some((H2StreamId::Zero, 9)));
        connection.drain.__test_set_draining();

        // The peer's encoder. `loona_hpack` indexes a header whose *name* is in
        // neither table, so this block appends `x-sozu-probe: alpha` to the
        // peer's dynamic table at index 62.
        let mut peer_encoder = loona_hpack::Encoder::new();
        let refused_block = peer_encoder.encode([
            (&b":method"[..], &b"GET"[..]),
            (&b":scheme"[..], &b"https"[..]),
            (&b":authority"[..], &b"example.com"[..]),
            (&b":path"[..], &b"/refused"[..]),
            (&b"x-sozu-probe"[..], &b"alpha"[..]),
        ]);
        assert!(
            refused_block.len() < 16_384,
            "the probe block must fit one HEADERS frame under the default max frame size"
        );

        // HEADERS, END_STREAM | END_HEADERS, stream 1.
        let mut frame = Vec::with_capacity(9 + refused_block.len());
        frame.extend_from_slice(&(refused_block.len() as u32).to_be_bytes()[1..]);
        frame.push(1);
        frame.push(parser::FLAG_END_STREAM | parser::FLAG_END_HEADERS);
        frame.extend_from_slice(&1u32.to_be_bytes());
        frame.extend_from_slice(&refused_block);
        peer.write_all(&frame)
            .expect("loopback write must complete");
        peer.flush().expect("loopback flush must complete");

        // Two passes consume the 9-byte header and then the payload; the rest
        // are `WouldBlock` no-ops. Bounded so a delivery hiccup fails loudly
        // instead of hanging.
        for _ in 0..64 {
            connection.readable(&mut context, EndpointClient(&mut router));
            if matches!(connection.state, H2State::Header)
                && matches!(
                    connection.stream_table.expect_read(),
                    Some((H2StreamId::Zero, 9))
                )
                && !connection.control_tx.pending().is_empty()
            {
                break;
            }
            std::thread::yield_now();
        }

        // Premise of the test: we really went through the refusal path, the
        // stream was refused rather than created, and the connection survived.
        assert_eq!(
            connection.control_tx.pending(),
            vec![(1, H2Error::RefusedStream)],
            "the drain gate must refuse stream 1 with RST_STREAM(REFUSED_STREAM)"
        );
        assert!(
            connection.stream_table.is_empty(),
            "a refused stream must not be created"
        );
        assert!(
            !matches!(connection.state, H2State::Error | H2State::GoAway),
            "refusing one stream must leave the connection usable: {:?}",
            connection.state
        );

        // The peer's next block references the dynamic entry its encoder added
        // while encoding the refused request — a single indexed field.
        let next_block = peer_encoder.encode([(&b"x-sozu-probe"[..], &b"alpha"[..])]);
        assert_eq!(
            next_block.len(),
            1,
            "the peer must now reference its dynamic entry, not re-send a literal"
        );

        let mut decoded = Vec::new();
        let status = connection
            .hpack
            .decoder_mut()
            .decode_with_cb(&next_block, |k, v| {
                decoded.push((k.into_owned(), v.into_owned()));
            });

        assert!(
            status.is_ok(),
            "the connection decoder must still resolve the peer's dynamic table \
             after a refused stream, got {status:?}"
        );
        assert_eq!(
            decoded,
            vec![(b"x-sozu-probe".to_vec(), b"alpha".to_vec())],
            "the refused stream's field block must have been decoded into the \
             connection's HPACK context"
        );
    }

    /// The RED-confirmed test above sends a HEADERS payload that is nothing
    /// but a field block: no `Pad Length`, no `Stream Dependency`/`Weight`.
    /// That shape cannot catch correction #1 — RFC 9113 §6.2 lays a HEADERS
    /// payload out as `[Pad Length?][Priority?][field block][padding?]`, and
    /// `refuse_stream_and_discard`'s new-stream callers pass the *whole*
    /// payload. A decode that forgets to strip the PADDED/PRIORITY prefix
    /// feeds the HPACK decoder six bytes of pad-length/dependency/weight
    /// garbage before the real field block starts.
    ///
    /// To SEE THIS RED: in [`decode_discarded_field_block`]'s
    /// `DiscardedFieldBlock::New` arm, replace the `let fragment = headers
    /// .header_block_fragment.data_opt(payload)...` lines with `let fragment
    /// = payload;` — i.e. hand the connection decoder the raw payload
    /// unstripped, the way a fix that only handled the un-padded case would.
    /// The peer's first block is still legitimate HPACK once you skip past
    /// the 6-byte prefix, but decoding from byte 0 instead makes the pad
    /// length byte and the first three dependency/weight octets look like
    /// HPACK opcodes. Verified 2026-09-21: the decode fails with
    /// `H2Error::CompressionError`, observed via the connection reaching
    /// `H2State::GoAway` (the panic is on the "connection usable" assertion,
    /// not the later decode assertion — the malformed prefix corrupts the
    /// connection before the probe even gets to send its second block).
    #[test]
    fn a_refused_padded_prioritized_stream_keeps_the_hpack_decoder_in_sync() {
        use std::io::Write;

        let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 20, 16_384)));
        let (mut connection, mut peer) = test_h2_connection(&pool, None);
        let mut context = test_context(&pool);
        let mut router = Router::new(Duration::from_secs(30), Duration::from_secs(30));

        connection.state = H2State::Header;
        connection
            .stream_table
            .set_expect_read(Some((H2StreamId::Zero, 9)));
        connection.drain.__test_set_draining();

        // Same probe technique as the sibling test: this appends
        // `x-sozu-probe: alpha` to the peer's dynamic table at index 62.
        let mut peer_encoder = loona_hpack::Encoder::new();
        let field_block = peer_encoder.encode([
            (&b":method"[..], &b"GET"[..]),
            (&b":scheme"[..], &b"https"[..]),
            (&b":authority"[..], &b"example.com"[..]),
            (&b":path"[..], &b"/refused"[..]),
            (&b"x-sozu-probe"[..], &b"alpha"[..]),
        ]);

        // RFC 9113 §6.2 HEADERS payload: Pad Length (1) + Stream Dependency
        // (4, exclusive bit clear, depends on stream 0) + Weight (1) + the
        // field block + 3 zero padding bytes.
        const PAD_LEN: u8 = 3;
        let mut payload = Vec::with_capacity(1 + 5 + field_block.len() + PAD_LEN as usize);
        payload.push(PAD_LEN);
        payload.extend_from_slice(&0u32.to_be_bytes()); // Stream Dependency
        payload.push(15); // Weight
        payload.extend_from_slice(&field_block);
        payload.extend(std::iter::repeat_n(0u8, PAD_LEN as usize));

        // HEADERS, END_STREAM | END_HEADERS | PADDED | PRIORITY, stream 1.
        let mut frame = Vec::with_capacity(9 + payload.len());
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes()[1..]);
        frame.push(1);
        frame.push(
            parser::FLAG_END_STREAM
                | parser::FLAG_END_HEADERS
                | parser::FLAG_PADDED
                | parser::FLAG_PRIORITY,
        );
        frame.extend_from_slice(&1u32.to_be_bytes());
        frame.extend_from_slice(&payload);
        peer.write_all(&frame)
            .expect("loopback write must complete");
        peer.flush().expect("loopback flush must complete");

        for _ in 0..64 {
            connection.readable(&mut context, EndpointClient(&mut router));
            if matches!(connection.state, H2State::Header)
                && matches!(
                    connection.stream_table.expect_read(),
                    Some((H2StreamId::Zero, 9))
                )
                && !connection.control_tx.pending().is_empty()
            {
                break;
            }
            std::thread::yield_now();
        }

        assert_eq!(
            connection.control_tx.pending(),
            vec![(1, H2Error::RefusedStream)],
            "the drain gate must refuse stream 1 with RST_STREAM(REFUSED_STREAM)"
        );
        assert!(
            connection.stream_table.is_empty(),
            "a refused stream must not be created"
        );
        assert!(
            !matches!(connection.state, H2State::Error | H2State::GoAway),
            "refusing a PADDED|PRIORITY HEADERS frame must leave the connection \
             usable: {:?}",
            connection.state
        );

        let next_block = peer_encoder.encode([(&b"x-sozu-probe"[..], &b"alpha"[..])]);
        assert_eq!(
            next_block.len(),
            1,
            "the peer must now reference its dynamic entry, not re-send a literal"
        );

        let mut decoded = Vec::new();
        let status = connection
            .hpack
            .decoder_mut()
            .decode_with_cb(&next_block, |k, v| {
                decoded.push((k.into_owned(), v.into_owned()));
            });

        assert!(
            status.is_ok(),
            "the connection decoder must still resolve the peer's dynamic table \
             after a refused PADDED|PRIORITY stream, got {status:?}"
        );
        assert_eq!(
            decoded,
            vec![(b"x-sozu-probe".to_vec(), b"alpha".to_vec())],
            "the refused stream's field block — located past the Pad Length and \
             Priority prefix — must have been decoded into the connection's \
             HPACK context"
        );
    }

    // ── `self.zero` is read-accumulation AND write scratch — that is the bug ──
    //
    // `ConnectionH2::zero` serves two roles: the read-side accumulation
    // buffer for a HEADERS/CONTINUATION field block in progress, and the
    // write scratch space `flush_pending_control_frames` reuses to
    // serialise ANY queued WINDOW_UPDATE or RST_STREAM — for any stream,
    // not just the one being reassembled. `mod.rs`'s inner event loop
    // dispatches frontend `readable()` then `writable()` in the same sweep,
    // so a legitimate, non-refused multi-frame header block can be
    // clobbered by completely unrelated connection-level flow-control
    // housekeeping before its CONTINUATION frame ever arrives — no
    // adversarial peer required.

    /// To SEE THIS RED: this is the pre-existing behaviour on `main`, no
    /// mutation needed. `flush_pending_control_frames`'s WINDOW_UPDATE-drain
    /// stage (`let kawa = &mut self.zero; kawa.storage.clear();`) runs
    /// unconditionally whenever a WINDOW_UPDATE is queued, with no check on
    /// `self.state`. Guarding that stage (and the RST_STREAM-drain stage
    /// beneath it) on `matches!(self.state, H2State::ContinuationHeader(_) |
    /// H2State::ContinuationFrame(_))` is exactly the fix this test proves.
    #[test]
    fn a_legitimate_continuation_survives_an_unrelated_window_update_flush() {
        use std::io::Write;

        let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 20, 16_384)));
        let (mut connection, mut peer) = test_h2_connection(&pool, None);
        let mut context = test_context(&pool);
        let mut router = Router::new(Duration::from_secs(30), Duration::from_secs(30));

        // Past the preface/SETTINGS handshake, waiting on a frame header.
        // `drain.draining` stays false: stream 1 must be genuinely accepted,
        // not refused — this is the ordinary, non-refusal path.
        connection.state = H2State::Header;
        connection
            .stream_table
            .set_expect_read(Some((H2StreamId::Zero, 9)));

        // The peer's encoder. The marker header's name is in neither table,
        // so encoding it appends `x-sozu-marker: legitimate-value` to the
        // peer's dynamic table at a known index — the same technique
        // `a_refused_stream_keeps_the_hpack_decoder_in_sync` uses to prove
        // decoder sync, here applied to a stream that is never refused.
        let mut peer_encoder = loona_hpack::Encoder::new();
        let field_block = peer_encoder.encode([
            (&b":method"[..], &b"GET"[..]),
            (&b":scheme"[..], &b"https"[..]),
            (&b":authority"[..], &b"example.com"[..]),
            (&b":path"[..], &b"/legit-multiframe"[..]),
            (&b"x-sozu-marker"[..], &b"legitimate-value"[..]),
        ]);
        // Split well past the 13 bytes a single WINDOW_UPDATE frame occupies,
        // so the clobber this test provokes lands inside real field-block
        // bytes rather than in a margin this test invented.
        assert!(
            field_block.len() > 26,
            "the probe block must be large enough to split meaningfully"
        );
        let split = field_block.len() / 2;
        let (first_half, second_half) = field_block.split_at(split);

        // HEADERS, END_STREAM but NOT END_HEADERS, stream 1: a legitimate
        // multi-frame header block, exactly as a large request produces.
        let mut headers_frame = Vec::with_capacity(9 + first_half.len());
        headers_frame.extend_from_slice(&(first_half.len() as u32).to_be_bytes()[1..]);
        headers_frame.push(1); // HEADERS
        headers_frame.push(parser::FLAG_END_STREAM);
        headers_frame.extend_from_slice(&1u32.to_be_bytes());
        headers_frame.extend_from_slice(first_half);
        peer.write_all(&headers_frame)
            .expect("loopback write must complete");
        peer.flush().expect("loopback flush must complete");

        // Drive reads until the connection is genuinely waiting on the
        // CONTINUATION frame for stream 1 — accepted, not refused.
        for _ in 0..64 {
            connection.readable(&mut context, EndpointClient(&mut router));
            if matches!(connection.state, H2State::ContinuationHeader(_)) {
                break;
            }
            std::thread::yield_now();
        }
        assert!(
            matches!(connection.state, H2State::ContinuationHeader(_)),
            "stream 1's HEADERS frame must be accepted and await CONTINUATION: {:?}",
            connection.state
        );
        assert!(
            !connection.stream_table.is_empty(),
            "a non-refused HEADERS frame must create stream 1"
        );

        // Completely unrelated connection-level housekeeping: replenishing
        // the connection flow-control window after DATA consumed on some
        // other stream. Nothing about it targets stream 1 or its
        // in-progress header block — this is the ordinary path, not an
        // attack.
        connection.queue_window_update(0, 65_535);
        connection.writable(&mut context, EndpointClient(&mut router));

        // Now the CONTINUATION frame completing the block arrives.
        let mut continuation_frame = Vec::with_capacity(9 + second_half.len());
        continuation_frame.extend_from_slice(&(second_half.len() as u32).to_be_bytes()[1..]);
        continuation_frame.push(9); // CONTINUATION
        continuation_frame.push(parser::FLAG_END_HEADERS);
        continuation_frame.extend_from_slice(&1u32.to_be_bytes());
        continuation_frame.extend_from_slice(second_half);
        peer.write_all(&continuation_frame)
            .expect("loopback write must complete");
        peer.flush().expect("loopback flush must complete");

        for _ in 0..64 {
            connection.readable(&mut context, EndpointClient(&mut router));
            if !matches!(
                connection.state,
                H2State::ContinuationHeader(_) | H2State::ContinuationFrame(_)
            ) {
                break;
            }
            std::thread::yield_now();
        }

        assert!(
            !matches!(connection.state, H2State::Error | H2State::GoAway),
            "an unrelated WINDOW_UPDATE flush must not corrupt a legitimate \
             in-progress header block into a connection error: {:?}",
            connection.state
        );

        // The peer's next block references the dynamic entry its encoder
        // added while encoding stream 1's request.
        let next_block = peer_encoder.encode([(&b"x-sozu-marker"[..], &b"legitimate-value"[..])]);
        assert_eq!(
            next_block.len(),
            1,
            "the peer must now reference its dynamic entry, not re-send a literal"
        );

        let mut decoded = Vec::new();
        let status = connection
            .hpack
            .decoder_mut()
            .decode_with_cb(&next_block, |k, v| {
                decoded.push((k.into_owned(), v.into_owned()));
            });

        assert!(
            status.is_ok(),
            "the connection decoder must still resolve the peer's dynamic table \
             after an unrelated WINDOW_UPDATE flush during header-block \
             reassembly, got {status:?}"
        );
        assert_eq!(
            decoded,
            vec![(b"x-sozu-marker".to_vec(), b"legitimate-value".to_vec())],
            "stream 1's field block must have been decoded byte-for-byte, not \
             clobbered by the unrelated WINDOW_UPDATE flush"
        );
    }

    /// Same shape as
    /// `a_legitimate_continuation_survives_an_unrelated_window_update_flush`,
    /// but the clobbering trigger is `graceful_goaway` itself instead of an
    /// unrelated WINDOW_UPDATE flush — the third instance of the same bug
    /// class (issue #1398).
    ///
    /// To SEE THIS RED: this is the pre-existing behaviour on `main`, no
    /// mutation needed. `graceful_goaway` clears `self.zero.storage`
    /// unconditionally to serialize the advisory GOAWAY, with no check on
    /// `header_block_reassembly_in_progress()`. Deferring that clear (and
    /// the serialization) until reassembly completes — mirroring the
    /// WINDOW_UPDATE/RST_STREAM drains `flush_pending_control_frames`
    /// already gates — is exactly the fix this test proves.
    #[test]
    fn a_legitimate_continuation_survives_a_graceful_goaway() {
        use std::io::{Read, Write};

        let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 20, 16_384)));
        let (mut connection, mut peer) = test_h2_connection(&pool, None);
        let mut context = test_context(&pool);
        let mut router = Router::new(Duration::from_secs(30), Duration::from_secs(30));

        // Past the preface/SETTINGS handshake, waiting on a frame header.
        // `drain.draining` stays false until `graceful_goaway` is called
        // below: stream 1 must be genuinely accepted, not refused — this is
        // the ordinary, non-refusal path.
        connection.state = H2State::Header;
        connection
            .stream_table
            .set_expect_read(Some((H2StreamId::Zero, 9)));

        // The peer's encoder. The marker header's name is in neither table,
        // so encoding it appends `x-sozu-marker: legitimate-value` to the
        // peer's dynamic table at a known index — the same technique
        // `a_legitimate_continuation_survives_an_unrelated_window_update_flush`
        // uses to prove decoder sync.
        let mut peer_encoder = loona_hpack::Encoder::new();
        let field_block = peer_encoder.encode([
            (&b":method"[..], &b"GET"[..]),
            (&b":scheme"[..], &b"https"[..]),
            (&b":authority"[..], &b"example.com"[..]),
            (&b":path"[..], &b"/legit-multiframe"[..]),
            (&b"x-sozu-marker"[..], &b"legitimate-value"[..]),
        ]);
        // Same header set and split proportions as
        // `a_legitimate_continuation_survives_an_unrelated_window_update_flush`:
        // `header_block_fragment`'s window is a `(start, len)` pair recorded
        // once, independent of `self.zero.storage`'s own bookkeeping, so
        // *any* clear-then-refill of that buffer — 13 bytes of WINDOW_UPDATE
        // or 17 bytes of GOAWAY, it makes no difference — leaves the window
        // pointing at the same wrong offset once `end`/`head` reset to 0 and
        // the CONTINUATION's own bytes land there instead. Matching the split
        // reproduces the same decodable-but-wrong HPACK byte sequence rather
        // than a split that happens to land on an invalid opcode.
        assert!(
            field_block.len() > 34,
            "the probe block must be large enough to split meaningfully"
        );
        let split = field_block.len() / 2;
        let (first_half, second_half) = field_block.split_at(split);

        // HEADERS, END_STREAM but NOT END_HEADERS, stream 1: a legitimate
        // multi-frame header block, exactly as a large request produces.
        let mut headers_frame = Vec::with_capacity(9 + first_half.len());
        headers_frame.extend_from_slice(&(first_half.len() as u32).to_be_bytes()[1..]);
        headers_frame.push(1); // HEADERS
        headers_frame.push(parser::FLAG_END_STREAM);
        headers_frame.extend_from_slice(&1u32.to_be_bytes());
        headers_frame.extend_from_slice(first_half);
        peer.write_all(&headers_frame)
            .expect("loopback write must complete");
        peer.flush().expect("loopback flush must complete");

        // Drive reads until the connection is genuinely waiting on the
        // CONTINUATION frame for stream 1 — accepted, not refused.
        for _ in 0..64 {
            connection.readable(&mut context, EndpointClient(&mut router));
            if matches!(connection.state, H2State::ContinuationHeader(_)) {
                break;
            }
            std::thread::yield_now();
        }
        assert!(
            matches!(connection.state, H2State::ContinuationHeader(_)),
            "stream 1's HEADERS frame must be accepted and await CONTINUATION: {:?}",
            connection.state
        );
        assert!(
            !connection.stream_table.is_empty(),
            "a non-refused HEADERS frame must create stream 1"
        );

        // The proxy decides to drain — a graceful shutdown or hot reload
        // landing mid-reassembly, exactly as issue #1398 describes. Nothing
        // about it targets stream 1 or its in-progress header block.
        let drain_at = connection.now;
        let result = connection.graceful_goaway(drain_at);
        assert!(
            matches!(result, MuxResult::Continue),
            "graceful_goaway must not tear the connection down while deferring: {result:?}"
        );

        // The forced-close budget must arm from the moment the proxy decided
        // to drain, not from whenever the deferred GOAWAY eventually goes
        // out — `Mux::shutting_down` samples exactly this field against
        // `graceful_shutdown_deadline`.
        assert!(
            connection.drain.draining(),
            "graceful_goaway must mark the connection as draining immediately, \
             even when the GOAWAY itself is deferred"
        );
        assert_eq!(
            connection.drain.__test_started_at(),
            Some(drain_at),
            "the forced-close budget must arm from `graceful_goaway`'s `now` \
             parameter immediately, not once the deferred GOAWAY is sent"
        );
        assert!(
            connection.drain.__test_initial_goaway_pending(),
            "the advisory GOAWAY must be deferred while a header block is \
             reassembling, not sent (and clobber the reassembly) or dropped"
        );

        // Mirror `Mux::shutting_down_inner` exactly: it calls
        // `flush_zero_buffer()` immediately after `graceful_goaway()`
        // returns `Continue`, because edge-triggered epoll won't deliver a
        // fresh WRITABLE event for an already-writable socket. Must be a
        // no-op while reassembly is still in progress.
        connection.flush_zero_buffer();

        // Now the CONTINUATION frame completing the block arrives.
        let mut continuation_frame = Vec::with_capacity(9 + second_half.len());
        continuation_frame.extend_from_slice(&(second_half.len() as u32).to_be_bytes()[1..]);
        continuation_frame.push(9); // CONTINUATION
        continuation_frame.push(parser::FLAG_END_HEADERS);
        continuation_frame.extend_from_slice(&1u32.to_be_bytes());
        continuation_frame.extend_from_slice(second_half);
        peer.write_all(&continuation_frame)
            .expect("loopback write must complete");
        peer.flush().expect("loopback flush must complete");

        for _ in 0..64 {
            connection.readable(&mut context, EndpointClient(&mut router));
            if !matches!(
                connection.state,
                H2State::ContinuationHeader(_) | H2State::ContinuationFrame(_)
            ) {
                break;
            }
            std::thread::yield_now();
        }

        assert!(
            !matches!(connection.state, H2State::Error | H2State::GoAway),
            "a graceful_goaway landing mid-reassembly must not corrupt a \
             legitimate in-progress header block into a connection error: {:?}",
            connection.state
        );

        // The peer's next block references the dynamic entry its encoder
        // added while encoding stream 1's request.
        let next_block = peer_encoder.encode([(&b"x-sozu-marker"[..], &b"legitimate-value"[..])]);
        assert_eq!(
            next_block.len(),
            1,
            "the peer must now reference its dynamic entry, not re-send a literal"
        );

        let mut decoded = Vec::new();
        let status = connection
            .hpack
            .decoder_mut()
            .decode_with_cb(&next_block, |k, v| {
                decoded.push((k.into_owned(), v.into_owned()));
            });

        assert!(
            status.is_ok(),
            "the connection decoder must still resolve the peer's dynamic table \
             after a graceful_goaway during header-block reassembly, got {status:?}"
        );
        assert_eq!(
            decoded,
            vec![(b"x-sozu-marker".to_vec(), b"legitimate-value".to_vec())],
            "stream 1's field block must have been decoded byte-for-byte, not \
             clobbered by graceful_goaway"
        );

        // The deferred advisory GOAWAY must not be lost: drive writable()
        // until it is queued and flushed, then verify the peer actually
        // received it, byte-for-byte identical to an immediate (non-deferred)
        // serialization.
        for _ in 0..8 {
            connection.writable(&mut context, EndpointClient(&mut router));
            if !connection.drain.__test_initial_goaway_pending()
                && connection.stream_table.expect_write().is_none()
            {
                break;
            }
        }
        assert!(
            !connection.drain.__test_initial_goaway_pending(),
            "the deferred advisory GOAWAY must be sent once reassembly completes, \
             not left pending forever"
        );
        assert!(
            connection.stream_table.expect_write().is_none(),
            "the advisory GOAWAY must have been fully flushed to the socket"
        );

        let mut expected = [0u8; 17];
        let (_, expected_size) =
            serializer::gen_goaway(&mut expected, STREAM_ID_MAX, H2Error::NoError)
                .expect("serializing the expected GOAWAY must succeed");
        assert_eq!(expected_size, 17);

        let mut received = [0u8; 17];
        peer.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("setting a read timeout must succeed");
        peer.read_exact(&mut received)
            .expect("the peer must receive the deferred advisory GOAWAY");
        assert_eq!(
            received, expected,
            "the deferred advisory GOAWAY must reach the peer byte-for-byte \
             once header-block reassembly completes"
        );
    }

    /// Regression test for #1423: `flush_pending_control_frames`'s
    /// `frontend_hung_up_while_draining()` stage cleared `zero.storage`
    /// unconditionally, with no `header_block_reassembly_in_progress()`
    /// guard — unlike its three sibling stages, which #1397 and #1401
    /// already gated. LIFECYCLE.md invariant 24 tracked this as a "known
    /// live gap, reported and NOT fixed" until this changeset.
    ///
    /// Same shape as `a_legitimate_continuation_survives_a_graceful_goaway`,
    /// but the clobbering trigger is a HUP/ERROR readiness event landing
    /// while draining — exactly `frontend_hung_up_while_draining()`'s own
    /// condition — instead of `graceful_goaway` itself. This test drives
    /// the gap BETWEEN two frames (a HUP arriving while
    /// `expect_read = Some((Zero, 9))`, waiting for the next CONTINUATION's
    /// header, with nothing of this block's history left in `zero.storage`
    /// to lose). Moving the accumulator out of `zero.storage` into
    /// `self.header_reassembly` (`h2_header_reassembly.rs`) makes THIS
    /// scenario safe regardless of whether this stage is guarded: there is
    /// no accumulated history left in `zero.storage` for it to clobber.
    ///
    /// That is NOT the whole of #1423, though — see
    /// `a_continuation_frame_split_by_tcp_segmentation_survives_a_hup_while_draining`
    /// below for the narrower "HUP lands mid-frame, not between frames"
    /// case this test does not cover, which review of this changeset's
    /// first version (`e1c3c2fb`) proved the accumulator move alone does
    /// NOT make safe, and which is why `frontend_hung_up_while_draining()`'s
    /// stage carries the same `header_block_reassembly_in_progress()` guard
    /// as its three siblings after all.
    ///
    /// To SEE THIS RED on this step's base commit (`f76c8eb3`, before the
    /// accumulator moved out of `zero.storage`): run this test unmodified
    /// against that commit. `flush_pending_control_frames`'s first stage
    /// clears `zero.storage` — which, on that commit, still holds stream 1's
    /// entire accumulated first-half fragment — before the CONTINUATION
    /// frame completing the block arrives; the CONTINUATION's own bytes
    /// then land at the wrong (reset-to-zero) offset, and the connection's
    /// HPACK decoder desyncs from the peer's encoder. The failure is
    /// silent corruption, not a crash (HPACK carries no self-check): the
    /// `decode_with_cb` assertion below fails with a decode error, or
    /// succeeds but yields something other than
    /// `[(b"x-sozu-marker", b"legitimate-value")]`.
    #[test]
    fn a_legitimate_continuation_survives_a_hup_while_draining() {
        use std::io::Write;

        let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 20, 16_384)));
        let (mut connection, mut peer) = test_h2_connection(&pool, None);
        let mut context = test_context(&pool);
        let mut router = Router::new(Duration::from_secs(30), Duration::from_secs(30));

        // Past the preface/SETTINGS handshake, waiting on a frame header.
        // `drain.draining` stays false until manually forced below: stream 1
        // must be genuinely accepted, not refused — this is the ordinary,
        // non-refusal path.
        connection.state = H2State::Header;
        connection
            .stream_table
            .set_expect_read(Some((H2StreamId::Zero, 9)));

        // The peer's encoder. The marker header's name is in neither table,
        // so encoding it appends `x-sozu-marker: legitimate-value` to the
        // peer's dynamic table at a known index — the same technique the
        // sibling WINDOW_UPDATE/graceful_goaway tests use to prove decoder
        // sync.
        let mut peer_encoder = loona_hpack::Encoder::new();
        let field_block = peer_encoder.encode([
            (&b":method"[..], &b"GET"[..]),
            (&b":scheme"[..], &b"https"[..]),
            (&b":authority"[..], &b"example.com"[..]),
            (&b":path"[..], &b"/legit-multiframe"[..]),
            (&b"x-sozu-marker"[..], &b"legitimate-value"[..]),
        ]);
        assert!(
            field_block.len() > 34,
            "the probe block must be large enough to split meaningfully"
        );
        let split = field_block.len() / 2;
        let (first_half, second_half) = field_block.split_at(split);

        // HEADERS, END_STREAM but NOT END_HEADERS, stream 1: a legitimate
        // multi-frame header block, exactly as a large request produces.
        let mut headers_frame = Vec::with_capacity(9 + first_half.len());
        headers_frame.extend_from_slice(&(first_half.len() as u32).to_be_bytes()[1..]);
        headers_frame.push(1); // HEADERS
        headers_frame.push(parser::FLAG_END_STREAM);
        headers_frame.extend_from_slice(&1u32.to_be_bytes());
        headers_frame.extend_from_slice(first_half);
        peer.write_all(&headers_frame)
            .expect("loopback write must complete");
        peer.flush().expect("loopback flush must complete");

        // Drive reads until the connection is genuinely waiting on the
        // CONTINUATION frame for stream 1 — accepted, not refused.
        for _ in 0..64 {
            connection.readable(&mut context, EndpointClient(&mut router));
            if matches!(connection.state, H2State::ContinuationHeader(_)) {
                break;
            }
            std::thread::yield_now();
        }
        assert!(
            matches!(connection.state, H2State::ContinuationHeader(_)),
            "stream 1's HEADERS frame must be accepted and await CONTINUATION: {:?}",
            connection.state
        );
        assert!(
            !connection.stream_table.is_empty(),
            "a non-refused HEADERS frame must create stream 1"
        );

        // Exactly #1423's trigger: the connection is draining AND the
        // frontend readiness reports HUP, landing squarely mid-reassembly.
        // A peer can legitimately produce this by sending TCP FIN together
        // with (or shortly after) the last CONTINUATION frame it manages to
        // get out, observed by edge-triggered epoll as one combined event.
        connection.drain.__test_set_draining();
        connection.readiness.event.insert(Ready::HUP);

        // `flush_pending_control_frames`'s first, HUP-gated stage fires
        // here.
        connection.writable(&mut context, EndpointClient(&mut router));

        // Real epoll delivers HUP once; un-signal it so the reads below
        // reflect the peer's normal traffic rather than a second synthetic
        // HUP this test injected by hand.
        connection.readiness.event.remove(Ready::HUP);

        // Now the CONTINUATION frame completing the block arrives — still
        // fully deliverable, exactly as it would be if the peer's FIN and
        // its last frame arrived close together but the block itself was
        // sent in full.
        let mut continuation_frame = Vec::with_capacity(9 + second_half.len());
        continuation_frame.extend_from_slice(&(second_half.len() as u32).to_be_bytes()[1..]);
        continuation_frame.push(9); // CONTINUATION
        continuation_frame.push(parser::FLAG_END_HEADERS);
        continuation_frame.extend_from_slice(&1u32.to_be_bytes());
        continuation_frame.extend_from_slice(second_half);
        peer.write_all(&continuation_frame)
            .expect("loopback write must complete");
        peer.flush().expect("loopback flush must complete");

        for _ in 0..64 {
            connection.readable(&mut context, EndpointClient(&mut router));
            if !matches!(
                connection.state,
                H2State::ContinuationHeader(_) | H2State::ContinuationFrame(_)
            ) {
                break;
            }
            std::thread::yield_now();
        }

        assert!(
            !matches!(connection.state, H2State::Error | H2State::GoAway),
            "a HUP-while-draining event landing mid-reassembly must not corrupt \
             a legitimate in-progress header block into a connection error: {:?}",
            connection.state
        );

        // The peer's next block references the dynamic entry its encoder
        // added while encoding stream 1's request.
        let next_block = peer_encoder.encode([(&b"x-sozu-marker"[..], &b"legitimate-value"[..])]);
        assert_eq!(
            next_block.len(),
            1,
            "the peer must now reference its dynamic entry, not re-send a literal"
        );

        let mut decoded = Vec::new();
        let status = connection
            .hpack
            .decoder_mut()
            .decode_with_cb(&next_block, |k, v| {
                decoded.push((k.into_owned(), v.into_owned()));
            });

        assert!(
            status.is_ok(),
            "the connection decoder must still resolve the peer's dynamic table \
             after a HUP-while-draining event during header-block reassembly, \
             got {status:?}"
        );
        assert_eq!(
            decoded,
            vec![(b"x-sozu-marker".to_vec(), b"legitimate-value".to_vec())],
            "stream 1's field block must have been decoded byte-for-byte, not \
             clobbered by the HUP-while-draining stage of \
             flush_pending_control_frames"
        );
    }

    /// Regression test for B1 (review of `e1c3c2fb`): `handle_headers_frame`'s
    /// RFC 9113 §5.3.1 self-dependency early return — `reset_stream` +
    /// `remove_dead_stream` when a stream's own PRIORITY depends on itself —
    /// used to skip retiring `self.header_reassembly` before returning. When
    /// the aborted stream's block had gone through CONTINUATION reassembly
    /// (`header_reassembly.is_in_progress() == true` at abort time), the
    /// flag leaked into the very NEXT HEADERS frame processed on this
    /// connection: that frame's own "read from `zero.storage`" fast path
    /// was silently skipped in favour of the aborted stream's stale,
    /// already-discarded bytes — corrupting an entirely unrelated stream's
    /// decoded request. This reproduces in BOTH debug and release builds:
    /// nothing on this path was ever a `debug_assert!`.
    ///
    /// To SEE THIS RED on `e1c3c2fb` (before this fix): stream 1's block
    /// completes reassembly, its RFC 7540 PRIORITY self-dependency
    /// (`stream_dependency.stream_id == 1`) aborts it via `reset_stream`
    /// without retiring the accumulator, and
    /// `header_reassembly.is_in_progress()` stays `true`. Stream 3's own,
    /// unrelated, single-frame request is then decoded from stream 1's
    /// stale fragment instead of its own bytes: the `:path` pseudo-header
    /// this test asserts on is corrupted rather than
    /// `"/stream-3-legitimate"`.
    #[test]
    fn a_priority_self_dependency_reset_does_not_leak_the_reassembly_accumulator() {
        use std::io::Write;

        let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 20, 16_384)));
        let (mut connection, mut peer) = test_h2_connection(&pool, None);
        let mut context = test_context(&pool);
        let mut router = Router::new(Duration::from_secs(30), Duration::from_secs(30));

        connection.state = H2State::Header;
        connection
            .stream_table
            .set_expect_read(Some((H2StreamId::Zero, 9)));

        // Stream 1: a legitimate multi-frame header block — split across
        // HEADERS + CONTINUATION, exactly like the sibling reassembly tests
        // — but the FIRST frame also carries an RFC 7540 PRIORITY field
        // whose stream dependency is stream 1 itself (RFC 9113 §5.3.1: a
        // stream cannot depend on itself).
        let mut peer_encoder = loona_hpack::Encoder::new();
        let field_block = peer_encoder.encode([
            (&b":method"[..], &b"GET"[..]),
            (&b":scheme"[..], &b"https"[..]),
            (&b":authority"[..], &b"example.com"[..]),
            (&b":path"[..], &b"/stream-1-aborted"[..]),
        ]);
        assert!(
            field_block.len() > 10,
            "the probe block must be large enough to split meaningfully"
        );
        let split = field_block.len() / 2;
        let (first_half, second_half) = field_block.split_at(split);

        // HEADERS, PRIORITY (self-dependency), NOT END_HEADERS.
        let mut priority_prefix = Vec::with_capacity(5);
        priority_prefix.extend_from_slice(&1u32.to_be_bytes()); // stream_dependency = 1 (self)
        priority_prefix.push(15); // weight
        let mut headers_frame = Vec::with_capacity(9 + priority_prefix.len() + first_half.len());
        headers_frame.extend_from_slice(
            &((priority_prefix.len() + first_half.len()) as u32).to_be_bytes()[1..],
        );
        headers_frame.push(1); // HEADERS
        headers_frame.push(parser::FLAG_PRIORITY);
        headers_frame.extend_from_slice(&1u32.to_be_bytes());
        headers_frame.extend_from_slice(&priority_prefix);
        headers_frame.extend_from_slice(first_half);
        peer.write_all(&headers_frame)
            .expect("loopback write must complete");
        peer.flush().expect("loopback flush must complete");

        for _ in 0..64 {
            connection.readable(&mut context, EndpointClient(&mut router));
            if matches!(connection.state, H2State::ContinuationHeader(_)) {
                break;
            }
            std::thread::yield_now();
        }
        assert!(
            matches!(connection.state, H2State::ContinuationHeader(_)),
            "stream 1's HEADERS frame must be accepted and await CONTINUATION: {:?}",
            connection.state
        );

        // Complete the block — this re-enters `handle_headers_frame`, which
        // detects the self-dependency and aborts stream 1.
        let mut continuation_frame = Vec::with_capacity(9 + second_half.len());
        continuation_frame.extend_from_slice(&(second_half.len() as u32).to_be_bytes()[1..]);
        continuation_frame.push(9); // CONTINUATION
        continuation_frame.push(parser::FLAG_END_HEADERS);
        continuation_frame.extend_from_slice(&1u32.to_be_bytes());
        continuation_frame.extend_from_slice(second_half);
        peer.write_all(&continuation_frame)
            .expect("loopback write must complete");
        peer.flush().expect("loopback flush must complete");

        for _ in 0..64 {
            connection.readable(&mut context, EndpointClient(&mut router));
            if connection.stream_table.get(1).is_none() {
                break;
            }
            std::thread::yield_now();
        }

        // Premise: stream 1 really was torn down for self-dependency.
        assert!(
            connection.stream_table.get(1).is_none(),
            "the self-dependent stream must have been torn down"
        );

        // The direct proof of the fix: the accumulator must be idle again
        // before any other stream's HEADERS frame is processed. `h2.rs`'s
        // own test module can reach the private field directly — no
        // test-only accessor needed.
        assert!(
            !connection.header_reassembly.is_in_progress(),
            "aborting a self-dependent stream mid-reassembly must retire the \
             accumulator, not leave it `is_in_progress() == true` for the \
             next HEADERS frame on this connection"
        );

        // Stream 3: an entirely unrelated, single-frame, complete request.
        let stream3_block = peer_encoder.encode([
            (&b":method"[..], &b"GET"[..]),
            (&b":scheme"[..], &b"https"[..]),
            (&b":authority"[..], &b"example.com"[..]),
            (&b":path"[..], &b"/stream-3-legitimate"[..]),
        ]);
        let mut stream3_frame = Vec::with_capacity(9 + stream3_block.len());
        stream3_frame.extend_from_slice(&(stream3_block.len() as u32).to_be_bytes()[1..]);
        stream3_frame.push(1); // HEADERS
        stream3_frame.push(parser::FLAG_END_STREAM | parser::FLAG_END_HEADERS);
        stream3_frame.extend_from_slice(&3u32.to_be_bytes());
        stream3_frame.extend_from_slice(&stream3_block);
        peer.write_all(&stream3_frame)
            .expect("loopback write must complete");
        peer.flush().expect("loopback flush must complete");

        // Wait until the connection is back at `Header`, waiting on the
        // NEXT frame's 9-byte header — not merely until the stream slot
        // exists, which `create_stream` populates as soon as the frame
        // HEADER is parsed, well before its payload is read and decoded.
        for _ in 0..64 {
            connection.readable(&mut context, EndpointClient(&mut router));
            if matches!(connection.state, H2State::Header)
                && matches!(
                    connection.stream_table.expect_read(),
                    Some((H2StreamId::Zero, 9))
                )
            {
                break;
            }
            std::thread::yield_now();
        }

        let stream3_gid = connection
            .stream_table
            .get(3)
            .expect("stream 3's HEADERS frame must create a stream");
        assert_eq!(
            context.streams[stream3_gid].context.path.as_deref(),
            Some("/stream-3-legitimate"),
            "stream 3 must decode its OWN bytes, not stream 1's stale, \
             aborted reassembly fragment"
        );
    }

    /// Regression test for B2 (review of `e1c3c2fb`): `flush_pending_control_frames`'s
    /// `frontend_hung_up_while_draining()` stage used to clear `zero.storage`
    /// unconditionally, on the premise that `Ready::HUP` means "no further
    /// bytes can ever arrive". That premise is false: `Ready::HUP` is
    /// `is_read_closed() || is_write_closed()` (`command/src/ready.rs`), and
    /// mio documents `is_read_closed()` as also true on a TCP half-close —
    /// a FIN with data the peer already sent still sitting, unread, in the
    /// kernel receive queue. A CONTINUATION frame's payload split across two
    /// TCP segments can have its FIRST segment already read into
    /// `zero.storage` (a genuine partial `socket_read()`, not stale bytes
    /// from a completed pass) when the readiness event batches HUP together
    /// with that first segment; `drive_frontend_shutdown_io` (`mod.rs`)
    /// force-calls `readable()` for H2 on every `shutting_down()` poll, so
    /// this is the ordinary soft-stop path, not a contrived corner case.
    ///
    /// To SEE THIS RED on `e1c3c2fb` (before this fix): the partial first
    /// segment is wiped by the unconditional clear; the second segment then
    /// lands at the wrong (reset-to-zero) offset once appended, and the
    /// connection's HPACK decoder desyncs from the peer's encoder — the
    /// same silent-wrong-values failure mode as #1397/#1401/#1423.
    #[test]
    fn a_continuation_frame_split_by_tcp_segmentation_survives_a_hup_while_draining() {
        use std::io::Write;

        let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 20, 16_384)));
        let (mut connection, mut peer) = test_h2_connection(&pool, None);
        let mut context = test_context(&pool);
        let mut router = Router::new(Duration::from_secs(30), Duration::from_secs(30));

        connection.state = H2State::Header;
        connection
            .stream_table
            .set_expect_read(Some((H2StreamId::Zero, 9)));

        let mut peer_encoder = loona_hpack::Encoder::new();
        let field_block = peer_encoder.encode([
            (&b":method"[..], &b"GET"[..]),
            (&b":scheme"[..], &b"https"[..]),
            (&b":authority"[..], &b"example.com"[..]),
            (&b":path"[..], &b"/tcp-segmented"[..]),
            (&b"x-sozu-marker"[..], &b"legitimate-value"[..]),
        ]);
        assert!(
            field_block.len() > 40,
            "the probe block must be large enough to split meaningfully"
        );
        let split = field_block.len() / 2;
        let (first_half, continuation_payload) = field_block.split_at(split);
        assert!(
            continuation_payload.len() > 10,
            "the CONTINUATION frame's own payload must be large enough to \
             split into two TCP segments"
        );
        let payload_split = continuation_payload.len() / 2;
        let (payload_segment_1, payload_segment_2) = continuation_payload.split_at(payload_split);

        // HEADERS, END_STREAM but NOT END_HEADERS, stream 1.
        let mut headers_frame = Vec::with_capacity(9 + first_half.len());
        headers_frame.extend_from_slice(&(first_half.len() as u32).to_be_bytes()[1..]);
        headers_frame.push(1); // HEADERS
        headers_frame.push(parser::FLAG_END_STREAM);
        headers_frame.extend_from_slice(&1u32.to_be_bytes());
        headers_frame.extend_from_slice(first_half);
        peer.write_all(&headers_frame)
            .expect("loopback write must complete");
        peer.flush().expect("loopback flush must complete");

        for _ in 0..64 {
            connection.readable(&mut context, EndpointClient(&mut router));
            if matches!(connection.state, H2State::ContinuationHeader(_)) {
                break;
            }
            std::thread::yield_now();
        }
        assert!(
            matches!(connection.state, H2State::ContinuationHeader(_)),
            "stream 1's HEADERS frame must be accepted and await CONTINUATION: {:?}",
            connection.state
        );

        // The CONTINUATION frame header, announcing the FULL payload
        // length, followed by only its FIRST TCP segment.
        let mut continuation_header_and_segment_1 = Vec::with_capacity(9 + payload_segment_1.len());
        continuation_header_and_segment_1
            .extend_from_slice(&(continuation_payload.len() as u32).to_be_bytes()[1..]);
        continuation_header_and_segment_1.push(9); // CONTINUATION
        continuation_header_and_segment_1.push(parser::FLAG_END_HEADERS);
        continuation_header_and_segment_1.extend_from_slice(&1u32.to_be_bytes());
        continuation_header_and_segment_1.extend_from_slice(payload_segment_1);
        peer.write_all(&continuation_header_and_segment_1)
            .expect("loopback write must complete");
        peer.flush().expect("loopback flush must complete");

        // Drive reads until the connection has consumed exactly the first
        // segment and is waiting on the rest of THIS SAME CONTINUATION
        // frame's payload — a genuine partial `socket_read()`, proven by
        // `expect_read` still wanting the remainder.
        for _ in 0..64 {
            connection.readable(&mut context, EndpointClient(&mut router));
            if matches!(connection.state, H2State::ContinuationFrame(_))
                && matches!(
                    connection.stream_table.expect_read(),
                    Some((H2StreamId::Zero, remaining)) if remaining == payload_segment_2.len()
                )
            {
                break;
            }
            std::thread::yield_now();
        }
        assert!(
            matches!(connection.state, H2State::ContinuationFrame(_)),
            "the CONTINUATION frame must be mid-payload, waiting on its \
             second TCP segment: {:?}",
            connection.state
        );
        assert_eq!(
            connection.stream_table.expect_read(),
            Some((H2StreamId::Zero, payload_segment_2.len())),
            "expect_read must want exactly the remaining bytes of this \
             frame's own payload — proof this is a genuine partial read, \
             not an inter-frame gap"
        );

        // Exactly B2's trigger: the connection is draining AND the frontend
        // readiness reports HUP, landing squarely mid-payload-read — the
        // shape a TCP half-close produces (mio's `is_read_closed()` can be
        // true with data the peer already sent still unread).
        connection.drain.__test_set_draining();
        connection.readiness.event.insert(Ready::HUP);
        connection.writable(&mut context, EndpointClient(&mut router));
        connection.readiness.event.remove(Ready::HUP);

        // Now the second TCP segment, completing the CONTINUATION frame's
        // payload, arrives.
        peer.write_all(payload_segment_2)
            .expect("loopback write must complete");
        peer.flush().expect("loopback flush must complete");

        for _ in 0..64 {
            connection.readable(&mut context, EndpointClient(&mut router));
            if !matches!(
                connection.state,
                H2State::ContinuationHeader(_) | H2State::ContinuationFrame(_)
            ) {
                break;
            }
            std::thread::yield_now();
        }

        assert!(
            !matches!(connection.state, H2State::Error | H2State::GoAway),
            "a HUP-while-draining event landing mid-TCP-segment must not \
             corrupt a legitimate in-progress header block into a \
             connection error: {:?}",
            connection.state
        );

        let next_block = peer_encoder.encode([(&b"x-sozu-marker"[..], &b"legitimate-value"[..])]);
        assert_eq!(
            next_block.len(),
            1,
            "the peer must now reference its dynamic entry, not re-send a literal"
        );

        let mut decoded = Vec::new();
        let status = connection
            .hpack
            .decoder_mut()
            .decode_with_cb(&next_block, |k, v| {
                decoded.push((k.into_owned(), v.into_owned()));
            });

        assert!(
            status.is_ok(),
            "the connection decoder must still resolve the peer's dynamic \
             table after a HUP-while-draining event landing mid-TCP-segment, \
             got {status:?}"
        );
        assert_eq!(
            decoded,
            vec![(b"x-sozu-marker".to_vec(), b"legitimate-value".to_vec())],
            "stream 1's field block must have been decoded byte-for-byte, \
             not clobbered by a HUP-while-draining event landing between \
             two TCP segments of the same CONTINUATION frame"
        );
    }

    // ── The HPACK encoder across one multi-stream write pass ────────────
    //
    // `converter.rs`'s own unit tests drive ONE `H2BlockConverter` over ONE
    // kawa. Nothing there can see a property that only exists across the
    // streams of a single `write_streams` pass, and those are precisely the
    // two the per-`kawa.prepare` converter scoping has to preserve: the
    // RFC 7541 §6.3 size-update prefix belongs to the first header block of
    // the pass and to no other, and every block of the pass is encoded
    // against the SAME encoder dynamic table.

    /// Split a captured H2 byte stream into its HEADERS frame payloads,
    /// each paired with the stream it belongs to, in wire order. DATA and
    /// control frames are skipped; a truncated capture is a test bug and
    /// asserts rather than silently returning a short list.
    fn headers_blocks(wire: &[u8]) -> Vec<(StreamId, Vec<u8>)> {
        let mut blocks = Vec::new();
        let mut offset = 0usize;
        while offset + parser::FRAME_HEADER_SIZE <= wire.len() {
            let payload_len = ((wire[offset] as usize) << 16)
                | ((wire[offset + 1] as usize) << 8)
                | wire[offset + 2] as usize;
            let frame_type = wire[offset + 3];
            let stream_id = u32::from_be_bytes([
                wire[offset + 5],
                wire[offset + 6],
                wire[offset + 7],
                wire[offset + 8],
            ]) & 0x7fff_ffff;
            let start = offset + parser::FRAME_HEADER_SIZE;
            let end = start + payload_len;
            assert!(
                end <= wire.len(),
                "captured frame must be complete: {payload_len} payload bytes \
                 declared at offset {offset} of a {} byte capture",
                wire.len()
            );
            if frame_type == 1 {
                blocks.push((stream_id, wire[start..end].to_vec()));
            }
            offset = end;
        }
        blocks
    }

    /// `true` when `block` opens with an RFC 7541 §6.3 dynamic-table-size
    /// update. The representation prefixes partition the first byte with no
    /// overlap — `1xxxxxxx` indexed, `01xxxxxx` literal-with-indexing,
    /// `0001xxxx` literal-never-indexed, `0000xxxx` literal-without-indexing
    /// — so the `001xxxxx` pattern identifies the update on its own,
    /// whatever integer follows it.
    fn starts_with_size_update(block: &[u8]) -> bool {
        block.first().is_some_and(|b| b & 0xE0 == 0x20)
    }

    /// Drive exactly ONE `write_streams` pass that emits a response header
    /// block on every `stream_ids` entry, and return those blocks in the
    /// order they reached the wire together with whatever size-update the
    /// connection still has queued afterwards.
    ///
    /// Each stream is opened by a real HEADERS frame from the peer and then
    /// filled with sozu's own 404 default answer through
    /// `answers::set_default_answer`, the same chokepoint the routing layer
    /// uses — so the blocks captured here are the ones production encodes.
    ///
    /// `pending_table_size_update` is installed the way `handle_settings_frame`
    /// installs it: the encoder cap and the queued signal move together.
    fn drive_one_write_pass(
        stream_ids: &[StreamId],
        pending_table_size_update: Option<u32>,
    ) -> (Vec<(StreamId, Vec<u8>)>, Option<u32>) {
        use std::io::{Read, Write};

        let pool = Rc::new(RefCell::new(Pool::with_capacity(8, 40, 16_384)));
        let (mut connection, mut peer) = test_h2_connection(&pool, None);
        let mut context = test_context(&pool);
        let mut router = Router::new(Duration::from_secs(30), Duration::from_secs(30));

        connection.state = H2State::Header;
        connection
            .stream_table
            .set_expect_read(Some((H2StreamId::Zero, 9)));

        let mut peer_encoder = loona_hpack::Encoder::new();
        for &stream_id in stream_ids {
            let field_block = peer_encoder.encode([
                (&b":method"[..], &b"GET"[..]),
                (&b":scheme"[..], &b"https"[..]),
                (&b":authority"[..], &b"example.com"[..]),
                (&b":path"[..], &b"/write-pass"[..]),
            ]);
            let mut frame = Vec::with_capacity(parser::FRAME_HEADER_SIZE + field_block.len());
            frame.extend_from_slice(&(field_block.len() as u32).to_be_bytes()[1..]);
            frame.push(1); // HEADERS
            frame.push(parser::FLAG_END_STREAM | parser::FLAG_END_HEADERS);
            frame.extend_from_slice(&stream_id.to_be_bytes());
            frame.extend_from_slice(&field_block);
            peer.write_all(&frame)
                .expect("loopback write must complete");
            peer.flush().expect("loopback flush must complete");
            for _ in 0..64 {
                connection.readable(&mut context, EndpointClient(&mut router));
                if connection.stream_table.streams().contains_key(&stream_id) {
                    break;
                }
                std::thread::yield_now();
            }
            assert!(
                connection.stream_table.streams().contains_key(&stream_id),
                "stream {stream_id} must be accepted before the write pass"
            );
        }

        let answers = context.listener.borrow().get_answers().clone();
        let open: Vec<GlobalStreamId> = connection
            .stream_table
            .streams()
            .values()
            .copied()
            .collect();
        for global_stream_id in open {
            crate::protocol::mux::answers::set_default_answer(
                &mut context.streams[global_stream_id],
                &mut connection.readiness,
                404,
                &answers.borrow(),
            );
        }

        // Discard whatever the handshake already put on the wire, so the
        // capture below holds this write pass and nothing else.
        let mut discard = Vec::new();
        let _ = peer.read_to_end(&mut discard);

        if let Some(size) = pending_table_size_update {
            connection.hpack.set_encoder_max_table_size(size as usize);
        }
        connection.pending_table_size_update = pending_table_size_update;
        connection.writable(&mut context, EndpointClient(&mut router));

        let mut wire = Vec::new();
        let _ = peer.read_to_end(&mut wire);
        (headers_blocks(&wire), connection.pending_table_size_update)
    }

    /// Two properties of a `write_streams` pass, both invisible to any
    /// single-stream test, pinned on the bytes two streams actually receive:
    ///
    /// 1. the RFC 7541 §6.3 dynamic-table-size-update prefix is written to
    ///    the FIRST header block of the pass and to no other, and the
    ///    connection's mirror is cleared only because a block carried it;
    /// 2. both blocks are encoded against ONE encoder dynamic table, so a
    ///    peer replaying them in order with a single decoder stays in sync —
    ///    and a decoder that never saw the first block CANNOT read the
    ///    second, which is the negative half that makes assertion 2
    ///    discriminating rather than vacuous.
    ///
    /// Scoping the `H2BlockConverter` to a single `kawa.prepare` call is
    /// what puts both at risk: the converter no longer spans the per-stream
    /// loop, so the signal and the encoder now cross from one stream to the
    /// next through [`converter::H2ConverterPass`] rather than through one
    /// long-lived struct.
    ///
    /// TO SEE THIS RED, break either half:
    /// (a) delete `self.pending_table_size_update = converter.pending_table_size_update;`
    ///     from `converter::H2ConverterPass::reclaim` — every block of the
    ///     pass then re-emits the prefix, and the second assertion fails with
    ///     `only the FIRST header block of a pass carries the size update;
    ///     stream 3 opened with 0x3f`;
    /// (b) pass a fresh `loona_hpack::Encoder::new()` to `pass.converter(..)`
    ///     in `write_streams` instead of `self.hpack.encoder_mut()` — every
    ///     block becomes self-contained and the LAST assertion fails with
    ///     `the second block must depend on the dynamic table the first one
    ///     built`.
    #[test]
    fn one_write_pass_prefixes_its_first_header_block_only_and_shares_one_encoder() {
        // 4096 is the HPACK default table size, so `[0x3f, 0xe1, 0x1f]` below
        // is the canonical §6.3 encoding: prefix bits `001`, a 5-bit prefix
        // integer saturated to 31, then 4096 - 31 = 4065 as two continuation
        // octets (0x61 | 0x80, 0x1f). Written out rather than produced with
        // the same encoder the production path uses, so the test cannot
        // agree with a broken encoder.
        const TABLE_SIZE: u32 = 4096;
        const SIZE_UPDATE_PREFIX: [u8; 3] = [0x3f, 0xe1, 0x1f];

        let (blocks, residual) = drive_one_write_pass(&[1, 3], Some(TABLE_SIZE));

        assert_eq!(
            blocks.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![1, 3],
            "one pass must emit one header block per open stream, in the \
             priority order (equal urgency, ascending stream id)"
        );

        assert_eq!(
            &blocks[0].1[..SIZE_UPDATE_PREFIX.len()],
            &SIZE_UPDATE_PREFIX,
            "the first header block of the pass must open with the \
             dynamic-table-size-update for {TABLE_SIZE}"
        );
        assert!(
            !starts_with_size_update(&blocks[1].1),
            "only the FIRST header block of a pass carries the size update; \
             stream {} opened with {:#04x}",
            blocks[1].0,
            blocks[1].1[0]
        );
        assert_eq!(
            residual, None,
            "the connection clears its mirror once a block carried the signal"
        );

        // Positive half: the peer's single decoder replays the pass in order.
        let mut peer_decoder = loona_hpack::Decoder::new();
        peer_decoder.set_max_allowed_table_size(TABLE_SIZE as usize);
        for (stream_id, block) in &blocks {
            let mut decoded = Vec::new();
            let status = peer_decoder.decode_with_cb(block, |key, value| {
                decoded.push((key.into_owned(), value.into_owned()));
            });
            assert!(
                status.is_ok(),
                "a peer replaying the pass in order must decode stream \
                 {stream_id}'s block: {status:?}"
            );
            assert!(
                decoded.contains(&(b":status".to_vec(), b"404".to_vec())),
                "stream {stream_id}'s block must carry the 404 status line, \
                 got {decoded:?}"
            );
        }

        // Negative half: the second block is not self-contained. It indexes
        // dynamic-table entries only the first block created, so a decoder
        // that never saw the first block cannot read it. Without this, a
        // per-stream encoder would satisfy every assertion above.
        let mut fresh_decoder = loona_hpack::Decoder::new();
        fresh_decoder.set_max_allowed_table_size(TABLE_SIZE as usize);
        let status = fresh_decoder.decode_with_cb(&blocks[1].1, |_, _| {});
        assert!(
            status.is_err(),
            "the second block must depend on the dynamic table the first one \
             built — a decoder that never saw the first block must fail on \
             it, otherwise the two blocks came from two encoders"
        );
    }

    // ── Property coverage: the HPACK encoder across a multi-stream pass ──
    //
    // The deterministic test above fixes ONE shape: two streams, the HPACK
    // default table size, the size update landing on the first of exactly
    // two blocks. This property generalises the stream count (2..=5) and the
    // advertised table size, which decides both how many octets the §6.3
    // prefix occupies and how much dynamic table the later blocks can index.
    //
    // It is a sibling of `reassembly_property` below rather than a case
    // inside it: that one drives the READ path and its oracle is "one
    // decoder stays in sync across a fragmented header block", while this
    // one drives the WRITE path and its oracle is "one encoder, one prefix,
    // N blocks a single peer decoder replays". One `quickcheck` verdict over
    // two unrelated state machines would say nothing about either.
    //
    // The negative half of the deterministic test — a fresh decoder must
    // FAIL on a later block — is deliberately left out here: a generated
    // table size small enough to evict the dynamic table makes every block
    // self-contained, which is correct behaviour and would fail that
    // assertion.
    mod write_pass_property {
        use quickcheck::{Arbitrary, Gen, TestResult, quickcheck};

        use super::*;

        /// One generated write pass: how many streams share it, and the
        /// `SETTINGS_HEADER_TABLE_SIZE` the peer advertised just before it.
        #[derive(Debug, Clone, Copy)]
        struct WritePassPlan {
            streams: u8,
            table_size: u16,
        }

        impl Arbitrary for WritePassPlan {
            fn arbitrary(g: &mut Gen) -> Self {
                WritePassPlan {
                    streams: *g.choose(&[2u8, 3, 4, 5]).expect("non-empty slice"),
                    // Bounded by the HPACK default a fresh `Decoder`
                    // accepts, mirrored onto the peer decoder below; a
                    // larger advertisement is a SETTINGS-path concern, not
                    // a write-pass one.
                    table_size: u16::arbitrary(g) % 4097,
                }
            }
        }

        fn drive(plan: WritePassPlan) -> TestResult {
            let stream_ids: Vec<StreamId> =
                (0..u32::from(plan.streams)).map(|i| 1 + 2 * i).collect();
            let (blocks, residual) =
                drive_one_write_pass(&stream_ids, Some(u32::from(plan.table_size)));

            if blocks
                .iter()
                .map(|(id, _)| *id)
                .ne(stream_ids.iter().copied())
            {
                return TestResult::error(format!(
                    "plan {plan:?}: expected one block per stream in priority \
                     order {stream_ids:?}, got {:?}",
                    blocks.iter().map(|(id, _)| *id).collect::<Vec<_>>()
                ));
            }
            if !starts_with_size_update(&blocks[0].1) {
                return TestResult::error(format!(
                    "plan {plan:?}: the first block must open with the \
                     size update, got {:#04x}",
                    blocks[0].1[0]
                ));
            }
            for (stream_id, block) in blocks.iter().skip(1) {
                if starts_with_size_update(block) {
                    return TestResult::error(format!(
                        "plan {plan:?}: stream {stream_id} re-emitted the \
                         size update the first block already carried"
                    ));
                }
            }
            if residual.is_some() {
                return TestResult::error(format!(
                    "plan {plan:?}: the mirror must be cleared once a block \
                     carried the signal, still {residual:?}"
                ));
            }

            let mut peer_decoder = loona_hpack::Decoder::new();
            peer_decoder.set_max_allowed_table_size(usize::from(plan.table_size));
            for (stream_id, block) in &blocks {
                let mut decoded = Vec::new();
                let status = peer_decoder.decode_with_cb(block, |key, value| {
                    decoded.push((key.into_owned(), value.into_owned()));
                });
                if status.is_err() {
                    return TestResult::error(format!(
                        "plan {plan:?}: a peer replaying the pass in order \
                         desynced on stream {stream_id}: {status:?}"
                    ));
                }
                if !decoded.contains(&(b":status".to_vec(), b"404".to_vec())) {
                    return TestResult::error(format!(
                        "plan {plan:?}: stream {stream_id} decoded to \
                         {decoded:?}, expected the 404 status line"
                    ));
                }
            }
            TestResult::passed()
        }

        quickcheck! {
            fn qc_one_write_pass_prefixes_its_first_header_block_only(plan: WritePassPlan) -> TestResult {
                drive(plan)
            }
        }
    }

    // ── Property coverage: interleaved reassembly + control-frame flushes ──
    //
    // The four deterministic pinning tests above each fix ONE interleaving
    // (an unrelated WINDOW_UPDATE flush, a graceful_goaway, a HUP-while-
    // draining event between frames, a HUP-while-draining event mid-TCP-
    // segment) at ONE point. This property generalises them: a
    // `quickcheck`-driven `ReassemblyPlan` splits a real HPACK field block
    // into 2..=5 CONTINUATION fragments and independently chooses, before
    // each one, whether to interleave nothing, an unrelated WINDOW_UPDATE
    // flush, a `graceful_goaway`, or a HUP-while-draining event — the same
    // four triggers, now composed in every order and count `quickcheck`
    // cares to generate — and, independently again, whether that interleave
    // lands BETWEEN two frames or in the MIDDLE of one frame's own TCP-level
    // write (review of `e1c3c2fb`, B2/B3: the two shapes are not
    // interchangeable — `zero.storage` is empty in the former and holds a
    // genuine partial `socket_read()` in the latter, and only the latter
    // reaches the narrower guard the four deterministic tests individually
    // pin). `quickcheck` is already a workspace dependency (see
    // `lib/src/router/mod.rs`'s `qc_router_hostname_resolution_matches_
    // the_documented_semantics`, whose `Arbitrary`/`quickcheck!` shape this
    // mirrors).
    //
    // Any out-of-bounds read or other panic inside `drive()` fails the
    // `quickcheck` run directly — there is no separate assertion for it.
    // CONTINUATION-flood abort and the CVE-2024-27316 refusal path are
    // deliberately NOT folded into this property: they end the block in a
    // *different* terminal state (refused, not decoded), which would split
    // this property's single pass/fail oracle in two. That path already has
    // dedicated deterministic coverage —
    // `a_refused_stream_keeps_the_hpack_decoder_in_sync` and
    // `a_refused_padded_prioritized_stream_keeps_the_hpack_decoder_in_sync`
    // above.
    mod reassembly_property {
        use quickcheck::{Arbitrary, Gen, TestResult, quickcheck};

        use super::*;

        /// One interleaved event the property driver may inject before a
        /// CONTINUATION fragment. Mirrors the four deterministic pinning
        /// tests' triggers.
        #[derive(Debug, Clone, Copy)]
        enum Interleave {
            None,
            UnrelatedWindowUpdateFlush,
            HupWhileDraining,
            GracefulGoawayDefer,
        }

        impl Arbitrary for Interleave {
            fn arbitrary(g: &mut Gen) -> Self {
                *g.choose(&[
                    Interleave::None,
                    Interleave::UnrelatedWindowUpdateFlush,
                    Interleave::HupWhileDraining,
                    Interleave::GracefulGoawayDefer,
                ])
                .expect("the choice slice above is non-empty")
            }
        }

        /// One interleave point, paired with whether it lands between two
        /// frames (`mid_frame == false`, the original shape) or in the
        /// middle of the following fragment's own TCP-level write
        /// (`mid_frame == true` — a genuine partial `socket_read()`, the
        /// shape B2 proved is NOT interchangeable with the former).
        #[derive(Debug, Clone, Copy)]
        struct InterleavePoint {
            interleave: Interleave,
            mid_frame: bool,
        }

        impl Arbitrary for InterleavePoint {
            fn arbitrary(g: &mut Gen) -> Self {
                InterleavePoint {
                    interleave: Interleave::arbitrary(g),
                    mid_frame: bool::arbitrary(g),
                }
            }
        }

        /// How many CONTINUATION frames to split the field block into
        /// (2..=5) and which [`InterleavePoint`] to inject before each one
        /// after the first — exactly `splits - 1` points, one per fragment
        /// boundary, never `splits` (there is no point "before the first
        /// fragment": the HEADERS frame that carries it is what starts the
        /// reassembly).
        #[derive(Debug, Clone)]
        struct ReassemblyPlan {
            splits: u8,
            points: Vec<InterleavePoint>,
        }

        impl Arbitrary for ReassemblyPlan {
            fn arbitrary(g: &mut Gen) -> Self {
                let splits = 2 + (u8::arbitrary(g) % 4); // 2..=5
                let points = (0..splits - 1)
                    .map(|_| InterleavePoint::arbitrary(g))
                    .collect();
                ReassemblyPlan { splits, points }
            }
        }

        /// Apply one interleave. `GracefulGoawayDefer` checks
        /// `connection.drain.draining()` — the AUTHORITATIVE state, not a
        /// separately tracked bool — before calling `graceful_goaway`:
        /// once the connection is already draining (whether a prior point
        /// in this same plan called `graceful_goaway` itself, OR a prior
        /// `HupWhileDraining` point set it via the test-only
        /// `__test_set_draining()` backdoor), a SECOND `graceful_goaway`
        /// call would be the RFC 9113 §6.8 FINAL GOAWAY
        /// (`GracefulDrainDecision::AlreadyDraining`), which drops
        /// `expect_read` and makes the block permanently uncompletable — a
        /// different scenario than this property drives, so a
        /// `GracefulGoawayDefer` point reached while already draining is
        /// treated as `None` instead. A local `bool` tracking only
        /// "did GracefulGoawayDefer itself fire" would miss the
        /// `HupWhileDraining` case and was exactly the bug `quickcheck`
        /// found while writing this property (review of `e1c3c2fb`, B3).
        fn apply_interleave<L>(
            connection: &mut ConnectionH2<mio::net::TcpStream>,
            context: &mut Context<L>,
            router: &mut Router,
            interleave: Interleave,
        ) where
            L: ListenerHandler + L7ListenerHandler,
        {
            match interleave {
                Interleave::None => {}
                Interleave::UnrelatedWindowUpdateFlush => {
                    connection.queue_window_update(0, 65_535);
                    connection.writable(context, EndpointClient(router));
                }
                Interleave::HupWhileDraining => {
                    connection.drain.__test_set_draining();
                    connection.readiness.event.insert(Ready::HUP);
                    connection.writable(context, EndpointClient(router));
                    connection.readiness.event.remove(Ready::HUP);
                }
                Interleave::GracefulGoawayDefer => {
                    if !connection.drain.draining() {
                        let now = connection.now;
                        connection.graceful_goaway(now);
                    }
                }
            }
        }

        /// Build the connection, split a real field block into
        /// `plan.splits` fragments, and send them as HEADERS + N-1
        /// CONTINUATION frames, injecting `plan.points[i - 1]` before
        /// fragment `i` (`i >= 1`) — either between the two frames, or
        /// split across two TCP-level writes of that fragment's OWN
        /// payload with the interleave firing in between. Asserts the
        /// connection stays usable and the block decodes byte-for-byte
        /// regardless of which interleaves fired or where.
        fn drive(plan: ReassemblyPlan) -> TestResult {
            use std::io::Write;

            let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 20, 16_384)));
            let (mut connection, mut peer) = test_h2_connection(&pool, None);
            let mut context = test_context(&pool);
            let mut router = Router::new(Duration::from_secs(30), Duration::from_secs(30));

            connection.state = H2State::Header;
            connection
                .stream_table
                .set_expect_read(Some((H2StreamId::Zero, 9)));

            let mut peer_encoder = loona_hpack::Encoder::new();
            let field_block = peer_encoder.encode([
                (&b":method"[..], &b"GET"[..]),
                (&b":scheme"[..], &b"https"[..]),
                (&b":authority"[..], &b"example.com"[..]),
                (&b":path"[..], &b"/qc-multiframe"[..]),
                (&b"x-sozu-qc-marker"[..], &b"qc-value"[..]),
            ]);
            let splits = plan.splits as usize;
            if field_block.len() < splits * 2 {
                // Too few bytes to split `splits` ways meaningfully.
                return TestResult::discard();
            }

            let chunk = (field_block.len() / splits).max(1);
            let mut fragments: Vec<&[u8]> = Vec::with_capacity(splits);
            let mut rest = &field_block[..];
            for i in 0..splits {
                if i + 1 == splits {
                    fragments.push(rest);
                } else {
                    let (a, b) = rest.split_at(chunk);
                    fragments.push(a);
                    rest = b;
                }
            }
            if fragments.iter().any(|f| f.is_empty()) {
                return TestResult::discard();
            }

            let first = fragments[0];
            let mut frame = Vec::with_capacity(9 + first.len());
            frame.extend_from_slice(&(first.len() as u32).to_be_bytes()[1..]);
            frame.push(1); // HEADERS
            frame.push(parser::FLAG_END_STREAM);
            frame.extend_from_slice(&1u32.to_be_bytes());
            frame.extend_from_slice(first);
            if peer.write_all(&frame).is_err() || peer.flush().is_err() {
                return TestResult::discard();
            }

            for _ in 0..64 {
                connection.readable(&mut context, EndpointClient(&mut router));
                if matches!(connection.state, H2State::ContinuationHeader(_)) {
                    break;
                }
                std::thread::yield_now();
            }
            if !matches!(connection.state, H2State::ContinuationHeader(_)) {
                return TestResult::discard();
            }

            for (idx, fragment) in fragments.iter().enumerate().skip(1) {
                let point = plan.points[idx - 1];
                let is_last = idx + 1 == fragments.len();
                let end_headers_flag = if is_last { parser::FLAG_END_HEADERS } else { 0 };

                if point.mid_frame && fragment.len() >= 2 {
                    // Split THIS fragment's own payload into two TCP-level
                    // writes, firing the interleave in between — a genuine
                    // partial `socket_read()`, not a gap between frames.
                    let mid = fragment.len() / 2;
                    let (segment_1, segment_2) = fragment.split_at(mid);

                    let mut header_and_segment_1 = Vec::with_capacity(9 + segment_1.len());
                    header_and_segment_1
                        .extend_from_slice(&(fragment.len() as u32).to_be_bytes()[1..]);
                    header_and_segment_1.push(9); // CONTINUATION
                    header_and_segment_1.push(end_headers_flag);
                    header_and_segment_1.extend_from_slice(&1u32.to_be_bytes());
                    header_and_segment_1.extend_from_slice(segment_1);
                    if peer.write_all(&header_and_segment_1).is_err() || peer.flush().is_err() {
                        return TestResult::discard();
                    }

                    for _ in 0..64 {
                        connection.readable(&mut context, EndpointClient(&mut router));
                        if matches!(connection.state, H2State::ContinuationFrame(_))
                            && matches!(
                                connection.stream_table.expect_read(),
                                Some((H2StreamId::Zero, remaining)) if remaining == segment_2.len()
                            )
                        {
                            break;
                        }
                        std::thread::yield_now();
                    }
                    if !matches!(connection.state, H2State::ContinuationFrame(_)) {
                        // The segment split did not land as a genuine
                        // partial read (e.g. too small to observe) — treat
                        // as a discard rather than a false failure.
                        return TestResult::discard();
                    }

                    apply_interleave(&mut connection, &mut context, &mut router, point.interleave);

                    if peer.write_all(segment_2).is_err() || peer.flush().is_err() {
                        return TestResult::discard();
                    }
                } else {
                    apply_interleave(&mut connection, &mut context, &mut router, point.interleave);

                    let mut cframe = Vec::with_capacity(9 + fragment.len());
                    cframe.extend_from_slice(&(fragment.len() as u32).to_be_bytes()[1..]);
                    cframe.push(9); // CONTINUATION
                    cframe.push(end_headers_flag);
                    cframe.extend_from_slice(&1u32.to_be_bytes());
                    cframe.extend_from_slice(fragment);
                    if peer.write_all(&cframe).is_err() || peer.flush().is_err() {
                        return TestResult::discard();
                    }
                }

                for _ in 0..64 {
                    connection.readable(&mut context, EndpointClient(&mut router));
                    if !matches!(
                        connection.state,
                        H2State::ContinuationHeader(_) | H2State::ContinuationFrame(_)
                    ) {
                        break;
                    }
                    std::thread::yield_now();
                }
            }

            if matches!(connection.state, H2State::Error | H2State::GoAway) {
                return TestResult::error(format!(
                    "connection reached {:?} while reassembling under plan {plan:?}",
                    connection.state
                ));
            }

            let next_block = peer_encoder.encode([(&b"x-sozu-qc-marker"[..], &b"qc-value"[..])]);
            let mut decoded = Vec::new();
            let status = connection
                .hpack
                .decoder_mut()
                .decode_with_cb(&next_block, |k, v| {
                    decoded.push((k.into_owned(), v.into_owned()));
                });
            if status.is_err() {
                return TestResult::error(format!(
                    "decoder desynced after plan {plan:?}: {status:?}"
                ));
            }
            if decoded != vec![(b"x-sozu-qc-marker".to_vec(), b"qc-value".to_vec())] {
                return TestResult::error(format!(
                    "decoded {decoded:?} after plan {plan:?}, expected the qc marker header"
                ));
            }

            TestResult::passed()
        }

        quickcheck! {
            fn qc_h2_header_reassembly_survives_interleaved_control_frame_flushes(plan: ReassemblyPlan) -> TestResult {
                drive(plan)
            }
        }
    }
}
