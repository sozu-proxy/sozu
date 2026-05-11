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
use sozu_command_lib::proto::command::AggregatedMetrics;
use sozu_lib::metrics::names;

use super::super::app::{App, count_value as count, gauge_value as gauge};
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
    let active_streams = gauge(m.proxying.get(names::h2::CONNECTION_ACTIVE_STREAMS)).unwrap_or(0);
    let alpn_h2 = count(m.proxying.get(names::http::ALPN_H2)).unwrap_or(0);
    let alpn_http11 = count(m.proxying.get(names::http::ALPN_HTTP11)).unwrap_or(0);
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
            names::h2::CONNECTION_WINDOW_BYTES,
            m,
            skin,
            false,
        ),
        gauge_row(
            "pending_window_updates",
            names::h2::CONNECTION_PENDING_WINDOW_UPDATES,
            m,
            skin,
            false,
        ),
        count_row(
            "flow_control_stall",
            names::h2::FLOW_CONTROL_STALL,
            m,
            skin,
            true,
        ),
        count_row(
            "frames.tx.window_update",
            names::h2::FRAMES_TX_WINDOW_UPDATE,
            m,
            skin,
            false,
        ),
        count_row(
            "frames.tx.rst_stream",
            names::h2::FRAMES_TX_RST_STREAM,
            m,
            skin,
            true,
        ),
        count_row(
            "frames.tx.goaway",
            names::h2::FRAMES_TX_GOAWAY,
            m,
            skin,
            true,
        ),
        count_row(
            "headers.rejected.budget_overrun",
            names::h2::HEADERS_REJECTED_BUDGET_OVERRUN,
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
        (names::h2::FLOOD_VIOLATION_GLITCH_WINDOW, "glitch_window"),
        (names::h2::FLOOD_VIOLATION_RAPID_RESET, "rapid_reset"),
        (
            names::h2::FLOOD_VIOLATION_CONTINUATION,
            "continuation_flood",
        ),
        (names::h2::FLOOD_VIOLATION_MADE_YOU_RESET, "made_you_reset"),
        (names::h2::FLOOD_VIOLATION_PING, "ping_flood"),
        (names::h2::FLOOD_VIOLATION_SETTINGS, "settings_flood"),
        (names::h2::FLOOD_VIOLATION_PRIORITY, "priority_flood"),
        (names::h2::WINDOW_UPDATE_DROPPED, "window_update_dropped"),
        (
            names::h2::CLOSE_WITH_ACTIVE_STREAMS,
            "close_with_active_streams",
        ),
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

// `gauge` / `count` helpers come from `super::super::app` (renamed at
// import time) so the H2 pane and the App-side rate calculators share
// one source of truth for `FilteredMetrics -> Option<{i64,u64}>`
// extraction. Closes PR #1256 simplify A3.
