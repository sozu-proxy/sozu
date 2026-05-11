//! BACKENDS pane — flat sortable table across every cluster's backends.
//!
//! Week-3 scope is a flat list; in-table cluster-scope filter (drill-down
//! from the CLUSTERS row) is week 4. The flat view already answers the
//! "which backend is on fire" question at a glance because the default
//! sort is bandwidth descending — the busiest backend lands at the top.

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Paragraph, Row, Table};

use super::super::app::{App, BackendSortKey, PulseKind};
use super::super::theme::Skin;
use super::sort_header;

pub fn render(f: &mut Frame<'_>, area: Rect, app: &App, skin: &Skin) {
    let rows = app.backend_rows();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(format!(
            " BACKENDS · sort: {} {} · {} backend{} ",
            app.backend_sort.label(),
            if app.backend_sort_reverse {
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
        let body = Paragraph::new(
            "No per-backend metrics yet. Either no traffic has reached a backend, \
             or the worker is configured below `metrics.detail = backend`. The \
             SetMetricDetail lease auto-elevates when supported; check the EVENTS \
             pane (tab 7) for METRIC_DETAIL_CHANGED to confirm.",
        )
        .style(Style::default().fg(skin.secondary));
        f.render_widget(body, inner);
        return;
    }

    let reverse = app.backend_sort_reverse;
    let active = |key: BackendSortKey| app.backend_sort == key;
    let header = Row::new(vec![
        sort_header("cluster", active(BackendSortKey::ClusterId), reverse, skin),
        sort_header("backend", active(BackendSortKey::BackendId), reverse, skin),
        sort_header(
            "bw down/up B",
            active(BackendSortKey::Bandwidth),
            reverse,
            skin,
        ),
        sort_header("conn", active(BackendSortKey::Connections), reverse, skin),
        sort_header("p50", active(BackendSortKey::LatencyP99), reverse, skin),
        sort_header("p99", active(BackendSortKey::LatencyP99), reverse, skin),
        sort_header("req", active(BackendSortKey::Requests), reverse, skin),
    ])
    .style(
        Style::default()
            .fg(skin.primary)
            .add_modifier(Modifier::BOLD),
    );

    let body: Vec<Row<'_>> = rows
        .iter()
        .map(|row| {
            let critical = row.p99_ms as f64 >= app.thresholds.latency_p99_critical_ms;
            // Pulse takes precedence over the steady tint so transitions
            // catch the eye even on rows that are already red.
            let row_style = match app.pulse.backend_pulse(&row.cluster_id, &row.backend_id) {
                Some(PulseKind::WentDown) | Some(PulseKind::Disappeared) => skin.pulse_hot(),
                Some(PulseKind::Appeared) => skin.pulse_cool(),
                None if critical => skin.row_critical(),
                None => Style::default().fg(skin.secondary),
            };
            Row::new(vec![
                Cell::from(row.cluster_id.clone()),
                Cell::from(row.backend_id.clone()),
                Cell::from(format!(
                    "{}/{}",
                    format_bytes(row.back_bytes_in),
                    format_bytes(row.back_bytes_out),
                )),
                Cell::from(format!("{}", row.connections)),
                Cell::from(format!("{}", row.p50_ms)),
                Cell::from(format!("{}", row.p99_ms)),
                Cell::from(format!("{}", row.requests_total)),
            ])
            .style(row_style)
        })
        .collect();

    let widths = [
        Constraint::Min(18),
        Constraint::Min(20),
        Constraint::Length(16),
        Constraint::Length(6),
        Constraint::Length(6),
        Constraint::Length(6),
        Constraint::Length(8),
    ];
    let table = Table::new(body, widths).header(header).block(block);
    f.render_widget(table, area);
}

fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if bytes >= GIB {
        format!("{:.1}G", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1}M", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1}K", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes}")
    }
}
