# `sozu top` — live operator TUI

`sozu top` is a btop/htop-style terminal dashboard for the Sōzu reverse
proxy. It surfaces metrics that Sōzu already emits (per-cluster, per-
backend, H2-flow, slab-saturation, recent control-plane events), in a
single screen with sparklines, sortable tables, and a colour-coded
event tail. The subcommand is gated behind the optional `tui` Cargo
feature so production binaries built without `--features tui` do not
pull `ratatui`, `crossterm`, or any other TUI dependency.

```bash
cargo build -p sozu --features tui --release
sozu --config /etc/sozu/config.toml top
sozu --version       # reports `+tui` when the subcommand is linked
```

## Architecture at a glance

- Four synchronous transport threads poll the master over the existing
  unix command socket, each on its own dedicated connection:
  - **Collector** sends `QueryMetrics` every `--refresh-ms`
    (default 1 s) and pushes each `AggregatedMetrics` into a
    bounded-1 channel with publish-or-skip-on-backpressure semantics.
  - **Events** subscribes to the control-plane event stream (backend
    up/down, cluster added/removed, certificate added/removed, …).
  - **Listeners** polls `ListListeners` every 5 s.
  - **Certs** polls `QueryCertificatesFromTheState` every 30 s.
- A single UI thread owns the terminal, polls crossterm input on a
  30 fps cap, and synchronises frame output via DEC mode 2026
  (`BeginSynchronizedUpdate` / `EndSynchronizedUpdate`) so tmux and
  iTerm2 see one atomic paint per frame.
- The TUI auto-elevates the worker's metric cardinality to `Backend`
  by leasing it through a `SetMetricDetail` runtime verb. The lease
  is `client_id`-keyed, TTL-bounded (default 60 s, clamp 300 s), and
  self-expires server-side if the TUI crashes — so a dead `sozu top`
  cannot permanently elevate cardinality. Renewal runs at half-TTL.

## Panes

Numbered tabs at the top of the screen; key digits map directly.

| Tab | Pane | Drives |
|-----|------|--------|
| `1` | OVERVIEW | Four sparklines (REQUESTS/SEC, p99 LATENCY, 5xx ERRORS, SATURATION) with big numerals + trend glyphs (`▲ ▼ ●`). |
| `2` | CLUSTERS | Sortable per-cluster table (cluster_id, rps, err%, p50, p99, backends_available/total). Default sort: error-rate desc. |
| `3` | BACKENDS | Sortable per-backend table (cluster, backend, bw down/up, connections, p50, p99, requests). Default sort: bandwidth desc. |
| `4` | LISTENERS | HTTP / HTTPS / TCP listener inventory; refreshed every 5 s. |
| `5` | CERTS | Certificate inventory by listener address + fingerprint + names; refreshed every 30 s. |
| `6` | H2 | Active streams, ALPN H2 share, flow-control gauges, frame counters, and CVE flood-mitigation counters (`h2.flood.violation.*`). |
| `7` | EVENTS | Colour-coded tail of `SubscribeEvents`. BACKEND_DOWN / NO_AVAILABLE_BACKENDS / WORKER_KILLED in hot, BACKEND_UP / CLUSTER_RECOVERED in cool, METRIC_DETAIL_CHANGED in accent. |

Threshold-driven row tinting is consistent across all panes:

- **Critical (hot, bold)**: p99 over `latency_p99_critical_ms` (500 ms
  default), 5xx ratio over `error_ratio_critical_pct` (1 %), all
  backends down for a cluster.
- **Pulse (background tint, 4 ticks ≈ 4 s)**: cluster appeared, cluster
  disappeared, backend went down. Pulses take precedence over the
  steady threshold tint so transitions catch the eye on rows that are
  already red.

A 5-line big-text alert banner overlays the active pane when the
threshold table is critical, with a copyable side strip for tmux
scrollback and screen readers.

## Key bindings

| Key | Action |
|-----|--------|
| `1`-`7` | Jump to the numbered tab. |
| `Tab` / `Shift-Tab` | Cycle tabs forward / backward. |
| `s` / `S` | Cycle / reverse the active sort column (CLUSTERS, BACKENDS). |
| `?` / `F1` | Toggle help overlay. |
| `q` / `Q` / `Ctrl-C` / `F10` | Quit. SIGINT / SIGTERM also restore the terminal cleanly. |

The bottom function-key bar (`F1 Help · F2 Theme · F3 Find · F4 Filter
· F5 Pause · F6 Sort · F7 Detail- · F8 Detail+ · F9 Config · F10 Quit`)
reserves slots for actions wired in future milestones. The currently
active sort column appears on the right of the bar.

## Cardinality (`SetMetricDetail` lease)

The TUI auto-applies a `Backend`-level lease on startup so the
BACKENDS / per-backend rows on CLUSTERS and OVERVIEW carry real
data. The lease appears as an `EventKind::METRIC_DETAIL_CHANGED` event
in the EVENTS pane. Override with `--detail process|frontend|cluster|
backend` or `--lease-ttl-seconds N`. Workers that pre-date the verb
(e.g. inherited from a prior master across an `UpgradeMain`) reply
with the standard `unknown request type` error; the remaining workers
still apply the lease normally. Production deployments keep master
and workers in sync via the upgrade path, so this mixed-version state
is transient.

## Skins

`--skin <name>` (with `SOZU_TOP_SKIN` env override for k9s parity)
resolves a TOML skin file from one of:

1. `$XDG_CONFIG_HOME/sozu/skins/<name>.toml` (fallback `$HOME/.config/
   sozu/skins/<name>.toml`).
2. `/etc/sozu/skins/<name>.toml` for system-wide skins.

Schema (all `#RRGGBB` hex strings; `categorical` is optional and falls
back to the built-in Okabe-Ito + Viridis high-end palette):

```toml
primary   = "#e8e8f0"   # headings, focused tab, big-text numerals
secondary = "#b4b8c0"   # status text, function-key labels
accent    = "#56c0f0"   # active sort column, selected row
cool      = "#39ad98"   # "all is well" sparkline tails
warm      = "#f5bf4f"   # mid-high sparkline tails
hot       = "#e8545a"   # alert peaks + row-critical
muted     = "#606470"   # tab labels not in focus, rule lines
categorical = [
    "#009e73", "#56b4e9", "#d55e00", "#cc79a7",
    "#f0e442", "#0072b2", "#e69f00", "#f7d13c", "#5ec961",
]
```

Path safety: `--skin` rejects names containing `..`, `/`, or `\` so
`--skin ../../etc/passwd` cannot escape the skins directory. On a
missing file or parse error the TUI surfaces a one-line diagnostic in
the status bar and falls back to the built-in palette.

## Glyphs

Three sparkline / bar ramp modes resolve via this cascade unless
`--glyphs braille|block|tty` is passed explicitly:

1. `TERM` reports `dumb`, `linux`, `xterm-old`, or `*-mono*` →
   7-bit ASCII (`. : - = + * # @`).
2. `LC_ALL` / `LC_CTYPE` / `LANG` ends in `UTF-8`/`UTF8` AND is not
   `C` / `POSIX` / `C.UTF-8` → Braille mosaics
   (`⠀ ⡀ ⣀ ⣄ ⣤ ⣦ ⣶ ⣷ ⣿`).
3. Otherwise: Unicode block elements (`▁ ▂ ▃ ▄ ▅ ▆ ▇ █`). Broadest
   Unicode-terminal compatibility without nerd-font dependency.

## Accessibility

The default Okabe-Ito categorical palette is colour-blind safe in
isolation and distinguishable across the three common dichromatic
types. Critical/warm/cool tiers are always backed up by a redundant
glyph (`▲ ▼ ●` on the OVERVIEW pane, `▲ ▼` on sortable column
headers) so the colour signal isn't load-bearing on its own.

## Mouse capture

Mouse is on by default. Some multiplexers mis-route SGR mouse events
to the underlying shell when the TUI exits; if that happens to you,
add `--no-mouse`. SIGINT / SIGTERM / panic all restore the terminal
via the `RawModeGuard::Drop` path (raw mode disabled, mouse capture
released, cursor shown, alt-screen left) so the shell stays usable
even after a hard crash.

## Test affordances

- `--tick-once` drives one data tick + one render tick and exits.
  Useful for smoke tests and CI snapshots.
- `--snapshot N` renders `N` frames and exits.
- The `insta` snapshot tests under `bin/src/ctl/top/snapshots/`
  exercise every pane at 80x24 and 120x40; run with `cargo test
  -p sozu --features tui snapshot_tests`. Accept changes with
  `cargo insta review`.

## Operational footguns

- `--features tui` materially changes the binary size and dependency
  graph. Default `sozu` builds stay lean; this is by design.
- The TUI's cardinality lease elevates the worker's keyspace while
  running; the `EventKind::METRIC_DETAIL_CHANGED` audit trail lets
  StatsD scrapers see the change. The configured floor is restored
  on lease expiry (TTL) or explicit revoke (clean exit).
- `MetricDetail = Backend` can balloon the per-backend keyspace on
  high-cardinality fleets — every backend gets its own labelled time
  series at the StatsD / Prometheus sink. If you see operator-side
  metric pipelines saturate, override with `--detail cluster` and
  lease at the lower tier.
- The render loop pauses redraws when state is unchanged AND no
  pulse is active; on a quiet system `sozu top` consumes ~2-3 % of
  one core on a Linux 6.x kernel + alacritty.
- Transport layout: four synchronous threads (snapshots, listeners,
  certs, events) open six unix-socket connections to the master per
  invocation. The four polling threads keep one connection each; the
  cardinality renewer holds a fifth for its half-TTL apply renewals
  and a sixth parked for the final clear-on-exit. Sized for a single
  operator on one terminal; spawning many concurrent `sozu top`
  invocations from a wrapper script saturates the master's accept
  backlog (`listen(2)` default = 128).
- Privacy: whatever you type in `--reason` lands in the audit log
  (text + JSON sinks) AND fans out via `SubscribeEvents` to any
  same-UID subscriber. Avoid embedding PII, customer IDs, or ticket
  references that should not leave the host's audit boundary.
  `client_id` and `reason` are length-capped server-side (64 / 256
  bytes) and stripped of `,`/`=`/control bytes before being rendered
  into the audit columns, so the values cannot smuggle adjacent KV
  columns into a SIEM that ingests on `, ` / `=`.
