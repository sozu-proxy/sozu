//! `sozu top` — live operator TUI for the Sōzu reverse proxy.
//!
//! The TUI surfaces metrics already collected by Sōzu (`QueryMetrics` over
//! the existing unix command socket) plus an `Event` stream subscription,
//! in a btop/htop-style layout — sparklines, sortable tables, function-key
//! bar, vim navigation, k9s-style colon palette, three glyph modes
//! (Braille / Block / TTY-ASCII).
//!
//! # Architecture
//!
//! Two synchronous transport threads (no `tokio` runtime in v1, by design
//! — see `tasks/todo.md` and the Codex cross-check that locked it):
//!
//! 1. The collector thread polls `RequestType::QueryMetrics` on the
//!    `--refresh-ms` ticker over its own `Channel` and pushes each
//!    `AggregatedMetrics` into a `crossbeam_channel::bounded::<Snapshot>(1)`.
//! 2. The events thread subscribes to `RequestType::SubscribeEvents` over
//!    a SEPARATE `Channel` (the unix `Channel` has no message-id
//!    correlation; multiplexing query and subscribe on one socket is
//!    unsafe) and forwards each `Event` into a `bounded::<TopEvent>(64)`.
//! 3. The UI thread (this thread) owns the terminal, drives crossterm
//!    event polling on a 30-fps cap, and consumes both channels.
//!
//! Cardinality elevation is automatic: on startup the TUI sends
//! `SetMetricDetail{ client_id, detail = Backend, ttl_seconds = 60 }`. A
//! renewer re-sends every `ttl/2` seconds. On exit (Drop, panic, SIGINT,
//! SIGTERM) the TUI sends a best-effort `SetMetricDetail{ client_id, clear:
//! true }`. Crash safety: the lease self-expires server-side after
//! `ttl_seconds` so a dead `sozu top` cannot permanently elevate
//! cardinality.

mod app;
mod cardinality;
mod panes;
mod render;
mod theme;
mod transport;

use std::path::PathBuf;

use crate::cli::{TopDetail, TopGlyphs};

use self::cardinality::DetailGuard;
use self::render::RenderConfig;
use self::transport::{spawn_collector, spawn_events, spawn_listeners};

use super::{CommandManager, CtlError};

/// Bag of arguments forwarded from the clap `SubCmd::Top { … }` variant.
#[derive(Debug, Clone)]
pub struct TopArgs {
    pub refresh_ms: u64,
    pub no_color: bool,
    pub no_mouse: bool,
    pub skin: Option<String>,
    pub detail: Option<TopDetail>,
    pub lease_ttl_seconds: u32,
    pub snapshot: Option<u32>,
    pub tick_once: bool,
    pub log_file: Option<PathBuf>,
    pub glyphs: Option<TopGlyphs>,
}

impl CommandManager {
    /// Entry point for the `sozu top` subcommand.
    ///
    /// Spins up the two transport threads, applies the cardinality lease,
    /// installs the SIGINT/SIGTERM + panic-hook restore guards, and runs
    /// the render loop until the user quits or a snapshot/tick budget
    /// exhausts. Returns once the terminal is restored.
    pub fn run_top(&mut self, args: TopArgs) -> Result<(), CtlError> {
        // Transport threads own their own `Channel` connections — see the
        // module-level docs for why we don't reuse `self.channel`.
        let (snapshot_rx, _collector) = spawn_collector(self.config.clone(), args.refresh_ms)?;
        let (events_rx, _events) = spawn_events(self.config.clone())?;
        let (listeners_rx, _listeners) = spawn_listeners(self.config.clone())?;

        // Apply the runtime cardinality lease. If the master/worker is too
        // old to decode `SetMetricDetail`, we surface the failure but keep
        // the TUI running — operators on degraded fleets still get the
        // cluster-level data the worker is already emitting.
        let detail = args.detail.unwrap_or(TopDetail::Backend);
        let lease =
            match DetailGuard::apply(&self.config, detail, args.lease_ttl_seconds, "sozu top") {
                Ok(g) => Some(g),
                Err(e) => {
                    eprintln!(
                        "sozu top: could not elevate metric detail (continuing without lease): {e}"
                    );
                    None
                }
            };

        let render_cfg = RenderConfig {
            mouse: !args.no_mouse,
            tick_once: args.tick_once,
            snapshot_frames: args.snapshot,
            skin: args.skin.clone(),
        };
        let result = render::run(render_cfg, snapshot_rx, events_rx, listeners_rx);

        // Drop order: lease first (issues the best-effort `clear`), then
        // the transport thread handles fall out of scope when this
        // function returns. Both transport threads exit when their
        // receivers are dropped at the end of `render::run`.
        drop(lease);

        result.map_err(|e| {
            CtlError::ResolvePath("sozu top render loop".to_owned(), std::io::Error::other(e))
        })
    }
}
