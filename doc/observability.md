# Observability — architecture & extension points

This document explains **how** Sōzu emits metrics, logs, audit events, and trace
context, where the seams are, and what conventions to follow when adding new
instrumentation. For the **inventory** of currently emitted metrics and access-
log fields, see [`configure.md`](configure.md). For the H2 mux internals see
[`h2_mux_internals.md`](h2_mux_internals.md).

## Topology

Each Sōzu worker is a **single-threaded mio event loop** with three concurrent
observability surfaces:

```
                            ┌──────────────────────────────┐
                            │       worker process          │
                            │                                │
   accept loop ──┐          │   ┌─ thread-local METRICS ─┐  │       ┌─ statsd UDP ─┐
   TLS handshake │          │   │                         │──┼──────▶│ network drain│
   H2 mux        │── incr! ─┼─▶ │   Aggregator            │  │       └──────────────┘
   H1 editor     │  count!  │   │  (counters, gauges,     │  │
   backend pool  │  gauge!  │   │   HDR-time histograms)  │──┼──┐
   socket I/O    │  gauge_  │   │                         │  │  │  ┌─ sozu CLI ─┐
   …             │  add!    │   └─────────────────────────┘  │  └─▶│ local drain│
                 │  time!   │                                 │     └────────────┘
                            │
                            │   log_context!  ─────────────► main log stream
                            │   (structured prefix)            (info/warn/error)
                            │
                            │   log_access!   ─────────────► access log stream
                            │   (RequestRecord)               (ASCII or protobuf)
                            │
                            │   SubscribeEvents bus ───────► control-plane events
                            │   (EventKind)                    (incl. audit log line — MUX-style, at info level)
                            └──────────────────────────────┘
```

Three properties shape every extension:

1. **Single-threaded per worker** — no `Arc<Mutex>` inside the loop. `METRICS`
   is `thread_local!` (`lib/src/metrics/mod.rs`).
2. **Edge-triggered epoll via mio** — anything queued from the read path must
   `signal_pending_write` on the readiness tracker (`feedback_epollet_signal_pending_write`
   in agent memory). This affects metric flush timing only when the metrics
   socket itself stalls — usually transparent to instrumentation authors.
3. **`&'static str` keys everywhere** — counters and gauges accept only
   `&'static str` keys (the type signature of `count_add` / `set_gauge`).
   Per-error / per-kind breakdowns must materialise the keys at compile time.

## Metric primitives

| Macro | Signature | When to use | Example |
|---|---|---|---|
| `incr!(key)` / `incr!(key, cluster_id, backend_id)` | `&'static str` | Increment by 1. The 3-arg form labels by cluster+backend. | `incr!("h2.frames.rx.data");` |
| `count!(key, value)` | `&'static str, i64` | Increment by N (e.g. byte counts). | `count!("bytes_in", n as i64);` |
| `decr!(key)` | `&'static str` | Decrement by 1. Pair with a prior `incr!`. | `decr!("http.active_requests");` |
| `gauge!(key, value)` | `&'static str, usize` | **Absolute snapshot.** ⚠️ Last-writer-wins across emit sites — only safe for proxy-level state with a single emitter (e.g. `client.connections`). | `gauge!("client.max_connections", n);` |
| `gauge_add!(key, delta)` / `gauge_add!(key, delta, cluster, backend)` | `&'static str, i64` | **Lifecycle delta.** Aggregates correctly across emit sites. Pair every `+1` with a `-1` on every close path. | `gauge_add!("backend.pool.size", 1);` |
| `time!(key, ms)` / `time!(key, cluster_id, ms)` | `&'static str, usize` | Latency in ms. Stored as HDR histogram in the local drain. | `time!("backend_response_time", cluster, ms);` |

### Gauge correctness

The most common metric bug is a `gauge!()` emitted from a per-connection /
per-stream context: the value the dashboard sees is "whatever the last
emitter wrote", not aggregate state. **Default to `gauge_add!()` for any
metric that is incremented from more than one emit site.** The H2 connection
gauges (`h2.connection.{active_streams,window_bytes,pending_window_updates}`)
were converted to `gauge_add!` lifecycle deltas plus `impl Drop` symmetric
teardown after this exact bug went unnoticed for a while.

### Gauge underflow

Past production incidents (`a650ad69`, `d2f01ed4`) all came from
`gauge_add!(-1)` running without a paired `+1` having run earlier.
`MetricValue::update` panics in debug builds (and clamps to zero in release)
on underflow, but the metric is wrong either way. Two patterns prevent this
class of bug:

1. **Co-locate increments and decrements with the state being tracked.** If
   the gauge measures "active backend connections", emit the `+1` at the same
   site that creates the backend connection and the `-1` at the same site
   that closes it.
2. **Move teardown into `impl Drop`** when the close paths are scattered
   across the codebase (graceful shutdown, force-disconnect, panic-unwind).
   `impl Drop for ConnectionH2` is the canonical example: it subtracts
   whatever `gauge_add!(+N)` the connection ever emitted, regardless of which
   close path runs.

### Cardinality budgets

StatsD (UDP) is forgiving but Prometheus / influxdb is not. Two rules:

1. **Static keys only.** A key is `&'static str` — synthesised at compile
   time via `concat!`, a literal, or `LazyLock`-leaked at startup. Per-IP
   labels MUST hash into a bounded bucket table (see
   `lib/src/server.rs::PER_SOURCE_BUCKETS` for the canonical 256-bucket
   pattern with `LazyLock` static-string array).
2. **Opt-in granularity.** `MetricDetail` (proto) /
   `MetricDetailLevel` (config) lets operators choose
   `process | frontend | cluster | backend`. Mirrors HAProxy's
   `extra-counters` opt-in. New label dimensions should respect this knob;
   the wiring layer is a follow-up MR.

## Log primitives

Structured prefixes via per-protocol `log_context!` / `log_module_context!` /
`log_context_lite!` macros — defined per file:

| Prefix | File | Carries |
|---|---|---|
| `MUX` | `protocol/mux/mod.rs` | session ULID, peer/local, frontend, backend list |
| `MUX-H2` | `protocol/mux/h2.rs` | …plus position, state, total RST counts, draining |
| `MUX-H1` | `protocol/mux/h1.rs` | …plus stream id, parked, close_notify |
| `MUX-ROUTER` | `protocol/mux/router.rs` | renders `[session req cluster backend]` via `HttpContext::log_context()` |
| `MUX-CONN` / `MUX-CONV` / `MUX-PARSER` / `MUX-PKAWA` / `MUX-STREAM` | corresponding files | module-level only (no per-session context) |
| `KAWA-H1` | `protocol/kawa_h1/mod.rs` | session, frontend, request/response parsing phase |
| `RUSTLS` | `protocol/rustls.rs` | sni, alpn, version, source, frontend |
| `PIPE` | `protocol/pipe.rs` | addresses, frontend/backend status & readiness |
| `TCP` | `tcp.rs` | frontend, backend, peer (cached on `SessionTcpStream`) |
| `SOCKET` | `socket.rs` | session, peer, local, RTT, state |

**Conventions:**

- Use the macro defined in the file. Do not call `log::info!`/`log::error!`
  directly from protocol code — the prefix tag is load-bearing for
  log-search.
- Tier severity by intent: `debug!`/`trace!` for expected idle closes,
  timeouts, noisy state. `warn!`/`error!` for real protocol errors or
  invariant breaks. (See `feedback_log_context_before_theorising` for the
  reasoning.)
- When an `HttpContext` is in scope, prefer `$http_ctx.log_context()`
  (`kawa_h1/editor.rs:587`) over hand-rolling a `LogContext { ... }`
  struct literal — the helper is the canonical formatter.

### Per-flood-detector helper macro pattern

When a violation funnel (`H2FloodViolation`, `RustlsError`, `H2Error`) needs a
`&'static str` metric key per variant, define a `concat!`-based macro:

```rust
macro_rules! h2_error_metric_key {
    ($prefix:literal, $error:expr) => {
        match $error {
            H2Error::NoError => concat!($prefix, ".no_error"),
            H2Error::ProtocolError => concat!($prefix, ".protocol_error"),
            // …14 arms, exhaustive — adding a new H2Error variant fails the build here
        }
    };
}
```

Then wrap with one helper per direction:

```rust
fn metric_for_goaway_sent(error: H2Error) -> &'static str {
    h2_error_metric_key!("h2.goaway.sent", error)
}
```

This keeps the breakdown in lock-step with the underlying enum (the build
breaks on a new variant) while preserving `&'static str` semantics.

## Access logs

Schema lives in `command/src/logging/access_logs.rs::RequestRecord` with the
protobuf wire shape in `command/src/command.proto::ProtobufAccessLog`. Adding
a field requires:

1. Field on `RequestRecord<'a>` (Rust struct, `lib/src/protocol/...` populates).
2. Field on `ProtobufAccessLog` (proto, append a new optional tag — never
   reuse or reorder existing tags).
3. Populate at every emit site:
   - H1: `lib/src/protocol/kawa_h1/mod.rs::log_request`
   - H2 mux: `lib/src/protocol/mux/stream.rs::generate_access_log`
   - TCP: `lib/src/tcp.rs::log_request`
   - WS / WSS post-upgrade pipe: `lib/src/protocol/pipe.rs::log_request`
4. Update `RequestRecord::duplicate()` in `access_logs.rs` so the protobuf
   path serialises the new field.
5. Document the field in [`configure.md`](configure.md) §OpenTelemetry / §TLS
   handshake metadata.

The TLS metadata fields (`tls_version`, `tls_cipher`, `tls_sni`, `tls_alpn`)
are sourced from the rustls handshake context in `lib/src/https.rs` and
plumbed via `mux::Context` into `HttpContext`. The pipe path picks them up
via `Pipe::set_tls_metadata` called from `https.rs::upgrade_mux`.

## Tracing — current state

**This is W3C `traceparent` passthrough only.** No span lifecycle, no OTLP
exporter, no SDK dependency. Behind the `opentelemetry` compile-time
feature flag:

- `traceparent` is parsed in `lib/src/protocol/kawa_h1/editor.rs::on_request_headers`
  (the same callback runs on H1 and on H2 frames decoded via `pkawa.rs`).
- A new span ID is generated per Sōzu hop; the value is rewritten on the
  outgoing request and stored on `HttpContext.otel`.
- Access logs surface `trace_id`, `span_id`, `parent_span_id`.

To go further (real spans, OTLP exporter, B3/Datadog propagation), see the
"Out of scope" section in [`configure.md`](configure.md). It would land
behind a new feature flag rather than expanding the scope of `opentelemetry`.

## Control-plane audit trail

Every privileged mutation on the unix command socket (`AddCluster`,
`RemoveCertificate`, `ActivateListener`, …) goes through three observability
surfaces:

1. An `Event` of the matching `EventKind` is published on the
   `SubscribeEvents` bus. The 14 mutation variants are enumerated in
   `command/src/command.proto::EventKind`.
2. An `incr!("config.<verb>")` counter is bumped (e.g.
   `config.cluster_added`).
3. A structured audit log line is emitted at `info!` level in the MUX-family
   layout (keyword `Command(...)` rather than `Session(...)`, which names a
   data-plane session). Every free-form field (`target`, `actor_comm`,
   `reason`) is sanitized at render time — control chars (`\x00..=\x1f`,
   `\x7f`) are replaced with `?` so attacker-influenced input cannot forge
   additional audit lines via embedded `\t` / `\n` / ANSI escapes. Rendered
   form (ANSI colours off):

   ```
   [01HXS4GZ9EYP3F2R7K8M6B4N2C 01HXS4H5K2QR9C7PVWXY8T6ZNA my_app -]	AUDIT	Command(verb=cluster_added, actor_uid=1000, actor_gid=1000, actor_pid=12345, actor_comm=sozuctl, client_id=42, target=cluster:my_app, result=ok, sozu_version=1.1.1)
   ```

   ### Field reference

   **Mandatory fields** (always present):

   - `verb` — stable static identifier for the audited operation (e.g.
     `cluster_added`, `state_loaded`, `listener_updated`). One `config.<verb>`
     statsd counter per verb.
   - `actor_uid` / `actor_gid` / `actor_pid` — peer credentials from
     `SO_PEERCRED` on the unix command socket. `unknown` on read failure
     / non-Linux builds.
   - `actor_comm` — `/proc/<pid>/comm` at accept time (up to 15 chars), lets
     SOC distinguish `sozuctl` from ad-hoc shells that share a UID.
   - `client_id` — per-accept monotonic counter. Distinct from the
     `session_ulid` bracket slot, which survives as a grep-correlation key
     across every verb a single sozuctl invocation emits.
   - `target` — free-form verb-specific descriptor (e.g.
     `cluster:my-cluster`, `file:/var/lib/sozu/state.bin`, `stop:hard`,
     `listener:http:127.0.0.1:8080`). Sanitized.
   - `result` — `ok` or `err`.
   - `sozu_version` — `CARGO_PKG_VERSION` at build time. Forensic pin for
     mixed-fleet audit streams.

   **Optional fields** (appear when relevant):

   - `error_code` — structured failure bucket: `dispatch_error`,
     `worker_failure`, `worker_timeout`, `peer_cred_unavailable`,
     `invalid_input`, `io_error`, `other`. Present when `result=err`.
   - `reason` — truncated human-readable failure detail (max 256 chars,
     sanitized). Pairs with `error_code`.
   - `elapsed_ms` — wall-clock time between request acceptance and audit
     emission. Set on completion-time lines.
   - `fanout` — worker fan-out outcome: `ok`, `partial`, `timeout`, or
     `local_only`. Set on completion-time lines for verbs that scatter to
     workers.
   - `workers` — `<ok>/<err>/<expected>` per-worker counts. Pairs with
     `fanout`.

   ### Two lines per worker-fanning verb

   Verbs that fan out to every worker (AddCluster, RemoveHttpFrontend,
   UpdateHttpsListener, AddCertificate, …) emit **two** audit lines:

   - **Attempt-time**: `result=ok` means "accepted by the main process
     state". Fires immediately after `state.dispatch` succeeds. No fanout /
     elapsed_ms.
   - **Completion-time**: emitted when every worker has responded (or the
     scatter deadline fires). Carries `fanout=ok|partial|timeout`,
     `workers=<ok>/<err>/<expected>`, `elapsed_ms`, and — on `result=err`
     — `error_code` + `reason`.

   Operators correlate the two via the shared `[session_ulid request_ulid …]`
   bracket.

   ### Local-only verbs

   Verbs that don't fan out — `SoftStop`/`HardStop` request,
   `LoggingLevelChanged`, `UpgradeMain` / `UpgradeWorker` init,
   `SubscribeEvents`, `SaveState`, and `LoadState` completion — emit a
   single audit line carrying `result` and (when applicable)
   `error_code` + `reason`. `LoadState` and `SaveState` embed
   `ok:<n> errors:<n>` counts in `target=file:<path>`.

   Bracket slots follow the `[session_ulid request_ulid cluster_id|- backend_id|-]`
   convention shared with `MUX` / `MUX-ROUTER` / `RUSTLS` / `PIPE` / `TCP` lines.

The `actor_uid` is captured at unix-socket accept time via `SO_PEERCRED`
(`bin/src/command/server.rs`, stored on `ClientSession.actor_uid`). Failed
syscalls or non-Linux builds collapse to `actor_uid=unknown` rather than
panicking. This satisfies PCI-DSS 10.2 / ISO 27001 A.8.15 / SOC 2 audit-trail
requirements without an external audit shim.

To add a new audited verb:

1. Add the `EventKind` variant to the proto (preserve existing tags).
2. Update `Display for Event` in `command/src/proto/display.rs`.
3. In the `bin/src/command/requests.rs` handler:
   - Push the `Event` to the bus (find an existing `EventKind::CLUSTER_ADDED`
     emit site to copy from).
   - Emit `incr!("config.<verb>")`.
   - Emit the audit line via the `audit_log_context!` macro in
     `bin/src/command/requests.rs` — it renders the verb into the MUX
     `Session(verb=..., actor_uid=..., client_id=..., target=..., result=...)`
     block automatically.

## Extension checklist

Before merging an instrumentation change:

- [ ] Metric keys are `&'static str` (compile-time literal or `LazyLock`-leaked).
- [ ] Per-direction / per-error / per-frame breakdowns use a `concat!`-based
  helper macro so the build breaks on a new enum variant.
- [ ] Cardinality is bounded — either no labels, or labels from a fixed-size
  table, or hashed into a bucket of known size.
- [ ] Gauges that are emitted from more than one site use `gauge_add!`
  (lifecycle delta) and pair every `+N` with a `-N` on the close path.
  Consider `impl Drop` for symmetric teardown.
- [ ] New protocol modules define their own `log_context!` /
  `log_module_context!` with a unique prefix tag.
- [ ] New access-log fields land on `RequestRecord`, `ProtobufAccessLog`
  (new tag), all four emit sites, and `RequestRecord::duplicate()`.
- [ ] [`configure.md`](configure.md) is updated in the same changeset.
- [ ] Privileged control-plane verbs land an `EventKind` + `config.<verb>`
  counter + audit log line (MUX `Session(...)` layout, `info!` level,
  routed via the `audit_log_context!` macro).

## See also

- [`configure.md`](configure.md) — full inventory of currently emitted
  metrics, access-log fields, OpenTelemetry passthrough config.
- [`h2_mux_internals.md`](h2_mux_internals.md) — H2 mux state machine and
  flood-detector design.
- [`lib/src/protocol/mux/LIFECYCLE.md`](../lib/src/protocol/mux/LIFECYCLE.md)
  — stream/slot lifecycle with `file.rs:LINE` citations.
- [`CLAUDE.md`](../CLAUDE.md) — agent conventions including log macro
  discipline, metric macros list, and security-sensitive areas.
