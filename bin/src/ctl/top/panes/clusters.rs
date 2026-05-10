//! CLUSTERS pane — sortable table of clusters with one row per cluster_id.
//! Default sort: 5xx error rate descending, then RPS — operators want the
//! unhealthy clusters at the top so the eye lands on them first.
//!
//! Pulse-tint on cluster disappearance / new-unhealthy-backend transitions
//! lands in week 3 (see `PulseTracker` in `tasks/todo.md`).

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Row, Table};

use super::super::app::{App, ClusterSortKey};
use super::super::theme::Skin;

pub fn render(f: &mut Frame<'_>, area: Rect, app: &App, skin: &Skin) {
    let rows = app.cluster_rows();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(format!(
            " CLUSTERS · sort: {} {} · {} cluster{} ",
            app.cluster_sort.label(),
            if app.cluster_sort_reverse {
                "asc"
            } else {
                "desc"
            },
            rows.len(),
            if rows.len() == 1 { "" } else { "s" },
        ))
        .style(Style::default().fg(skin.muted));

    if rows.is_empty() {
        let inner = block.inner(area);
        f.render_widget(block, area);
        let body = ratatui::widgets::Paragraph::new(
            "No cluster metrics yet. The first poll lands within --refresh-ms; \
             if the screen stays empty, ensure the worker has \
             `metrics.detail = backend` (or auto-elevation via the \
             SetMetricDetail lease has acknowledged).",
        )
        .style(Style::default().fg(skin.secondary));
        f.render_widget(body, inner);
        return;
    }

    let header = Row::new(vec![
        sort_header("cluster_id", ClusterSortKey::ClusterId, app, skin),
        sort_header("rps", ClusterSortKey::Rps, app, skin),
        sort_header("err %", ClusterSortKey::ErrorRate, app, skin),
        sort_header("p50", ClusterSortKey::LatencyP99, app, skin),
        sort_header("p99", ClusterSortKey::LatencyP99, app, skin),
        sort_header("backends", ClusterSortKey::BackendsAvailable, app, skin),
    ])
    .style(
        Style::default()
            .fg(skin.primary)
            .add_modifier(Modifier::BOLD),
    );

    let body: Vec<Row<'_>> = rows
        .iter()
        .map(|row| {
            let row_critical = row.error_rate_pct >= app.thresholds.error_ratio_critical_pct
                || row.p99_ms as f64 >= app.thresholds.latency_p99_critical_ms
                || (row.backends_total > 0 && row.backends_available == 0);
            let row_style = if row_critical {
                skin.row_critical()
            } else {
                Style::default().fg(skin.secondary)
            };
            Row::new(vec![
                Cell::from(row.cluster_id.clone()),
                Cell::from(format!("{}", row.requests_total)),
                Cell::from(format!("{:.2}", row.error_rate_pct)),
                Cell::from(format!("{}", row.p50_ms)),
                Cell::from(format!("{}", row.p99_ms)),
                Cell::from(format!("{}/{}", row.backends_available, row.backends_total)),
            ])
            .style(row_style)
        })
        .collect();

    let widths = [
        Constraint::Min(20),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(6),
        Constraint::Length(6),
        Constraint::Length(10),
    ];
    let table = Table::new(body, widths).header(header).block(block);
    f.render_widget(table, area);
}

fn sort_header(label: &str, column_key: ClusterSortKey, app: &App, skin: &Skin) -> Cell<'static> {
    if app.cluster_sort == column_key {
        let arrow = if app.cluster_sort_reverse {
            "▲"
        } else {
            "▼"
        };
        Cell::from(Line::from(vec![Span::styled(
            format!("{label} {arrow}"),
            Style::default()
                .fg(skin.accent)
                .add_modifier(Modifier::BOLD),
        )]))
    } else {
        Cell::from(Line::from(Span::styled(
            label.to_owned(),
            Style::default().fg(skin.secondary),
        )))
    }
}
