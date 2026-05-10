//! EVENTS pane — tail of the `SubscribeEvents` stream the transport thread
//! pulls from the master. Newest at the top so the eye lands on what just
//! happened. Backend-state events (`BACKEND_DOWN`, `BACKEND_UP`,
//! `NO_AVAILABLE_BACKENDS`) carry the cluster + address; control-plane
//! mutations (`CLUSTER_ADDED`, `LISTENER_UPDATED`, `STATE_LOADED`,
//! `METRIC_DETAIL_CHANGED`, …) carry whichever of the optional fields are
//! meaningful for the verb.

use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Paragraph, Row, Table};
use sozu_command_lib::proto::command::EventKind;

use super::super::app::App;
use super::super::theme::Skin;

pub fn render(f: &mut Frame<'_>, area: Rect, app: &App, skin: &Skin) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(format!(" EVENTS · {} retained ", app.events.len()))
        .style(Style::default().fg(skin.muted));

    if app.events.is_empty() {
        let inner = block.inner(area);
        f.render_widget(block, area);
        let body = Paragraph::new(
            "No events yet. The events thread subscribes on startup; the \
             first BACKEND_UP / CLUSTER_ADDED / METRIC_DETAIL_CHANGED arrives \
             whenever the master emits one.",
        )
        .style(Style::default().fg(skin.secondary));
        f.render_widget(body, inner);
        return;
    }

    let header = Row::new(vec![
        Cell::from("when"),
        Cell::from("event"),
        Cell::from("cluster"),
        Cell::from("backend"),
        Cell::from("address"),
    ])
    .style(
        Style::default()
            .fg(skin.primary)
            .add_modifier(Modifier::BOLD),
    );

    let now = Instant::now();
    // Newest first — operators want "what just happened" at the top of the
    // pane. The transport thread pushes onto the back of the VecDeque, so
    // we iterate in reverse.
    let rows: Vec<Row<'_>> = app
        .events
        .iter()
        .rev()
        .map(|topev| {
            let kind = EventKind::try_from(topev.event.kind).ok();
            let kind_label = kind.map(event_kind_label).unwrap_or("UNKNOWN");
            let style = Style::default().fg(event_kind_color(kind, skin));
            Row::new(vec![
                Cell::from(format_relative_age(now, topev.received_at)),
                Cell::from(kind_label.to_owned()),
                Cell::from(topev.event.cluster_id.clone().unwrap_or_default()),
                Cell::from(topev.event.backend_id.clone().unwrap_or_default()),
                Cell::from(
                    topev
                        .event
                        .address
                        .as_ref()
                        .map(|a| format!("{a:?}"))
                        .unwrap_or_default(),
                ),
            ])
            .style(style)
        })
        .collect();

    let widths = [
        Constraint::Length(10),
        Constraint::Length(28),
        Constraint::Min(20),
        Constraint::Min(16),
        Constraint::Min(20),
    ];
    let table = Table::new(rows, widths).header(header).block(block);
    f.render_widget(table, area);
}

fn event_kind_label(kind: EventKind) -> &'static str {
    match kind {
        EventKind::BackendDown => "BACKEND_DOWN",
        EventKind::BackendUp => "BACKEND_UP",
        EventKind::NoAvailableBackends => "NO_AVAILABLE_BACKENDS",
        EventKind::RemovedBackendHasNoConnections => "REMOVED_BACKEND_HAS_NO_CONNECTIONS",
        EventKind::ClusterAdded => "CLUSTER_ADDED",
        EventKind::ClusterRemoved => "CLUSTER_REMOVED",
        EventKind::FrontendAdded => "FRONTEND_ADDED",
        EventKind::FrontendRemoved => "FRONTEND_REMOVED",
        EventKind::CertificateAdded => "CERTIFICATE_ADDED",
        EventKind::CertificateRemoved => "CERTIFICATE_REMOVED",
        EventKind::CertificateReplaced => "CERTIFICATE_REPLACED",
        EventKind::ListenerActivated => "LISTENER_ACTIVATED",
        EventKind::ListenerDeactivated => "LISTENER_DEACTIVATED",
        EventKind::ConfigurationReloaded => "CONFIGURATION_RELOADED",
        EventKind::WorkerKilled => "WORKER_KILLED",
        EventKind::WorkerRelaunched => "WORKER_RELAUNCHED",
        EventKind::LoggingLevelChanged => "LOGGING_LEVEL_CHANGED",
        EventKind::MetricsConfigured => "METRICS_CONFIGURED",
        EventKind::ListenerUpdated => "LISTENER_UPDATED",
        EventKind::StateLoaded => "STATE_LOADED",
        EventKind::StateSaved => "STATE_SAVED",
        EventKind::ListenerAdded => "LISTENER_ADDED",
        EventKind::ListenerRemoved => "LISTENER_REMOVED",
        EventKind::SozuStopRequested => "SOZU_STOP_REQUESTED",
        EventKind::MainUpgraded => "MAIN_UPGRADED",
        EventKind::WorkerUpgraded => "WORKER_UPGRADED",
        EventKind::EventsSubscribed => "EVENTS_SUBSCRIBED",
        EventKind::HealthCheckHealthy => "HEALTH_CHECK_HEALTHY",
        EventKind::HealthCheckUnhealthy => "HEALTH_CHECK_UNHEALTHY",
        EventKind::ClusterRecovered => "CLUSTER_RECOVERED",
        EventKind::MetricDetailChanged => "METRIC_DETAIL_CHANGED",
    }
}

/// Map an event kind to the appropriate skin colour. "Bad" events (down /
/// no backends / unhealthy / killed / cluster removed) take the hot tier;
/// "good" events (up / recovered / healthy / added) take the cool tier;
/// audit-style mutations take the secondary muted colour so they don't
/// drown out actionable signals.
fn event_kind_color(kind: Option<EventKind>, skin: &Skin) -> ratatui::style::Color {
    match kind {
        Some(
            EventKind::BackendDown
            | EventKind::NoAvailableBackends
            | EventKind::HealthCheckUnhealthy
            | EventKind::ClusterRemoved
            | EventKind::WorkerKilled
            | EventKind::SozuStopRequested,
        ) => skin.hot,
        Some(
            EventKind::BackendUp
            | EventKind::ClusterRecovered
            | EventKind::HealthCheckHealthy
            | EventKind::ClusterAdded,
        ) => skin.cool,
        Some(EventKind::MetricDetailChanged) => skin.accent,
        _ => skin.secondary,
    }
}

fn format_relative_age(now: Instant, received_at: Instant) -> String {
    let age = now.saturating_duration_since(received_at);
    let secs = age.as_secs();
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs / 60) % 60)
    }
}
