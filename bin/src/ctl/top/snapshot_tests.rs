//! Pane snapshot tests via `insta` + ratatui's `TestBackend`.
//!
//! Each pane is rendered against an `App` fixture into a `TestBackend` of a
//! fixed size, the resulting `Buffer` is converted to a plain `String`, and
//! `insta::assert_snapshot!` compares against the on-disk snapshot under
//! `bin/src/ctl/top/snapshots/`.
//!
//! Three canonical sizes cover the realistic operator-terminal envelope:
//!
//! - 80x24  — htop's default and the lower-bound modern terminal.
//! - 120x40 — typical full-screen SSH session.
//! - 200x60 — multi-monitor pane / projector.
//!
//! Snapshots strip background-colour metadata: we only assert the
//! character grid + foreground style, since terminal-specific colour
//! rendering varies by emulator and would make snapshots brittle without
//! adding regression value.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use sozu_command_lib::proto::command::{
    AggregatedMetrics, BackendMetrics, ClusterMetrics, FilteredMetrics, ListenersList, Percentiles,
    filtered_metrics,
};
use sozu_lib::metrics::names;

use super::app::App;
use super::panes;
use super::theme::Skin;
use super::transport::{ListenersSnapshot, Snapshot};

/// Render `draw` into a `TestBackend` of the given size and return the
/// character grid as a `\n`-delimited string. Style and colour are
/// dropped — the snapshot file would otherwise track ANSI escape codes
/// that vary across ratatui patch releases without saying anything
/// useful about the rendered layout.
fn render_to_string<F>(width: u16, height: u16, app: &App, draw: F) -> String
where
    F: FnOnce(&mut ratatui::Frame<'_>, Rect, &App, &Skin),
{
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("TestBackend Terminal");
    let skin = Skin::default_dark();
    terminal
        .draw(|f| draw(f, f.area(), app, &skin))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let area = *buffer.area();
    let mut out = String::with_capacity((area.width as usize + 1) * area.height as usize);
    for y in 0..area.height {
        for x in 0..area.width {
            // `buffer[(x, y)]` is a `Cell`; `.symbol()` returns the
            // grapheme cluster (default " " for an empty cell).
            out.push_str(buffer[(x, y)].symbol());
        }
        // Trim trailing spaces per line so the snapshot file is smaller
        // and operator diffs are easier to read.
        while out.ends_with(' ') {
            out.pop();
        }
        out.push('\n');
    }
    out
}

/// Build a synthetic `AggregatedMetrics` payload at a synthetic `tick`.
/// Three clusters, two backends each, a smattering of gauges and
/// counters that trip the threshold table without firing the alert
/// banner. Cumulative counters scale linearly with `tick` so two
/// ingestions at `tick=0` then `tick=1` (one second apart) make the
/// `RateCalculator` emit a stable delta matching the cumulative
/// scaling — the snapshot then shows realistic Mbps / req-per-s
/// values rather than a flat 0 from a first-observation `None`.
fn fixture_metrics_at(tick: u64) -> AggregatedMetrics {
    let tick = tick as i64;
    let mut proxying: BTreeMap<String, FilteredMetrics> = BTreeMap::new();
    proxying.insert(names::slab::USAGE_PERCENT.into(), gauge(45));
    proxying.insert(names::client::CONNECTIONS.into(), gauge(312));
    proxying.insert(names::http::ACTIVE_REQUESTS.into(), gauge(87));
    proxying.insert(names::h2::CONNECTION_ACTIVE_STREAMS.into(), gauge(24));
    proxying.insert(names::http::ALPN_H2.into(), count(1_000 + 100 * tick));
    proxying.insert(names::http::ALPN_HTTP11.into(), count(500 + 50 * tick));
    proxying.insert(names::h2::CONNECTION_WINDOW_BYTES.into(), gauge(65_535));
    proxying.insert(
        names::h2::CONNECTION_PENDING_WINDOW_UPDATES.into(),
        gauge(0),
    );
    proxying.insert(names::h2::FLOW_CONTROL_STALL.into(), count(2));
    proxying.insert(names::h2::FRAMES_TX_WINDOW_UPDATE.into(), count(42));
    proxying.insert(names::h2::FRAMES_TX_RST_STREAM.into(), count(0));
    proxying.insert(names::h2::FRAMES_TX_GOAWAY.into(), count(0));
    proxying.insert(names::h2::HEADERS_REJECTED_BUDGET_OVERRUN.into(), count(0));
    proxying.insert(
        names::event_loop::SERVICE_TIME.into(),
        percentiles(3, 8, 12),
    );

    let mut clusters: BTreeMap<String, ClusterMetrics> = BTreeMap::new();
    for (i, id) in ["api-prod", "static-cdn", "queue-worker"]
        .iter()
        .enumerate()
    {
        let mut cluster: BTreeMap<String, FilteredMetrics> = BTreeMap::new();
        let i64_i = i as i64;
        cluster.insert(
            names::backend::REQUESTS.into(),
            count((1_000 + i64_i * 500) * tick),
        );
        cluster.insert(names::http_status::S500.into(), count(2 + i64_i));
        cluster.insert(names::http_status::S503.into(), count(1));
        cluster.insert(
            names::backend::RESPONSE_TIME.into(),
            percentiles(20, 80, 180 + i as u64 * 20),
        );
        cluster.insert(names::cluster::TOTAL_BACKENDS.into(), gauge(2));
        cluster.insert(
            names::cluster::AVAILABLE_BACKENDS.into(),
            gauge(if i == 2 { 0 } else { 2 }),
        );
        let tick_u64 = tick as u64;
        let backends = vec![
            backend_metrics(
                format!("{id}-1"),
                125_000 * (i as u64 + 1) * tick_u64,
                30,
                110,
            ),
            backend_metrics(
                format!("{id}-2"),
                250_000 * (i as u64 + 1) * tick_u64,
                35,
                120,
            ),
        ];
        clusters.insert((*id).into(), ClusterMetrics { cluster, backends });
    }

    AggregatedMetrics {
        main: BTreeMap::new(),
        workers: BTreeMap::new(),
        clusters,
        proxying,
    }
}

fn fixture_listeners() -> ListenersList {
    ListenersList {
        http_listeners: BTreeMap::new(),
        https_listeners: BTreeMap::new(),
        tcp_listeners: BTreeMap::new(),
    }
}

fn gauge(v: u64) -> FilteredMetrics {
    FilteredMetrics {
        inner: Some(filtered_metrics::Inner::Gauge(v)),
    }
}

fn count(v: i64) -> FilteredMetrics {
    FilteredMetrics {
        inner: Some(filtered_metrics::Inner::Count(v)),
    }
}

fn percentiles(p50: u64, p90: u64, p99: u64) -> FilteredMetrics {
    FilteredMetrics {
        inner: Some(filtered_metrics::Inner::Percentiles(Percentiles {
            samples: 1_000,
            p_50: p50,
            p_90: p90,
            p_99: p99,
            p_99_9: p99 * 2,
            p_99_99: p99 * 3,
            p_99_999: p99 * 4,
            p_100: p99 * 5,
            sum: 1_000 * p50,
        })),
    }
}

fn backend_metrics(id: String, bytes: u64, p50: u64, p99: u64) -> BackendMetrics {
    let mut metrics: BTreeMap<String, FilteredMetrics> = BTreeMap::new();
    // `BYTES_IN` / `BYTES_OUT` are the per-backend backend-socket
    // counters published by `record_backend_metrics!`; the BACKENDS pane
    // reads them as "bw down/up". `BACK_BYTES_IN` / `BACK_BYTES_OUT` are
    // emitted as no-label proxy counters and never land per-backend, so
    // the fixture must not pretend otherwise.
    metrics.insert(names::backend::BYTES_IN.into(), count(bytes as i64));
    metrics.insert(names::backend::BYTES_OUT.into(), count((bytes * 4) as i64));
    metrics.insert(names::backend::CONNECTIONS_PER_BACKEND.into(), gauge(12));
    metrics.insert(
        names::backend::RESPONSE_TIME.into(),
        percentiles(p50, p50 + 30, p99),
    );
    metrics.insert(names::backend::REQUESTS.into(), count(bytes as i64 / 4));
    BackendMetrics {
        backend_id: id,
        metrics,
    }
}

/// Fresh fixture App at OVERVIEW with two synthetic snapshots already
/// ingested, one second apart. The `RateCalculator` returns `None` on
/// the first observation, so a single ingestion would show a flat 0 in
/// every rate-derived column (cluster RPS, backend bandwidth) — the
/// double ingest exercises the rate path so the snapshot reflects real
/// per-second values.
fn fixture_app() -> App {
    let mut app = App::new();
    let t0 = Instant::now();
    app.ingest_snapshot(&Snapshot {
        metrics: fixture_metrics_at(0),
        received_at: t0,
    });
    app.ingest_snapshot(&Snapshot {
        metrics: fixture_metrics_at(1),
        received_at: t0 + Duration::from_secs(1),
    });
    app.ingest_listeners(ListenersSnapshot {
        list: fixture_listeners(),
    });
    app
}

// ── OVERVIEW pane ─────────────────────────────────────────────────────

#[test]
fn snapshot_overview_80x24() {
    let app = fixture_app();
    let out = render_to_string(80, 24, &app, |f, area, app, skin| {
        panes::overview::render(f, area, app, skin)
    });
    insta::assert_snapshot!(out);
}

#[test]
fn snapshot_overview_120x40() {
    let app = fixture_app();
    let out = render_to_string(120, 40, &app, |f, area, app, skin| {
        panes::overview::render(f, area, app, skin)
    });
    insta::assert_snapshot!(out);
}

// ── CLUSTERS pane ─────────────────────────────────────────────────────

#[test]
fn snapshot_clusters_80x24() {
    let app = fixture_app();
    let out = render_to_string(80, 24, &app, |f, area, app, skin| {
        panes::clusters::render(f, area, app, skin)
    });
    insta::assert_snapshot!(out);
}

#[test]
fn snapshot_clusters_empty_120x40() {
    // No `last_metrics` — the empty-state copy must render legibly so
    // operators see why the table is blank.
    let app = App::new();
    let out = render_to_string(120, 40, &app, |f, area, app, skin| {
        panes::clusters::render(f, area, app, skin)
    });
    insta::assert_snapshot!(out);
}

// ── BACKENDS pane ─────────────────────────────────────────────────────

#[test]
fn snapshot_backends_120x40() {
    let app = fixture_app();
    let out = render_to_string(120, 40, &app, |f, area, app, skin| {
        panes::backends::render(f, area, app, skin)
    });
    insta::assert_snapshot!(out);
}

// ── LISTENERS pane ────────────────────────────────────────────────────

#[test]
fn snapshot_listeners_empty_80x24() {
    let app = fixture_app(); // empty `ListenersList`, exercises empty-state copy
    let out = render_to_string(80, 24, &app, |f, area, app, skin| {
        panes::listeners::render(f, area, app, skin)
    });
    insta::assert_snapshot!(out);
}

// ── H2 pane ───────────────────────────────────────────────────────────

#[test]
fn snapshot_h2_120x40() {
    let app = fixture_app();
    let out = render_to_string(120, 40, &app, |f, area, app, skin| {
        panes::h2::render(f, area, app, skin)
    });
    insta::assert_snapshot!(out);
}

// ── EVENTS pane (empty) ───────────────────────────────────────────────

#[test]
fn snapshot_events_empty_80x24() {
    let app = App::new();
    let out = render_to_string(80, 24, &app, |f, area, app, skin| {
        panes::events::render(f, area, app, skin)
    });
    insta::assert_snapshot!(out);
}
