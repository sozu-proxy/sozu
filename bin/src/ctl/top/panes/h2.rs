//! H2 pane — HTTP/2 health snapshot.
//!
//! Operators reading this pane want three answers fast:
//!
//! 1. How much H2 is happening right now? (active streams gauge,
//!    connection count by ALPN class.)
//! 2. Is anything backed up? (`flow_control_stall` rate,
//!    `pending_window_updates` gauge, RST_STREAM/GOAWAY rates.)
//! 3. Has a flood detector tripped? (CVE-2023-44487 / CVE-2024-27316 /
//!    CVE-2025-8671 mitigations are surfaced as critical-tier counters.)
//!
//! All metric keys are pulled from the freshest `AggregatedMetrics`
//! snapshot's `proxying` map (the per-cluster `clusters[*].cluster` map
//! aggregates the same names). The pane reads gauges directly and computes
//! per-second rates for counters via the shared `App.rates`-style logic
//! the OVERVIEW pane already exercises.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Paragraph, Row, Table};
use sozu_command_lib::proto::command::{AggregatedMetrics, FilteredMetrics, filtered_metrics};

use super::super::app::App;
use super::super::theme::Skin;

pub fn render(f: &mut Frame<'_>, area: Rect, app: &App, skin: &Skin) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" H2 · streams · flow control · flood mitigations ")
        .style(Style::default().fg(skin.muted));

    let metrics = match app.last_metrics.as_ref() {
        Some(m) => m,
        None => {
            let inner = block.inner(area);
            f.render_widget(block, area);
            f.render_widget(
                Paragraph::new(
                    "No snapshot yet. The H2 pane reads from the same QueryMetrics \
                     poll as OVERVIEW; data appears once the first poll lands.",
                )
                .style(Style::default().fg(skin.secondary)),
                inner,
            );
            return;
        }
    };

    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7), // streams + connections gauges
            Constraint::Length(7), // flow control + frame counters
            Constraint::Min(5),    // flood mitigations
        ])
        .split(inner);

    render_streams(f, chunks[0], app, skin, metrics);
    render_flow(f, chunks[1], skin, metrics);
    render_floods(f, chunks[2], skin, metrics);
}

fn render_streams(f: &mut Frame<'_>, area: Rect, app: &App, skin: &Skin, m: &AggregatedMetrics) {
    let active_streams = gauge(m.proxying.get("h2.connection.active_streams")).unwrap_or(0);
    let alpn_h2 = count(m.proxying.get("http.alpn.h2")).unwrap_or(0);
    let alpn_http11 = count(m.proxying.get("http.alpn.http11")).unwrap_or(0);
    let total_alpn = alpn_h2 + alpn_http11;
    let h2_pct = if total_alpn > 0 {
        (alpn_h2 as f64 / total_alpn as f64) * 100.0
    } else {
        0.0
    };

    let big = Line::from(vec![
        Span::styled(
            format!("{active_streams}"),
            Style::default()
                .fg(skin.primary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" active H2 streams · ", Style::default().fg(skin.secondary)),
        Span::styled(
            format!("{h2_pct:.1} %"),
            Style::default()
                .fg(skin.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" of accepts on H2", Style::default().fg(skin.secondary)),
    ]);

    let header = Row::new(vec!["metric", "value", "trend (60 s)"]).style(
        Style::default()
            .fg(skin.primary)
            .add_modifier(Modifier::BOLD),
    );
    // Sparkline trend belongs to OVERVIEW; the H2 pane sticks to gauges
    // + counters in week 3. The trend column is reserved so a future
    // commit can plug per-row sparklines without re-laying out.
    let rows = [
        Row::new(vec![
            Cell::from("active streams"),
            Cell::from(format!("{active_streams}")),
            Cell::from("—"),
        ])
        .style(Style::default().fg(skin.secondary)),
        Row::new(vec![
            Cell::from("H2 connections accepted"),
            Cell::from(format!("{alpn_h2}")),
            Cell::from("—"),
        ])
        .style(Style::default().fg(skin.secondary)),
        Row::new(vec![
            Cell::from("HTTP/1.1 accepted"),
            Cell::from(format!("{alpn_http11}")),
            Cell::from("—"),
        ])
        .style(Style::default().fg(skin.secondary)),
        Row::new(vec![
            Cell::from("client.connections (gauge)"),
            Cell::from(format!("{}", app.overview.client_connections)),
            Cell::from("—"),
        ])
        .style(Style::default().fg(skin.secondary)),
    ];

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(3)])
        .split(area);
    f.render_widget(Paragraph::new(big), chunks[0]);
    f.render_widget(
        Table::new(
            rows,
            [
                Constraint::Min(28),
                Constraint::Length(16),
                Constraint::Min(16),
            ],
        )
        .header(header),
        chunks[1],
    );
}

fn render_flow(f: &mut Frame<'_>, area: Rect, skin: &Skin, m: &AggregatedMetrics) {
    let header = Row::new(vec!["flow control", "value"]).style(
        Style::default()
            .fg(skin.primary)
            .add_modifier(Modifier::BOLD),
    );
    let rows = [
        gauge_row(
            "connection.window_bytes",
            "h2.connection.window_bytes",
            m,
            skin,
            false,
        ),
        gauge_row(
            "pending_window_updates",
            "h2.connection.pending_window_updates",
            m,
            skin,
            false,
        ),
        count_row("flow_control_stall", "h2.flow_control_stall", m, skin, true),
        count_row(
            "frames.tx.window_update",
            "h2.frames.tx.window_update",
            m,
            skin,
            false,
        ),
        count_row(
            "frames.tx.rst_stream",
            "h2.frames.tx.rst_stream",
            m,
            skin,
            true,
        ),
        count_row("frames.tx.goaway", "h2.frames.tx.goaway", m, skin, true),
        count_row(
            "headers.rejected.budget_overrun",
            "h2.headers.rejected.budget_overrun",
            m,
            skin,
            true,
        ),
    ];
    f.render_widget(
        Table::new(rows, [Constraint::Min(36), Constraint::Length(16)]).header(header),
        area,
    );
}

fn render_floods(f: &mut Frame<'_>, area: Rect, skin: &Skin, m: &AggregatedMetrics) {
    // Critical-tier counters: any non-zero value is a documented attack
    // mitigation firing. Keep them in their own block with a hot-tier title
    // so the eye is drawn here when it should be.
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" flood mitigations · CVE-2023-44487 / CVE-2024-27316 / CVE-2025-8671 ")
        .style(Style::default().fg(skin.hot));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let header = Row::new(vec!["counter", "value"]).style(
        Style::default()
            .fg(skin.primary)
            .add_modifier(Modifier::BOLD),
    );

    let candidates = [
        ("h2.flood.violation.glitch_window", "glitch_window"),
        ("h2.flood.violation.rapid_reset", "rapid_reset"),
        ("h2.flood.violation.continuation", "continuation_flood"),
        ("h2.flood.violation.made_you_reset", "made_you_reset"),
        ("h2.flood.violation.ping", "ping_flood"),
        ("h2.flood.violation.settings", "settings_flood"),
        ("h2.flood.violation.priority", "priority_flood"),
        ("h2.window_update_dropped", "window_update_dropped"),
        ("h2.close_with_active_streams", "close_with_active_streams"),
    ];

    let rows: Vec<Row<'_>> = candidates
        .iter()
        .map(|(key, label)| {
            let v = count(m.proxying.get(*key)).unwrap_or(0);
            let style = if v > 0 {
                Style::default().fg(skin.hot).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(skin.secondary)
            };
            Row::new(vec![Cell::from(*label), Cell::from(format!("{v}"))]).style(style)
        })
        .collect();

    f.render_widget(
        Table::new(rows, [Constraint::Min(28), Constraint::Length(16)]).header(header),
        inner,
    );
}

fn gauge_row<'a>(
    label: &'a str,
    key: &str,
    m: &AggregatedMetrics,
    skin: &Skin,
    warn_when_nonzero: bool,
) -> Row<'a> {
    let v = gauge(m.proxying.get(key));
    let style = row_style(skin, v.unwrap_or(0) > 0 && warn_when_nonzero);
    Row::new(vec![
        Cell::from(label),
        Cell::from(v.map(|v| format!("{v}")).unwrap_or_else(|| "—".into())),
    ])
    .style(style)
}

fn count_row<'a>(
    label: &'a str,
    key: &str,
    m: &AggregatedMetrics,
    skin: &Skin,
    warn_when_nonzero: bool,
) -> Row<'a> {
    let v = count(m.proxying.get(key));
    let style = row_style(skin, v.unwrap_or(0) > 0 && warn_when_nonzero);
    Row::new(vec![
        Cell::from(label),
        Cell::from(v.map(|v| format!("{v}")).unwrap_or_else(|| "—".into())),
    ])
    .style(style)
}

fn row_style(skin: &Skin, warn: bool) -> Style {
    if warn {
        skin.row_critical()
    } else {
        Style::default().fg(skin.secondary)
    }
}

fn gauge(metric: Option<&FilteredMetrics>) -> Option<u64> {
    let inner = metric?.inner.as_ref()?;
    match inner {
        filtered_metrics::Inner::Gauge(v) => Some(*v),
        _ => None,
    }
}

fn count(metric: Option<&FilteredMetrics>) -> Option<i64> {
    let inner = metric?.inner.as_ref()?;
    match inner {
        filtered_metrics::Inner::Count(v) => Some(*v),
        _ => None,
    }
}
