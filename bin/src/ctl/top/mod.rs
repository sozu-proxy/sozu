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
//! Four synchronous transport threads (no `tokio` runtime in v1, by design):
//!
//! 1. The **collector** thread polls `RequestType::QueryMetrics` on the
//!    `--refresh-ms` ticker over its own `Channel` and pushes each
//!    `AggregatedMetrics` into a `crossbeam_channel::bounded::<Snapshot>(1)`.
//! 2. The **events** thread subscribes to `RequestType::SubscribeEvents`
//!    over a SEPARATE `Channel` (the unix `Channel` has no message-id
//!    correlation; multiplexing query and subscribe on one socket is
//!    unsafe) and forwards each `Event` into a `bounded::<TopEvent>(64)`.
//! 3. The **listeners-collector** thread polls `RequestType::ListListeners`
//!    every 5 s over its own channel into a `bounded::<ListenersSnapshot>(1)`.
//! 4. The **certs-collector** thread polls
//!    `RequestType::QueryCertificatesFromTheState` every 30 s over its own
//!    channel into a `bounded::<CertsSnapshot>(1)`.
//!
//! The UI thread (this thread) owns the terminal, drives crossterm event
//! polling on a 30-fps cap, and consumes all four channels.
//!
//! Cardinality elevation is automatic: on startup the TUI sends
//! `SetMetricDetail{ client_id, detail = Backend, ttl_seconds = 60 }`. A
//! renewer re-sends every `ttl/2` seconds. On exit (Drop, panic, SIGINT,
//! SIGTERM, SIGHUP) the TUI sends a best-effort `SetMetricDetail{
//! client_id, clear: true }`. SIGTERM and SIGHUP coverage requires the
//! `termination` feature of the `ctrlc` crate — without it `set_handler`
//! catches SIGINT only and `kill -TERM` would leave the terminal in
//! alt-screen + raw mode. Crash safety: the lease self-expires
//! server-side after `ttl_seconds` so a dead `sozu top` cannot
//! permanently elevate cardinality.

mod app;
mod cardinality;
mod panes;
mod render;
mod theme;
mod transport;

#[cfg(test)]
mod snapshot_tests;

use crate::cli::{TopDetail, TopGlyphs};

use self::cardinality::DetailGuard;
use self::render::RenderConfig;
use self::transport::{spawn_certs, spawn_collector, spawn_events, spawn_listeners};

use super::{CommandManager, CtlError};

/// Bag of arguments forwarded from the clap `SubCmd::Top { … }` variant.
#[derive(Debug, Clone)]
pub struct TopArgs {
    pub refresh_ms: u64,
    pub no_mouse: bool,
    pub skin: Option<String>,
    pub detail: Option<TopDetail>,
    pub lease_ttl_seconds: u32,
    pub snapshot: Option<u32>,
    pub tick_once: bool,
    pub glyphs: Option<TopGlyphs>,
}

impl CommandManager {
    /// Entry point for the `sozu top` subcommand.
    ///
    /// Spins up the four transport threads (collector, events, listeners,
    /// certs), applies the cardinality lease, installs the SIGINT/SIGTERM
    /// and panic-hook restore guards, then runs the render loop until the
    /// user quits or a snapshot/tick budget exhausts. Returns once the
    /// terminal is restored.
    pub fn run_top(&mut self, args: TopArgs) -> Result<(), CtlError> {
        // Transport threads own their own `Channel` connections — see the
        // module-level docs for why we don't reuse `self.channel`.
        //
        // The shutdown flag is the canonical wake-up for the events thread:
        // dropping its `Receiver<TopEvent>` cannot propagate across the
        // unix socket. The three poll-driven threads still exit on
        // receiver-drop, but we join them all on the way out for symmetry.
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (snapshot_rx, collector) = spawn_collector(self.config.clone(), args.refresh_ms)?;
        let (events_rx, events) =
            spawn_events(self.config.clone(), std::sync::Arc::clone(&shutdown))?;
        let (listeners_rx, listeners) = spawn_listeners(self.config.clone())?;
        let (certs_rx, certs) = spawn_certs(self.config.clone())?;

        // Apply the runtime cardinality lease. If the master/worker is too
        // old to decode `SetMetricDetail`, we surface the failure but keep
        // the TUI running — operators on degraded fleets still get the
        // cluster-level data the worker is already emitting.
        //
        // The failure path stashes a diagnostic into a string that the
        // renderer surfaces via `App.status`, rather than writing to
        // `stderr` directly: an `eprintln!` from this site would land
        // *between* the spawn calls above and `enable_raw_mode` inside the
        // renderer, leaving a one-line warning the operator never sees
        // (alt-screen wipes it on entry) and that pollutes the shell
        // scrollback on exit.
        let detail = args.detail.unwrap_or(TopDetail::Backend);
        // Shared status slot the renewer publishes degraded-mode notes
        // into. The render loop drains it once per tick (see
        // `DetailGuard::take_status`) and the renderer also keeps a
        // clone for post-`apply` lifetime so the F-key bar can surface
        // late renewer failures.
        let lease_status_slot = cardinality::new_status_slot();
        let (lease, lease_status) = match DetailGuard::apply(
            &self.config,
            detail,
            args.lease_ttl_seconds,
            "sozu top",
            std::sync::Arc::clone(&lease_status_slot),
        ) {
            Ok(g) => (Some(g), None),
            Err(e) => (
                None,
                Some(format!(
                    "could not elevate metric detail (continuing without lease): {e}"
                )),
            ),
        };

        let render_cfg = RenderConfig {
            mouse: !args.no_mouse,
            tick_once: args.tick_once,
            snapshot_frames: args.snapshot,
            skin: args.skin.clone(),
            glyphs: args.glyphs,
            initial_status: lease_status,
            lease_status: lease_status_slot,
        };
        let result = render::run(render_cfg, snapshot_rx, events_rx, listeners_rx, certs_rx);

        // Drop order: lease first (issues the best-effort `clear`), then
        // flip the shutdown flag so the events thread observes it on the
        // next bounded read, then join all four transport handles so we
        // never return with background threads still queueing into a
        // detached aggregator.
        drop(lease);
        shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = collector.join();
        let _ = listeners.join();
        let _ = certs.join();
        let _ = events.join();

        result.map_err(|e| {
            CtlError::ResolvePath("sozu top render loop".to_owned(), std::io::Error::other(e))
        })
    }
}
