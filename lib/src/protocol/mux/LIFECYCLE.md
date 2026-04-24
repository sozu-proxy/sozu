# H2 Mux — Session Workflow and Stream Lifecycle

Reference document for maintainers of the HTTP/1.1 + HTTP/2 multiplexing layer
under `lib/src/protocol/mux/`.

Every claim below is anchored to a concrete `file.rs:LINE` so you can jump
straight to the code. Line numbers reflect the `feat/h2-mux` branch at the time
this file was written; if you land a refactor that moves any of them, please
update the citations in the same changeset — stale pointers here are treated
as broken documentation.

Scope: the server-side frontend path is the primary subject, because that is
where the dangerous invariants live. The client-side (backend) path is covered
only where it diverges.

---

## 1. Architecture Overview

The mux layer is a single [`Mux`] session that wraps one frontend connection
and zero-or-more backend connections. It dispatches to either an H1 or an H2
implementation depending on what the frontend negotiated.

### 1.1 Module layout

| Module | Path | Responsibility |
|---|---|---|
| `mod.rs` | `lib/src/protocol/mux/mod.rs` | `Mux`, `Context`, `SessionState` impl, event loop |
| `connection.rs` | `lib/src/protocol/mux/connection.rs` | `Connection` enum dispatching H1/H2 |
| `h1.rs` | `lib/src/protocol/mux/h1.rs` | HTTP/1.1 connection state machine |
| `h2.rs` | `lib/src/protocol/mux/h2.rs` | HTTP/2 connection state machine (the big one) |
| `stream.rs` | `lib/src/protocol/mux/stream.rs` | `Stream` and `StreamState` |
| `router.rs` | `lib/src/protocol/mux/router.rs` | Backend selection / connection pooling |
| `parser.rs` / `serializer.rs` | idem | RFC 9113 frame codec |
| `converter.rs` / `pkawa.rs` | idem | HPACK ↔ kawa block conversion |

### 1.2 The split between `ConnectionH2` and `Context`

Two orthogonal structs hold H2 session state:

- [`ConnectionH2<Front>`] — `lib/src/protocol/mux/h2.rs:980` — **per-connection**
  wire-level state: HPACK coders, frame-parser state (`H2State`), flow control
  window, flood counters, priority map, `self.streams: HashMap<StreamId,
  GlobalStreamId>`.
- [`Context<L>`] — `lib/src/protocol/mux/mod.rs:337` — **per-session** stream
  buffers and routing data: `context.streams: Vec<Stream>`, `pending_links`,
  `backend_streams`, pool handle, listener, debug history.

A `Stream` lives in `context.streams`; it is referenced from a connection by
index (`GlobalStreamId = usize`). `ConnectionH2.streams` maps the on-the-wire
`StreamId` (u32, odd for client-initiated, even for server-initiated) to that
index.

```
        ConnectionH2 (frontend)      Context                 ConnectionH2 (backend)
        ─────────────────────        ─────────────────        ──────────────────────
        streams: HashMap             streams: Vec<Stream>    streams: HashMap
          0x1 ─┐                       [0] ─┐ active          0x1 ──┐
          0x3 ─┼── gid=0 ──▶ ──────▶   [1] ─┘ active  ◀───── 0x5   │
               │                       [2] Recycle              │
          ...  │                       [3] ...  ◀──── gid=3 ────┘
               │
               └── state machine, flow window, HPACK, ...
```

### 1.3 Key type declarations

| Type | Declaration | Purpose |
|---|---|---|
| `Mux<Front, L>` | `mod.rs:518` | Top-level session state |
| `Context<L>` | `mod.rs:337` | Per-session context |
| `Connection<Front>` | `connection.rs:54` | `H1 | H2` dispatch |
| `ConnectionH2<Front>` | `h2.rs:980` | H2 connection state |
| `ConnectionH1<Front>` | `h1.rs` (struct `ConnectionH1`) | H1 connection state |
| `Stream` | `stream.rs:59` | Per-request buffers + HTTP context |
| `StreamState` | `stream.rs:40` | `Idle`/`Link`/`Linked`/`Unlinked`/`Recycle` |
| `H2State` | `h2.rs:778` | H2 connection-level state machine |
| `H2StreamId` | `h2.rs:1095` | `Zero` or `Other { id, gid }` — used by `expect_read`/`expect_write` |
| `StreamId` (alias for `u32`) | `mod.rs:183` | On-the-wire H2 stream identifier |
| `GlobalStreamId` (alias for `usize`) | `mod.rs:184` | Index into `context.streams` |
| `H2FlowControl` | `h2.rs:953` | Connection-level send/recv window |
| `H2DrainState` | `h2.rs:973` | Graceful-shutdown bookkeeping |
| `H2ByteAccounting` | `h2.rs:963` | Overhead byte attribution |
| `H2ConnectionConfig` | `h2.rs:360` | Per-listener tuning |
| `H2FloodConfig` | `h2.rs:268` | CVE-mitigation thresholds |

---

## 2. Session Lifecycle (Server Position)

### 2.1 From accept to preface

1. The server-level session (HttpsSession / HttpSession) accepts a TCP
   connection, performs the TLS handshake, and upgrades to the mux protocol.
2. `Connection::new_h2_server` (`connection.rs:132`) constructs a
   `ConnectionH2` pre-seeded with:
   - `expect_read = Some((H2StreamId::Zero, CLIENT_PREFACE_SIZE))` —
     24-byte preface + 9-byte SETTINGS header (`connection.rs:150`,
     `h2.rs:187`).
   - `readiness.interest = READABLE | HUP | ERROR`.
3. The event loop fires `ready()` in `mod.rs:627` which dispatches to
   `Connection::readable` → `ConnectionH2::readable` (`h2.rs:1607`).

### 2.2 Connection state machine (`H2State`)

Declared at `h2.rs:778` (`pub enum H2State`):

```
                 ClientPreface                 ← server read: 24B magic + 9B SETTINGS header
                       │
                       ▼
                ClientSettings                 ← consume SETTINGS payload, reply with our SETTINGS
                       │
                       ▼
                ServerSettings                 ← flush our SETTINGS, (optionally) enlarge window
                       │
                       ▼
                    Header      ◀──────────┐  ← read 9-byte frame header
                       │                    │
            ┌──────────┼──────────┐         │
            ▼          ▼          ▼         │
     Frame(...)   Continuation    Discard   │  ← payload consumption / RST discard
            │     Header/Frame       │      │
            └──────────┴─────────────┴──────┘
                       │
                       ▼
                    GoAway        ← sent GOAWAY; draining
                       │
                       ▼
                    Error         ← terminal (force_disconnect queued)
```

- `ClientPreface` → `ClientSettings` at `h2.rs:1728`.
- `ClientSettings` → `ServerSettings` at `h2.rs:1751`.
- `Discard` (stream refused) is set in `refuse_stream_and_discard`
  (`h2.rs:2919`) and exited at `h2.rs:1721`.
- `Continuation*` states handle multi-frame HEADERS per RFC 9113 §4.3 —
  enter at `h2.rs:1494`.

### 2.3 Entry points

| Method | Definition | Called from |
|---|---|---|
| `ConnectionH2::readable` | `h2.rs:1607` | `Mux::ready` when `READABLE` asserted (`mod.rs:667`) |
| `ConnectionH2::writable` | `h2.rs:2445` | `Mux::ready` when `WRITABLE` asserted (`mod.rs:969`) |
| `ConnectionH2::close` | `h2.rs:4256` | `Mux::close` (`mod.rs:1519`) and dead-backend cleanup |
| `ConnectionH2::graceful_goaway` | `h2.rs:3051` | `Mux::shutting_down` (`mod.rs:1596`) |
| `ConnectionH2::cancel_timed_out_streams` | `h2.rs:2823` | top of every `readable()` call (`h2.rs:1616`) |

### 2.4 Termination triggers

The session returns to the higher-level server loop via `SessionResult`
returned from `Mux::ready`. Termination may be triggered by:

- Frontend HUP — detected at `mod.rs:635`, subject to
  `delay_close_for_frontend_flush` (`mod.rs:538`) to avoid truncating TLS.
- `MuxResult::CloseSession` from any readable/writable path
  (`mod.rs:682`, `mod.rs:984`, etc.).
- A loop-iteration budget overrun (`MAX_LOOP_ITERATIONS = 10_000`,
  `mod.rs:178`, check at `mod.rs:1014`).
- Timeout (`Mux::timeout`, `mod.rs:1175`).
- Graceful shutdown initiated by the server
  (`Mux::shutting_down`, `mod.rs:1589`).

---

## 3. Stream Lifecycle

### 3.1 HTTP/2 logical states vs Sōzu `StreamState`

RFC 9113 §5.1 defines six stream states. Sōzu collapses this onto the
five-variant `StreamState` declared at `stream.rs:40`:

```
RFC 9113:        idle → open → half-closed(remote) → closed
                               half-closed(local)
                               reserved(local/remote)

StreamState:     Idle  → Link → Linked(Token) → Unlinked → Recycle
```

- `Idle` — slot created but no request attached yet. `stream.rs:41-42`.
- `Link` — request fully parsed; waiting for a backend connection.
  Transitions: `h1.rs:365`, `h1.rs:576`, `h1.rs:789`, `h2.rs:3631`,
  `h2.rs:4013`, `h2.rs:4506`.
- `Linked(Token)` — bound to a backend (`token` identifies which one); H2
  request/response bytes flow both ways. Set by `Context::link_stream`
  (`mod.rs:411`), cleared by `Context::unlink_stream` (`mod.rs:421`).
- `Unlinked` — backend finished or was reset; response may still need to
  drain to the client. Transitions: `answers.rs:209/224`, `h1.rs:737-772`,
  `h2.rs:4424/4450/4469`.
- `Recycle` — slot fully finalized; reusable by `Context::create_stream`.
  Transitions: `mod.rs:1515`, `h2.rs:2255`, `h2.rs:2911`, `h2.rs:3782`.

`StreamState::is_open()` returns true for everything except `Idle` and
`Recycle` (`stream.rs:54`).

### 3.2 Who transitions what

- **Stream creation.** `Context::create_stream` (`mod.rs:430`) either
  reuses a `Recycle` slot or pushes a new `Stream`. For H2, the per-stream
  wire-id mapping is added by `ConnectionH2::create_stream` (`h2.rs:3153`)
  which also records `stream_last_activity_at` (`h2.rs:3180`).
- **Backend attach.** `Router::connect` (called from `mod.rs:1049` during
  the `pending_links` drain) eventually calls `Context::link_stream`
  (`router.rs:233` and `:358`) which sets `Linked(token)` and pushes to
  `context.backend_streams`.
- **Backend detach.** `Context::unlink_stream` (`mod.rs:421`) — called
  from timeout paths (`mod.rs:1218/1233/1311/1321`), from H1 EOF
  (`h1.rs:722`), and from H2 reset/end (`h2.rs:4344/4383`).
- **Recycle.** The H2 write path recycles a server stream once both the
  front request and back response are `is_terminated() && is_completed()`
  — see `try_recycle_server_stream` flow at `h2.rs:1924-1953`.

### 3.3 What "Recycle" means and when slots are popped

`StreamState::Recycle` marks a slot as **logically free but still
allocated**. It means:

- The pool buffers have been cleared (`mod.rs:466-471`).
- Metrics have been reset (`mod.rs:471`).
- The slot can be handed back to a new request by
  `Context::create_stream` which, on entry, searches for a `Recycle` slot
  (`mod.rs:451-454`).

A `Recycle` slot is **physically popped** only when `shrink_trailing_recycle`
runs — see §6.

---

## 4. The Two Stream Maps

### 4.1 `ConnectionH2.streams` (per-connection wire map)

- Type: `HashMap<StreamId, GlobalStreamId>` — `h2.rs:998`.
- Scope: one map per `ConnectionH2` (frontend *and* each H2 backend).
- Key: the 31-bit wire stream ID negotiated in the H2 frame header.
- Value: the `Vec` index (`GlobalStreamId`) pointing into
  `context.streams`.
- Mutated by:
  - insert — `create_stream` (`h2.rs:3179`), `start_stream`
    (`h2.rs:4571`).
  - remove — `remove_dead_stream` (`h2.rs:2202`), inline removals at
    `h2.rs:1562`, `h2.rs:2863`, `h2.rs:3739`, `h2.rs:4016`,
    `h2.rs:4408`.

### 4.2 `Context.streams` (per-session buffer array)

- Type: `Vec<Stream>` — `mod.rs:338`.
- Scope: one `Vec` per `Mux` session. **Both** H1 and H2 frontends use it,
  and **every** backend `ConnectionH2` attached to this session indexes
  into it.
- Index: `GlobalStreamId = usize` (`mod.rs:184`).
- Mutated by:
  - push — `create_stream` when no `Recycle` slot is available
    (`mod.rs:482-484`).
  - pop — `shrink_trailing_recycle` (`mod.rs:491-499`).
  - in-place state edits — everywhere.

### 4.3 Invariants

1. For every entry `(wire_id → gid)` in `ConnectionH2.streams`, `gid` is
   a valid in-bounds index into `context.streams`.
2. The `Stream` at that index is not in `StreamState::Recycle` — or, if
   it is, the caller is in the middle of a teardown pass that is about
   to remove the wire-id entry (never a public surface).
3. A `StreamState::Linked(token)` must have a matching entry in
   `context.backend_streams[token]`. This is asserted in debug builds at
   `mod.rs:1130-1160` after every `ready()` pass.

The H1 side keeps its single `stream: Option<GlobalStreamId>` in
`ConnectionH1` — there is no hashmap because H1 multiplexing is limited to
request pipelining with at most one live stream at a time.

---

## 5. Index-Caching Invariants (`expect_write` / `expect_read`)

### 5.1 The fields

Both declared on `ConnectionH2` (`h2.rs:988-989`):

```rust
pub expect_read:  Option<(H2StreamId, usize)>,  // (id, remaining bytes)
pub expect_write: Option<H2StreamId>,
```

`H2StreamId` (`h2.rs:1095`):

```rust
pub enum H2StreamId {
    Zero,                                            // connection-level (preface, SETTINGS, etc.)
    Other { id: StreamId, gid: GlobalStreamId },     // a specific stream
}
```

Their meaning:

- `expect_read = Some((sid, n))` — we need `n` more bytes of payload on
  stream `sid`; the read path will top up the relevant buffer.
- `expect_write = Some(sid)` — a partial write on `sid` is parked;
  `write_streams` will resume it on the next writable pass.

### 5.2 Load-bearing invariant

> If `expect_write` or `expect_read` holds an `H2StreamId::Other { gid, .. }`,
> then `gid` **MUST** be a valid in-bounds index into `context.streams`
> at the moment it is dereferenced.

Dereference sites:

- `write_streams` resume path — reads `context.streams[global_stream_id]`
  at `h2.rs:1896`.
- `try_resume_reading` — reads `context.streams[global_stream_id]` at
  `h2.rs:2783` (method body starting at `h2.rs:2771`).
- `readable` — reads `context.streams[global_stream_id]` at `h2.rs:1648`.

An out-of-bounds index will panic via `Vec`'s bounds check.

### 5.3 Why the invariant is non-trivial

`gid` is a `Vec` index — it is **physically invalidated** when
`shrink_trailing_recycle` pops slots from the end of `context.streams`
(§6). A `gid` that was valid at the time it was cached in `expect_write`
can therefore become out-of-bounds later on if:

1. The stream pointed to by `gid` (or a trailing block of streams beyond it)
   is transitioned to `StreamState::Recycle`;
2. A new stream is created which triggers `shrink_trailing_recycle`;
3. The slot at `gid` is popped;
4. The cached `expect_write` is then dereferenced.

Step 1 happens in every `remove_dead_stream` caller and in every reset /
cancel path (see §8). Step 2 happens on every `create_stream` call that
finds a recycled slot to reuse and then crosses the shrink ratio
(`mod.rs:473-479`). Step 3 follows automatically.

### 5.4 Who invalidates these fields

**Every site that removes a stream from `self.streams` is responsible for
invalidating both `expect_write` and `expect_read` when they reference
that stream's `gid`.** The canonical invalidation helper is
`ConnectionH2::remove_dead_stream` (`h2.rs:2201`), which:

```rust
if matches!(self.expect_write, Some(H2StreamId::Other { gid, .. }) if gid == global_stream_id) {
    self.expect_write = None;
}
if matches!(self.expect_read, Some((H2StreamId::Other { gid, .. }, _)) if gid == global_stream_id) {
    self.expect_read = None;
}
```

Call sites that currently go through `remove_dead_stream` (and are
therefore safe):

- `write_streams` after end-of-stream — `h2.rs:1944`.
- `prune_inactive_streams_while_closing` — `h2.rs:2245`.
- `handle_window_update_frame` zero-increment path — `h2.rs:4074`.

Call sites that currently perform `self.streams.remove(...)` inline
instead of going through `remove_dead_stream` must apply the same two
invalidation checks (or be refactored to route through the helper):

- `handle_continuation_header_state` CONTINUATION oversize — `h2.rs:1562`.
- `cancel_timed_out_streams` slow-multiplex guard — `h2.rs:2863`.
- `handle_rst_stream_frame` peer RST — `h2.rs:3739`.
- `handle_goaway_frame` retry loop — `h2.rs:4016`.
- `end_stream` client-side retirement — `h2.rs:4420`.
- `close` backend-stream teardown — `h2.rs:4323` (does not remove, only
  notifies endpoint — but the surrounding `close` path pops the whole
  connection shortly after).

The two-check pattern — one for `expect_write`, one for `expect_read` —
is the mechanical work a reviewer should apply at every such site.

---

## 6. `shrink_trailing_recycle` (mod.rs:491–499)

```rust
pub fn shrink_trailing_recycle(&mut self) {
    while self.streams.last().is_some_and(|s| s.state == StreamState::Recycle) {
        self.streams.pop();
    }
}
```

### 6.1 When it runs

Called from `Context::create_stream` after a `Recycle` slot is reused,
guarded by a ratio threshold so we don't thrash on every request
(`mod.rs:473-479`):

```rust
if total > 1 && active > 0 && total > active * self.h2_stream_shrink_ratio {
    self.shrink_trailing_recycle();
}
```

The default ratio is 2 (`h2.rs:353`, `DEFAULT_STREAM_SHRINK_RATIO`),
overrideable per listener via `H2ConnectionConfig::stream_shrink_ratio`
(`h2.rs:366`). In short: if more than `2×active` slots are held, trim
trailing `Recycle` entries.

### 6.2 What it pops

Only **trailing** `Recycle` slots. Interior `Recycle` slots are kept so
that live `GlobalStreamId` values in the middle of the `Vec` stay
stable. This means:

- A `gid` in the middle of the `Vec` keeps its index across a shrink,
  regardless of whether the `Vec` shrinks or not.
- A `gid` **past the new length** after shrink becomes invalid — any
  cache (notably `expect_write` / `expect_read`) that holds such a `gid`
  will panic on its next dereference.

### 6.3 Why it matters for `GlobalStreamId` validity

Together with §5: a `gid` that was valid when written into
`expect_write`/`expect_read` can silently turn into a dangling index. The
defence is the invalidation discipline in §5.4 — combined with the
invariant that an entry cached for a specific `gid` must be cleared
*before* that `gid`'s slot can be popped.

---

## 7. Timeouts

There are four timer surfaces, fired by the proxy's central timer wheel
and funneled into `Mux::timeout` (`mod.rs:1175`). Their `duration` is
configured per listener.

### 7.1 Connection-level (frontend) idle timeout

- Tracker: `ConnectionH{1,2}.timeout_container` (shared across the
  session under the frontend token).
- Fired when: no traffic observed for `configured_frontend_timeout`
  (`mod.rs:519`) while any stream is live, or the shorter
  `request_timeout` until the first `Link` transition
  (`mod.rs:1042-1044`).
- Reset: on meaningful activity — HEADERS for an existing stream, DATA
  bytes — see `h2.rs:1646` and `h2.rs:1884`. Control frames (PING,
  WINDOW_UPDATE, SETTINGS) deliberately do **not** reset it so a
  misbehaving peer cannot pin the session with keepalive noise
  (`h2.rs:1630-1636`).
- Handling: `Mux::timeout` inspects each stream's state (`mod.rs:1193`)
  and either writes a default 408/503/504 answer, forcefully terminates,
  or keeps draining.

### 7.2 Per-stream idle timeout (slow-multiplex guard)

- Tracker: `ConnectionH2.stream_last_activity_at: HashMap<StreamId,
  Instant>` — `h2.rs:1057`.
- Cap: `ConnectionH2.stream_idle_timeout` — `h2.rs:1060`.
- Fired by: `ConnectionH2::cancel_timed_out_streams` at `h2.rs:2823`,
  called at the top of every `readable()` call (`h2.rs:1616`).
- Action: queue `RST_STREAM(CANCEL)`, remove the wire-id mapping, mark
  the stream `Recycle`, notify the backend endpoint.
- Does **not** feed `total_rst_streams_queued` — it is proxy-initiated,
  not peer-driven (`h2.rs:2854`).
- Mitigates slow-multiplex Slowloris: connection-level timer resets on
  every frame, so without this per-stream guard a peer could hold
  `max_concurrent_streams` slots for the full session timeout.

### 7.3 Backend timeout

- Tracker: per-backend `Connection.timeout_container` in
  `self.router.backends`.
- Set to `configured_backend_timeout` after successful connect
  (`mod.rs:773`).
- Fired by: timer wheel → `Mux::timeout` with the backend token
  (`mod.rs:1279`).
- Action: for each stream linked to that backend, either send 504, or
  forcefully terminate, or keep draining — see `mod.rs:1292-1326`. The
  timeout is re-armed if the session stays alive (`mod.rs:1332`) to
  avoid the "immortal zombie" state.

### 7.4 Interaction order on a loaded connection

1. `readable()` entry runs `cancel_timed_out_streams` first (§7.2).
2. Then it optionally fires `goaway(SettingsTimeout)` if the SETTINGS ACK
   is overdue (`h2.rs:1619-1628`).
3. Then it consumes the frame / payload.
4. `writable()` mirrors this check (`h2.rs:2309-2318`).
5. If the frontend timer fires while streams are linked, the timeout
   logic in `mod.rs:1175` decides per-stream; backend timer fires
   independently.
6. Loop budget (`MAX_LOOP_ITERATIONS = 10_000`) is a hard backstop at
   `mod.rs:1014`.

---

## 8. Termination Paths

### 8.1 Graceful GOAWAY (double-GOAWAY per RFC 9113 §6.8)

Two-phase in `ConnectionH2::graceful_goaway` (`h2.rs:3051`):

1. **Phase 1** — send GOAWAY with `last_stream_id = 0x7FFFFFFF`
   (`STREAM_ID_MAX`, `h2.rs:178`); keep `READABLE` so in-flight request
   bodies can still arrive. Called first time from
   `Mux::shutting_down` at `mod.rs:1596`. Draining flag set.
2. **Phase 2** — on the second invocation (draining already true), call
   `goaway(NoError)` (`h2.rs:3054`) with the actual
   `highest_peer_stream_id`, remove `READABLE` interest
   (`h2.rs:3026`), transition to `H2State::GoAway`. Caller is
   `finalize_write` when all streams drain (`h2.rs:2273`).

`peer_gone_after_final_goaway` (`h2.rs:1111`) guards against deadlock on
a peer-side HUP after the final GOAWAY.

### 8.2 RST_STREAM

Three directions:

- **Peer-initiated** — `handle_rst_stream_frame` (`h2.rs` — reaches
  `self.streams.remove(&rst_stream.stream_id)`). Also runs flood
  counters (CVE-2023-44487 Rapid Reset).
- **Proxy-initiated, error response** — `reset_stream`
  (`h2.rs:5180`). Transitions to `Unlinked`, calls
  `forcefully_terminate_answer` for kawa cleanup, queues the outgoing
  RST via [`enqueue_rst`](#proxy-rst-emission-path), and counts against
  `record_rst_emitted` (CVE-2025-8671 MadeYouReset) unless `NoError`.
- **Proxy-initiated, idle cancel** — `cancel_timed_out_streams`
  (§7.2); the per-stream `forcefully_terminate_answer` call in the
  `mod.rs:1299` timeout path now arms `Ready::WRITABLE` via
  `arm_writable()` so the pair with `event::WRITABLE` actually
  schedules the next `writable()` tick under edge-triggered epoll.

All three paths eventually purge the wire-id from `self.streams`.

#### Proxy-RST emission path

`ConnectionH2::enqueue_rst(wire_stream_id, error)` (`h2.rs:3494`) is
the canonical entry point for every proxy-emitted stream reset. It
delegates to the free-function primitive
`enqueue_rst_into` (`h2.rs:604`) and the free function is unit-tested
without a full `ConnectionH2` fixture (see the four
`test_enqueue_rst_into_*` tests). Three invariants are kept in lock-step:

- **Dedupe** via `self.rst_sent: HashSet<StreamId>`: at most one RST
  per wire stream id. `HashSet::insert` returns `false` when the id
  is already present; the helper short-circuits on that branch so
  `pending_rst_streams` and `total_rst_streams_queued` stay
  consistent even when a cascading error path re-enters the reset
  flow for the same stream.
- **MadeYouReset queued cap** via `self.total_rst_streams_queued`
  (capped at `MAX_PENDING_RST_STREAMS = 200`). Each freshly queued
  RST bumps the counter; `flush_pending_control_frames` escalates to
  `GOAWAY(ENHANCE_YOUR_CALM)` when the cap is exceeded. Orthogonal
  to `record_rst_emitted` (the 500-emitted MadeYouReset lifetime
  cap) — a RST can be queued-but-not-yet-emitted.
- **Invariant 15** via `Readiness::arm_writable()` (`lib/src/lib.rs:1010`):
  pairs `Ready::WRITABLE` interest with the matching event bit so
  `writable()` is scheduled on the next epoll tick.

The three RST push sites retrofit to `enqueue_rst`:
- DATA-on-closed-stream (`h2.rs:1689` — `H2Error::StreamClosed`).
- `refuse_stream_and_discard` (`h2.rs:3488` — MCS / pool exhaustion).
- `reset_stream` (`h2.rs:5180` — per-stream error paths: malformed
  HEADERS, content-length mismatch, WINDOW_UPDATE zero-increment or
  overflow, unauthorised priority updates, self-dependent HEADERS).

Serialisation happens in `flush_pending_control_frames`
(`h2.rs:2867` → Phase 3 at the pending-RST-streams check, which drains
`self.pending_rst_streams` into `self.zero` via
`serializer::gen_rst_stream`). This path is independent of the owning
`Stream` still being present in `self.streams`, so it survives the
immediate `remove_dead_stream` call that every `reset_stream` caller
performs synchronously after return.

`finalize_write` (`h2.rs:2795`) retains `Ready::WRITABLE` when the
pass completes but `pending_rst_streams` or
`flow_control.pending_window_updates` are still non-empty — otherwise
a partial write that deferred the Phase-3 flush (gated on
`expect_write.is_none()`) would strand a queued RST until an
unrelated event re-raised the writable bit. This guard, together
with the invariant-15 arm inside `enqueue_rst`, closes the 18-check
h2spec 2.0 gap previously reported in RFC 9113 §§5.3, 6.9, 8.1.2.

### 8.3 Session drain

`Mux::shutting_down` (`mod.rs:1620`) is called by the server loop during
process shutdown or listener reload. It:

1. Initiates the double-GOAWAY (`mod.rs:1627`).
2. Drives frontend I/O outside the epoll loop
   (`drive_frontend_shutdown_io`, `mod.rs:562`) — H2 needs extra passes
   for the peer's END_STREAM and final TLS flush.
3. Checks the graceful-shutdown forced-close deadline: when
   `Connection::graceful_shutdown_deadline_elapsed` returns `true` (i.e.
   `drain.started_at + drain.graceful_shutdown_deadline <= Instant::now`)
   the session returns `true` immediately so the server loop can tear
   the connection down even with Linked streams still in flight.
4. Marks `front_received_end_of_stream` on streams whose request is
   already complete and consumed.
5. Returns `true` when no `Linked` or non-quiesced `Unlinked` streams
   remain.

The forced-close deadline is armed the first time `graceful_goaway`
transitions `drain.draining` to `true` (see `h2.rs`): that site sets
`drain.started_at = Some(Instant::now())`. The budget itself comes from
the listener knob `h2_graceful_shutdown_deadline_seconds` (proto field
`h2_graceful_shutdown_deadline_seconds`, defaulting to 5 s). Setting
the knob to `0` maps to `graceful_shutdown_deadline = None`, which
disables the forced-close branch entirely — shutdown then reverts to
"wait for every stream to drain" semantics. Peer-initiated drains
received via `handle_goaway_frame` deliberately do **not** arm
`started_at`: the budget only applies to the proxy's own soft-stop.

### 8.4 `Connection::end_stream` (backend-side retirement)

`ConnectionH2::end_stream` (`h2.rs:4380`) is the server-side wiper for a
single stream that has completed on the backend. Behavior depends on
`Position`:

- **Client** position (i.e. the backend's view) — `h2.rs:4391-4434`.
  Sends RST_STREAM(CANCEL) unless both request and response have
  terminated, removes the wire mapping, marks the stream `Unlinked` if
  not already `Recycle`.
- **Server** position — `h2.rs:4435-4511`. Dispatches on
  `end_stream_decision` (helper defined elsewhere in the file): either
  `ForwardTerminated`, `CloseDelimited`, `ForwardUnterminated`,
  `SendDefault(status)`, or `Reconnect` — each path sets the appropriate
  `StreamState` and schedules the frontend write.

---

## 9. Known Invariants Checklist

Mechanical list of invariants. A reviewer can check each one on a PR
that touches `h2.rs`, `mod.rs`, or `stream.rs`.

1. **Wire-map validity.** For every entry `(sid → gid)` in
   `ConnectionH2.streams` (`h2.rs:998`), `gid < context.streams.len()`.
2. **Backend index consistency.** If
   `context.streams[gid].state == StreamState::Linked(token)`, then
   `context.backend_streams[&token]` contains `gid`. Asserted under
   `debug_assertions` in `Mux::ready` at `mod.rs:1130-1160`.
3. **`expect_write` validity.** If
   `expect_write == Some(H2StreamId::Other { gid, .. })`, then
   `gid < context.streams.len()` and the slot at `gid` is not
   `Recycle`. Every stream-removal site MUST null `expect_write` out
   before returning; the helper is `remove_dead_stream`
   (`h2.rs:2201`). Manual-removal sites (§5.4) must replicate the
   check inline.
4. **`expect_read` validity.** Same as (3) for `expect_read`.
5. **Recycled slot cleanliness.** A `StreamState::Recycle` slot has
   cleared `front`, `back`, `front.storage`, `back.storage`, reset
   metrics (`mod.rs:466-472`).
6. **No stale `Linked` after backend close.** Before transitioning a
   stream away from `Linked(token)`, call `unlink_stream`
   (`mod.rs:421`) or `remove_backend_stream` (`mod.rs:505`) to keep the
   reverse index honest.
7. **No duplicate RST_STREAM on the wire.** Check
   `self.rst_sent.contains(&sid)` (`h2.rs:1028`) before queuing
   another.
8. **No new streams during drain.** `create_stream` and `start_stream`
   both short-circuit when `self.drain.draining`
   (`h2.rs:3160-3167`, `h2.rs:4518-4525`).
9. **Connection-level timer resets only on application activity.** H2
   control frames (PING / WINDOW_UPDATE / SETTINGS) do **not** reset
   `timeout_container` — enforced in `readable()` by setting it only on
   DATA payload (`h2.rs:1646`) and HEADERS
   (inside `handle_frame`).
10. **Single `graceful_goaway` per session outside phase 2.**
    `Mux::shutting_down` (`mod.rs:1595`) only calls it if
    `!self.frontend.is_draining()`; a second unconditional call would
    collapse phase 1 into phase 2 and disconnect in-flight streams.
11. **Frontend HUP defers close when output is pending.**
    `delay_close_for_frontend_flush` (`mod.rs:538`) must be consulted
    before returning `SessionResult::Close` so that unflushed
    TLS/GOAWAY records are not lost.
12. **Loop budget.** Every inner loop in `Mux::ready` and
    `drive_frontend_shutdown_io` bounds iterations at
    `MAX_LOOP_ITERATIONS = 10_000` (`mod.rs:178`).
13. **`shrink_trailing_recycle` runs only from `create_stream`.**
    Calling it from elsewhere can invalidate cached `GlobalStreamId`
    values (including `expect_write`/`expect_read`) that the caller is
    not prepared to re-check. Keep the single call site at
    `mod.rs:478`.
14. **Backend `active_requests` balance.** Every site that removes a
    stream outside `Connection::end_stream` must decrement
    `backend.active_requests` itself (e.g. `h2.rs:2886-2888`,
    `h2.rs:3755-3756`, `h2.rs:4022-4025`). Otherwise load-balancing
    counters drift monotonically.
15. **Incremental scheduler — solo-bucket non-yield.** The converter
    (`converter.rs` `H2BlockConverter::call` DATA arm) must only yield
    after a DATA frame when `incremental_mode == true` *and*
    `incremental_peer_count > 1`. A solo incremental stream with no
    peer to interleave with must drain sequentially — otherwise
    `finalize_write` (`h2.rs` `fn finalize_write`) would withdraw
    `Ready::WRITABLE` on a clean pass (no `expect_write` set by the
    converter) and edge-triggered epoll would never re-fire.
    The scheduler populates both fields once per write pass from
    `apply_incremental_rotation`'s returned count.
16. **Never withdraw `Ready::WRITABLE` with pending back-buffer after a
    pass that made forward progress.** `finalize_write`
    (`h2.rs` `fn finalize_write`) removes `Ready::WRITABLE` only when
    the pass drained cleanly (`!socket_wants_write &&
    expect_write.is_none()`) *and either* `bytes_written_this_pass == 0`
    *or* no open stream has queued response bytes
    (`back.out`/`back.blocks`). The progress check is load-bearing: a
    zero-progress pass (e.g. all streams flow-control-starved) must
    relinquish `Ready::WRITABLE` so the session dispatcher does not
    busy-spin — the next wake-up arrives from `WINDOW_UPDATE`,
    backend readable, or a new request. Enforced at runtime via the
    file-private helper `any_stream_has_pending_back` (`h2.rs`).
    A voluntary scheduler yield (RFC 9218 incremental rotation and
    any future yield site) can leave bytes buffered without
    `expect_write` being set; stripping `WRITABLE` in that state
    strands the stream because edge-triggered epoll never re-fires
    for sozu-owned buffers. Defence-in-depth for invariant 15.
    The same helper backs `ConnectionH2::has_pending_write_full`,
    consulted by `delay_close_for_frontend_flush` so shutdown-drain
    does not close before the stream bytes land on the socket.
17. **RFC 9218 incremental peer count is bucket-scoped, ready-only,
    and kept live mid-pass.** `converter.incremental_peer_count`
    (consumed by the converter's DATA-arm yield decision) counts
    incremental streams in the *same urgency bucket* that are *also
    ready to emit this pass* (`is_main_phase()` /
    terminated-but-not-completed / error-but-RST-not-yet-sent).
    Computed in `write_streams` as `ready_incremental_by_urgency`
    and looked up per-stream by its own urgency. A connection-global
    count would wrongly yield a solo incremental stream when an
    unrelated incremental stream sits in a different urgency bucket
    — regressing the invariant-15 solo-bucket fast path. Any
    transition to ineligible *mid-pass* MUST decrement the matching
    bucket before subsequent `'outer` iterations read it: the
    freshly-inserted `rst_sent` path (`h2.rs` ~2412-2425) and the
    mid-loop `completed_streams.push` path (`h2.rs` ~2481) both
    `saturating_sub(1)` their urgency bucket so later same-urgency
    peers do not read the stale snapshot. Post-loop the connection
    emits `h2.streams.ready_incremental.by_urgency` as a gauge.
    Guarded by e2e test
    `test_h2_rfc9218_incremental_multi_bucket_drains_sequentially`
    and by the scalar unit tests
    `ready_incremental_bucket_decrement_reduces_same_urgency_only` /
    `_saturates_at_zero` / `_skipped_for_non_incremental`.
18. **RFC 9218 §7.1 PRIORITY_UPDATE is parsed and honoured.**
    Frame type `0x10` is recognised at the parser (`parser.rs`
    `FrameType::PriorityUpdate` + `priority_update_frame`) rather
    than swallowed as `FrameType::Unknown`. `frame_header` enforces
    `stream_id == 0` at receipt (RFC 9218 §7.1), and the handler
    (`h2.rs` `handle_priority_update_frame`) rejects a
    prioritized-stream-id of `0` with
    GOAWAY(PROTOCOL_ERROR). Valid frames are decoded via
    `pkawa::parse_rfc9218_priority` and pushed through the same
    memory-guarded path as standalone PRIORITY frames
    (`Prioriser::push_priority_guarded`), so a flood of
    PRIORITY_UPDATEs for far-future stream IDs cannot pin more than
    `MAX_PRIORITIES` entries. Updates for streams that are no longer
    open and outside the idle look-ahead are silently dropped.
    Guarded by e2e tests
    `test_h2_priority_update_on_open_stream_is_accepted` and
    `test_h2_priority_update_on_stream_zero_is_protocol_error`.
19. **Converter close-race: no yield between last DATA and closing
    Flags.** The H2 converter's DATA arm (`converter.rs`
    `H2BlockConverter::call`) peeks `kawa.blocks.front()` and
    suppresses the RFC 9218 incremental-mode yield when the next
    queued block is a closing `Block::Flags` with `end_stream=true`.
    Yielding between the last DATA and its companion END_STREAM
    marker would strand the closing frame in `kawa.blocks` — the
    invariant-16 guard in `finalize_write` already prevents the
    wake-up being lost, but forcing a round-trip through the event
    loop for the terminal 9-byte empty-DATA frame is wasted work.
    The suppression is scoped narrowly: intermediate Flags blocks
    (e.g. chunked `end_chunk` without `end_stream`) still yield,
    and a trailing `Block::Chunk` also yields. Unit-tested in
    `converter::tests::test_converter_suppresses_yield_before_closing_end_stream_flags`
    / `test_converter_yields_before_trailing_flags_without_end_stream`
    / `test_converter_yields_before_chunk_block`; end-to-end guard
    `test_h2_incremental_round_robin_closes_every_stream`.

---

## 10. Pointers for First Steps

If you are fixing a bug in this module:

- **Panic on `context.streams[gid]`** — start at §5, then walk every
  stream-removal site listed in §5.4. The fix is usually a missed
  invalidation of `expect_write`/`expect_read`.
- **Hung session** — check §7. Verify the timer in question is being
  reset only on application activity, not on control frames.
- **Doubled metrics / gauge drift** — check §9 item 14 and the
  backend-stream accounting paths in `mod.rs:913-953` and
  `mod.rs:1522-1586`.
- **Truncated response / TLS decode error** — check `has_pending_write`
  (`h2.rs:3100`), `delay_close_for_frontend_flush` (`mod.rs:538`), and
  the TLS drain logic at `h2.rs:4298-4319`.
- **Stream count underflows `max_concurrent_streams`** — see
  `prune_inactive_streams_while_closing` (`h2.rs:2218`) and make sure
  every removal increments nothing and decrements what it should.

Last revision date of the `feat/h2-mux` line numbers: keep this line
*actually* current when you touch the file — greppable as
"Last revision date".
