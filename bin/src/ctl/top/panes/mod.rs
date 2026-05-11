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
