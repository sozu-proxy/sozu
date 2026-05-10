//! Pane modules — one per `ActiveTab` variant. Each pane is pure rendering:
//! it takes a `&App` and a `Frame` slice and draws into it. State stays in
//! `App`; panes never mutate.
//!
//! Week-2 ships two panes (OVERVIEW, CLUSTERS). The remaining tabs (BACKENDS,
//! LISTENERS, CERTS, H2, EVENTS) ship in week 3 with their own files. The
//! placeholder `render_placeholder` is shared so unfinished tabs show a
//! consistent "(week 3)" notice instead of a blank pane.

pub mod backends;
pub mod certs;
pub mod clusters;
pub mod events;
pub mod h2;
pub mod listeners;
pub mod overview;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::{Block, Borders, Paragraph};

use super::theme::Skin;

/// Empty-state placeholder used for not-yet-implemented panes. Rounded
/// border + secondary-foreground text so it visually matches the OVERVIEW
/// / CLUSTERS panes once they land.
pub fn render_placeholder(f: &mut Frame<'_>, area: Rect, skin: &Skin, label: &str) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .title(format!(" {label} "))
        .style(Style::default().fg(skin.muted));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let body = Paragraph::new(format!(
        "{label} pane — coming in week 3.\n\
         Use Tab / Shift-Tab or 1..7 to switch panes."
    ))
    .style(Style::default().fg(skin.secondary));
    f.render_widget(body, inner);
}
