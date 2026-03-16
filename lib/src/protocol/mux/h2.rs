use std::{
    cmp::min,
    collections::{HashMap, HashSet},
    time::Instant,
};

/// Compile-time guard: `payload_len as usize` casts in the H2 parser assume at
/// least 32-bit pointer width.  This prevents silent truncation on platforms
/// with smaller pointers (e.g. 16-bit embedded targets).
const _: () = assert!(
    std::mem::size_of::<usize>() >= 4,
    "sozu requires at least 32-bit pointers"
);

use rusty_ulid::Ulid;
use sozu_command::ready::Ready;

use crate::{
    L7ListenerHandler, ListenerHandler, Protocol, Readiness, SessionMetrics,
    protocol::mux::{
        BackendStatus, Context, DebugEvent, DebugHistory, Endpoint, GenericHttpStream,
        GlobalStreamId, MuxResult, Position, StreamId, StreamState, converter,
        forcefully_terminate_answer,
        parser::{
            self, Frame, FrameHeader, FrameType, H2Error, Headers, ParserError, ParserErrorKind,
            WindowUpdate,
        },
        pkawa, serializer, set_default_answer, update_readiness_after_read,
        update_readiness_after_write,
    },
    socket::SocketHandler,
    timer::TimeoutContainer,
};

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
/// Default maximum PING frames per window (CVE-2019-9512 Ping Flood)
const DEFAULT_MAX_PING_PER_WINDOW: u32 = 100;
/// Default maximum SETTINGS frames per window (CVE-2019-9515 Settings Flood)
const DEFAULT_MAX_SETTINGS_PER_WINDOW: u32 = 50;
/// Default maximum empty DATA frames per window (CVE-2019-9518 Empty Frames)
const DEFAULT_MAX_EMPTY_DATA_PER_WINDOW: u32 = 100;
/// Default maximum CONTINUATION frames per header block (CVE-2024-27316)
const DEFAULT_MAX_CONTINUATION_FRAMES: u32 = 20;
/// Maximum accumulated header block size across CONTINUATION frames (64KB)
pub(super) const MAX_HEADER_LIST_SIZE: u32 = 65536;
/// Duration of the sliding window for rate-based flood counters
const FLOOD_WINDOW_DURATION: std::time::Duration = std::time::Duration::from_secs(1);
/// Default maximum general anomaly count before triggering ENHANCE_YOUR_CALM
const DEFAULT_MAX_GLITCH_COUNT: u32 = 100;

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
    /// Maximum CONTINUATION frames per header block (CVE-2024-27316)
    pub max_continuation_frames: u32,
    /// Maximum accumulated protocol anomalies before ENHANCE_YOUR_CALM
    pub max_glitch_count: u32,
}

impl Default for H2FloodConfig {
    fn default() -> Self {
        Self {
            max_rst_stream_per_window: DEFAULT_MAX_RST_STREAM_PER_WINDOW,
            max_ping_per_window: DEFAULT_MAX_PING_PER_WINDOW,
            max_settings_per_window: DEFAULT_MAX_SETTINGS_PER_WINDOW,
            max_empty_data_per_window: DEFAULT_MAX_EMPTY_DATA_PER_WINDOW,
            max_continuation_frames: DEFAULT_MAX_CONTINUATION_FRAMES,
            max_glitch_count: DEFAULT_MAX_GLITCH_COUNT,
        }
    }
}

/// Maximum pending WINDOW_UPDATE entries before forcing a flush.
/// Sized to cover connection-level + per-stream entries with headroom under load.
const MAX_PENDING_WINDOW_UPDATES: usize = 1 + DEFAULT_MAX_CONCURRENT_STREAMS as usize * 4;

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
) {
    let share_in = if total_bytes.0 > 0 {
        // Clamp to remaining overhead — integer division rounding across multiple
        // streams can cause accumulated shares to exceed the total.
        (*overhead_bin * stream_bytes.0 / total_bytes.0).min(*overhead_bin)
    } else {
        // No stream has transferred any inbound bytes — fall back to even split.
        *overhead_bin / active_streams.max(1)
    };
    let share_out = if total_bytes.1 > 0 {
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

/// Tracks per-connection frame rates to detect and mitigate H2 flood attacks.
///
/// Monitors RST_STREAM (CVE-2023-44487), PING (CVE-2019-9512), SETTINGS (CVE-2019-9515),
/// empty DATA (CVE-2019-9518), and CONTINUATION (CVE-2024-27316) flood patterns.
/// When any counter exceeds its threshold, `check_flood()` returns `EnhanceYourCalm`.
///
/// Thresholds are configurable via [`H2FloodConfig`], with safe defaults matching
/// the original compile-time constants.
#[derive(Debug)]
pub struct H2FloodDetector {
    /// RST_STREAM frames received in current window (CVE-2023-44487 + CVE-2019-9514)
    pub(super) rst_stream_count: u32,
    /// PING frames received in current window (CVE-2019-9512)
    pub(super) ping_count: u32,
    /// SETTINGS frames received in current window (CVE-2019-9515)
    pub(super) settings_count: u32,
    /// Empty DATA frames received in current window (CVE-2019-9518)
    pub(super) empty_data_count: u32,
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
            ping_count: 0,
            settings_count: 0,
            empty_data_count: 0,
            continuation_count: 0,
            accumulated_header_size: 0,
            glitch_count: 0,
            window_start: Instant::now(),
            config,
        }
    }

    /// Half-decay rate-based counters if the current window has expired.
    /// Uses half-window decay instead of full reset to catch burst-then-wait attacks.
    fn maybe_reset_window(&mut self) {
        if self.window_start.elapsed() >= FLOOD_WINDOW_DURATION {
            self.rst_stream_count /= 2;
            self.ping_count /= 2;
            self.settings_count /= 2;
            self.empty_data_count /= 2;
            self.glitch_count /= 2;
            self.window_start = Instant::now();
        }
    }

    /// Check all flood counters. Returns `Some(EnhanceYourCalm)` if any threshold is exceeded.
    pub fn check_flood(&mut self) -> Option<H2Error> {
        self.maybe_reset_window();

        if self.rst_stream_count > self.config.max_rst_stream_per_window {
            warn!(
                "H2 flood detected: RST_STREAM count {} exceeds threshold {}",
                self.rst_stream_count, self.config.max_rst_stream_per_window
            );
            return Some(H2Error::EnhanceYourCalm);
        }
        if self.ping_count > self.config.max_ping_per_window {
            warn!(
                "H2 flood detected: PING count {} exceeds threshold {}",
                self.ping_count, self.config.max_ping_per_window
            );
            return Some(H2Error::EnhanceYourCalm);
        }
        if self.settings_count > self.config.max_settings_per_window {
            warn!(
                "H2 flood detected: SETTINGS count {} exceeds threshold {}",
                self.settings_count, self.config.max_settings_per_window
            );
            return Some(H2Error::EnhanceYourCalm);
        }
        if self.empty_data_count > self.config.max_empty_data_per_window {
            warn!(
                "H2 flood detected: empty DATA count {} exceeds threshold {}",
                self.empty_data_count, self.config.max_empty_data_per_window
            );
            return Some(H2Error::EnhanceYourCalm);
        }
        if self.continuation_count > self.config.max_continuation_frames {
            warn!(
                "H2 flood detected: CONTINUATION count {} exceeds threshold {}",
                self.continuation_count, self.config.max_continuation_frames
            );
            return Some(H2Error::EnhanceYourCalm);
        }
        if self.accumulated_header_size > MAX_HEADER_LIST_SIZE {
            warn!(
                "H2 flood detected: accumulated header size {} exceeds threshold {}",
                self.accumulated_header_size, MAX_HEADER_LIST_SIZE
            );
            return Some(H2Error::EnhanceYourCalm);
        }
        if self.glitch_count > self.config.max_glitch_count {
            warn!(
                "H2 flood detected: glitch count {} exceeds threshold {}",
                self.glitch_count, self.config.max_glitch_count
            );
            return Some(H2Error::EnhanceYourCalm);
        }
        None
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

#[derive(Debug)]
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
            settings_max_header_list_size: MAX_HEADER_LIST_SIZE,
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
/// Streams without an explicit `priority` header get the RFC 9218 defaults:
/// urgency 3, incremental false.
#[derive(Default)]
pub struct Prioriser {
    /// Per-stream priority: stream_id -> (urgency 0-7, incremental flag)
    priorities: HashMap<StreamId, (u8, bool)>,
}

/// RFC 9218 §4 default urgency value.
const DEFAULT_URGENCY: u8 = 3;

/// Maximum entries in the priority map to prevent flooding via PRIORITY frames.
const MAX_PRIORITIES: usize = 4096;

impl Prioriser {
    /// Record or update the priority for a stream.
    ///
    /// Returns `true` if the priority is invalid (self-dependency for RFC 7540),
    /// signalling the caller should reset the stream with a protocol error.
    pub fn push_priority(&mut self, stream_id: StreamId, priority: parser::PriorityPart) -> bool {
        trace!("PRIORITY REQUEST FOR {}: {:?}", stream_id, priority);
        // Cap the priority map to prevent flooding via PRIORITY frames
        if !self.priorities.contains_key(&stream_id) && self.priorities.len() >= MAX_PRIORITIES {
            return false;
        }
        match priority {
            parser::PriorityPart::Rfc7540 {
                stream_dependency,
                weight: _,
            } => {
                if stream_dependency.stream_id == stream_id {
                    error!("STREAM CAN'T DEPEND ON ITSELF");
                    true
                } else {
                    // RFC 7540 tree-based priority is deprecated; log and ignore.
                    false
                }
            }
            parser::PriorityPart::Rfc9218 {
                urgency,
                incremental,
            } => {
                self.priorities
                    .insert(stream_id, (urgency.min(7), incremental));
                false
            }
        }
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
}

pub struct ConnectionH2<Front: SocketHandler> {
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
    /// Reusable buffer for HPACK-encoded headers in the H2 block converter.
    pub converter_buf: Vec<u8>,
    /// Reusable buffer for lowercasing header keys in the H2 block converter.
    pub lowercase_buf: Vec<u8>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum H2StreamId {
    Zero,
    Other(StreamId, GlobalStreamId),
}

impl<Front: SocketHandler> ConnectionH2<Front> {
    /// Shared constructor for both server and client H2 connections.
    ///
    /// Differences between server and client are captured by the caller-provided
    /// `position`, `expect_read`, and `readiness_interest` parameters.
    pub(super) fn new(
        socket: Front,
        position: super::Position,
        pool: std::rc::Weak<std::cell::RefCell<crate::pool::Pool>>,
        flood_config: H2FloodConfig,
        timeout_container: crate::timer::TimeoutContainer,
        expect_read: Option<(H2StreamId, usize)>,
        readiness_interest: sozu_command::ready::Ready,
    ) -> Option<Self> {
        let buffer = pool
            .upgrade()
            .and_then(|pool| pool.borrow_mut().checkout())?;
        let local_settings = H2Settings::default();
        let mut decoder = loona_hpack::Decoder::new();
        // RFC 7541 §4.2: enforce SETTINGS_HEADER_TABLE_SIZE as the upper bound
        // for dynamic table size updates from the peer
        decoder.set_max_allowed_table_size(local_settings.settings_header_table_size as usize);
        Some(ConnectionH2 {
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
            converter_buf: Vec::new(),
            lowercase_buf: Vec::new(),
            drain: H2DrainState {
                draining: false,
                peer_last_stream_id: None,
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
        })
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
        trace!("  header: {:?}", i);
        match parser::frame_header(i, self.local_settings.settings_max_frame_size) {
            Ok((_, header)) => {
                trace!("{:#?}", header);
                self.zero.storage.clear();
                let stream_id = header.stream_id;
                let read_stream = if stream_id == 0 {
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
                        "REQUESTING EXISTING STREAM {}: {}/{:?}",
                        stream_id, received_eos, stream.state
                    );
                    if !allowed_on_half_closed && (received_eos || !stream.state.is_open()) {
                        error!(
                            "CANNOT RECEIVE {:?} ON THIS STREAM {:?}",
                            header.frame_type, stream.state
                        );
                        return self.goaway(H2Error::StreamClosed);
                    }
                    if header.frame_type == FrameType::Data {
                        H2StreamId::Other(stream_id, *global_stream_id)
                    } else {
                        H2StreamId::Zero
                    }
                } else {
                    if header.frame_type == FrameType::Headers
                        && self.position.is_server()
                        && stream_id % 2 == 1
                        && stream_id >= self.last_stream_id
                    {
                        if self.streams.len()
                            >= self.local_settings.settings_max_concurrent_streams as usize
                        {
                            error!(
                                "MAX CONCURRENT STREAMS: limit={}, current={}",
                                self.local_settings.settings_max_concurrent_streams,
                                self.streams.len()
                            );
                            // RFC 9113 §5.1.2: refuse with RST_STREAM, not GOAWAY.
                            // Queue the RST_STREAM for the writable path to send
                            // (can't write to kawa.storage here because we also
                            // need to discard the HEADERS payload through it).
                            self.pending_rst_streams
                                .push((stream_id, H2Error::RefusedStream));
                            self.total_rst_streams_queued += 1;
                            self.readiness.interest.insert(Ready::WRITABLE);
                            // Discard the HEADERS payload before reading the
                            // next frame — otherwise the HPACK bytes would be
                            // misinterpreted as a frame header.
                            self.state = H2State::Discard;
                            self.expect_read =
                                Some((H2StreamId::Zero, header.payload_len as usize));
                            return MuxResult::Continue;
                        }
                        match self.create_stream(stream_id, context) {
                            Some(_) => {}
                            None => {
                                // Buffer pool exhaustion is transient — refuse
                                // this stream but keep the connection alive so
                                // existing streams can complete and free buffers.
                                error!(
                                    "Could not create stream {}: buffer pool exhausted",
                                    stream_id
                                );
                                self.pending_rst_streams
                                    .push((stream_id, H2Error::RefusedStream));
                                self.total_rst_streams_queued += 1;
                                self.readiness.interest.insert(Ready::WRITABLE);
                                // Discard the HEADERS payload (same as
                                // MAX_CONCURRENT_STREAMS path above).
                                self.state = H2State::Discard;
                                self.expect_read =
                                    Some((H2StreamId::Zero, header.payload_len as usize));
                                return MuxResult::Continue;
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
                                        "Ignoring {:?} on closed stream {}",
                                        header.frame_type, header.stream_id
                                    );
                                    self.flood_detector.glitch_count += 1;
                                    if let Some(error) = self.flood_detector.check_flood() {
                                        return self.goaway(error);
                                    }
                                }
                                FrameType::Data => {
                                    // RFC 9113 §5.1: DATA on a closed stream is a
                                    // stream error of type STREAM_CLOSED. Queue
                                    // RST_STREAM (not GOAWAY) to preserve the
                                    // connection for other streams. The payload is
                                    // still routed through stream 0 so handle_frame
                                    // can do connection-level flow control accounting.
                                    debug!(
                                        "DATA on closed stream {}, sending RST_STREAM(STREAM_CLOSED)",
                                        header.stream_id
                                    );
                                    self.flood_detector.glitch_count += 1;
                                    if let Some(error) = self.flood_detector.check_flood() {
                                        return self.goaway(error);
                                    }
                                    self.pending_rst_streams
                                        .push((header.stream_id, H2Error::StreamClosed));
                                    self.total_rst_streams_queued += 1;
                                    self.readiness.interest.insert(Ready::WRITABLE);
                                }
                                _ => {
                                    // RFC 9113 §5.1: HEADERS or other frames on a
                                    // closed stream → connection error STREAM_CLOSED.
                                    error!(
                                        "Received {:?} on closed stream {}, sending GOAWAY(STREAM_CLOSED)",
                                        header.frame_type, header.stream_id
                                    );
                                    return self.goaway(H2Error::StreamClosed);
                                }
                            }
                        } else {
                            error!(
                                "Received {:?} on idle stream {}, sending GOAWAY(PROTOCOL_ERROR)",
                                header.frame_type, header.stream_id
                            );
                            return self.goaway(H2Error::ProtocolError);
                        }
                    }
                    H2StreamId::Zero
                };
                trace!("{} {:?} {:#?}", header.stream_id, stream_id, self.streams);
                self.expect_read = Some((read_stream, header.payload_len as usize));
                self.state = H2State::Frame(header);
            }
            Err(nom::Err::Failure(ParserError {
                kind: ParserErrorKind::UnknownFrame(skip),
                ..
            })) => {
                self.expect_read = Some((H2StreamId::Zero, skip as usize));
                self.state = H2State::Discard;
            }
            Err(error) => {
                let error = error_nom_to_h2(error);
                error!("COULD NOT PARSE FRAME HEADER");
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
        trace!("  continuation header: {:?}", i);
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
                        "CONTINUATION header: storage.end ({}) too small to remove frame header",
                        self.zero.storage.end
                    );
                    return self.goaway(H2Error::InternalError);
                }
                self.zero.storage.end -= 9;
                if stream_id != headers.stream_id {
                    error!(
                        "CONTINUATION stream_id {} does not match HEADERS stream_id {}",
                        stream_id, headers.stream_id
                    );
                    return self.goaway(H2Error::ProtocolError);
                }
                // CVE-2024-27316: track CONTINUATION frame count and accumulated size
                self.flood_detector.continuation_count += 1;
                self.flood_detector.accumulated_header_size += payload_len;
                if let Some(error) = self.flood_detector.check_flood() {
                    return self.goaway(error);
                }
                self.expect_read = Some((H2StreamId::Zero, payload_len as usize));
                let mut headers = headers.clone();
                headers.end_headers = flags & parser::FLAG_END_HEADERS != 0;
                headers.header_block_fragment.len += payload_len;
                self.state = H2State::ContinuationFrame(headers);
            }
            Err(error) => {
                let error = error_nom_to_h2(error);
                error!("COULD NOT PARSE CONTINUATION HEADER");
                return self.goaway(error);
            }
            other => {
                error!("UNEXPECTED {:?} WHILE PARSING CONTINUATION HEADER", other);
                return self.goaway(H2Error::ProtocolError);
            }
        };
        MuxResult::Continue
    }

    pub fn readable<E, L>(&mut self, context: &mut Context<L>, endpoint: E) -> MuxResult
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        // RFC 9113 §6.5: check if peer has timed out on SETTINGS ACK
        if let Some(sent_at) = self.settings_sent_at {
            if sent_at.elapsed() >= SETTINGS_ACK_TIMEOUT {
                error!(
                    "SETTINGS ACK timeout: peer did not acknowledge within {:?}",
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
                H2StreamId::Other(_, global_stream_id) => {
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
            trace!("{:?}({:?}, {})", self.state, stream_id, amount);
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
                            debug!("EARLY INVALID PREFACE: {:?}", i);
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
                    "Unexpected combination: (Readable, {:?}, {:?})",
                    self.state, self.position
                );
                return self.force_disconnect();
            }
            (H2State::Discard, _) => {
                let _i = kawa.storage.data();
                trace!("DISCARDING: {:?}", _i);
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
                        // RFC 9113 §6.5: start tracking SETTINGS ACK timeout
                        self.settings_sent_at = Some(Instant::now());
                    }
                    Err(error) => {
                        error!("Could not serialize SettingsFrame: {:?}", error);
                        return self.force_disconnect();
                    }
                };

                self.state = H2State::ServerSettings;
                self.expect_write = Some(H2StreamId::Zero);
                return self.handle_frame(settings, context, endpoint);
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
                trace!("  data: {:?}", i);
                let frame = match parser::frame_body(i, header) {
                    Ok((_, frame)) => frame,
                    Err(error) => {
                        let error = error_nom_to_h2(error);
                        error!("COULD NOT PARSE FRAME BODY");
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
                return self.handle_frame(frame, context, endpoint);
            }
            (H2State::ContinuationFrame(headers), _) => {
                kawa.storage.head = kawa.storage.end;
                let i = kawa.storage.data();
                trace!("  data: {:?}", i);
                let headers = headers.clone();
                self.expect_header();
                return self.handle_frame(Frame::Headers(headers), context, endpoint);
            }
        }
        MuxResult::Continue
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
        let mut dead_streams = Vec::new();
        // Pre-compute byte totals for proportional overhead distribution.
        let byte_totals = self.compute_stream_byte_totals(context);

        if let Some(write_stream @ H2StreamId::Other(stream_id, global_stream_id)) =
            self.expect_write
        {
            let stream = &mut context.streams[global_stream_id];
            let stream_state = stream.state;
            let parts = stream.split(&self.position);
            let kawa = parts.wbuffer;
            while !kawa.out.is_empty() {
                let bufs = kawa.as_io_slice();
                let (size, status) = self.socket.socket_write_vectored(&bufs);
                context
                    .debug
                    .push(DebugEvent::SocketIO(2, global_stream_id, size));
                kawa.consume(size);
                self.position.count_bytes_out_counter(size);
                self.position.count_bytes_out(parts.metrics, size);
                if let Some((read_stream, amount)) = self.expect_read {
                    if write_stream == read_stream && kawa.storage.available_space() >= amount {
                        self.readiness.interest.insert(Ready::READABLE);
                    }
                }
                if update_readiness_after_write(size, status, &mut self.readiness) {
                    return MuxResult::Continue;
                }
            }
            self.expect_write = None;
            if (kawa.is_terminated() || kawa.is_error())
                && kawa.is_completed()
                && !Self::handle_1xx_reset(kawa, stream_state, &mut endpoint)
            {
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
                ) {
                    if let Some(token) = token {
                        endpoint.end_stream(token, global_stream_id, context);
                    }
                    dead_streams.push(dead_id);
                }
            }
        }

        // H2 flow control observability
        gauge!(
            "h2.connection_window",
            self.flow_control.window.max(0) as usize
        );
        gauge!("h2.active_streams", self.streams.len());
        gauge!(
            "h2.pending_window_updates",
            self.flow_control.pending_window_updates.len()
        );

        let scheme: &'static [u8] = if context.listener.borrow().protocol() == Protocol::HTTPS {
            b"https"
        } else {
            b"http"
        };
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
        };
        let mut priorities = self.streams.keys().collect::<Vec<_>>();
        priorities.sort_by(|a, b| {
            let (ua, _) = self.prioriser.get(a);
            let (ub, _) = self.prioriser.get(b);
            ua.cmp(&ub).then_with(|| a.cmp(b))
        });

        trace!("PRIORITIES: {:?}", priorities);
        let mut socket_write = false;
        'outer: for stream_id in priorities {
            let Some(&global_stream_id) = self.streams.get(stream_id) else {
                error!(
                    "stream_id {} from sorted keys missing in streams map",
                    stream_id
                );
                continue;
            };
            let stream = &mut context.streams[global_stream_id];
            let stream_state = stream.state;
            let parts = stream.split(&self.position);
            let kawa = parts.wbuffer;
            if kawa.is_main_phase()
                || (kawa.is_terminated() && !kawa.is_completed())
                || (kawa.is_error() && !self.rst_sent.contains(stream_id))
            {
                let window = min(*parts.window, self.flow_control.window);
                converter.window = window;
                converter.stream_id = *stream_id;
                // Track RST_STREAM dedup: if kawa is in error state, the converter
                // will generate a RST_STREAM frame. Mark it so we don't send a
                // duplicate on the next writable cycle.
                if kawa.is_error() {
                    self.rst_sent.insert(*stream_id);
                }
                kawa.prepare(&mut converter);
                let consumed = window - converter.window;
                *parts.window = parts.window.saturating_sub(consumed);
                self.flow_control.window = self.flow_control.window.saturating_sub(consumed);
            }
            context.debug.push(DebugEvent::S(
                *stream_id,
                global_stream_id,
                kawa.parsing_phase,
                kawa.blocks.len(),
                kawa.out.len(),
            ));
            while !kawa.out.is_empty() {
                socket_write = true;
                let bufs = kawa.as_io_slice();
                let (size, status) = self.socket.socket_write_vectored(&bufs);
                context
                    .debug
                    .push(DebugEvent::SocketIO(3, global_stream_id, size));
                kawa.consume(size);
                self.position.count_bytes_out_counter(size);
                self.position.count_bytes_out(parts.metrics, size);
                if update_readiness_after_write(size, status, &mut self.readiness) {
                    self.expect_write = Some(H2StreamId::Other(*stream_id, global_stream_id));
                    break 'outer;
                }
            }
            self.expect_write = None;
            if (kawa.is_terminated() || kawa.is_error())
                && kawa.is_completed()
                && !Self::handle_1xx_reset(kawa, stream_state, &mut endpoint)
            {
                if let Some((dead_id, token)) = Self::try_recycle_server_stream(
                    &self.position,
                    &mut self.bytes,
                    &self.streams,
                    stream,
                    global_stream_id,
                    *stream_id,
                    byte_totals,
                    &mut context.debug,
                    context.listener.clone(),
                ) {
                    if let Some(token) = token {
                        endpoint.end_stream(token, global_stream_id, context);
                    }
                    dead_streams.push(dead_id);
                }
            }
        }
        // Reclaim the converter's reusable buffers before any &mut self calls,
        // since the converter borrows self.encoder.
        self.converter_buf = converter.out;
        self.lowercase_buf = converter.lowercase_buf;
        self.shrink_converter_buffers();

        self.remove_dead_streams(dead_streams);
        self.finalize_write(socket_write, context)
    }

    /// Remove streams that completed their lifecycle from all tracking maps.
    /// After forwarding a 1xx informational response (100 Continue, 103 Early Hints),
    /// reset the back buffer and re-enable backend readable so the final response
    /// can arrive on the same stream. Returns true if the response was 1xx.
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
        debug!("H2 write_streams: 1xx informational forwarded, resetting back buffer");
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

    fn remove_dead_streams(&mut self, dead_streams: Vec<StreamId>) {
        for stream_id in dead_streams {
            if self.streams.remove(&stream_id).is_none() {
                error!("dead stream_id {} missing from streams map", stream_id);
            }
            self.rst_sent.remove(&stream_id);
            self.prioriser.remove(&stream_id);
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
    }

    /// Post-write phase: check drain completion, flush TLS, and update readiness.
    ///
    /// Returns `MuxResult::Continue` in the normal case, or triggers a graceful
    /// GOAWAY when draining and all streams have completed.
    fn finalize_write<L>(&mut self, socket_write: bool, context: &mut Context<L>) -> MuxResult
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
            // We wrote everything
            #[cfg(debug_assertions)]
            context.debug.push(DebugEvent::Str(format!(
                "Wrote everything: {:?}",
                self.streams
            )));
            self.readiness.interest.remove(Ready::WRITABLE);
        }
        MuxResult::Continue
    }

    /// Flush pending control frames (zero-buffer resume, WINDOW_UPDATEs, RST_STREAMs)
    /// before entering the main writable state machine.
    ///
    /// Returns `Some(result)` if the caller should return early (e.g. socket would
    /// block, GOAWAY triggered), or `None` if writable() should proceed normally.
    fn flush_pending_control_frames(&mut self) -> Option<MuxResult> {
        // RFC 9113 §6.5: check if peer has timed out on SETTINGS ACK
        if let Some(sent_at) = self.settings_sent_at {
            if sent_at.elapsed() >= SETTINGS_ACK_TIMEOUT {
                error!(
                    "SETTINGS ACK timeout: peer did not acknowledge within {:?}",
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
        if self.total_rst_streams_queued >= MAX_PENDING_RST_STREAMS {
            error!(
                "total RST_STREAM count {} exceeds cap {}, sending GOAWAY(ENHANCE_YOUR_CALM)",
                self.total_rst_streams_queued, MAX_PENDING_RST_STREAMS
            );
            return Some(self.goaway(H2Error::EnhanceYourCalm));
        }

        // Flush pending RST_STREAM frames (queued when refusing streams).
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
        if let Some(result) = self.flush_pending_control_frames() {
            return result;
        }

        match (&self.state, &self.position) {
            (H2State::Error, _)
            | (H2State::ClientPreface, Position::Server)
            | (H2State::ClientSettings, Position::Server)
            | (H2State::ServerSettings, Position::Client(..)) => {
                error!(
                    "Unexpected combination: (Writable, {:?}, {:?})",
                    self.state, self.position
                );
                self.force_disconnect()
            }
            // Discard state: pending data (e.g. RST_STREAM) was already
            // written in the preamble above; let the readable path consume
            // the remaining frame payload.
            (H2State::Discard, _) => MuxResult::Continue,
            (H2State::GoAway, _) => self.force_disconnect(),
            (H2State::ClientPreface, Position::Client(..)) => {
                trace!("Preparing preface and settings");
                let pri = serializer::H2_PRI.as_bytes();
                let kawa = &mut self.zero;

                kawa.storage.space()[0..pri.len()].copy_from_slice(pri);
                kawa.storage.fill(pri.len());
                match serializer::gen_settings(kawa.storage.space(), &self.local_settings) {
                    Ok((_, size)) => {
                        kawa.storage.fill(size);
                        // RFC 9113 §6.5: start tracking SETTINGS ACK timeout
                        self.settings_sent_at = Some(Instant::now());
                    }
                    Err(error) => {
                        error!("Could not serialize SettingsFrame: {:?}", error);
                        return self.force_disconnect();
                    }
                };

                self.state = H2State::ClientSettings;
                self.expect_write = Some(H2StreamId::Zero);
                MuxResult::Continue
            }
            (H2State::ClientSettings, Position::Client(..)) => {
                trace!("Sent preface and settings");
                self.state = H2State::ServerSettings;
                self.expect_read = Some((H2StreamId::Zero, 9));
                self.readiness.interest.remove(Ready::WRITABLE);
                MuxResult::Continue
            }
            (H2State::ServerSettings, Position::Server) => {
                // Enlarge the connection-level receive window from 64KB to 1MB.
                // The default 65 535-byte window is too small for high-throughput
                // proxying and causes excessive WINDOW_UPDATE round-trips.
                // Use additive increment rather than unconditional assignment to
                // preserve any window changes that occurred during setup.
                let increment = ENLARGED_CONNECTION_WINDOW - DEFAULT_INITIAL_WINDOW_SIZE;
                self.queue_window_update(0, increment);
                self.flow_control.window += increment as i32;
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

    /// Try to recycle a completed server-side stream by distributing overhead,
    /// generating access logs, and transitioning the stream to `Recycle` state.
    ///
    /// Returns `Some((stream_id, Option<token>))` if the stream was recycled, so the
    /// caller can add `stream_id` to the dead-streams list and call `endpoint.end_stream()`
    /// if a token was returned. Returns `None` if recycling was deferred or not applicable.
    ///
    /// Takes individual field references instead of `&mut self` to avoid borrow
    /// conflicts when the H2 block converter holds `&mut self.encoder`.
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
    ) -> Option<(StreamId, Option<mio::Token>)>
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        match position {
            Position::Client(..) => None,
            Position::Server => {
                // Don't recycle if the client hasn't sent END_STREAM yet —
                // more DATA frames may arrive for this stream.
                if !stream.front_received_end_of_stream {
                    trace!(
                        "Defer recycle stream {}: client still sending",
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
                );
                debug.push(DebugEvent::StreamEvent(4, global_stream_id));
                trace!("Recycle stream: {}", global_stream_id);
                let token = Self::complete_server_stream(stream, listener);
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
    ) -> Option<mio::Token>
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        incr!("http.e2e.h2");
        stream.metrics.backend_stop();
        stream.generate_access_log(false, Some("H2::Complete"), listener);
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
    fn queue_window_update(&mut self, stream_id: u32, increment: u32) {
        let max_increment = i32::MAX as u32;
        if let Some(existing) = self.flow_control.pending_window_updates.get_mut(&stream_id) {
            let old = *existing;
            *existing = existing.saturating_add(increment).min(max_increment);
            trace!(
                "WINDOW_UPDATE coalesced: stream={} old={} new={}",
                stream_id, old, *existing
            );
        } else if self.flow_control.pending_window_updates.len() < MAX_PENDING_WINDOW_UPDATES {
            self.flow_control
                .pending_window_updates
                .insert(stream_id, increment.min(max_increment));
            trace!(
                "WINDOW_UPDATE queued: stream={} increment={}",
                stream_id,
                increment.min(max_increment)
            );
        } else {
            error!(
                "WINDOW_UPDATE dropped: queue full ({} entries), stream={} increment={}",
                MAX_PENDING_WINDOW_UPDATES, stream_id, increment
            );
            incr!("h2.window_update_dropped");
            // Ensure a writable event so the existing queue gets flushed,
            // freeing space for the dropped WINDOW_UPDATE on the next cycle.
            self.readiness.interest.insert(Ready::WRITABLE);
        }
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
        if let Some((H2StreamId::Other(_, global_stream_id), amount)) = self.expect_read {
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

    pub fn goaway(&mut self, error: H2Error) -> MuxResult {
        self.state = H2State::Error;
        self.drain.draining = true;
        self.expect_read = None;
        let kawa = &mut self.zero;
        kawa.storage.clear();
        if matches!(error, H2Error::NoError) {
            debug!("GOAWAY: {:?}", error);
        } else {
            error!("GOAWAY: {:?}", error);
        }

        // RFC 9113 §6.8: last_stream_id is the highest peer-initiated stream we processed
        match serializer::gen_goaway(kawa.storage.space(), self.highest_peer_stream_id, error) {
            Ok((_, size)) => {
                kawa.storage.fill(size);
                self.state = H2State::GoAway;
                self.expect_write = Some(H2StreamId::Zero);
                self.readiness.interest = Ready::WRITABLE | Ready::HUP | Ready::ERROR;
                MuxResult::Continue
            }
            Err(error) => {
                error!("Could not serialize GoAwayFrame: {:?}", error);
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
        // Keep expect_read as-is: existing streams should continue reading
        // data during phase 1. Only phase 2 (goaway()) removes READABLE.
        let kawa = &mut self.zero;
        kawa.storage.clear();
        debug!("GOAWAY (graceful, phase 1): last_stream_id=0x7FFFFFFF");

        const MAX_STREAM_ID: u32 = 0x7FFF_FFFF;
        match serializer::gen_goaway(kawa.storage.space(), MAX_STREAM_ID, H2Error::NoError) {
            Ok((_, size)) => {
                kawa.storage.fill(size);
                // Stay in the current state so the connection can continue processing
                // existing streams. The second GOAWAY will transition to GoAway state.
                // Keep READABLE so in-flight request bodies can still be received
                // during phase 1. Only remove READABLE in the second GOAWAY (goaway()).
                self.expect_write = Some(H2StreamId::Zero);
                self.readiness.interest.insert(Ready::WRITABLE);
                MuxResult::Continue
            }
            Err(error) => {
                error!("Could not serialize graceful GoAwayFrame: {:?}", error);
                self.force_disconnect()
            }
        }
    }

    /// Returns `true` if there is data queued in the zero buffer waiting
    /// to be flushed (e.g. a GOAWAY frame serialized but not yet written
    /// to the socket).
    pub fn has_pending_write(&self) -> bool {
        self.expect_write.is_some() || !self.zero.storage.is_empty()
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
                "flush_zero_to_socket: written={}, status={:?}, wants_write={}",
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
            error!("Rejecting new stream {} on draining connection", stream_id);
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
        Some(global_stream_id)
    }

    pub fn new_stream_id(&mut self) -> Option<StreamId> {
        let next = self.last_stream_id.checked_add(2)?;
        if next > FLOW_CONTROL_MAX_WINDOW {
            return None;
        }
        self.last_stream_id = next;
        match self.position {
            Position::Client(..) => Some(self.last_stream_id - 1),
            Position::Server => Some(self.last_stream_id - 2),
        }
    }

    fn handle_frame<E, L>(
        &mut self,
        frame: Frame,
        context: &mut Context<L>,
        endpoint: E,
    ) -> MuxResult
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        trace!("{:#?}", frame);
        match frame {
            Frame::Data(data) => self.handle_data_frame(data, context, endpoint),
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
            Frame::Continuation(_) => {
                self.attribute_bytes_to_overhead();
                error!("CONTINUATION frames are handled inline during header parsing");
                self.goaway(H2Error::ProtocolError)
            }
        }
    }

    fn handle_data_frame<E, L>(
        &mut self,
        data: parser::Data,
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
            if let Some(error) = self.flood_detector.check_flood() {
                return self.goaway(error);
            }
        }
        let Some(global_stream_id) = self.streams.get(&data.stream_id).copied() else {
            // The stream was terminated while data was expected,
            // probably due to automatic answer for invalid/unauthorized access.
            // RFC 9113 §6.9: we MUST still account for the DATA payload in
            // connection-level flow control, otherwise the window shrinks
            // permanently and eventually stalls the connection.
            let payload_len = data.payload.len() as u32;
            self.flow_control.received_bytes_since_update += payload_len;
            let conn_threshold = ENLARGED_CONNECTION_WINDOW / 2;
            if self.flow_control.received_bytes_since_update >= conn_threshold {
                let increment = self.flow_control.received_bytes_since_update;
                self.queue_window_update(0, increment);
                self.flow_control.received_bytes_since_update = 0;
                self.readiness.interest.insert(Ready::WRITABLE);
            }
            self.attribute_bytes_to_overhead();
            return MuxResult::Continue;
        };
        let mut slice = data.payload;
        let stream = &mut context.streams[global_stream_id];
        let payload_len = slice.len();

        // Extract declared content-length and update position-aware data counter
        let (data_received, declared_length) = {
            let parts = stream.split(&self.position);
            *parts.data_received += payload_len;
            let total = *parts.data_received;
            let declared = match parts.rbuffer.body_size {
                kawa::BodySize::Length(n) => Some(n),
                _ => None,
            };
            (total, declared)
        };

        // RFC 9113 §8.1.1: if Content-Length is present, total DATA payload
        // must not exceed the declared length (check on every frame)
        if let Some(expected) = declared_length {
            if data_received > expected {
                error!(
                    "Content-Length mismatch: received {} > declared {}",
                    data_received, expected
                );
                return self.reset_stream(
                    global_stream_id,
                    context,
                    endpoint,
                    H2Error::ProtocolError,
                );
            }
        }

        let stream = &mut context.streams[global_stream_id];
        self.attribute_bytes_to_stream(&mut stream.metrics);
        let stream_state = stream.state;
        let is_unlinked = matches!(stream_state, StreamState::Unlinked);
        let parts = stream.split(&self.position);
        let kawa = parts.rbuffer;
        self.position.count_bytes_in(parts.metrics, payload_len);

        // RFC 9113 §6.9: Update flow control after consuming DATA.
        // Track bytes received and queue WINDOW_UPDATE when threshold reached.
        let payload_u32 = payload_len as u32;

        // Connection-level flow control (use enlarged window threshold)
        let conn_threshold = ENLARGED_CONNECTION_WINDOW / 2;
        self.flow_control.received_bytes_since_update += payload_u32;
        if self.flow_control.received_bytes_since_update >= conn_threshold {
            let increment = self.flow_control.received_bytes_since_update;
            self.queue_window_update(0, increment);
            self.flow_control.received_bytes_since_update = 0;
        }

        // Stream-level flow control (only if stream is still open)
        if !data.end_stream {
            self.queue_window_update(data.stream_id, payload_u32);
        }

        // If we have pending updates, ensure we get a writable event
        if !self.flow_control.pending_window_updates.is_empty() {
            self.readiness.interest.insert(Ready::WRITABLE);
        }

        if is_unlinked {
            // Backend is gone but client is still sending DATA.
            // Discard the data (flow control updates were already
            // queued above) to prevent the buffer from filling up.
            kawa.storage.clear();
            if data.end_stream {
                kawa.parsing_phase = kawa::ParsingPhase::Terminated;
                if self.position.is_server() {
                    stream.front_received_end_of_stream = true;
                } else {
                    stream.back_received_end_of_stream = true;
                }
            }
        } else {
            slice.start += kawa.storage.head as u32;
            kawa.storage.head += payload_len;
            kawa.push_block(kawa::Block::Chunk(kawa::Chunk {
                data: kawa::Store::Slice(slice),
            }));

            if data.end_stream {
                // RFC 9113 §8.1.1: on end_stream, total DATA must equal Content-Length
                if let Some(expected) = declared_length {
                    if data_received != expected {
                        error!(
                            "Content-Length mismatch: received {} != declared {}",
                            data_received, expected
                        );
                        return self.reset_stream(
                            global_stream_id,
                            context,
                            endpoint,
                            H2Error::ProtocolError,
                        );
                    }
                }
                kawa.push_block(kawa::Block::Flags(kawa::Flags {
                    end_body: true,
                    end_chunk: false,
                    end_header: false,
                    end_stream: true,
                }));
                kawa.parsing_phase = kawa::ParsingPhase::Terminated;
                if self.position.is_server() {
                    stream.front_received_end_of_stream = true;
                } else {
                    stream.back_received_end_of_stream = true;
                }
            }
            if let StreamState::Linked(token) = stream_state {
                endpoint
                    .readiness_mut(token)
                    .interest
                    .insert(Ready::WRITABLE);
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
            debug!("FRAGMENT: {:?}", self.zero.storage.data());
            self.state = H2State::ContinuationHeader(headers);
            return MuxResult::Continue;
        }
        // Header block is complete — reset CONTINUATION counters
        self.flood_detector.reset_continuation();
        // can this fail?
        let stream_id = headers.stream_id;
        let Some(global_stream_id) = self.streams.get(&stream_id).copied() else {
            error!("Handling Headers frame with no attached stream {:#?}", self);
            incr!("h2.headers_no_stream.error");
            self.attribute_bytes_to_overhead();
            return self.force_disconnect();
        };

        if let Some(priority) = &headers.priority {
            if self.prioriser.push_priority(stream_id, priority.clone()) {
                self.reset_stream(global_stream_id, context, endpoint, H2Error::ProtocolError);
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
        let status = pkawa::handle_header(
            &mut self.decoder,
            &mut self.prioriser,
            stream_id,
            parts.rbuffer,
            buffer,
            headers.end_stream,
            parts.context,
        );
        kawa.storage.clear();
        if let Err((error, global)) = status {
            match self.position {
                Position::Client(..) => incr!("http.backend_parse_errors"),
                Position::Server => incr!("http.frontend_parse_errors"),
            }
            if global {
                error!("GOT GLOBAL ERROR WHILE PROCESSING HEADERS");
                return self.goaway(error);
            } else {
                return self.reset_stream(global_stream_id, context, endpoint, error);
            }
        }
        if headers.end_stream {
            // RFC 9113 §8.1.1: when END_STREAM arrives via trailers,
            // validate that total DATA received matches Content-Length.
            if !was_initial {
                let parts = stream.split(&self.position);
                if let kawa::BodySize::Length(expected) = parts.rbuffer.body_size {
                    if *parts.data_received != expected {
                        error!(
                            "Content-Length mismatch on trailers: received {} != declared {}",
                            *parts.data_received, expected
                        );
                        return self.reset_stream(
                            global_stream_id,
                            context,
                            endpoint,
                            H2Error::ProtocolError,
                        );
                    }
                }
            }
            if self.position.is_server() {
                stream.front_received_end_of_stream = true;
            } else {
                stream.back_received_end_of_stream = true;
            }
        }
        if let StreamState::Linked(token) = stream.state {
            endpoint
                .readiness_mut(token)
                .interest
                .insert(Ready::WRITABLE)
        }
        // was_initial prevents trailers from triggering connection
        if was_initial && self.position.is_server() {
            incr!("http.requests");
            gauge_add!("http.active_requests", 1);
            stream.metrics.service_start();
            stream.request_counted = true;
            stream.state = StreamState::Link;
        }
        MuxResult::Continue
    }

    fn handle_push_promise_frame(&mut self) -> MuxResult {
        self.attribute_bytes_to_overhead();
        match self.position {
            Position::Client(..) => {
                // RFC 9113 §8.4: Server push is deprecated. Sozu never sends
                // SETTINGS_ENABLE_PUSH=1, so receiving PUSH_PROMISE is a protocol error.
                error!("Received PUSH_PROMISE but server push is not supported");
                self.goaway(H2Error::ProtocolError)
            }
            Position::Server => {
                // Clients must never send PUSH_PROMISE (RFC 9113 §8.4)
                error!("Received PUSH_PROMISE from client");
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
        if self
            .prioriser
            .push_priority(priority.stream_id, priority.inner)
        {
            if let Some(global_stream_id) = self.streams.get(&priority.stream_id) {
                return self.reset_stream(
                    *global_stream_id,
                    context,
                    endpoint,
                    H2Error::ProtocolError,
                );
            } else {
                error!("INVALID PRIORITY RECEIVED ON INVALID STREAM");
                return self.goaway(H2Error::ProtocolError);
            }
        }
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
        // CVE-2023-44487 Rapid Reset + CVE-2019-9514: track RST_STREAM rate
        self.flood_detector.rst_stream_count += 1;
        if let Some(error) = self.flood_detector.check_flood() {
            return self.goaway(error);
        }
        debug!(
            "RstStream({} -> {})",
            rst_stream.error_code,
            H2Error::try_from(rst_stream.error_code).map_or("UNKNOWN_ERROR", |e| e.as_str())
        );
        // Compute totals before removing the stream from the map,
        // so the removed stream's bytes are included in the total.
        let rst_byte_totals = self.compute_stream_byte_totals(context);
        if let Some(stream_id) = self.streams.remove(&rst_stream.stream_id) {
            self.prioriser.remove(&rst_stream.stream_id);
            let stream = &mut context.streams[stream_id];
            self.attribute_bytes_to_stream(&mut stream.metrics);
            if let StreamState::Linked(token) = stream.state {
                endpoint.end_stream(token, stream_id, context);
            }
            let stream = &mut context.streams[stream_id];
            match self.position {
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
                    );
                    stream.state = StreamState::Recycle;
                }
            }
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
                error!("SETTINGS ACK with non-empty payload");
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
        if let Some(error) = self.flood_detector.check_flood() {
            return self.goaway(error);
        }
        for setting in settings.settings {
            let v = setting.value;
            let mut is_error = false;
            #[rustfmt::skip]
            match setting.identifier {
                parser::SETTINGS_HEADER_TABLE_SIZE => {
                    self.peer_settings.settings_header_table_size = v;
                    // Propagate peer's table size to our HPACK encoder
                    self.encoder.set_max_table_size(v as usize);
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
                error!("INVALID SETTING");
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
            let increment = ENLARGED_CONNECTION_WINDOW - DEFAULT_INITIAL_WINDOW_SIZE;
            self.queue_window_update(0, increment);
            self.flow_control.window += increment as i32;
        }

        let kawa = &mut self.zero;
        let ack = &serializer::SETTINGS_ACKNOWLEDGEMENT;
        let buf = kawa.storage.space();
        if buf.len() < ack.len() {
            error!(
                "No space in zero buffer for SETTINGS ACK ({} available, {} needed)",
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
        MuxResult::Continue
    }

    fn handle_ping_frame(&mut self, ping: parser::Ping) -> MuxResult {
        if ping.ack {
            self.attribute_bytes_to_overhead();
            return MuxResult::Continue;
        }
        // CVE-2019-9512: track non-ACK PING frame rate
        self.flood_detector.ping_count += 1;
        if let Some(error) = self.flood_detector.check_flood() {
            return self.goaway(error);
        }
        self.attribute_bytes_to_overhead();
        let kawa = &mut self.zero;
        let ping_response_size = serializer::PING_ACKNOWLEDGEMENT_HEADER.len() + 8;
        if kawa.storage.space().len() < ping_response_size {
            error!(
                "No space in zero buffer for PING response ({} available, {} needed)",
                kawa.storage.space().len(),
                ping_response_size
            );
            return self.force_disconnect();
        }
        match serializer::gen_ping_acknowledgement(kawa.storage.space(), &ping.payload) {
            Ok((_, size)) => kawa.storage.fill(size),
            Err(error) => {
                error!("Could not serialize PingFrame: {:?}", error);
                return self.force_disconnect();
            }
        };
        self.readiness.interest.insert(Ready::WRITABLE);
        self.readiness.interest.remove(Ready::READABLE);
        self.expect_write = Some(H2StreamId::Zero);
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
                "Received GOAWAY: last_stream_id={}, error={}, debug_data={:?}",
                goaway.last_stream_id, error_name, goaway.additional_debug_data
            );
        } else {
            error!(
                "Received GOAWAY: last_stream_id={}, error={}, debug_data={:?}",
                goaway.last_stream_id, error_name, goaway.additional_debug_data
            );
        }
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
            let stream = &mut context.streams[*global_stream_id];
            if stream.front.consumed {
                // Request was already sent to this backend — we can't
                // replay it. Use the linked token's readiness (via endpoint)
                // so the RST_STREAM reaches the client.
                debug!(
                    "GOAWAY: stream {} already consumed, cannot retry",
                    stream_id
                );
                if let StreamState::Linked(token) = stream.state {
                    let front_readiness = endpoint.readiness_mut(token);
                    forcefully_terminate_answer(stream, front_readiness, H2Error::RefusedStream);
                } else {
                    warn!(
                        "GOAWAY: stream {} consumed but not Linked, cannot notify frontend",
                        stream_id
                    );
                }
            } else {
                stream.state = StreamState::Link;
            }
            self.streams.remove(stream_id);
            self.prioriser.remove(stream_id);
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
        let increment = wu.increment as i32;
        if stream_id == 0 {
            self.attribute_bytes_to_overhead();
            if let Some(window) = self.flow_control.window.checked_add(increment) {
                if self.flow_control.window <= 0 && window > 0 {
                    self.readiness.interest.insert(Ready::WRITABLE);
                }
                self.flow_control.window = window;
                debug!(
                    "WINDOW_UPDATE received: stream=0 increment={} new_connection_window={}",
                    increment, self.flow_control.window
                );
            } else {
                error!("INVALID WINDOW INCREMENT");
                return self.goaway(H2Error::FlowControlError);
            }
        } else if let Some(global_stream_id) = self.streams.get(&stream_id).copied() {
            let stream = &mut context.streams[global_stream_id];
            self.attribute_bytes_to_stream(&mut stream.metrics);
            if let Some(window) = stream.window.checked_add(increment) {
                if stream.window <= 0 && window > 0 {
                    self.readiness.interest.insert(Ready::WRITABLE);
                }
                stream.window = window;
                debug!(
                    "WINDOW_UPDATE received: stream={} increment={} new_stream_window={}",
                    stream_id, increment, stream.window
                );
            } else {
                return self.reset_stream(
                    global_stream_id,
                    context,
                    endpoint,
                    H2Error::FlowControlError,
                );
            }
        } else {
            self.attribute_bytes_to_overhead();
            trace!(
                "Ignoring window update on closed stream {}: {}",
                stream_id, increment
            );
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
                error!("initial window size delta overflow");
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
            "UPDATE INIT WINDOW: {} {} {:?}",
            delta, open_window, self.readiness
        );
        if open_window {
            self.readiness.interest.insert(Ready::WRITABLE);
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
                MuxResult::Continue
            }
            Position::Server => MuxResult::CloseSession,
        }
    }

    pub fn close<E, L>(&mut self, context: &mut Context<L>, mut endpoint: E)
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        match self.position {
            Position::Client(_, _, BackendStatus::KeepAlive) => {
                error!("H2 connections do not use KeepAlive backend status");
                return;
            }
            Position::Client(..) => {}
            Position::Server => {
                trace!("H2 SENDING CLOSE NOTIFY");
                self.socket.socket_close();
                let _ = self.socket.socket_write_vectored(&[]);
                return;
            }
        }
        // reconnection is handled by the server for each stream separately
        for global_stream_id in self.streams.values() {
            trace!("end stream: {}", global_stream_id);
            if let StreamState::Linked(token) = context.streams[*global_stream_id].state {
                endpoint.end_stream(token, *global_stream_id, context);
            }
        }
    }

    pub fn reset_stream<E, L>(
        &mut self,
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
        let stream = &mut context.streams[stream_id];
        trace!("reset H2 stream {}: {:#?}", stream_id, stream.context);
        let old_state = std::mem::replace(&mut stream.state, StreamState::Unlinked);
        forcefully_terminate_answer(stream, &mut self.readiness, error);
        if let StreamState::Linked(token) = old_state {
            endpoint.end_stream(token, stream_id, context);
        }
        // Emit access log for server-side resets on streams that had active requests
        if self.position.is_server()
            && matches!(old_state, StreamState::Link | StreamState::Linked(_))
        {
            let stream = &mut context.streams[stream_id];
            self.distribute_overhead(&mut stream.metrics, reset_byte_totals);
            stream.metrics.backend_stop();
            stream.generate_access_log(true, Some("H2::Reset"), context.listener.clone());
        }
        MuxResult::Continue
    }

    pub fn end_stream<L>(&mut self, stream_gid: GlobalStreamId, context: &mut Context<L>)
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        let stream_context = &mut context.streams[stream_gid].context;
        trace!("end H2 stream {}: {:#?}", stream_gid, stream_context);
        match self.position {
            Position::Client(..) => {
                for (stream_id, global_stream_id) in &self.streams {
                    if *global_stream_id == stream_gid {
                        let id = *stream_id;
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
                                    self.readiness.interest.insert(Ready::WRITABLE);
                                    self.rst_sent.insert(id);
                                }
                            }
                        }
                        self.streams.remove(&id);
                        self.prioriser.remove(&id);
                        return;
                    }
                }
                error!(
                    "end_stream called for unknown global_stream_id {}",
                    stream_gid
                );
            }
            Position::Server => {
                let answers_rc = context.listener.borrow().get_answers().clone();
                let stream = &mut context.streams[stream_gid];
                match (stream.front.consumed, stream.back.is_main_phase()) {
                    (_, true) => {
                        // front might not have been consumed (in case of PushPromise)
                        // we have a "forwardable" answer from the back
                        // if the answer is not terminated we send an RstStream to properly clean the stream
                        // if it is terminated, we finish the transfer, the backend is not necessary anymore
                        if !stream.context.keep_alive_backend && !stream.back.is_terminated() {
                            // Close-delimited response: the backend closed the
                            // connection to signal end-of-body (no Content-Length).
                            // Mark the kawa as terminated so the H2 converter
                            // emits DATA with END_STREAM instead of RST_STREAM.
                            debug!("CLOSE DELIMITED H2 STREAM {} {:?}", stream_gid, stream);
                            stream.back.push_block(kawa::Block::Flags(kawa::Flags {
                                end_body: true,
                                end_chunk: false,
                                end_header: false,
                                end_stream: true,
                            }));
                            stream.back.parsing_phase = kawa::ParsingPhase::Terminated;
                            stream.state = StreamState::Unlinked;
                            self.readiness.interest.insert(Ready::WRITABLE);
                        } else if !stream.back.is_terminated() {
                            #[cfg(debug_assertions)]
                            context
                                .debug
                                .push(DebugEvent::Str(format!("Close unterminated {stream_gid}")));
                            debug!("CLOSING H2 UNTERMINATED STREAM {} {:?}", stream_gid, stream);
                            forcefully_terminate_answer(
                                stream,
                                &mut self.readiness,
                                H2Error::InternalError,
                            );
                        } else {
                            #[cfg(debug_assertions)]
                            context
                                .debug
                                .push(DebugEvent::Str(format!("Close terminated {stream_gid}")));
                            debug!("CLOSING H2 TERMINATED STREAM {} {:?}", stream_gid, stream);
                            stream.state = StreamState::Unlinked;
                            self.readiness.interest.insert(Ready::WRITABLE);
                        }
                        context.debug.set_interesting(true);
                    }
                    (true, false) => {
                        // we do not have an answer, but the request has already been partially consumed
                        // so we can't retry, send a 502 bad gateway instead
                        // note: it might be possible to send a RstStream with an adequate error code
                        #[cfg(debug_assertions)]
                        context.debug.push(DebugEvent::Str(format!(
                            "Can't retry, send 502 on {stream_gid}"
                        )));
                        let answers = answers_rc.borrow();
                        set_default_answer(stream, &mut self.readiness, 502, &answers);
                    }
                    (false, false) => {
                        // we do not have an answer, but the request is untouched so we can retry
                        debug!("H2 RECONNECT");
                        #[cfg(debug_assertions)]
                        context
                            .debug
                            .push(DebugEvent::Str(format!("Retry {stream_gid}")));
                        stream.state = StreamState::Link
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
                "Cannot open new stream on draining connection (stream {})",
                stream
            );
            return false;
        }
        // RFC 9113 §5.1.2: respect peer's max concurrent streams limit
        if self.streams.len() >= self.peer_settings.settings_max_concurrent_streams as usize {
            error!(
                "Cannot open new stream: active={} >= peer max_concurrent_streams={}",
                self.streams.len(),
                self.peer_settings.settings_max_concurrent_streams
            );
            return false;
        }
        trace!("start new H2 stream {} {:?}", stream, self.readiness);
        let Some(stream_id) = self.new_stream_id() else {
            error!("Stream ID space exhausted, cannot open new stream");
            return false;
        };
        self.streams.insert(stream_id, stream);
        self.readiness.interest.insert(Ready::WRITABLE);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(detector.check_flood(), Some(H2Error::EnhanceYourCalm));
    }

    #[test]
    fn test_flood_detector_detects_ping_flood() {
        let config = H2FloodConfig::default();
        let mut detector = H2FloodDetector::new(config);

        detector.ping_count = config.max_ping_per_window + 1;
        assert_eq!(detector.check_flood(), Some(H2Error::EnhanceYourCalm));
    }

    #[test]
    fn test_flood_detector_detects_settings_flood() {
        let config = H2FloodConfig::default();
        let mut detector = H2FloodDetector::new(config);

        detector.settings_count = config.max_settings_per_window + 1;
        assert_eq!(detector.check_flood(), Some(H2Error::EnhanceYourCalm));
    }

    #[test]
    fn test_flood_detector_detects_empty_data_flood() {
        let config = H2FloodConfig::default();
        let mut detector = H2FloodDetector::new(config);

        detector.empty_data_count = config.max_empty_data_per_window + 1;
        assert_eq!(detector.check_flood(), Some(H2Error::EnhanceYourCalm));
    }

    #[test]
    fn test_flood_detector_detects_continuation_flood() {
        let config = H2FloodConfig::default();
        let mut detector = H2FloodDetector::new(config);

        detector.continuation_count = config.max_continuation_frames + 1;
        assert_eq!(detector.check_flood(), Some(H2Error::EnhanceYourCalm));
    }

    #[test]
    fn test_flood_detector_detects_header_size_flood() {
        let config = H2FloodConfig::default();
        let mut detector = H2FloodDetector::new(config);

        detector.accumulated_header_size = MAX_HEADER_LIST_SIZE + 1;
        assert_eq!(detector.check_flood(), Some(H2Error::EnhanceYourCalm));
    }

    #[test]
    fn test_flood_detector_detects_glitch_flood() {
        let config = H2FloodConfig::default();
        let mut detector = H2FloodDetector::new(config);

        detector.glitch_count = config.max_glitch_count + 1;
        assert_eq!(detector.check_flood(), Some(H2Error::EnhanceYourCalm));
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
        };
        let mut detector = H2FloodDetector::new(config);

        // Below custom threshold -> no flood
        detector.rst_stream_count = 5;
        assert!(detector.check_flood().is_none());

        // Above custom threshold -> flood
        detector.rst_stream_count = 6;
        assert_eq!(detector.check_flood(), Some(H2Error::EnhanceYourCalm));
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
        detector.glitch_count = 50;

        // Force window expiry by setting window_start to the past
        detector.window_start = Instant::now() - FLOOD_WINDOW_DURATION;

        // check_flood calls maybe_reset_window which halves counters
        let _ = detector.check_flood();

        assert_eq!(detector.rst_stream_count, 40);
        assert_eq!(detector.ping_count, 30);
        assert_eq!(detector.settings_count, 20);
        assert_eq!(detector.empty_data_count, 10);
        assert_eq!(detector.glitch_count, 25);
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
        assert_eq!(detector.check_flood(), Some(H2Error::EnhanceYourCalm));

        // Reset and simulate window expiry
        detector.rst_stream_count = 12;
        detector.window_start = Instant::now() - FLOOD_WINDOW_DURATION;

        // After decay: 12/2 = 6, which is below threshold 10 -> no flood
        assert!(detector.check_flood().is_none());
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
    }

    // ── distribute_overhead ─────────────────────────────────────────────

    #[test]
    fn test_distribute_overhead_proportional() {
        let mut metrics = SessionMetrics::new(None);
        let mut overhead_bin = 1000;
        let mut overhead_bout = 500;

        // Stream transferred 60% of total bytes
        distribute_overhead(
            &mut metrics,
            &mut overhead_bin,
            &mut overhead_bout,
            (600, 300),  // stream_bytes
            (1000, 500), // total_bytes
            2,           // active_streams
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

        // No bytes transferred -> even distribution
        distribute_overhead(
            &mut metrics,
            &mut overhead_bin,
            &mut overhead_bout,
            (0, 0), // stream_bytes
            (0, 0), // total_bytes
            4,      // active_streams
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

        // Stream claims 100% of bytes but overhead is small
        distribute_overhead(
            &mut metrics,
            &mut overhead_bin,
            &mut overhead_bout,
            (1000, 1000), // stream_bytes
            (1000, 1000), // total_bytes
            1,            // active_streams
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

        // 0 active streams (edge case) — falls back to max(1)
        distribute_overhead(
            &mut metrics,
            &mut overhead_bin,
            &mut overhead_bout,
            (0, 0),
            (0, 0),
            0,
        );

        assert_eq!(metrics.bin, 100); // 100 / max(0,1) = 100
        assert_eq!(metrics.bout, 100);
        assert_eq!(overhead_bin, 0);
        assert_eq!(overhead_bout, 0);
    }

    // ── H2FlowControl (additional edge cases) ─────────────────────────

    #[test]
    fn test_flow_control_queue_window_update_cap() {
        // Verify MAX_PENDING_WINDOW_UPDATES reflects 1 + 4*MAX_CONCURRENT_STREAMS
        assert_eq!(MAX_PENDING_WINDOW_UPDATES, 1 + 100 * 4);

        // Simulate queue reaching capacity
        let mut updates: HashMap<u32, u32> = HashMap::new();
        for i in 0..MAX_PENDING_WINDOW_UPDATES as u32 {
            updates.insert(i, 1000);
        }
        assert_eq!(updates.len(), MAX_PENDING_WINDOW_UPDATES);

        // A new stream ID beyond capacity should be rejected
        let next_stream = MAX_PENDING_WINDOW_UPDATES as u32;
        let at_cap = updates.len() >= MAX_PENDING_WINDOW_UPDATES;
        assert!(at_cap);
        assert!(!updates.contains_key(&next_stream));
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

        // Stream transferred 100% inbound, 0% outbound
        distribute_overhead(
            &mut metrics,
            &mut overhead_bin,
            &mut overhead_bout,
            (500, 0),   // stream_bytes
            (500, 100), // total_bytes
            2,          // active_streams
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
        // Integer division means shares shrink as the remainder shrinks:
        //   call 1: 120 * 100/300 = 40 -> remaining 80
        //   call 2:  80 * 100/300 = 26 -> remaining 54
        //   call 3:  54 * 100/300 = 18 -> remaining 36
        // Total distributed: 40 + 26 + 18 = 84 (some rounding loss remains)
        for _ in 0..3 {
            distribute_overhead(
                &mut metrics,
                &mut overhead_bin,
                &mut overhead_bout,
                (100, 100), // stream_bytes
                (300, 300), // total_bytes
                3,          // active_streams
            );
        }

        assert_eq!(metrics.bin, 84);
        assert_eq!(metrics.bout, 84);
        // Rounding residual left over
        assert_eq!(overhead_bin, 36);
        assert_eq!(overhead_bout, 36);
    }
}
