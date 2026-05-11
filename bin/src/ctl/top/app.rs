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

use sozu_command_lib::proto::command::{AggregatedMetrics, FilteredMetrics, filtered_metrics};
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
    /// `App::ingest_snapshot` with the set of keys present in the freshest
    /// snapshot so a cluster / backend that disappears doesn't leave its
    /// `(prev_value, prev_at)` lingering forever — the `HashMap` would
    /// otherwise grow with fleet churn without ever shrinking back, since
    /// `record` only `insert`s and never `remove`s.
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
        Self::tick_one(&mut self.cluster_disappeared);
        Self::tick_one(&mut self.cluster_appeared);
        let to_drop: Vec<_> = self
            .backend_down
            .iter_mut()
            .filter_map(|(k, v)| {
                if *v == 0 {
                    Some(k.clone())
                } else {
                    *v -= 1;
                    None
                }
            })
            .collect();
        for k in to_drop {
            self.backend_down.remove(&k);
        }
    }

    fn tick_one(map: &mut HashMap<String, u32>) {
        let to_drop: Vec<_> = map
            .iter_mut()
            .filter_map(|(k, v)| {
                if *v == 0 {
                    Some(k.clone())
                } else {
                    *v -= 1;
                    None
                }
            })
            .collect();
        for k in to_drop {
            map.remove(&k);
        }
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
        if self.cluster_disappeared.contains_key(cluster_id) {
            Some(PulseKind::Disappeared)
        } else if self.cluster_appeared.contains_key(cluster_id) {
            Some(PulseKind::Appeared)
        } else {
            None
        }
    }

    pub fn backend_pulse(&self, cluster_id: &str, backend_id: &str) -> Option<PulseKind> {
        if self
            .backend_down
            .contains_key(&(cluster_id.to_owned(), backend_id.to_owned()))
        {
            Some(PulseKind::WentDown)
        } else {
            None
        }
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
            .error_ratio_pct
            .last()
            // error_ratio_pct stored as integer × 100 (two decimals), so
            // the threshold needs the same scaling for the comparison.
            .is_some_and(|v| (v as f64 / 100.0) >= self.error_ratio_critical_pct)
        {
            return Some("ERROR SURGE");
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
    pub error_ratio_pct: SparkRing,
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
        }
    }

    /// Read-and-clear the dirty flag for the renderer. Returns `true` when
    /// the App folded new data or aged a pulse since the last frame; the
    /// renderer combines this with `PulseTracker::has_active()` to decide
    /// whether to repaint.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::replace(&mut self.is_dirty, false)
    }

    /// Fold an inbound transport `Snapshot` into the ring buffers. Called by
    /// the render loop on every drain of the `crossbeam_channel`. Cheap (one
    /// linear scan over the proxy / cluster maps).
    pub fn ingest_snapshot(&mut self, snap: &Snapshot) {
        self.last_snapshot_at = Some(snap.received_at);
        self.fold_overview(&snap.metrics, snap.received_at);
        // Diff vs the last seen snapshot BEFORE replacing it, so the
        // pulse tracker can emit ClusterDisappeared / BackendWentDown
        // transitions against the previous set.
        self.pulse.diff(&snap.metrics);
        // The rate calculator currently only carries the two overview-
        // aggregate keys recorded by `fold_overview` below. The retain
        // here is a defence-in-depth no-op while that invariant holds; if
        // per-cluster series get added later (each (cluster_id, "requests")
        // key plus the per-cluster 5xx series) this is the spot to widen
        // `live_keys` so removed clusters / backends do not accumulate
        // forever.
        let live_keys: [&str; 2] = ["__overview.requests", "__overview.errors_5xx"];
        self.rates.retain(|k| live_keys.contains(&k));
        // Keep the latest `AggregatedMetrics` for the CLUSTERS / BACKENDS
        // panes. Cloning isn't cheap for very-high-cardinality fleets
        // (>1000 clusters), but in practice the master already paid this
        // cost on the wire; revisit only if profile data justifies it.
        self.last_metrics = Some(snap.metrics.clone());
        self.is_dirty = true;
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
                let requests = count_value(cm.cluster.get("requests")).unwrap_or(0);
                let errors_5xx: i64 = [
                    names::http_status::S500,
                    names::http_status::S502,
                    names::http_status::S503,
                    names::http_status::S504,
                    names::http_status::S507,
                ]
                .iter()
                .filter_map(|k| count_value(cm.cluster.get(*k)))
                .sum();
                let p99_ms =
                    percentile_p99_ms(cm.cluster.get(names::backend::RESPONSE_TIME)).unwrap_or(0);
                let p50_ms =
                    percentile_p50_ms(cm.cluster.get(names::backend::RESPONSE_TIME)).unwrap_or(0);
                // Backends-up vs backends-total; both are gauges and so
                // survive the hourly counter clear.
                let backends_total =
                    gauge_value(cm.cluster.get(names::cluster::TOTAL_BACKENDS)).unwrap_or(0) as u32;
                let backends_available =
                    gauge_value(cm.cluster.get(names::backend::AVAILABLE)).unwrap_or(0) as u32;
                let error_rate_pct = if requests > 0 {
                    (errors_5xx as f64 / requests as f64) * 100.0
                } else {
                    0.0
                };
                ClusterRow {
                    cluster_id: id.clone(),
                    requests_total: requests as u64,
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
                ClusterSortKey::Rps => a.requests_total.cmp(&b.requests_total).reverse(),
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
                rows.push(BackendRow {
                    cluster_id: cluster_id.clone(),
                    backend_id: bm.backend_id.clone(),
                    back_bytes_in: count_value(bm.metrics.get(names::backend::BACK_BYTES_IN))
                        .unwrap_or(0) as u64,
                    back_bytes_out: count_value(bm.metrics.get(names::backend::BACK_BYTES_OUT))
                        .unwrap_or(0) as u64,
                    connections: gauge_value(
                        bm.metrics.get(names::backend::CONNECTIONS_PER_BACKEND),
                    )
                    .unwrap_or(0),
                    p50_ms: percentile_p50_ms(bm.metrics.get(names::backend::RESPONSE_TIME))
                        .unwrap_or(0),
                    p99_ms: percentile_p99_ms(bm.metrics.get(names::backend::RESPONSE_TIME))
                        .unwrap_or(0),
                    requests_total: count_value(bm.metrics.get("requests")).unwrap_or(0) as u64,
                });
            }
        }
        rows.sort_by(|a, b| {
            let ord = match self.backend_sort {
                BackendSortKey::ClusterId => a
                    .cluster_id
                    .cmp(&b.cluster_id)
                    .then(a.backend_id.cmp(&b.backend_id)),
                BackendSortKey::BackendId => a.backend_id.cmp(&b.backend_id),
                BackendSortKey::Bandwidth => {
                    let abw = a.back_bytes_in + a.back_bytes_out;
                    let bbw = b.back_bytes_in + b.back_bytes_out;
                    abw.cmp(&bbw).reverse()
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
        // Sōzu emits `proxying` (per-cluster aggregated across workers) and
        // `main` (master process). For the OVERVIEW we want the global rate
        // of incoming requests; sum across clusters. The `requests` counter
        // is the canonical RPS source — see `lib/src/metrics/mod.rs`.
        let mut total_requests: i64 = 0;
        let mut total_errors_5xx: i64 = 0;
        let mut total_requests_observed: i64 = 0;
        for cm in m.clusters.values() {
            if let Some(v) = count_value(cm.cluster.get("requests")) {
                total_requests = total_requests.saturating_add(v);
                total_requests_observed = total_requests_observed.saturating_add(v);
            }
            for code in &[
                names::http_status::S500,
                names::http_status::S502,
                names::http_status::S503,
                names::http_status::S504,
                names::http_status::S507,
            ] {
                if let Some(v) = count_value(cm.cluster.get(*code)) {
                    total_errors_5xx = total_errors_5xx.saturating_add(v);
                }
            }
        }
        // Fall back to `proxying` if no cluster metrics are exposed (the
        // worker has `MetricDetail::Process` configured; OVERVIEW should
        // still show *something*).
        if total_requests_observed == 0 {
            if let Some(v) = count_value(m.proxying.get("requests")) {
                total_requests = total_requests.saturating_add(v);
            }
        }

        // Per-second rate from the cumulative counter. The first observation
        // returns `None` and shows as 0 in the ring (nothing useful to plot
        // before we have a baseline).
        let rps = self
            .rates
            .record("__overview.requests", total_requests, sampled_at)
            .unwrap_or(0.0)
            .max(0.0);
        self.overview.rps.push(rps as u64);

        // Error ratio (5xx as percent of requests over the same delta).
        let err_rate = self
            .rates
            .record("__overview.errors_5xx", total_errors_5xx, sampled_at)
            .unwrap_or(0.0)
            .max(0.0);
        let err_pct = if rps > 0.0 {
            (err_rate / rps) * 100.0
        } else {
            0.0
        };
        // SparkRing is `u64`; multiply by 100 to keep two-decimal precision
        // (the renderer divides back when printing the big numeral).
        self.overview.error_ratio_pct.push((err_pct * 100.0) as u64);

        // Latency p99 — sum of cluster `backend_response_time` percentiles
        // is not meaningful (you cannot average percentiles), so we take
        // the max p99 across clusters. Operators reading the OVERVIEW want
        // "is anyone slow?" not "average latency".
        let mut max_p99_ms: u64 = 0;
        for cm in m.clusters.values() {
            if let Some(p99) = percentile_p99_ms(cm.cluster.get(names::backend::RESPONSE_TIME)) {
                if p99 > max_p99_ms {
                    max_p99_ms = p99;
                }
            }
        }
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
    /// renderer when the operator presses `:`.
    pub fn open_palette(&mut self) {
        self.palette_open = true;
        self.palette_input = tui_input::Input::default();
        self.palette_error = None;
    }

    /// Close the palette without applying the in-progress command.
    /// Called on Escape / Ctrl-C while the palette is open.
    pub fn cancel_palette(&mut self) {
        self.palette_open = false;
        self.palette_input = tui_input::Input::default();
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
        match cmd {
            "overview" | "o" => self.active_tab = ActiveTab::Overview,
            "cluster" | "clusters" | "c" => self.active_tab = ActiveTab::Clusters,
            "backend" | "backends" | "b" => self.active_tab = ActiveTab::Backends,
            "listener" | "listeners" | "l" => self.active_tab = ActiveTab::Listeners,
            "cert" | "certs" => self.active_tab = ActiveTab::Certs,
            "h2" => self.active_tab = ActiveTab::H2,
            "event" | "events" | "e" => self.active_tab = ActiveTab::Events,
            "help" | "h" | "?" => self.help_visible = !self.help_visible,
            "quit" | "q" => self.should_quit = true,
            "" => {} // empty command — just close the palette
            other => {
                self.palette_error = Some(format!("unknown command: :{other}"));
                self.palette_open = false;
                return;
            }
        }
        self.palette_open = false;
        self.palette_input = tui_input::Input::default();
        self.palette_error = None;
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

fn count_value(metric: Option<&FilteredMetrics>) -> Option<i64> {
    let inner = metric?.inner.as_ref()?;
    match inner {
        filtered_metrics::Inner::Count(v) => Some(*v),
        _ => None,
    }
}

fn gauge_value(metric: Option<&FilteredMetrics>) -> Option<u64> {
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

/// Per-cluster row produced by `App::cluster_rows`. Pure data; the renderer
/// turns this into a `Row` widget. Pulse-tint state comes in week 3.
#[derive(Debug, Clone)]
pub struct ClusterRow {
    pub cluster_id: String,
    pub requests_total: u64,
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
    pub back_bytes_in: u64,
    pub back_bytes_out: u64,
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
