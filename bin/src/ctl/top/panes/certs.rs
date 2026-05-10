//! CERTS pane — TLS certificate inventory grouped by bound address.
//!
//! Pulled from `QueryCertificatesFromTheState` on a 30 s ticker (cert
//! lifecycle is operator-paced; every transition also lands as a
//! `CERTIFICATE_ADDED` / `CERTIFICATE_REMOVED` / `CERTIFICATE_REPLACED`
//! event on the EVENTS pane). Each row is one certificate with the bound
//! address, the SNI it serves, and a truncated fingerprint suffix the
//! operator can match against `sozu certificate list` output.

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
            " CERTS · refresh 30 s · {} ",
            app.last_certs
                .as_ref()
                .map(|_| "live".to_owned())
                .unwrap_or_else(|| "no snapshot yet".into())
        ))
        .style(Style::default().fg(skin.muted));

    let certs = match app.last_certs.as_ref() {
        Some(c) => &c.list,
        None => {
            let inner = block.inner(area);
            f.render_widget(block, area);
            f.render_widget(
                Paragraph::new(
                    "Polling QueryCertificatesFromTheState every 30 s. First snapshot \
                     lands shortly after `sozu top` starts; check the EVENTS pane (tab \
                     7) for CERTIFICATE_ADDED / REMOVED / REPLACED transitions in the \
                     meantime.",
                )
                .style(Style::default().fg(skin.secondary)),
                inner,
            );
            return;
        }
    };

    // Flatten every (address, summary) pair so the table sorts naturally
    // by address. Fingerprint suffix only — long hex digests bury the
    // domain column on narrow terminals and the operator can always
    // match against `sozu certificate list` for the full hash.
    let mut rows: Vec<Row<'_>> = Vec::new();
    let mut total_certs = 0u32;
    for by_address in &certs.certificates {
        let addr = format!("{:?}", by_address.address);
        for summary in &by_address.certificate_summaries {
            let fp_suffix = if summary.fingerprint.len() > 12 {
                let tail = summary.fingerprint.len() - 12;
                format!("…{}", &summary.fingerprint[tail..])
            } else {
                summary.fingerprint.clone()
            };
            rows.push(
                Row::new(vec![
                    Cell::from(addr.clone()),
                    Cell::from(summary.domain.clone()),
                    Cell::from(fp_suffix),
                ])
                .style(Style::default().fg(skin.secondary)),
            );
            total_certs += 1;
        }
    }

    if rows.is_empty() {
        let inner = block.inner(area);
        f.render_widget(block, area);
        f.render_widget(
            Paragraph::new("No certificates loaded.").style(Style::default().fg(skin.secondary)),
            inner,
        );
        return;
    }

    let header = Row::new(vec![
        Cell::from(format!("address · {total_certs} cert(s)")),
        Cell::from("domain (SNI)"),
        Cell::from("fingerprint (suffix)"),
    ])
    .style(
        Style::default()
            .fg(skin.primary)
            .add_modifier(Modifier::BOLD),
    );
    let widths = [
        Constraint::Min(28),
        Constraint::Min(28),
        Constraint::Length(20),
    ];
    let table = Table::new(rows, widths).header(header).block(block);
    f.render_widget(table, area);
}
