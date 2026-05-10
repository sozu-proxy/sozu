//! `sozu top` — live operator TUI for the Sōzu reverse proxy.
//!
//! This module is the entry point for the `sozu top` subcommand introduced
//! by the `tui` Cargo feature. The TUI surfaces metrics already collected by
//! Sōzu (`QueryMetrics` over the existing unix command socket) plus an
//! `Event` stream subscription, in a btop/htop-style layout — sparklines,
//! sortable tables, function-key bar, vim navigation, k9s-style colon
//! palette, three glyph modes (Braille / Block / TTY-ASCII).
//!
//! # Architecture (week-1 skeleton)
//!
//! Two-channel synchronous design (no `tokio` runtime in v1, by design — see
//! `tasks/todo.md` for the rationale and the Codex cross-check that locked
//! it). One thread polls `QueryMetrics` on the configurable `--refresh-ms`
//! ticker over its own `Channel`. A second thread streams `SubscribeEvents`
//! over a SEPARATE `Channel` (the unix `Channel` has no message-id
//! correlation; multiplexing query and subscribe on one socket is unsafe).
//! The UI thread owns the terminal, drives crossterm event polling on a
//! 30-fps cap, and consumes both channels via `crossbeam_channel`.
//!
//! Cardinality elevation is automatic: on startup the TUI sends
//! `SetMetricDetail{ client_id, detail = Backend, ttl_seconds = 60 }`. A
//! renewer re-sends every `ttl/2` seconds. On exit (Drop, panic, SIGINT,
//! SIGTERM) the TUI sends a best-effort `SetMetricDetail{ client_id, clear:
//! true }`. Crash safety: the lease self-expires server-side after
//! `ttl_seconds` so a dead `sozu top` cannot permanently elevate cardinality.
//!
//! Week 1 lands the subcommand skeleton, the proto + master + worker
//! plumbing, the `Aggregator` lease bookkeeping, and the validation gate.
//! Weeks 2-3 land the panes (overview, clusters, backends, listeners, h2,
//! events, certs), theming + glyph cascade. Week 4 lands the test ladder
//! (`insta` snapshots at 80x24 / 120x40 / 200x60, integration via the
//! existing `port_registry` harness) and the docs.
//!
//! See `tasks/todo.md` for the full plan, including the Codex cross-check
//! and the audited approval gates.

use std::path::PathBuf;

use crate::cli::{TopDetail, TopGlyphs};

use super::{CommandManager, CtlError};

/// Bag of arguments forwarded from the clap `SubCmd::Top { … }` variant.
/// Kept in sync with the clap declaration in `bin/src/cli.rs`.
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
    /// Week-1 skeleton: validates the argument bag, prints a placeholder
    /// notice, and exits cleanly. The render loop, transport threads,
    /// `DetailGuard` lease lifecycle, and pane implementations land in
    /// subsequent steps.
    pub fn run_top(&mut self, args: TopArgs) -> Result<(), CtlError> {
        // Surface the argument bag so reviewers can confirm clap wiring
        // before the renderer lands. Replaced by `render_loop` in week 2.
        eprintln!(
            "sozu top — week-1 skeleton.\n\
             refresh_ms={refresh_ms} no_color={no_color} no_mouse={no_mouse}\n\
             skin={skin:?} detail={detail:?} lease_ttl_seconds={lease_ttl_seconds}\n\
             snapshot={snapshot:?} tick_once={tick_once} log_file={log_file:?} glyphs={glyphs:?}",
            refresh_ms = args.refresh_ms,
            no_color = args.no_color,
            no_mouse = args.no_mouse,
            skin = args.skin,
            detail = args.detail,
            lease_ttl_seconds = args.lease_ttl_seconds,
            snapshot = args.snapshot,
            tick_once = args.tick_once,
            log_file = args.log_file,
            glyphs = args.glyphs,
        );
        eprintln!("(render loop, transport threads, and panes land in week 2.)");
        Ok(())
    }
}
