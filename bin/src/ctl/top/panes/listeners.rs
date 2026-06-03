//! LISTENERS pane — the static map of bound sockets.
//!
//! The listeners thread polls `RequestType::ListListeners` at a slower 5 s
//! cadence (operator-paced; listener state changes flow through the EVENTS
//! pane). This pane just renders the freshest snapshot as a flat table.
//!
//! Three columns per address: protocol (HTTP / HTTPS / TCP / UDP), address,
//! status hint (active, scheme, ALPN summary for HTTPS, datagram/flow caps
//! for UDP).

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Paragraph, Row, Table};

use super::super::app::App;
use super::super::theme::Skin;

pub fn render(f: &mut Frame<'_>, area: Rect, app: &App, skin: &Skin) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(format!(
            " LISTENERS · refresh 5 s · {} ",
            app.last_listeners
                .as_ref()
                .map(|_| "live".to_owned())
                .unwrap_or_else(|| "no snapshot yet".into())
        ))
        .style(Style::default().fg(skin.muted));

    let listeners = match app.last_listeners.as_ref() {
        Some(l) => &l.list,
        None => {
            let inner = block.inner(area);
            f.render_widget(block, area);
            f.render_widget(
                Paragraph::new(
                    "Polling ListListeners every 5 s. First snapshot lands shortly \
                     after `sozu top` starts; see the EVENTS pane for \
                     LISTENER_ADDED / LISTENER_REMOVED / LISTENER_UPDATED audit \
                     transitions in the meantime.",
                )
                .style(Style::default().fg(skin.secondary)),
                inner,
            );
            return;
        }
    };

    let mut rows: Vec<Row<'_>> = Vec::new();
    for (addr, cfg) in &listeners.http_listeners {
        rows.push(Row::new(vec![
            Cell::from("HTTP"),
            Cell::from(addr.to_owned()),
            Cell::from(format!("active={}", cfg.active)),
        ]));
    }
    for (addr, cfg) in &listeners.https_listeners {
        let alpn = cfg.alpn_protocols.to_vec().join(",");
        let alpn = if alpn.is_empty() { "—".into() } else { alpn };
        rows.push(
            Row::new(vec![
                Cell::from("HTTPS"),
                Cell::from(addr.to_owned()),
                Cell::from(format!("active={} · alpn={}", cfg.active, alpn)),
            ])
            .style(Style::default().fg(skin.secondary)),
        );
    }
    for (addr, cfg) in &listeners.tcp_listeners {
        rows.push(
            Row::new(vec![
                Cell::from("TCP"),
                Cell::from(addr.to_owned()),
                Cell::from(format!("active={}", cfg.active)),
            ])
            .style(Style::default().fg(skin.secondary)),
        );
    }
    for (addr, cfg) in &listeners.udp_listeners {
        rows.push(
            Row::new(vec![
                Cell::from("UDP"),
                Cell::from(addr.to_owned()),
                Cell::from(format!(
                    "active={} · max_rx={} · max_flows={}",
                    cfg.active, cfg.max_rx_datagram_size, cfg.max_flows
                )),
            ])
            .style(Style::default().fg(skin.secondary)),
        );
    }

    if rows.is_empty() {
        let inner = block.inner(area);
        f.render_widget(block, area);
        f.render_widget(
            Paragraph::new("No listeners configured.").style(Style::default().fg(skin.secondary)),
            inner,
        );
        return;
    }

    let header = Row::new(vec!["proto", "address", "status"]).style(
        Style::default()
            .fg(skin.primary)
            .add_modifier(Modifier::BOLD),
    );
    let widths = [
        Constraint::Length(8),
        Constraint::Min(22),
        Constraint::Min(40),
    ];
    let table = Table::new(rows, widths).header(header).block(block);
    f.render_widget(table, area);
}
