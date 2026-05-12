//! Application state for `sozu top`.
//!
//! Pure data — no I/O, no rendering. The render loop reads `App` to draw a
//! frame; the transport threads push `Snapshot`s and `TopEvent`s that
//! `App::ingest_*` folds into the ring buffers, rate calculator, threshold
//! table, and pulse tracker.
//!
//! Three derived state primitives:
//!
//! - [`SparkRing`] — fixed-capacity VecDeque for sparkline samples (60 by
//!   default, one per second of history at the default `--refresh-ms = 1000`).
//! - [`RateCalculator`] — turns Sōzu's cumulative `Count` metrics into
//!   per-second deltas. Detects the hourly `LocalDrain::clear` (which drops
//!   `Count`/`Time` while preserving Gauges) by looking for monotonic-decrease
//!   between samples and emits `0` for that tick instead of a negative spike.
//! - [`ThresholdTable`] — colour-coding rules: any time a value crosses a
//!   threshold (5xx ratio > 1 %, slab.usage_percent > 80, h2 flood counters
//!   non-zero) the relevant pane flips to a warning/critical hue.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use sozu_command_lib::proto::command::{
    AggregatedMetrics, ClusterMetrics, FilteredMetrics, filtered_metrics,
};
use sozu_lib::metrics::names;

use super::theme::GlyphMode;
use super::transport::{CertsSnapshot, ListenersSnapshot, Snapshot, TopEvent};

/// Default ring depth for the on-screen sparkline series. 60 samples = one
/// minute of history at the default 1 s data tick. Matches the proto
/// `FilteredTimeSerie.last_minute` cadence so swapping to a server-side
/// time series later is a one-line change.
pub const SPARKLINE_DEPTH: usize = 60;

/// Default capacity for the in-memory recent-events ring shown in the
/// EVENTS pane / overview footer. Larger than the transport's `bounded(64)`
/// so the UI can keep an audit-style scrollback without forcing the
/// transport to back-pressure on its own bound.
pub const EVENT_RING_DEPTH: usize = 200;

/// Number of consecutive frames a pulse stays visible. The tick decrement
/// is `if value == 0 { drop } else { value -= 1 }`, so a pulse inserted
/// with `N` visits `N → N-1 → … → 1 → 0` (dropped on the next tick).
/// That is `N + 1` rendered frames; `5` renders ~5 s of background tint
/// at the default 1 s data-tick cadence — long enough to catch the eye
/// without monopolising the colour-coding when multiple events land
/// close together. The name reflects the observed frame count rather
/// than the seed value to avoid the off-by-one trap.
pub const PULSE_PERSIST_FRAMES: u32 = 5;

/// Top-level pane selector mirrored to numbered tabs at the top of the
/// screen. Numbers match the keymap (`1 OVERVIEW`, `2 CLUSTERS`, …) so
/// muscle memory from hatop / btop / k9s carries over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveTab {
    Overview,
    Clusters,
    Backends,
    Listeners,
    Certs,
    H2,
    Events,
}

impl ActiveTab {
    pub const ALL: &'static [Self] = &[
        Self::Overview,
        Self::Clusters,
        Self::Backends,
        Self::Listeners,
        Self::Certs,
        Self::H2,
        Self::Events,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Overview => "OVERVIEW",
            Self::Clusters => "CLUSTERS",
            Self::Backends => "BACKENDS",
            Self::Listeners => "LISTENERS",
            Self::Certs => "CERTS",
            Self::H2 => "H2",
            Self::Events => "EVENTS",
        }
    }

    /// Map a 1-based key (`1`..`7`) to a tab. Returns `None` for keys
    /// outside the range so the caller can silently ignore.
    pub fn from_digit(d: u8) -> Option<Self> {
        Self::ALL.get(d.checked_sub(1)? as usize).copied()
    }

    /// Resolve a `:command-palette` alias to a tab. Centralises the
    /// alias table so adding a new tab only touches `ActiveTab` (label +
    /// from_alias) instead of also patching `apply_palette`.
    pub fn from_alias(alias: &str) -> Option<Self> {
        match alias {
            "overview" | "o" => Some(Self::Overview),
            "cluster" | "clusters" | "c" => Some(Self::Clusters),
            "backend" | "backends" | "b" => Some(Self::Backends),
            "listener" | "listeners" | "l" => Some(Self::Listeners),
            "cert" | "certs" => Some(Self::Certs),
            "h2" => Some(Self::H2),
            "event" | "events" | "e" => Some(Self::Events),
            _ => None,
        }
    }

    pub fn cycle(self, forward: bool) -> Self {
        let idx = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        let len = Self::ALL.len();
        let next = if forward {
            (idx + 1) % len
        } else {
            (idx + len - 1) % len
        };
        Self::ALL[next]
    }
}

/// Fixed-capacity sparkline ring. Newest sample at the back of the deque.
#[derive(Debug, Clone)]
pub struct SparkRing {
    samples: VecDeque<u64>,
    capacity: usize,
}

impl SparkRing {
    pub fn new(capacity: usize) -> Self {
        Self {
            samples: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn push(&mut self, value: u64) {
        if self.samples.len() == self.capacity {
            self.samples.pop_front();
        }
        self.samples.push_back(value);
    }

    pub fn samples(&self) -> std::collections::vec_deque::Iter<'_, u64> {
        self.samples.iter()
    }

    pub fn last(&self) -> Option<u64> {
        self.samples.back().copied()
    }

    pub fn max(&self) -> u64 {
        self.samples.iter().copied().max().unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Render the ring as a `Vec<u64>` so ratatui's `Sparkline` widget can
    /// consume it without holding a borrow.
    pub fn to_vec(&self) -> Vec<u64> {
        self.samples.iter().copied().collect()
    }
}

/// Derives per-second rates from Sōzu's cumulative `Count` metrics. Stores
/// the previous (value, instant) per metric key. `record` returns the rate
/// or `None` when:
///
/// - this is the first observation (no baseline yet),
/// - or the new value is less than the stored one (the hourly
///   `LocalDrain::clear` reset the counter — emit `0` rather than a
///   negative spike).
#[derive(Debug, Default)]
pub struct RateCalculator {
    history: HashMap<String, (i64, Instant)>,
}

impl RateCalculator {
    pub fn record(&mut self, key: &str, value: i64, sampled_at: Instant) -> Option<f64> {
        let result = match self.history.get(key) {
            Some((prev_value, prev_at)) if value >= *prev_value => {
                let dt = sampled_at.saturating_duration_since(*prev_at).as_secs_f64();
                if dt > 0.0 {
                    Some(((value - *prev_value) as f64) / dt)
                } else {
                    Some(0.0)
                }
            }
            Some(_) => Some(0.0), // hourly reset: emit 0, not a negative
            None => None,         // first observation: caller can show "—"
        };
        self.history.insert(key.to_owned(), (value, sampled_at));
        result
    }

    /// Drop history entries whose key the predicate rejects. Called from
    /// `App::ingest_snapshot` after recording with the per-cluster keys
    /// present in the freshest snapshot so disappearing clusters do not
    /// leave `(prev_value, prev_at)` lingering forever.
    pub fn retain<F: FnMut(&str) -> bool>(&mut self, mut keep: F) {
        self.history.retain(|k, _| keep(k.as_str()));
    }
}

/// What-changed pulse tracker. Snapshot-to-snapshot diffs surface as a
/// short-lived tint on the affected rows so the eye catches transitions
/// even when the operator looked away for a moment.
///
/// Three classes of pulse:
///
/// - `ClusterDisappeared(cluster_id)` fires when a cluster id present in
///   the previous `AggregatedMetrics.clusters` map is absent in the new
///   one. Operationally meaningful: the master reconfig dropped a
///   cluster, or the worker fleet went silent on it.
/// - `BackendWentDown(cluster_id, backend_id)` fires when a backend's
///   `backend.available` gauge transitioned from `>= 1` to `0`. The
///   companion `EventKind::BackendDown` event lands in the EVENTS pane;
///   this pulse highlights the BACKENDS row in lockstep.
/// - `ClusterAppeared(cluster_id)` fires when a new cluster id arrives —
///   surfaces fresh additions without burying them at the bottom of the
///   sort order. Lower-priority pulse (uses `cool` tier, not `hot`).
#[derive(Debug, Default)]
pub struct PulseTracker {
    cluster_disappeared: HashMap<String, u32>,
    cluster_appeared: HashMap<String, u32>,
    backend_down: HashMap<(String, String), u32>,
    last_clusters: HashSet<String>,
    last_backend_up: HashSet<(String, String)>,
}

impl PulseTracker {
    /// Decrement every active pulse by one render frame. Drop entries that
    /// reach zero. Called once per render tick by `App::tick_pulses`.
    fn tick(&mut self) {
        Self::tick_map(&mut self.cluster_disappeared);
        Self::tick_map(&mut self.cluster_appeared);
        Self::tick_map(&mut self.backend_down);
    }

    /// In-place retain: zero-aged entries are filtered out and survivors
    /// decrement by one. Same semantics for all three pulse maps; generic
    /// over the key so `(String, String)` (backend_down) reuses it.
    fn tick_map<K>(map: &mut HashMap<K, u32>) {
        map.retain(|_, v| {
            if *v == 0 {
                false
            } else {
                *v -= 1;
                true
            }
        });
    }

    /// Diff a new snapshot against the previous seen set and emit fresh
    /// pulses for every transition. Called from `App::ingest_snapshot`
    /// BEFORE the snapshot replaces `last_metrics`.
    fn diff(&mut self, m: &AggregatedMetrics) {
        let mut new_clusters: HashSet<String> = HashSet::new();
        let mut new_backend_up: HashSet<(String, String)> = HashSet::new();
        for (cluster_id, cm) in &m.clusters {
            new_clusters.insert(cluster_id.clone());
            for bm in &cm.backends {
                let available = bm
                    .metrics
                    .get(names::backend::AVAILABLE)
                    .and_then(|m| match m.inner.as_ref()? {
                        filtered_metrics::Inner::Gauge(v) => Some(*v),
                        _ => None,
                    })
                    .unwrap_or(0);
                if available >= 1 {
                    new_backend_up.insert((cluster_id.clone(), bm.backend_id.clone()));
                }
            }
        }
        // Cluster disappear: in last set, not in new set. Skip on the very
        // first snapshot to avoid pulsing for "every cluster appeared" on
        // startup (the first `diff` runs against an empty last_clusters).
        if !self.last_clusters.is_empty() {
            for missing in self.last_clusters.difference(&new_clusters) {
                self.cluster_disappeared
                    .insert(missing.clone(), PULSE_PERSIST_FRAMES);
            }
            // Cluster appear: in new set, not in last set.
            for fresh in new_clusters.difference(&self.last_clusters) {
                self.cluster_appeared
                    .insert(fresh.clone(), PULSE_PERSIST_FRAMES);
            }
        }
        // Backend down: was up in last snapshot, not up in new snapshot
        // (either backend.available dropped to 0 OR the backend row went
        // missing). Both cases surface as "this backend can't take traffic
        // right now" so they share a pulse class.
        for prev_up in &self.last_backend_up {
            if !new_backend_up.contains(prev_up) {
                self.backend_down
                    .insert(prev_up.clone(), PULSE_PERSIST_FRAMES);
            }
        }
        self.last_clusters = new_clusters;
        self.last_backend_up = new_backend_up;
    }

    pub fn cluster_pulse(&self, cluster_id: &str) -> Option<PulseKind> {
        self.cluster_disappeared
            .contains_key(cluster_id)
            .then_some(PulseKind::Disappeared)
            .or_else(|| {
                self.cluster_appeared
                    .contains_key(cluster_id)
                    .then_some(PulseKind::Appeared)
            })
    }

    pub fn backend_pulse(&self, cluster_id: &str, backend_id: &str) -> Option<PulseKind> {
        self.backend_down
            .contains_key(&(cluster_id.to_owned(), backend_id.to_owned()))
            .then_some(PulseKind::WentDown)
    }

    /// True when at least one pulse is still animating. The render loop
    /// uses this together with `App::take_dirty` to decide whether to
    /// repaint on a frame that received no fresh snapshot or event —
    /// without it, the fading background tint would freeze on screen
    /// between snapshots.
    pub fn has_active(&self) -> bool {
        !self.cluster_disappeared.is_empty()
            || !self.cluster_appeared.is_empty()
            || !self.backend_down.is_empty()
    }
}

/// Visual class of a pulse, mapped to a skin tier by the renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PulseKind {
    /// Cluster row vanished — operationally surprising. Hot tier.
    Disappeared,
    /// Cluster row appeared. Cool tier.
    Appeared,
    /// Backend transitioned from up to down. Hot tier.
    WentDown,
}

/// Threshold table used by the colour-coding rules. Each field is the
/// boundary above which the relevant pane flips its row/cell into a
/// warning/critical hue. Defaults are documented in `doc/sozu-top.md`
/// (week 4); for now we ship sane fixed values and revisit once operators
/// see the TUI in anger.
#[derive(Debug, Clone)]
pub struct ThresholdTable {
    /// % of requests that returned 5xx in the most recent tick. Rows above
    /// this go critical.
    pub error_ratio_critical_pct: f64,
    /// `slab.usage_percent` value above which the saturation sparkline goes hot.
    pub slab_critical_pct: f64,
    /// p99 in ms above which the latency sparkline goes hot.
    pub latency_p99_critical_ms: f64,
}

impl Default for ThresholdTable {
    fn default() -> Self {
        Self {
            error_ratio_critical_pct: 1.0,
            slab_critical_pct: 80.0,
            latency_p99_critical_ms: 500.0,
        }
    }
}

impl ThresholdTable {
    /// Return a short critical-banner headline if any of the configured
    /// thresholds is currently crossed. Used by the renderer to drive the
    /// big-text alert overlay. `None` means "everything's fine"; the
    /// renderer skips the banner row in that case.
    pub fn critical_message(&self, overview: &OverviewState) -> Option<&'static str> {
        if overview
            .latency_p99_ms
            .last()
            .is_some_and(|v| v as f64 >= self.latency_p99_critical_ms)
        {
            return Some("HIGH LATENCY");
        }
        if overview
            .service_time_p99_ms
            .last()
            .is_some_and(|v| v as f64 >= self.latency_p99_critical_ms)
        {
            return Some("SOZU SLOW");
        }
        if overview
            .saturation_pct
            .last()
            .is_some_and(|v| v as f64 >= self.slab_critical_pct)
        {
            return Some("SATURATION");
        }
        None
    }
}

/// HTTP 5xx error-status counters Sōzu synthesises as default answers
/// (500, 502, 503, 504, 507). Hoisted out of the cluster_rows /
/// fold_overview iterators so both call sites share one source of
/// truth for "what counts as a 5xx error in operator dashboards" and a
/// new error variant can be added in one place.
const ERRORS_5XX: [&str; 5] = [
    names::http_status::S500,
    names::http_status::S502,
    names::http_status::S503,
    names::http_status::S504,
    names::http_status::S507,
];

/// Synthetic `RateCalculator` key for the aggregate overview RPS.
/// Namespaced so it cannot collide with the per-cluster keys
/// `cluster_rate_key` produces. Hidden from operator-facing surfaces.
const OVERVIEW_REQUESTS_KEY: &str = "__overview.requests";

/// Build the `RateCalculator` key for a cluster's requests counter.
/// Same `__cluster.` namespace as other future per-cluster series.
fn cluster_rate_key(cluster_id: &str) -> String {
    format!("__cluster.{cluster_id}.requests")
}

/// Build the `RateCalculator` key for a per-(cluster, backend) counter.
/// `suffix` distinguishes `bytes_in` vs `bytes_out` so both rates share
/// one namespace without aliasing.
fn backend_rate_key(cluster_id: &str, backend_id: &str, suffix: &str) -> String {
    format!("__backend.{cluster_id}.{backend_id}.{suffix}")
}

/// Proxy-level metric keys the H2 pane plots in its trend column.
/// Kept in one place so a key rename in `lib::metrics::names` cannot
/// silently drop a sparkline from the pane.
const H2_TRACKED_KEYS: &[&str] = &[
    names::h2::CONNECTION_ACTIVE_STREAMS,
    names::http::ALPN_H2,
    names::http::ALPN_HTTP11,
    names::client::CONNECTIONS,
    names::h2::CONNECTION_WINDOW_BYTES,
    names::h2::CONNECTION_PENDING_WINDOW_UPDATES,
    names::h2::FLOW_CONTROL_STALL,
    names::h2::FRAMES_TX_WINDOW_UPDATE,
    names::h2::FRAMES_TX_RST_STREAM,
    names::h2::FRAMES_TX_GOAWAY,
    names::h2::HEADERS_REJECTED_BUDGET_OVERRUN,
    names::h2::FLOOD_VIOLATION_GLITCH_WINDOW,
    names::h2::FLOOD_VIOLATION_RAPID_RESET,
    names::h2::FLOOD_VIOLATION_CONTINUATION,
    names::h2::FLOOD_VIOLATION_MADE_YOU_RESET,
    names::h2::FLOOD_VIOLATION_PING,
    names::h2::FLOOD_VIOLATION_SETTINGS,
    names::h2::FLOOD_VIOLATION_PRIORITY,
    names::h2::WINDOW_UPDATE_DROPPED,
    names::h2::CLOSE_WITH_ACTIVE_STREAMS,
];

/// Inline sparkline as a single-line glyph string. Used in table cells
/// where the ratatui `Sparkline` widget is overkill. `alphabet` is the
/// glyph ramp from `GlyphMode::trend_alphabet` (Block / Braille / Tty);
/// samples are scaled to the ring's max so a flat-zero series prints
/// as the lowest glyph on every position.
fn render_spark_bars<I: IntoIterator<Item = u64>>(samples: I, alphabet: &[char]) -> String {
    let samples: Vec<u64> = samples.into_iter().collect();
    if samples.is_empty() || alphabet.is_empty() {
        return "—".to_owned();
    }
    let max = samples.iter().copied().max().unwrap_or(0).max(1);
    let last_idx = alphabet.len() - 1;
    samples
        .iter()
        .map(|v| {
            let idx = ((v * last_idx as u64) / max) as usize;
            alphabet[idx.min(last_idx)]
        })
        .collect()
}

/// Top-level UI state. Pure data; the render loop snapshots it for each
/// frame and the transport threads push into it via `App::ingest_*`.
#[derive(Debug)]
pub struct App {
    pub active_tab: ActiveTab,
    pub overview: OverviewState,
    pub events: VecDeque<TopEvent>,
    pub thresholds: ThresholdTable,
    pub last_snapshot_at: Option<Instant>,
    /// Most recent `AggregatedMetrics` retained verbatim so panes that need
    /// the live cluster / backend / listener map (CLUSTERS, BACKENDS, …)
    /// don't have to maintain their own derivation. The OVERVIEW pane
    /// reads ring buffers, not this; this is for table-shaped panes.
    pub last_metrics: Option<AggregatedMetrics>,
    /// Most recent `ListenersList` from the listeners-collector thread.
    /// Polled at a slower cadence than metrics (5 s) because listener
    /// state changes are operator-paced.
    pub last_listeners: Option<ListenersSnapshot>,
    /// Most recent certificate inventory from the certs-collector thread.
    /// Polled every 30 s — cert mutations flow through the EVENTS pane
    /// in real time, so the slow refresh is fine for the inventory view.
    pub last_certs: Option<CertsSnapshot>,
    pub status: String,
    pub should_quit: bool,
    pub help_visible: bool,
    /// Cluster-table sort column. Default surfaces unhealthy/error-rate
    /// first, then RPS — operators want the failing clusters at the top of
    /// the pane so the eye lands on them without scrolling.
    pub cluster_sort: ClusterSortKey,
    /// Reverse the cluster-table sort ordering when `true`.
    pub cluster_sort_reverse: bool,
    /// Backend-table sort column. Default `Bandwidth` (busiest backend at
    /// the top).
    pub backend_sort: BackendSortKey,
    pub backend_sort_reverse: bool,
    /// "What changed" tracker — surfaces disappearing clusters and newly-
    /// unhealthy backends as a short-lived row tint.
    pub pulse: PulseTracker,
    /// Resolved glyph mode (Braille / Block / TTY-ASCII). Set once at
    /// startup by `GlyphMode::resolve(cfg.glyphs)`. Panes that draw
    /// sparkline-adjacent custom glyphs read this to pick a ramp; the
    /// rest of the UI uses ratatui's built-in `Sparkline` widget which
    /// has its own internal ramp.
    pub glyphs: GlyphMode,
    /// k9s-style colon palette state. When `palette_open` is true the
    /// renderer replaces the function-key bar with a one-line input
    /// box; `palette_input` carries the in-progress text.
    pub palette_open: bool,
    pub palette_input: tui_input::Input,
    /// Last unknown command or recoverable error from the palette. The
    /// renderer surfaces this on the function-key bar so the operator
    /// sees why their command bounced.
    pub palette_error: Option<String>,
    /// Dirty flag for the render loop: set whenever an `ingest_*` call
    /// folds new data into the state OR a pulse decrements in
    /// `tick_pulses`. The renderer checks this via `take_dirty` and
    /// skips the synchronized-update + draw cycle when neither path
    /// produced a visible change AND no pulse is mid-animation. Avoids
    /// burning ~2-3 % of one core for an idle TUI on a quiet system.
    /// Initialised `true` so the first frame paints unconditionally.
    is_dirty: bool,
    rates: RateCalculator,
    /// Per-cluster requests-per-second derived from `names::backend::REQUESTS`
    /// counters. Computed once per `ingest_snapshot` via `RateCalculator`
    /// and consumed by `cluster_rows` so the CLUSTERS pane shows a rate,
    /// not a cumulative counter.
    cluster_rps: HashMap<String, u64>,
    /// Per-(cluster, backend) bytes-per-second download rate derived
    /// from cumulative `names::backend::BYTES_IN` counters. Stored as
    /// bytes/sec; the renderer scales to bits/sec at format time so
    /// the column reads in Kbps / Mbps / Gbps. Cleared and rebuilt on
    /// every `ingest_snapshot`.
    backend_rate_in_bps: HashMap<(String, String), f64>,
    /// Per-(cluster, backend) bytes-per-second upload rate derived
    /// from cumulative `names::backend::BYTES_OUT`.
    backend_rate_out_bps: HashMap<(String, String), f64>,
    /// Per-metric sample rings driving the H2 pane's trend column.
    /// Populated by `fold_h2_trends` once per `ingest_snapshot`. Keys
    /// are the canonical `names::*` strings the pane reads, so a key
    /// rename can never silently freeze a trend column.
    h2_trends: HashMap<&'static str, SparkRing>,
    /// Snapshot ingest is suspended when `true`. The transport keeps
    /// polling and the cardinality lease keeps renewing, but the App
    /// ignores incoming `Snapshot`s so the visible state is frozen on
    /// the last frame. Toggled by F5 / `p`.
    pub paused: bool,
}

/// Columns the CLUSTERS pane can sort by. Cycled with `s`; reversed with
/// `S`. Default: `ErrorRate` desc → unhealthy clusters surface first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClusterSortKey {
    ClusterId,
    Rps,
    ErrorRate,
    LatencyP99,
    BackendsAvailable,
}

impl ClusterSortKey {
    pub const ALL: &'static [Self] = &[
        Self::ErrorRate,
        Self::Rps,
        Self::LatencyP99,
        Self::BackendsAvailable,
        Self::ClusterId,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::ClusterId => "id",
            Self::Rps => "rps",
            Self::ErrorRate => "err%",
            Self::LatencyP99 => "p99",
            Self::BackendsAvailable => "backends",
        }
    }

    pub fn cycle(self) -> Self {
        let idx = Self::ALL.iter().position(|k| *k == self).unwrap_or(0);
        Self::ALL[(idx + 1) % Self::ALL.len()]
    }
}

#[derive(Debug, Default)]
pub struct OverviewState {
    pub rps: SparkRing,
    pub latency_p99_ms: SparkRing,
    /// p99 of sozu's own `service_time` (request processing inside the
    /// proxy, NOT including the backend round-trip). Sparkline kept as
    /// raw milliseconds for direct comparison with `latency_p99_ms`.
    pub service_time_p99_ms: SparkRing,
    pub saturation_pct: SparkRing,
    /// Active session gauge (`http.active_requests` or fallback). Surfaced
    /// as a big numeral above the saturation sparkline.
    pub active_sessions: u64,
    /// `client.connections` gauge. Surfaced as a big numeral above the
    /// RPS sparkline.
    pub client_connections: u64,
}

impl Default for SparkRing {
    fn default() -> Self {
        Self::new(SPARKLINE_DEPTH)
    }
}

impl App {
    pub fn new() -> Self {
        Self {
            active_tab: ActiveTab::Overview,
            overview: OverviewState::default(),
            events: VecDeque::with_capacity(EVENT_RING_DEPTH),
            thresholds: ThresholdTable::default(),
            last_snapshot_at: None,
            last_metrics: None,
            last_listeners: None,
            last_certs: None,
            status: String::new(),
            should_quit: false,
            help_visible: false,
            cluster_sort: ClusterSortKey::ErrorRate,
            cluster_sort_reverse: false,
            backend_sort: BackendSortKey::Bandwidth,
            backend_sort_reverse: false,
            pulse: PulseTracker::default(),
            glyphs: GlyphMode::Block,
            palette_open: false,
            palette_input: tui_input::Input::default(),
            palette_error: None,
            is_dirty: true,
            rates: RateCalculator::default(),
            cluster_rps: HashMap::new(),
            backend_rate_in_bps: HashMap::new(),
            backend_rate_out_bps: HashMap::new(),
            h2_trends: HashMap::new(),
            paused: false,
        }
    }

    /// Read-and-clear the dirty flag for the renderer. Returns `true` when
    /// the App folded new data or aged a pulse since the last frame; the
    /// renderer combines this with `PulseTracker::has_active()` to decide
    /// whether to repaint.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::replace(&mut self.is_dirty, false)
    }

    /// Force the next frame to redraw. Used by the render loop when it
    /// drains an out-of-band status message (renewer error) into
    /// `App::status` and the operator needs the F-key bar repainted on
    /// the next tick even though no snapshot landed.
    pub fn mark_dirty(&mut self) {
        self.is_dirty = true;
    }

    /// Fold an inbound transport `Snapshot` into the ring buffers. Called by
    /// the render loop on every drain of the `crossbeam_channel`. Cheap (one
    /// linear scan over the proxy / cluster maps).
    pub fn ingest_snapshot(&mut self, snap: &Snapshot) {
        if self.paused {
            // F5 / `p` freezes the visible state without dropping the
            // transport lease. Skip the entire fold so sparklines and
            // tables stay on the last frame instead of advancing.
            return;
        }
        self.last_snapshot_at = Some(snap.received_at);
        self.fold_overview(&snap.metrics, snap.received_at);
        self.fold_h2_trends(&snap.metrics);
        // Diff vs the last seen snapshot BEFORE replacing it, so the
        // pulse tracker can emit ClusterDisappeared / BackendWentDown
        // transitions against the previous set.
        self.pulse.diff(&snap.metrics);
        // Keep the latest `AggregatedMetrics` for the CLUSTERS / BACKENDS
        // panes. Cloning isn't cheap for very-high-cardinality fleets
        // (>1000 clusters), but in practice the master already paid this
        // cost on the wire; revisit only if profile data justifies it.
        self.last_metrics = Some(snap.metrics.clone());
        self.is_dirty = true;
    }

    /// Push the latest sample for every H2-pane-tracked metric into its
    /// SparkRing. Reads gauges and counters uniformly so flow-control
    /// gauges and frame counters share one trend renderer. New keys are
    /// allocated on first observation; the SparkRing rolls a 60-sample
    /// window forward as snapshots arrive.
    fn fold_h2_trends(&mut self, m: &AggregatedMetrics) {
        for key in H2_TRACKED_KEYS {
            let value = gauge_value(m.proxying.get(*key))
                .or_else(|| count_value(m.proxying.get(*key)).map(|c| c.max(0) as u64))
                .unwrap_or(0);
            self.h2_trends
                .entry(*key)
                .or_insert_with(|| SparkRing::new(SPARKLINE_DEPTH))
                .push(value);
        }
    }

    /// Render the trend bars for an H2-pane metric. Returns "—" when no
    /// samples have landed (cold start). Otherwise produces a string of
    /// characters from the resolved `GlyphMode` alphabet, scaled to the
    /// largest sample in the ring so a flat-line zero series prints as
    /// the minimum-height character.
    pub fn h2_trend_bars(&self, key: &str) -> String {
        match self.h2_trends.get(key) {
            Some(ring) if !ring.is_empty() => {
                render_spark_bars(ring.samples().copied(), self.glyphs.trend_alphabet())
            }
            _ => "—".to_owned(),
        }
    }

    /// Advance the pulse tracker by one render frame. Called from the
    /// render loop's draw step so pulses decay even when no new snapshot
    /// arrived this frame. Sets the dirty flag when at least one pulse
    /// was still active before the tick so the next frame paints the
    /// fading tint instead of freezing it on screen.
    pub fn tick_pulses(&mut self) {
        if self.pulse.has_active() {
            self.is_dirty = true;
        }
        self.pulse.tick();
    }

    /// Build the per-cluster summary rows for the CLUSTERS pane. Computed
    /// on demand (renderer call) rather than cached so the sort + reverse
    /// state can change between frames without a full re-fold.
    pub fn cluster_rows(&self) -> Vec<ClusterRow> {
        let metrics = match self.last_metrics.as_ref() {
            Some(m) => m,
            None => return Vec::new(),
        };
        let mut rows: Vec<ClusterRow> = metrics
            .clusters
            .iter()
            .map(|(id, cm)| {
                let requests = cluster_count_total(cm, names::backend::REQUESTS);
                let errors_5xx: i64 = ERRORS_5XX.iter().map(|k| cluster_count_total(cm, k)).sum();
                let p99_ms = cluster_p99_max(cm);
                let p50_ms = cluster_p50_max(cm);
                // Backends-up vs backends-total. Three reading paths,
                // tried in order, so the column populates whatever
                // metric_detail level or worker lifecycle stage the
                // operator is in.
                //
                // 1. `cluster.total_backends` / `cluster.available_backends`
                //    cluster-level rollup gauges. These are the
                //    authoritative per-cluster aggregates published by
                //    `BackendMap::record_cluster_availability` whenever a
                //    backend registers, transitions, or first serves a
                //    request. Survives the hourly counter clear.
                // 2. Per-backend `backend.available` gauge summed across
                //    `cm.backends[].metrics`. Populated under backend-
                //    detail filing whenever a backend up/down event
                //    fires. Independent of the cluster-level rollup so a
                //    fresh worker before the first `record_cluster_…`
                //    call still surfaces something.
                // 3. `cm.backends.len()` as a last-resort total. Under
                //    backend-detail every backend that emitted any
                //    metric (bytes, response_time, requests) lands in
                //    this Vec, so the cardinality is a useful lower
                //    bound on "backends the worker has seen this
                //    minute" even before any cluster-level gauge
                //    arrives.
                let rollup_total =
                    gauge_value(cm.cluster.get(names::cluster::TOTAL_BACKENDS)).unwrap_or(0) as u32;
                let rollup_available =
                    gauge_value(cm.cluster.get(names::cluster::AVAILABLE_BACKENDS)).unwrap_or(0)
                        as u32;
                let backend_available_sum: u32 = cm
                    .backends
                    .iter()
                    .filter_map(|b| gauge_value(b.metrics.get(names::backend::AVAILABLE)))
                    .map(|v| v as u32)
                    .sum();
                let backends_total = if rollup_total > 0 {
                    rollup_total
                } else {
                    cm.backends.len() as u32
                };
                // Final fallback: if the rollup gauge hasn't been
                // published yet and no per-backend gauge is
                // present either, assume every backend the worker
                // has observed is up. The first health-check
                // failure / `record_cluster_availability` call
                // refreshes this with the authoritative value.
                let backends_available = if rollup_total == 0 && backend_available_sum == 0 {
                    cm.backends.len() as u32
                } else {
                    rollup_available.max(backend_available_sum)
                };
                let error_rate_pct = if requests > 0 {
                    (errors_5xx as f64 / requests as f64) * 100.0
                } else {
                    0.0
                };
                ClusterRow {
                    cluster_id: id.clone(),
                    rps: self.cluster_rps.get(id).copied().unwrap_or(0),
                    error_rate_pct,
                    p50_ms,
                    p99_ms,
                    backends_total,
                    backends_available,
                }
            })
            .collect();
        rows.sort_by(|a, b| {
            use std::cmp::Ordering;
            let ord = match self.cluster_sort {
                ClusterSortKey::ClusterId => a.cluster_id.cmp(&b.cluster_id),
                ClusterSortKey::Rps => a.rps.cmp(&b.rps).reverse(),
                ClusterSortKey::ErrorRate => a
                    .error_rate_pct
                    .partial_cmp(&b.error_rate_pct)
                    .unwrap_or(Ordering::Equal)
                    .reverse(),
                ClusterSortKey::LatencyP99 => a.p99_ms.cmp(&b.p99_ms).reverse(),
                ClusterSortKey::BackendsAvailable => {
                    a.backends_available.cmp(&b.backends_available)
                }
            };
            if self.cluster_sort_reverse {
                ord.reverse()
            } else {
                ord
            }
        });
        rows
    }

    /// Build the per-backend rows for the BACKENDS pane. Flattens every
    /// `(cluster_id, BackendMetrics)` pair across the freshest snapshot.
    /// Sorted per `backend_sort` / `backend_sort_reverse`.
    pub fn backend_rows(&self) -> Vec<BackendRow> {
        let metrics = match self.last_metrics.as_ref() {
            Some(m) => m,
            None => return Vec::new(),
        };
        let mut rows: Vec<BackendRow> = Vec::new();
        for (cluster_id, cm) in &metrics.clusters {
            for bm in &cm.backends {
                // Per-second rate from the cumulative `BYTES_IN`/`OUT`
                // counters. The rate map is populated by `fold_overview`
                // on every snapshot so the lookup is constant-time and
                // the row stays in lock-step with the freshest poll.
                let key = (cluster_id.clone(), bm.backend_id.clone());
                let bw_in_bps = self.backend_rate_in_bps.get(&key).copied().unwrap_or(0.0);
                let bw_out_bps = self.backend_rate_out_bps.get(&key).copied().unwrap_or(0.0);
                rows.push(BackendRow {
                    cluster_id: cluster_id.clone(),
                    backend_id: bm.backend_id.clone(),
                    bw_in_bps,
                    bw_out_bps,
                    connections: gauge_value(
                        bm.metrics.get(names::backend::CONNECTIONS_PER_BACKEND),
                    )
                    .unwrap_or(0),
                    p50_ms: percentile_p50_ms(bm.metrics.get(names::backend::RESPONSE_TIME))
                        .unwrap_or(0),
                    p99_ms: percentile_p99_ms(bm.metrics.get(names::backend::RESPONSE_TIME))
                        .unwrap_or(0),
                    requests_total: count_value(bm.metrics.get(names::backend::REQUESTS))
                        .unwrap_or(0) as u64,
                });
            }
        }
        rows.sort_by(|a, b| {
            use std::cmp::Ordering;
            let ord = match self.backend_sort {
                BackendSortKey::ClusterId => a
                    .cluster_id
                    .cmp(&b.cluster_id)
                    .then(a.backend_id.cmp(&b.backend_id)),
                BackendSortKey::BackendId => a.backend_id.cmp(&b.backend_id),
                BackendSortKey::Bandwidth => {
                    let abw = a.bw_in_bps + a.bw_out_bps;
                    let bbw = b.bw_in_bps + b.bw_out_bps;
                    abw.partial_cmp(&bbw).unwrap_or(Ordering::Equal).reverse()
                }
                BackendSortKey::Connections => a.connections.cmp(&b.connections).reverse(),
                BackendSortKey::LatencyP99 => a.p99_ms.cmp(&b.p99_ms).reverse(),
                BackendSortKey::Requests => a.requests_total.cmp(&b.requests_total).reverse(),
            };
            // `Ordering::reverse` chains nicely with `.then()` above; if all
            // primary keys tie we drop to (cluster_id, backend_id) lex order
            // for a deterministic on-screen layout. The comparator MUST
            // return `Equal` on full-tie; the reverse direction is applied
            // by `rows.reverse()` after the sort. Returning `Greater` on
            // self-comparison would violate strict-weak-ordering.
            ord.then_with(|| a.cluster_id.cmp(&b.cluster_id))
                .then_with(|| a.backend_id.cmp(&b.backend_id))
        });
        if self.backend_sort_reverse {
            rows.reverse();
        }
        rows
    }

    fn fold_overview(&mut self, m: &AggregatedMetrics, sampled_at: Instant) {
        // Sōzu emits the canonical per-request counter (`names::backend::REQUESTS`)
        // from `record_backend_metrics!`. The cardinality knob routes the
        // counter to either `cm.cluster[REQUESTS]` (detail = cluster, default)
        // or `cm.backends[i].metrics[REQUESTS]` (detail = backend, the level
        // `sozu top` auto-leases on startup). `cluster_count_total` handles
        // both filings transparently so the OVERVIEW stays correct regardless
        // of the active detail level.
        let mut total_requests: i64 = 0;
        let mut total_requests_observed: i64 = 0;
        // Per-cluster RPS via the same RateCalculator. Stored on
        // `cluster_rps` so the CLUSTERS pane can render rates without
        // re-deriving from the cumulative counter. Keys are namespaced
        // `__cluster.<id>.requests` to keep them separate from the
        // overview-aggregate keys.
        self.cluster_rps.clear();
        self.backend_rate_in_bps.clear();
        self.backend_rate_out_bps.clear();
        let mut live_rate_keys: HashSet<String> = HashSet::new();
        live_rate_keys.insert(OVERVIEW_REQUESTS_KEY.to_owned());
        for (id, cm) in &m.clusters {
            let cluster_requests = cluster_count_total(cm, names::backend::REQUESTS);
            if cluster_requests > 0 {
                total_requests = total_requests.saturating_add(cluster_requests);
                total_requests_observed = total_requests_observed.saturating_add(cluster_requests);
            }
            let key = cluster_rate_key(id);
            let rate = self
                .rates
                .record(&key, cluster_requests, sampled_at)
                .unwrap_or(0.0)
                .max(0.0);
            self.cluster_rps.insert(id.clone(), rate as u64);
            live_rate_keys.insert(key);
            // Per-backend bytes-in / bytes-out rates. `record_backend_metrics!`
            // (`lib/src/metrics/mod.rs`) emits the cumulative byte counters
            // per (cluster, backend) at request end; the RateCalculator
            // turns those cumulative counts into a per-second delta the
            // BACKENDS pane renders as Mbps. First observation returns
            // `None` so the row prints `0.00 / 0.00` until the second
            // snapshot lands — matches the cluster_rps shape.
            for bm in &cm.backends {
                let bid = &bm.backend_id;
                let bytes_in = count_value(bm.metrics.get(names::backend::BYTES_IN)).unwrap_or(0);
                let bytes_out = count_value(bm.metrics.get(names::backend::BYTES_OUT)).unwrap_or(0);
                let in_key = backend_rate_key(id, bid, "bytes_in");
                let out_key = backend_rate_key(id, bid, "bytes_out");
                let rate_in = self
                    .rates
                    .record(&in_key, bytes_in, sampled_at)
                    .unwrap_or(0.0)
                    .max(0.0);
                let rate_out = self
                    .rates
                    .record(&out_key, bytes_out, sampled_at)
                    .unwrap_or(0.0)
                    .max(0.0);
                let map_key = (id.clone(), bid.clone());
                self.backend_rate_in_bps.insert(map_key.clone(), rate_in);
                self.backend_rate_out_bps.insert(map_key, rate_out);
                live_rate_keys.insert(in_key);
                live_rate_keys.insert(out_key);
            }
        }
        // Drop history entries for clusters / backends that disappeared
        // this tick; otherwise `RateCalculator.history` would accumulate
        // stale `(prev_value, prev_at)` for every (cluster, backend)
        // tuple ever seen.
        self.rates.retain(|k| live_rate_keys.contains(k));
        // Final fallback when no per-cluster counter is populated yet (no
        // backend round-trip completed since the worker started, OR
        // `metric_detail = process` strips both labels). `http.requests` is
        // incremented at request-receive time without cluster_id, so it
        // surfaces traffic the user is generating even before the first
        // response cycle finishes.
        if total_requests_observed == 0 {
            if let Some(v) = count_value(m.proxying.get(names::http::REQUESTS)) {
                total_requests = total_requests.saturating_add(v);
            }
        }

        // Per-second rate from the cumulative counter. The first observation
        // returns `None` and shows as 0 in the ring (nothing useful to plot
        // before we have a baseline).
        let rps = self
            .rates
            .record(OVERVIEW_REQUESTS_KEY, total_requests, sampled_at)
            .unwrap_or(0.0)
            .max(0.0);
        self.overview.rps.push(rps as u64);

        // Sozu's own request-processing time (`service_time`, distinct
        // from `backend_response_time`). p99 milliseconds matches the
        // shape of the LATENCY p99 cell so operators can compare the
        // two at a glance.
        let service_p99 =
            percentile_p99_ms(m.proxying.get(names::event_loop::SERVICE_TIME)).unwrap_or(0);
        self.overview.service_time_p99_ms.push(service_p99);

        // Latency p99 — sum of cluster `backend_response_time` percentiles
        // is not meaningful (you cannot average percentiles), so we take
        // the max p99 across clusters. Operators reading the OVERVIEW want
        // "is anyone slow?" not "average latency". `cluster_p99_max` also
        // walks per-backend filings so the answer holds under any detail
        // level.
        let max_p99_ms = m.clusters.values().map(cluster_p99_max).max().unwrap_or(0);
        self.overview.latency_p99_ms.push(max_p99_ms);

        // Saturation: prefer `slab.usage_percent`; fall back to
        // `client.connections` / `client.connections_max` ratio when the
        // gauge isn't surfaced. Both are gauges so they survive the hourly
        // counter clear unchanged.
        let saturation = gauge_value(m.proxying.get(names::slab::USAGE_PERCENT))
            .or_else(|| gauge_value(m.proxying.get(names::buffer::USAGE_PERCENT)))
            .map(|v| v.min(100))
            .unwrap_or(0);
        self.overview.saturation_pct.push(saturation as u64);

        self.overview.active_sessions =
            gauge_value(m.proxying.get(names::http::ACTIVE_REQUESTS)).unwrap_or(0) as u64;
        self.overview.client_connections =
            gauge_value(m.proxying.get(names::client::CONNECTIONS)).unwrap_or(0) as u64;
    }

    /// Append a transport-published `TopEvent` into the recent-events ring.
    /// Drops the oldest when capacity is reached so the UI shows a sliding
    /// window of the last `EVENT_RING_DEPTH` events.
    pub fn ingest_event(&mut self, event: TopEvent) {
        if self.events.len() == EVENT_RING_DEPTH {
            self.events.pop_front();
        }
        self.events.push_back(event);
        self.is_dirty = true;
    }

    /// Replace the cached listener inventory with a fresh snapshot.
    pub fn ingest_listeners(&mut self, snap: ListenersSnapshot) {
        self.last_listeners = Some(snap);
        self.is_dirty = true;
    }

    /// Replace the cached certificate inventory with a fresh snapshot.
    pub fn ingest_certs(&mut self, snap: CertsSnapshot) {
        self.last_certs = Some(snap);
        self.is_dirty = true;
    }

    /// Open the colon palette and clear any pending error. Called by the
    /// renderer when the operator presses `:`. Marks the App dirty so the
    /// render loop's dirty-gate paints the palette line on the next frame
    /// instead of waiting for the next snapshot tick (~1 s on a quiet
    /// system).
    pub fn open_palette(&mut self) {
        self.palette_open = true;
        self.palette_input = tui_input::Input::default();
        self.palette_error = None;
        self.is_dirty = true;
    }

    /// Close the palette without applying the in-progress command.
    /// Called on Escape / Ctrl-C while the palette is open. Marks dirty
    /// so the F-key bar redraws over the dismissed palette immediately.
    pub fn cancel_palette(&mut self) {
        self.palette_open = false;
        self.palette_input = tui_input::Input::default();
        self.is_dirty = true;
    }

    /// Apply the in-progress palette command. Recognised commands:
    ///
    /// - `:overview` / `:o` — jump to OVERVIEW.
    /// - `:cluster` / `:clusters` / `:c` — jump to CLUSTERS.
    /// - `:backend` / `:backends` / `:b` — jump to BACKENDS.
    /// - `:listener` / `:listeners` / `:l` — jump to LISTENERS.
    /// - `:cert` / `:certs` — jump to CERTS.
    /// - `:h2` — jump to H2.
    /// - `:event` / `:events` / `:e` — jump to EVENTS.
    /// - `:help` / `:h` / `:?` — toggle help.
    /// - `:quit` / `:q` — exit cleanly.
    ///
    /// Unknown commands flip `palette_error`; the renderer surfaces
    /// the message on the function-key bar.
    pub fn apply_palette(&mut self) {
        let raw = self.palette_input.value().trim().to_owned();
        let cmd = raw.trim_start_matches(':');
        if let Some(tab) = ActiveTab::from_alias(cmd) {
            self.active_tab = tab;
        } else {
            match cmd {
                "help" | "h" | "?" => self.help_visible = !self.help_visible,
                "quit" | "q" => self.should_quit = true,
                "" => {} // empty command — just close the palette
                other => {
                    self.palette_error = Some(format!("unknown command: :{other}"));
                    self.palette_open = false;
                    // Clear the input on the unknown-command path too so
                    // the next `:` keypress opens a fresh palette rather
                    // than re-populating with the operator's previous
                    // typo. Mirrors the success path (line below).
                    self.palette_input = tui_input::Input::default();
                    // palette_error is rendered on the F-key bar; the
                    // dirty-gate would otherwise hide the message until
                    // the next snapshot or pulse-tick.
                    self.is_dirty = true;
                    return;
                }
            }
        }
        self.palette_open = false;
        self.palette_input = tui_input::Input::default();
        self.palette_error = None;
        // Apply always mutates visible state (active_tab / help_visible /
        // should_quit / palette_error), so the next frame must repaint
        // regardless of which branch ran above.
        self.is_dirty = true;
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

pub(super) fn count_value(metric: Option<&FilteredMetrics>) -> Option<i64> {
    let inner = metric?.inner.as_ref()?;
    match inner {
        filtered_metrics::Inner::Count(v) => Some(*v),
        _ => None,
    }
}

pub(super) fn gauge_value(metric: Option<&FilteredMetrics>) -> Option<u64> {
    let inner = metric?.inner.as_ref()?;
    match inner {
        filtered_metrics::Inner::Gauge(v) => Some(*v),
        _ => None,
    }
}

fn percentile_p99_ms(metric: Option<&FilteredMetrics>) -> Option<u64> {
    let inner = metric?.inner.as_ref()?;
    match inner {
        filtered_metrics::Inner::Percentiles(p) => Some(p.p_99),
        _ => None,
    }
}

fn percentile_p50_ms(metric: Option<&FilteredMetrics>) -> Option<u64> {
    let inner = metric?.inner.as_ref()?;
    match inner {
        filtered_metrics::Inner::Percentiles(p) => Some(p.p_50),
        _ => None,
    }
}

/// Sum a count counter across both filings of the cardinality knob: the
/// cluster-level entry (`metric_detail = cluster`, default) AND the
/// per-backend entries (`metric_detail = backend`, the level the TUI
/// auto-elevates to on startup). The two filings are disjoint under any
/// given detail level — only one is populated at a time — so summing
/// both yields the correct cluster-wide total in either configuration.
fn cluster_count_total(cm: &ClusterMetrics, key: &str) -> i64 {
    let cluster_level = count_value(cm.cluster.get(key)).unwrap_or(0);
    let backend_sum: i64 = cm
        .backends
        .iter()
        .filter_map(|b| count_value(b.metrics.get(key)))
        .sum();
    cluster_level.saturating_add(backend_sum)
}

/// Max of the cluster-level percentile and every per-backend percentile.
/// Under cluster-detail the backends list is empty; under backend-detail
/// only the per-backend entries are populated. Taking the max matches
/// "is anyone slow?" — the operator-facing intent of the OVERVIEW pane.
fn cluster_p99_max(cm: &ClusterMetrics) -> u64 {
    let cluster_level = percentile_p99_ms(cm.cluster.get(names::backend::RESPONSE_TIME));
    let backend_max = cm
        .backends
        .iter()
        .filter_map(|b| percentile_p99_ms(b.metrics.get(names::backend::RESPONSE_TIME)))
        .max();
    cluster_level
        .into_iter()
        .chain(backend_max)
        .max()
        .unwrap_or(0)
}

fn cluster_p50_max(cm: &ClusterMetrics) -> u64 {
    let cluster_level = percentile_p50_ms(cm.cluster.get(names::backend::RESPONSE_TIME));
    let backend_max = cm
        .backends
        .iter()
        .filter_map(|b| percentile_p50_ms(b.metrics.get(names::backend::RESPONSE_TIME)))
        .max();
    cluster_level
        .into_iter()
        .chain(backend_max)
        .max()
        .unwrap_or(0)
}

/// Per-cluster row produced by `App::cluster_rows`. Pure data; the renderer
/// turns this into a `Row` widget. Pulse-tint state comes in week 3.
#[derive(Debug, Clone)]
pub struct ClusterRow {
    pub cluster_id: String,
    /// Per-cluster requests-per-second derived once on `ingest_snapshot`
    /// via the shared `RateCalculator` and cached on `App.cluster_rps`.
    /// `0` until two snapshots have landed for this cluster.
    pub rps: u64,
    pub error_rate_pct: f64,
    pub p50_ms: u64,
    pub p99_ms: u64,
    pub backends_total: u32,
    pub backends_available: u32,
}

/// Per-backend row produced by `App::backend_rows`. Flattens every
/// `(cluster_id, BackendMetrics)` pair into a single sortable list.
#[derive(Debug, Clone)]
pub struct BackendRow {
    pub cluster_id: String,
    pub backend_id: String,
    /// Per-second bytes received from the backend (`BYTES_IN`). The
    /// renderer scales `× 8` to bits/sec and formats with a Kbps /
    /// Mbps / Gbps suffix. `0.0` on the first observation and after
    /// the hourly counter clear (the RateCalculator emits `Some(0.0)`
    /// on monotonic decrease rather than a negative spike).
    pub bw_in_bps: f64,
    /// Per-second bytes sent to the backend (`BYTES_OUT`).
    pub bw_out_bps: f64,
    pub connections: u64,
    pub p50_ms: u64,
    pub p99_ms: u64,
    pub requests_total: u64,
}

/// Sort columns for the BACKENDS pane. Default `Bandwidth` (back_bytes_out
/// — the most operationally-loaded backend at the top).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendSortKey {
    ClusterId,
    BackendId,
    Bandwidth,
    Connections,
    LatencyP99,
    Requests,
}

impl BackendSortKey {
    pub const ALL: &'static [Self] = &[
        Self::Bandwidth,
        Self::LatencyP99,
        Self::Connections,
        Self::Requests,
        Self::ClusterId,
        Self::BackendId,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::ClusterId => "cluster",
            Self::BackendId => "backend",
            Self::Bandwidth => "bw",
            Self::Connections => "conn",
            Self::LatencyP99 => "p99",
            Self::Requests => "req",
        }
    }

    pub fn cycle(self) -> Self {
        let idx = Self::ALL.iter().position(|k| *k == self).unwrap_or(0);
        Self::ALL[(idx + 1) % Self::ALL.len()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spark_ring_drops_oldest_at_capacity() {
        let mut r = SparkRing::new(3);
        r.push(1);
        r.push(2);
        r.push(3);
        r.push(4);
        assert_eq!(r.to_vec(), vec![2, 3, 4]);
        assert_eq!(r.last(), Some(4));
        assert_eq!(r.max(), 4);
    }

    #[test]
    fn rate_calculator_first_observation_returns_none() {
        let mut rc = RateCalculator::default();
        let now = Instant::now();
        assert!(rc.record("k", 100, now).is_none());
    }

    #[test]
    fn rate_calculator_handles_monotonic_increase() {
        let mut rc = RateCalculator::default();
        let t0 = Instant::now();
        let _ = rc.record("k", 100, t0);
        let t1 = t0 + std::time::Duration::from_secs(1);
        let r = rc.record("k", 150, t1).unwrap();
        assert!((r - 50.0).abs() < 0.001);
    }

    #[test]
    fn rate_calculator_emits_zero_on_hourly_reset() {
        // Sōzu's `LocalDrain::clear` drops `Count`/`Time` every hour; a
        // counter going backwards must not produce a negative rate.
        let mut rc = RateCalculator::default();
        let t0 = Instant::now();
        let _ = rc.record("k", 500, t0);
        let t1 = t0 + std::time::Duration::from_secs(1);
        let r = rc.record("k", 10, t1).unwrap();
        assert_eq!(r, 0.0);
    }

    #[test]
    fn active_tab_round_trips_digits_and_cycle() {
        assert_eq!(ActiveTab::from_digit(1), Some(ActiveTab::Overview));
        assert_eq!(ActiveTab::from_digit(7), Some(ActiveTab::Events));
        assert_eq!(ActiveTab::from_digit(8), None);
        assert_eq!(ActiveTab::Overview.cycle(true), ActiveTab::Clusters);
        assert_eq!(ActiveTab::Overview.cycle(false), ActiveTab::Events);
    }
}
