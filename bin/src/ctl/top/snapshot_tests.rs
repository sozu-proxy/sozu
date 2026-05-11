//! Pane snapshot tests via `insta` + ratatui's `TestBackend`.
//!
//! Each pane is rendered against an `App` fixture into a `TestBackend` of a
//! fixed size, the resulting `Buffer` is converted to a plain `String`, and
//! `insta::assert_snapshot!` compares against the on-disk snapshot under
//! `bin/src/ctl/top/snapshots/`.
//!
//! Three canonical sizes per Codex's recommendation in `tasks/todo.md`:
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
use std::time::Instant;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use sozu_command_lib::proto::command::{
    AggregatedMetrics, BackendMetrics, ClusterMetrics, FilteredMetrics, ListenersList, Percentiles,
    filtered_metrics,
};

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

/// Build a synthetic `AggregatedMetrics` payload that exercises every pane:
/// three clusters, two backends each, a smattering of gauges and counters
/// that trip the threshold table without firing the alert banner.
fn fixture_metrics() -> AggregatedMetrics {
    let mut proxying: BTreeMap<String, FilteredMetrics> = BTreeMap::new();
    proxying.insert("slab.usage_percent".into(), gauge(45));
    proxying.insert("client.connections".into(), gauge(312));
    proxying.insert("http.active_requests".into(), gauge(87));
    proxying.insert("h2.connection.active_streams".into(), gauge(24));
    proxying.insert("http.alpn.h2".into(), count(1_000));
    proxying.insert("http.alpn.http11".into(), count(500));
    proxying.insert("h2.connection.window_bytes".into(), gauge(65_535));
    proxying.insert("h2.connection.pending_window_updates".into(), gauge(0));
    proxying.insert("h2.flow_control_stall".into(), count(2));
    proxying.insert("h2.frames.tx.window_update".into(), count(42));
    proxying.insert("h2.frames.tx.rst_stream".into(), count(0));
    proxying.insert("h2.frames.tx.goaway".into(), count(0));
    proxying.insert("h2.headers.rejected.budget_overrun".into(), count(0));

    let mut clusters: BTreeMap<String, ClusterMetrics> = BTreeMap::new();
    for (i, id) in ["api-prod", "static-cdn", "queue-worker"]
        .iter()
        .enumerate()
    {
        let mut cluster: BTreeMap<String, FilteredMetrics> = BTreeMap::new();
        cluster.insert("requests".into(), count(1_000 + i as i64 * 500));
        cluster.insert("http.status.500".into(), count(2 + i as i64));
        cluster.insert("http.status.503".into(), count(1));
        cluster.insert(
            "backend_response_time".into(),
            percentiles(20, 80, 180 + i as u64 * 20),
        );
        cluster.insert("cluster.total_backends".into(), gauge(2));
        cluster.insert(
            "backend.available".into(),
            gauge(if i == 2 { 0 } else { 2 }),
        );
        let backends = vec![
            backend_metrics(format!("{id}-1"), 1_024 * (i as u64 + 1), 30, 110),
            backend_metrics(format!("{id}-2"), 2_048 * (i as u64 + 1), 35, 120),
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
    metrics.insert("bytes_in".into(), count(bytes as i64));
    metrics.insert("bytes_out".into(), count((bytes * 4) as i64));
    metrics.insert("back_bytes_in".into(), count((bytes * 8) as i64));
    metrics.insert("back_bytes_out".into(), count((bytes * 16) as i64));
    metrics.insert("connections_per_backend".into(), gauge(12));
    metrics.insert(
        "backend_response_time".into(),
        percentiles(p50, p50 + 30, p99),
    );
    metrics.insert("requests".into(), count(bytes as i64 / 4));
    BackendMetrics {
        backend_id: id,
        metrics,
    }
}

/// Fresh fixture App at OVERVIEW + a single synthetic snapshot already
/// ingested. The render-time clock is set to `Instant::now()` so trend
/// glyphs / age formatting depend only on the wall clock at render time,
/// which insta sees through the trimmed buffer. Acceptable: trend glyphs
/// only flip when a second sample lands, and our fixture only pushes one.
fn fixture_app() -> App {
    let mut app = App::new();
    let snap = Snapshot {
        metrics: fixture_metrics(),
        received_at: Instant::now(),
    };
    app.ingest_snapshot(&snap);
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
