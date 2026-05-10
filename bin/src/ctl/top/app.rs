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

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use sozu_command_lib::proto::command::{AggregatedMetrics, FilteredMetrics, filtered_metrics};

use super::transport::{Snapshot, TopEvent};

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

    pub fn samples(&self) -> impl Iterator<Item = &u64> {
        self.samples.iter()
    }

    pub fn last(&self) -> Option<u64> {
        self.samples.back().copied()
    }

    pub fn max(&self) -> u64 {
        self.samples.iter().copied().max().unwrap_or(0)
    }

    pub fn len(&self) -> usize {
        self.samples.len()
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
    /// `client.connections` / `client.connections_max` ratio above which
    /// the saturation cell goes warm.
    pub conn_warn_pct: f64,
    /// p99 in ms above which the latency sparkline goes hot.
    pub latency_p99_critical_ms: f64,
}

impl Default for ThresholdTable {
    fn default() -> Self {
        Self {
            error_ratio_critical_pct: 1.0,
            slab_critical_pct: 80.0,
            conn_warn_pct: 70.0,
            latency_p99_critical_ms: 500.0,
        }
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
    pub status: String,
    pub should_quit: bool,
    pub help_visible: bool,
    rates: RateCalculator,
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
            status: String::new(),
            should_quit: false,
            help_visible: false,
            rates: RateCalculator::default(),
        }
    }

    /// Fold an inbound transport `Snapshot` into the ring buffers. Called by
    /// the render loop on every drain of the `crossbeam_channel`. Cheap (one
    /// linear scan over the proxy / cluster maps).
    pub fn ingest_snapshot(&mut self, snap: &Snapshot) {
        self.last_snapshot_at = Some(snap.received_at);
        self.fold_overview(&snap.metrics, snap.received_at);
    }

    fn fold_overview(&mut self, m: &AggregatedMetrics, sampled_at: Instant) {
        // Sōzu emits `proxying` (per-cluster aggregated across workers) and
        // `main` (master process). For the OVERVIEW we want the global rate
        // of incoming requests; sum across clusters. The `requests` counter
        // is the canonical RPS source — see `lib/src/metrics/mod.rs`.
        let mut total_requests: i64 = 0;
        let mut total_errors_5xx: i64 = 0;
        let mut total_requests_observed: i64 = 0;
        for (_cluster_id, cm) in &m.clusters {
            if let Some(v) = count_value(cm.cluster.get("requests")) {
                total_requests = total_requests.saturating_add(v);
                total_requests_observed = total_requests_observed.saturating_add(v);
            }
            for code in &["http.status.500", "http.status.502", "http.status.503", "http.status.504", "http.status.507"] {
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
        self.overview
            .error_ratio_pct
            .push((err_pct * 100.0) as u64);

        // Latency p99 — sum of cluster `backend_response_time` percentiles
        // is not meaningful (you cannot average percentiles), so we take
        // the max p99 across clusters. Operators reading the OVERVIEW want
        // "is anyone slow?" not "average latency".
        let mut max_p99_ms: u64 = 0;
        for (_cluster_id, cm) in &m.clusters {
            if let Some(p99) = percentile_p99_ms(cm.cluster.get("backend_response_time")) {
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
        let saturation = gauge_value(m.proxying.get("slab.usage_percent"))
            .or_else(|| gauge_value(m.proxying.get("buffer.usage_percent")))
            .map(|v| v.min(100))
            .unwrap_or(0);
        self.overview.saturation_pct.push(saturation as u64);

        self.overview.active_sessions =
            gauge_value(m.proxying.get("http.active_requests")).unwrap_or(0) as u64;
        self.overview.client_connections =
            gauge_value(m.proxying.get("client.connections")).unwrap_or(0) as u64;
    }

    /// Append a transport-published `TopEvent` into the recent-events ring.
    /// Drops the oldest when capacity is reached so the UI shows a sliding
    /// window of the last `EVENT_RING_DEPTH` events.
    pub fn ingest_event(&mut self, event: TopEvent) {
        if self.events.len() == EVENT_RING_DEPTH {
            self.events.pop_front();
        }
        self.events.push_back(event);
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
