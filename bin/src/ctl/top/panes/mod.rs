//! Pane modules — one per `ActiveTab` variant. Each pane is pure rendering:
//! it takes a `&App` and a `Frame` slice and draws into it. State stays in
//! `App`; panes never mutate.

pub mod backends;
pub mod certs;
pub mod clusters;
pub mod events;
pub mod h2;
pub mod listeners;
pub mod overview;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Cell;

use super::theme::Skin;

/// Render a sortable table header cell. When `active` is true, the column is
/// the current sort key and gets the accent colour, bold, and an arrow glyph
/// (▲ for ascending / reversed, ▼ for descending / default). Otherwise the
/// label is drawn in the muted secondary tint.
///
/// Shared by the CLUSTERS and BACKENDS panes; the call sites compute the
/// `active`/`reverse` booleans against their own enum (`ClusterSortKey`,
/// `BackendSortKey`) so this helper stays generic over the column type.
pub(super) fn sort_header(label: &str, active: bool, reverse: bool, skin: &Skin) -> Cell<'static> {
    if active {
        let arrow = if reverse { "▲" } else { "▼" };
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
