//! CLUSTERS pane — sortable table of clusters with one row per cluster_id.
//! Default sort: 5xx error rate descending, then RPS — operators want the
//! unhealthy clusters at the top so the eye lands on them first.
//!
//! Pulse-tint on cluster disappearance and new-unhealthy-backend transitions
//! is driven by [`crate::ctl::top::app::PulseTracker`].

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Row, Table};

use super::super::app::{App, ClusterSortKey, PulseKind};
use super::super::theme::Skin;
use super::sort_header;

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

    let reverse = app.cluster_sort_reverse;
    let active = |key: ClusterSortKey| app.cluster_sort == key;
    let header = Row::new(vec![
        sort_header(
            "cluster_id",
            active(ClusterSortKey::ClusterId),
            reverse,
            skin,
        ),
        sort_header("rps", active(ClusterSortKey::Rps), reverse, skin),
        sort_header("err %", active(ClusterSortKey::ErrorRate), reverse, skin),
        sort_header("p50", active(ClusterSortKey::LatencyP99), reverse, skin),
        sort_header("p99", active(ClusterSortKey::LatencyP99), reverse, skin),
        sort_header(
            "backends",
            active(ClusterSortKey::BackendsAvailable),
            reverse,
            skin,
        ),
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
            // Pulse takes precedence over the steady "critical" tint so a
            // transition catches the eye even on a row that's already red.
            let row_style = match app.pulse.cluster_pulse(&row.cluster_id) {
                Some(PulseKind::Disappeared) | Some(PulseKind::WentDown) => skin.pulse_hot(),
                Some(PulseKind::Appeared) => skin.pulse_cool(),
                None if row_critical => skin.row_critical(),
                None => Style::default().fg(skin.secondary),
            };
            Row::new(vec![
                Cell::from(row.cluster_id.clone()),
                Cell::from(format!("{} req/s", row.rps)),
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
