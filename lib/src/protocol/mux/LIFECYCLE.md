# H2 Mux — Session Workflow and Stream Lifecycle

Reference document for maintainers of the HTTP/1.1 + HTTP/2 multiplexing layer
under `lib/src/protocol/mux/`.

Every claim is anchored to code. Where the prose names an item — a function, a
method, a struct, an enum — the anchor is that item plus its file, with no line
number, because a line number does not survive an edit above it. A `file.rs:LINE`
or `file.rs:LINE-LINE` anchor is kept only where the claim is about a specific
statement or branch inside an item; those were refreshed against `main` at
`0cb1e2a7` on 2026-09-20.

Scope: the server-side frontend path is the primary subject, because that is
where the dangerous invariants live. The client-side (backend) path is covered
only where it diverges.

---

## 1. Architecture Overview

The mux layer is a single [`Mux`] session that wraps one frontend connection and
zero-or-more backend connections. It dispatches to either an H1 or an H2
implementation depending on what the frontend negotiated.

### 1.1 Module layout

| Module                        | Path                                 | Responsibility                                    |
| ----------------------------- | ------------------------------------ | ------------------------------------------------- |
| `mod.rs`                      | `lib/src/protocol/mux/mod.rs`        | `Mux`, `Context`, `SessionState` impl, event loop |
| `connection.rs`               | `lib/src/protocol/mux/connection.rs` | `Connection` enum dispatching H1/H2               |
| `h1.rs`                       | `lib/src/protocol/mux/h1.rs`         | HTTP/1.1 connection state machine                 |
| `h2.rs`                       | `lib/src/protocol/mux/h2.rs`         | HTTP/2 connection state machine (the big one)     |
| `stream.rs`                   | `lib/src/protocol/mux/stream.rs`     | `Stream` and `StreamState`                        |
| `router.rs`                   | `lib/src/protocol/mux/router.rs`     | Backend selection / connection pooling            |
| `parser.rs` / `serializer.rs` | idem                                 | RFC 9113 frame codec                              |
| `converter.rs` / `pkawa.rs`   | idem                                 | HPACK ↔ kawa block conversion                     |
| `h2_scheduler.rs`             | `lib/src/protocol/mux/h2_scheduler.rs` | RFC 9218 priorities + the write-pass order/yield decision |

### 1.2 The split between `ConnectionH2` and `Context`

Two orthogonal structs hold H2 session state:

- [`ConnectionH2<Front>`] — `lib/src/protocol/mux/h2.rs` —
  **per-connection** wire-level state: HPACK coders, frame-parser state
  (`H2State`), flow control window, flood counters, priority map, and a
  `HashMap<StreamId, GlobalStreamId>` wire map — private to
  [`H2StreamTable`](h2_stream_table.rs), reached via `self.stream_table` (§4.1).
- [`Context<L>`] — `lib/src/protocol/mux/mod.rs` — **per-session** stream
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

The `Context` also carries two connection-scoped TLS fields populated once at
handshake and propagated to every per-stream `HttpContext` via
`Context::create_stream`:

- `tls_server_name: Option<String>` — the SNI the client sent (lowercased,
  trailing dot stripped). `None` on plaintext listeners or when the client
  omitted SNI.
- `tls_cert_names: Option<Arc<Vec<String>>>` — snapshot of the SAN dNSName
  entries of the certificate Sōzu actually served on this TLS session
  (RFC 6125 §6.4.4: when the SAN extension contains at least one dNSName
  entry, those entries are authoritative and the Common Name is ignored;
  CN is honoured only as a fallback when SAN is absent or has no dNSName
  entry). Captured in
  `lib/src/https.rs::HttpsStateMachine::upgrade_handshake` from the resolver.
  `Arc`-shared so fan-out across N H2 streams allocates once; frozen for the
  connection lifetime even if the operator swaps the cert mid-flight (mirrors
  browser cache semantics). `None` for plaintext or no-SNI handshakes, and
  also when rustls fell back to the default cert (no SAN matched the SNI) —
  routing then uses the legacy `authority_matches_sni` exact-match
  predicate, preserving the pre-fix safety guarantee without blocking
  intentionally-misconfigured listeners.

The routing layer (`lib/src/protocol/mux/router.rs::route_from_request`)
matches each request's `:authority` (H2) or `Host` (H1) against
`tls_cert_names` with RFC 6125 §6.4.3 wildcard handling — accepting H2
connection coalescing per RFC 7540 §9.1.1 / RFC 9113 §9.1.1, the way Firefox
and Chrome do. Misses are answered with **421 Misdirected Request** (RFC 9110
§15.5.20), which both browsers retry on a fresh connection with the correct
SNI. Coalesced acceptances (matched SAN != initial SNI) bump
`h2.coalescing.accepted` and emit a `debug!` log for ops observability.

### 1.3 Key type declarations

| Type                                 | Declaration                     | Purpose                                                              |
| ------------------------------------ | ------------------------------- | -------------------------------------------------------------------- | ------------ |
| `Mux<Front, L>`                      | `mod.rs`                        | Top-level session state                                              |
| `Context<L>`                         | `mod.rs`                        | Per-session context                                                  |
| `Connection<Front>`                  | `connection.rs`                 | `H1                                                                  | H2` dispatch |
| `ConnectionH2<Front>`                | `h2.rs`                         | H2 connection state                                                  |
| `ConnectionH1<Front>`                | `h1.rs` (struct `ConnectionH1`) | H1 connection state                                                  |
| `Stream`                             | `stream.rs`                     | Per-request buffers + HTTP context                                   |
| `StreamState`                        | `stream.rs`                     | `Idle`/`Link`/`Linked`/`Unlinked`/`Recycle`                          |
| `H2State`                            | `h2.rs`                         | H2 connection-level state machine                                    |
| `H2StreamId`                         | `h2.rs`                         | `Zero` or `Other { id, gid }` — used by `expect_read`/`expect_write` |
| `StreamId` (alias for `u32`)         | `mod.rs`                        | On-the-wire H2 stream identifier                                     |
| `GlobalStreamId` (alias for `usize`) | `mod.rs`                        | Index into `context.streams`                                         |
| `H2FlowControl`                      | `h2_flow_control.rs`            | Connection-level send/recv window                                    |
| `H2DrainState`                       | `h2_drain.rs`                   | Graceful-shutdown bookkeeping                                        |
| `H2ByteAccounting`                   | `h2.rs`                         | Overhead byte attribution                                            |
| `H2ConnectionConfig`                 | `h2.rs`                         | Per-listener tuning                                                  |
| `H2FloodConfig`                      | `h2_flood_detector.rs`          | CVE-mitigation thresholds                                            |
| `H2FloodViolation`                   | `h2_flood_detector.rs`          | A tripped threshold's (reason, count, threshold)                     |
| `H2FloodDetector`                    | `h2_flood_detector.rs`          | CVE-mitigation rate/lifetime counters                                |

---

## 2. Session Lifecycle (Server Position)

### 2.1 From accept to preface

1. The server-level session (HttpsSession / HttpSession) accepts a TCP
   connection, performs the TLS handshake, and upgrades to the mux protocol.
2. `Connection::new_h2_server` (`connection.rs`) constructs a `ConnectionH2`
   pre-seeded with:
   - `expect_read = Some((H2StreamId::Zero, CLIENT_PREFACE_SIZE))` — 24-byte
     preface + 9-byte SETTINGS header (see `h2.rs::CLIENT_PREFACE_SIZE`).
   - `readiness.interest = READABLE | HUP | ERROR`.
3. The event loop fires `ready()` which dispatches to `Connection::readable`
   (`connection.rs`) → `ConnectionH2::readable` (`h2.rs`).

### 2.2 Connection state machine (`H2State`)

Declared in `h2.rs` (`pub enum H2State`):

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

- `ClientPreface` → `ClientSettings` at `h2.rs:1885`.
- `ClientSettings` → `ServerSettings` at `h2.rs:1927`.
- `Discard` (stream refused) is set in `refuse_stream_and_discard`
  (`h2.rs`) and exited by the `H2State::Discard` arm of `ConnectionH2::readable`
  (`h2.rs:1866` — one of four identical `self.expect_header();` lines, so the
  enclosing arm is named as well).
- `Continuation*` states handle multi-frame HEADERS per RFC 9113 §4.3 — enter at
  `h2.rs:4813`.
- **Discard does not skip HPACK.** HPACK field-compression state is scoped to
  the *connection* (RFC 9113 §4.3), not the stream, so the bytes `Discard`
  drops on a refused stream are still a field block the peer's encoder has
  already applied to its own dynamic table. Before `H2State::Discard`'s
  `readable()` arm clears `zero.storage`, it decodes that field block into
  `ConnectionH2::decoder` (result discarded, callback is a no-op) via the
  free function `decode_discarded_field_block`, using the
  `ConnectionH2::discarded_field_block` value `refuse_stream_and_discard`
  stashed for it (see `DiscardedFieldBlock`, `h2.rs`). A brand-new stream's
  whole HEADERS payload still carries its own PADDED/PRIORITY prefix
  (RFC 9113 §6.2), which is stripped by re-running `parser::headers_frame`
  before decoding; a CONTINUATION frame refused mid-block (CVE-2024-27316
  flood mitigation) gets its prior frames' bytes from
  `HeaderBlockAccumulator::finish` (`h2_header_reassembly.rs`) — they have
  lived in the owned `ConnectionH2::header_reassembly` accumulator since
  each prior frame was read, not in `zero.storage`, so there is nothing to
  copy out of `zero.storage` at refusal time; `enqueue_rst` still arms
  WRITABLE and the same event-loop pass's `writable()` preamble
  (`flush_pending_control_frames`) still reuses `zero.storage` as RST_STREAM
  scratch before the next `readable()` pass reads this frame's own payload,
  but that reuse now targets an empty, unrelated scratch region — see
  invariant 24. When END_HEADERS was not set on the refused frame, decoding
  is skipped rather than attempted: a refused
  multi-frame block cannot legally continue (`handle_header_state` treats a
  standalone CONTINUATION as PROTOCOL_ERROR), so there is nothing complete
  to decode. A genuine decode failure of a complete block is reported as
  `GOAWAY(COMPRESSION_ERROR)` per RFC 9113 §4.3.
- **A non-refused `Continuation*` block must not be flushed out from under
  itself.** An accepted, in-progress header block accumulates in
  `ConnectionH2::header_reassembly` (`h2_header_reassembly.rs`), a `Vec<u8>`
  no write-side code has a field to reach — see invariant 24 for the full
  design and why this makes the accumulated history unclobberable BY
  CONSTRUCTION rather than merely guarded. What remains representable, and
  still guarded, is narrower: `zero.storage` can hold a single CONTINUATION
  frame's own not-yet-fully-read payload (a partial `socket_read()`) at the
  moment `flush_pending_control_frames`'s WINDOW_UPDATE and RST_STREAM drain
  stages (`h2.rs`) would otherwise reuse it as write scratch for ANY queued
  frame, on ANY stream — connection-level flow-control WINDOW_UPDATE
  housekeeping is the ordinary case, not an attack. Since `Mux::ready`
  dispatches frontend `readable()` then `writable()` in the same sweep, an
  unrelated flush landing mid-read of a CONTINUATION frame would clear that
  frame's partial bytes out from under it. Both drain stages therefore check
  `ConnectionH2::header_block_reassembly_in_progress` (`h2.rs`) — true while
  `self.state` is `ContinuationHeader`/`ContinuationFrame` — and defer rather
  than flush. Nothing is lost: `Self::queue_window_update` and
  `Self::enqueue_rst` already arm WRITABLE when they queue a frame, so the
  next `writable()` call after the block completes drains it normally. This
  mirrors the write side's own protection of the same buffer — while a
  zero-buffer write is stalled (`expect_write == Some(Zero)`), READABLE
  interest is disabled so a fresh frame read cannot clobber it either.
- **`graceful_goaway` is subject to the same guard, unconditionally — the
  third and previously unguarded instance of this bug class.** Its first
  GOAWAY also clears and reuses `zero.storage` to serialize the advisory
  `GOAWAY(NO_ERROR, last_stream_id=MAX)`, directly contradicting its own
  intent that in-flight streams keep reading during the drain window. Unlike
  the WINDOW_UPDATE/RST_STREAM case, no coincident traffic is required — every
  drain during an in-flight reassembly hits it. Since the step-5 GOAWAY/drain
  extraction (`h2_drain.rs`), the decision lives in
  [`H2DrainState::begin_graceful_drain`](h2_drain.rs): `ConnectionH2::graceful_goaway`
  passes it `now` and its own `header_block_reassembly_in_progress()` result,
  and the method returns `GracefulDrainDecision::DeferInitial` — arming
  `initial_goaway_pending` internally — when a block is in progress, instead
  of `SendInitial`. Unlike the WINDOW_UPDATE/RST_STREAM stages, which already
  had a pending queue to fall back on, a deferred advisory GOAWAY has nothing
  else to carry it, so `flush_pending_control_frames` drains the flag via
  `H2DrainState::take_deferred_initial_goaway` and the new
  `ConnectionH2::send_initial_goaway` (split out of `graceful_goaway`) as soon
  as reassembly completes, guaranteeing it is still sent rather than silently
  lost. `begin_graceful_drain` marks the connection draining and arms the
  forced-close budget unconditionally before the reassembly check, so the
  forced-close budget invariant (§9.3 below) still arms on time.
  `ConnectionH2::goaway` (the final GOAWAY) calls
  [`H2DrainState::enter_final_goaway`](h2_drain.rs), which clears
  `initial_goaway_pending` too: `goaway` drops `expect_read`, so no further
  reassembly will ever complete, and `ConnectionH2::flush_zero_buffer` — the
  direct, `writable()`-bypassing flush `Mux::shutting_down` calls right after
  `graceful_goaway` — is itself now a no-op while reassembly is in progress,
  for the same reason.

### 2.3 Entry points

| Method                                   | Definition   | Called from                           |
| ---------------------------------------- | ------------ | ------------------------------------- |
| `ConnectionH2::readable`                 | `h2.rs`      | `Mux::ready` when `READABLE` asserted |
| `ConnectionH2::writable`                 | `h2.rs`      | `Mux::ready` when `WRITABLE` asserted |
| `ConnectionH2::close`                    | `h2.rs`      | `Mux::close` and dead-backend cleanup |
| `ConnectionH2::graceful_goaway`          | `h2.rs`      | `Mux::shutting_down` (`mod.rs`)       |
| `ConnectionH2::cancel_timed_out_streams` | `h2.rs`      | top of every `readable()` call        |

### 2.4 Termination triggers

The session returns to the higher-level server loop via `SessionResult` returned
from `Mux::ready`. Termination may be triggered by:

- Frontend HUP — detected at `mod.rs:1343`, subject to
  `delay_close_for_frontend_flush` (`mod.rs`) to avoid truncating TLS.
- `MuxResult::CloseSession` from any readable/writable path (`mod.rs:1408`,
  `mod.rs:1735`, etc.).
- A loop-iteration budget overrun (`MAX_LOOP_ITERATIONS = 10_000`, `mod.rs`,
  check at `mod.rs:1768`).
- Timeout (`Mux::timeout`, `mod.rs`).
- Graceful shutdown initiated by the server (`Mux::shutting_down`,
  `mod.rs`).

---

## 3. Stream Lifecycle

### 3.1 HTTP/2 logical states vs Sōzu `StreamState`

RFC 9113 §5.1 defines six stream states. Sōzu collapses this onto the
five-variant `StreamState` declared in `stream.rs`:

```
RFC 9113:        idle → open → half-closed(remote) → closed
                               half-closed(local)
                               reserved(local/remote)

StreamState:     Idle  → Link → Linked(Token) → Unlinked → Recycle
```

- `Idle` — slot created but no request attached yet. `stream.rs`.
- `Link` — request fully parsed; waiting for a backend connection. Transitions:
  `ConnectionH1::readable`, `ConnectionH1::writable`, and the `Reconnect` and
  `ReplayOnFreshBackend` arms of `ConnectionH1::end_stream` (`h1.rs`);
  `ConnectionH2::handle_headers_frame`, `ConnectionH2::handle_goaway_frame`,
  and the same two arms of `ConnectionH2::end_stream` (`h2.rs`). Eight sites;
  the reviewer check is `grep -n 'state = StreamState::Link;'` over both
  files. Symbols rather than line numbers here on purpose: this list was a
  hand-maintained index of six numbers that was mechanically renumbered
  while the two arms of `end_stream` added by sozu-proxy/sozu#1442 were
  never added to it.
- `Linked(Token)` — bound to a backend (`token` identifies which one); H2
  request/response bytes flow both ways. Set by `Context::link_stream`
  (`mod.rs`), cleared by `Context::unlink_stream` (`mod.rs`).
- `Unlinked` — backend finished or was reset; response may still need to drain
  to the client. Transitions: `answers.rs:326/342`, `h1.rs:1002-1059`,
  `h2.rs:5875/5971/5996/6014`.
- `Recycle` — slot fully finalized; reusable by `Context::create_stream`.
  Transitions: `Mux::close` (`mod.rs`);
  `ConnectionH2::prune_inactive_streams_while_closing`,
  `ConnectionH2::cancel_timed_out_streams` and
  `ConnectionH2::handle_rst_stream_frame` (`h2.rs`). Four sites; the reviewer
  check is `grep -rn 'state = StreamState::Recycle;' lib/src/protocol/mux/`.
  Symbols rather than line numbers, for the reason given under `Link` above.

`StreamState::is_open()` returns true for everything except `Idle` and `Recycle`
(`stream.rs`).

### 3.2 Who transitions what

- **Stream creation.** `Context::create_stream` (`mod.rs`) either reuses a
  `Recycle` slot or pushes a new `Stream`. For H2, the per-stream wire-id
  mapping and the liveness timer are both armed together by
  `ConnectionH2::create_stream` (`h2.rs`) via `H2StreamTable::register`
  (`h2_stream_table.rs`) — cited by symbol on both ends because the call site's
  own line, `self.stream_table`, is one of thirteen identical lines in `h2.rs`.
- **Backend attach.** `Router::connect` (called from `mod.rs:1800` during the
  `pending_links` drain) eventually calls `Context::link_stream`
  (`router.rs:442/622`) which sets `Linked(token)` and pushes to
  `context.backend_streams`.
- **Backend detach.** `Context::unlink_stream` (`mod.rs`) — called from
  timeout paths (`mod.rs:2068/2085/2169/2180`), from H1 EOF (`h1.rs:1016`), and
  from H2 reset/end (`h2.rs:5922/5867`).
- **Recycle.** The H2 write path recycles a server stream once both the front
  request and back response are `is_terminated() && is_completed()` — see
  `try_recycle_server_stream` flow in `h2.rs`.

### 3.3 What "Recycle" means and when slots are popped

`StreamState::Recycle` marks a slot as **logically free but still allocated**.
It means:

- The pool buffers have been cleared (`mod.rs:668-671`).
- Metrics have been reset (`mod.rs:673`).
- The slot can be handed back to a new request by `Context::create_stream`
  which, on entry, searches for a `Recycle` slot (`mod.rs:652-655`).

A `Recycle` slot is **physically popped** only when `shrink_trailing_recycle`
runs — see §6.

---

## 4. The Two Stream Maps

### 4.1 `ConnectionH2.streams` (per-connection wire map)

- Type: `HashMap<StreamId, GlobalStreamId>` — private field of
  [`H2StreamTable`](h2_stream_table.rs) (`h2_stream_table.rs:125`) since the
  step-3 stream-slot-bookkeeping extraction. `ConnectionH2` holds a single
  `stream_table: H2StreamTable` field (`h2.rs`) and reaches the map only
  through `H2StreamTable`'s closed API — see §5.4.
- Scope: one map per `ConnectionH2` (frontend _and_ each H2 backend).
- Key: the 31-bit wire stream ID negotiated in the H2 frame header.
- Value: the `Vec` index (`GlobalStreamId`) pointing into `context.streams`.
- Mutated by:
  - insert — `H2StreamTable::register` (`h2_stream_table.rs`), called from
    `ConnectionH2::create_stream` and `ConnectionH2::start_stream` (`h2.rs`).
  - remove — `H2StreamTable::remove` (`h2_stream_table.rs`) only, called from
    `ConnectionH2::remove_dead_stream` (`h2.rs`); every caller routes through
    it (see §5.4). This is now a **compiler-enforced** fact, not just a
    convention: `streams` is private to `h2_stream_table.rs`, so
    `self.streams.remove(...)` cannot even be written from `h2.rs`.

### 4.2 `Context.streams` (per-session buffer array)

- Type: `Vec<Stream>` — `mod.rs:422`.
- Scope: one `Vec` per `Mux` session. **Both** H1 and H2 frontends use it, and
  **every** backend `ConnectionH2` attached to this session indexes into it.
- Index: `GlobalStreamId = usize` (`mod.rs`).
- Mutated by:
  - push — `create_stream` when no `Recycle` slot is available (`mod.rs:625-626`).
  - pop — `shrink_trailing_recycle` (`mod.rs`).
  - in-place state edits — everywhere.

### 4.3 Invariants

1. For every entry `(wire_id → gid)` in `ConnectionH2.streams`, `gid` is a valid
   in-bounds index into `context.streams`.
2. The `Stream` at that index is not in `StreamState::Recycle` — or, if it is,
   the caller is in the middle of a teardown pass that is about to remove the
   wire-id entry (never a public surface).
3. A `StreamState::Linked(token)` must have a matching entry in
   `context.backend_streams[token]`. This is asserted in debug builds at
   `mod.rs:1948-1978` after every `ready()` pass.

The H1 side keeps its single `stream: Option<GlobalStreamId>` in `ConnectionH1`
— there is no hashmap because H1 multiplexing is limited to request pipelining
with at most one live stream at a time.

---

## 5. Index-Caching Invariants (`expect_write` / `expect_read`)

### 5.1 The fields

Both private fields of [`H2StreamTable`](h2_stream_table.rs)
(`h2_stream_table.rs:129-131`) since the step-3 extraction, reached from
`h2.rs` only through `expect_read()`/`set_expect_read()` and
`expect_write()`/`set_expect_write()`:

```rust
expect_read:  Option<(H2StreamId, usize)>,  // (id, remaining bytes)
expect_write: Option<H2StreamId>,
```

`H2StreamId` (`h2.rs`):

```rust
pub enum H2StreamId {
    Zero,                                            // connection-level (preface, SETTINGS, etc.)
    Other { id: StreamId, gid: GlobalStreamId },     // a specific stream
}
```

Their meaning:

- `expect_read = Some((sid, n))` — we need `n` more bytes of payload on stream
  `sid`; the read path will top up the relevant buffer.
- `expect_write = Some(sid)` — a partial write on `sid` is parked;
  `write_streams` will resume it on the next writable pass.

### 5.2 Load-bearing invariant

> If `expect_write` or `expect_read` holds an `H2StreamId::Other { gid, .. }`,
> then `gid` **MUST** be a valid in-bounds index into `context.streams` at the
> moment it is dereferenced.

Dereference sites:

- `write_streams` resume path — reads `context.streams[global_stream_id]` in
  the `H2StreamId::Other` arm of `ConnectionH2::write_streams`'s `expect_write`
  block (`h2.rs`). By symbol, not line: that read is one of fourteen identical
  `let stream = &mut context.streams[global_stream_id];` lines.
- `try_resume_reading` — reads `context.streams[global_stream_id]` at
  `h2.rs:3484`.
- `readable` — reads `context.streams[global_stream_id]` at `h2.rs:1770`.

An out-of-bounds index will panic via `Vec`'s bounds check.

### 5.3 Why the invariant is non-trivial

`gid` is a `Vec` index — it is **physically invalidated** when
`shrink_trailing_recycle` pops slots from the end of `context.streams` (§6). A
`gid` that was valid at the time it was cached in `expect_write` can therefore
become out-of-bounds later on if:

1. The stream pointed to by `gid` (or a trailing block of streams beyond it) is
   transitioned to `StreamState::Recycle`;
2. A new stream is created which triggers `shrink_trailing_recycle`;
3. The slot at `gid` is popped;
4. The cached `expect_write` is then dereferenced.

Step 1 happens in every `remove_dead_stream` caller and in every reset / cancel
path (see §8). Step 2 happens on every `create_stream` call that finds a
recycled slot to reuse and then crosses the shrink ratio (`mod.rs:679-680`).
Step 3 follows automatically.

### 5.4 Who invalidates these fields

**Every site that removes a stream from the wire map is responsible for
invalidating both `expect_write` and `expect_read` when they reference that
stream's `gid`.** Since the step-3 extraction this is enforced by
[`H2StreamTable::remove`](h2_stream_table.rs) itself (`h2_stream_table.rs`),
the sole function that can touch the private `streams` field:

```rust
if matches!(self.expect_write, Some(H2StreamId::Other { gid, .. }) if gid == global_stream_id) {
    self.expect_write = None;
}
if matches!(self.expect_read, Some((H2StreamId::Other { gid, .. }, _)) if gid == global_stream_id) {
    self.expect_read = None;
}
```

`ConnectionH2::remove_dead_stream` (`h2.rs`) is the thin orchestrating wrapper
every caller still goes through — it delegates the bookkeeping above to
`H2StreamTable::remove` and additionally evicts the RFC 9218 `prioriser` entry
(out of `h2_stream_table.rs`'s scope; see that module's doc). Call sites
(non-exhaustive — new ones may be added without updating this list, but the
routing discipline is compiler-enforced, not just documented):

- `write_streams` after end-of-stream — `h2.rs:2187`/`h2.rs:2584`.
- `prune_inactive_streams_while_closing` — `h2.rs:2760`.
- `handle_window_update_frame` zero-increment path — `h2.rs:5498`.
- `cancel_timed_out_streams` slow-multiplex guard — `h2.rs:3653`.
- `handle_continuation_header_state` CONTINUATION oversize — `h2.rs:1667`.
- `handle_rst_stream_frame` peer RST — `h2.rs:5202`.
- `handle_goaway_frame` retry loop — `h2.rs:5448`.
- `end_stream` client-side retirement — `h2.rs:5969`.

No call site in this file performs `self.streams.remove(...)` inline: it
cannot — `streams` is a private field of `h2_stream_table.rs`, so that
expression does not even compile from `h2.rs` (verified: a deliberately
reintroduced inline `self.streams.remove(...)` inside `ConnectionH2` fails
with `E0609: no field 'streams'`). The one exception below does not remove at
all:

- `close` backend-stream teardown — `h2.rs:5835-5837` (does not remove, only
  notifies the endpoint — the surrounding `close` path drops the whole
  connection, and every entry in the wire map (`self.stream_table`) with it,
  shortly after).

The two-check pattern — one for `expect_write`, one for `expect_read` — is
mechanical work `remove_dead_stream` now performs for every removal above; a
reviewer's job on a new removal site is to confirm it calls the helper rather
than reintroducing an inline `self.streams.remove(...)`.

---

## 6. `shrink_trailing_recycle` (mod.rs)

```rust
pub fn shrink_trailing_recycle(&mut self) {
    while self.streams.last().is_some_and(|s| s.state == StreamState::Recycle) {
        self.streams.pop();
    }
}
```

### 6.1 When it runs

Called from `Context::create_stream` after a `Recycle` slot is reused, guarded
by a ratio threshold so we don't thrash on every request (`mod.rs:679-680`):

```rust
if total > 1 && active > 0 && total > active * self.h2_stream_shrink_ratio {
    self.shrink_trailing_recycle();
}
```

The default ratio is 2 (`h2.rs`, `DEFAULT_STREAM_SHRINK_RATIO`),
overrideable per listener via `H2ConnectionConfig::stream_shrink_ratio`
(`h2.rs:313`). In short: if more than `2×active` slots are held, trim trailing
`Recycle` entries.

### 6.2 What it pops

Only **trailing** `Recycle` slots. Interior `Recycle` slots are kept so that
live `GlobalStreamId` values in the middle of the `Vec` stay stable. This means:

- A `gid` in the middle of the `Vec` keeps its index across a shrink, regardless
  of whether the `Vec` shrinks or not.
- A `gid` **past the new length** after shrink becomes invalid — any cache
  (notably `expect_write` / `expect_read`) that holds such a `gid` will panic on
  its next dereference.

### 6.3 Why it matters for `GlobalStreamId` validity

Together with §5: a `gid` that was valid when written into
`expect_write`/`expect_read` can silently turn into a dangling index. The
defence is the invalidation discipline in §5.4 — combined with the invariant
that an entry cached for a specific `gid` must be cleared _before_ that `gid`'s
slot can be popped.

---

## 7. Timeouts

There are four timer surfaces, fired by the proxy's central timer wheel and
funneled into `Mux::timeout` (`mod.rs`). Their `duration` is configured per
listener. All four are evaluated against the per-pass clock snapshot of §7.5,
not against a fresh `Instant::now()`. The wheel can hand an entry over up to a
full tick minus a millisecond BEFORE its deadline, so `Mux::timeout`
re-validates every delivery and puts an early one back — §7.6.

### 7.1 Connection-level (frontend) idle timeout

- Tracker: `ConnectionH{1,2}.timeout_deadline`, the instant the core wants its
  embedder to call back at. The wheel handle lives in `Mux.timeouts` under the
  frontend token — see §7.7.
- Fired when: no traffic observed for `configured_frontend_timeout`
  (`mod.rs:721`) while any stream is live, or the shorter `request_timeout`
  until the first `Link` transition (`mod.rs:1795-1797`).
- Reset: on meaningful activity — HEADERS for an existing stream, DATA bytes —
  see `h2.rs:4769` and `h2.rs:1768`. Control frames (PING, WINDOW_UPDATE,
  SETTINGS) deliberately do **not** reset it so a misbehaving peer cannot pin
  the session with keepalive noise (`h2.rs:1752-1758`).
- Handling: `Mux::timeout` inspects each stream's state (`mod.rs:2040`) and
  either writes a default 408/503/504 answer, forcefully terminates, or keeps
  draining.
- Access-log discriminator: before each `set_default_answer` or
  `forcefully_terminate_answer`, the arm sets
  `stream.context.access_log_message` to a stable token surfaced via
  `Stream::generate_access_log` (`stream.rs`): `client_timeout` for the H1
  `Idle` 408 arm (mux H2 `Idle` is silently ignored, not a timeout from the
  operator's view), and `client_timeout_during_response` for both the `Linked`
  504 arm and the `Linked` `forcefully_terminate_answer(InternalError)` arm. The
  503 arm in `StreamState::Link` (no backend resolved yet) is not a timeout and
  leaves `access_log_message = None`. See `doc/configure.md` § "Access log
  message field" for the full vocabulary.

### 7.2 Per-stream reap guards (slow-multiplex + window-stall)

Two independent per-stream deadlines, both bounded by
`ConnectionH2.stream_idle_timeout` and reaped by
`ConnectionH2::cancel_timed_out_streams` (`h2.rs`), which unions them
(deduped) via the unit-testable free function `collect_timed_out_streams`. Both
deadlines are compared against `ConnectionH2.now` (§7.5):

- **Bidirectional-silence guard** — `ConnectionH2.stream_last_activity_at:
  HashMap<StreamId, Instant>`. Refreshed on every non-empty inbound DATA frame,
  on HEADERS for an existing stream (trailers), and on outbound bytes written.
  Catches a stream making no forward progress in either direction
  (slow-multiplex Slowloris: the connection-level timer resets on every frame,
  so without this per-stream guard a peer could hold `max_concurrent_streams`
  slots for the full session timeout).
- **Outbound-flow-control-stall guard** — `ConnectionH2.stream_fc_stalled_since:
  HashMap<StreamId, Instant>`, paired with
  `ConnectionH2.stream_fc_stalled_progress: HashMap<StreamId, usize>` (the
  cumulative-stall budget). Armed (in `write_streams`) whenever a stream holds
  sendable buffered data it cannot send because its effective send window
  `min(stream.window, connection.window)` is exhausted. This is
  **bidirectional**: the buffered data is the **response** on a `Position::Server`
  (frontend) connection and the **request upload** on a `Position::Client`
  (backend) connection — so a slot pinned by a stalled upload to a slow H2 backend
  is reaped too (a legitimately slow upload to a window-shut backend is then
  cancelled and returned to the client as a `502` via `end_stream_decision`; size
  `h2_stream_idle_timeout_seconds` accordingly). It is **never** refreshed by
  inbound DATA/HEADERS, so a peer keeping the silence guard warm with an inbound
  1-byte DATA drip cannot keep it alive. The deadline clears only on a genuinely
  open window OR once `stream_fc_stalled_progress` reaches `FC_STALL_CLEAR_FLOOR`
  (16 KiB = one max DATA frame). A `WINDOW_UPDATE(+1)` drip that trickles ~1 byte
  per idle period — on the main write loop **and** on the socket-backpressure
  resume path (`h2.rs:2479`) — therefore never reaches the floor, so the deadline
  ages out and the stream is reaped (the HTTP/2 window-stall / `WINDOW_UPDATE`-drip
  vector is closed). The progress accumulator is kept in lockstep with
  `stream_fc_stalled_since` at every arm/clear/evict site.

- Action (both): queue `RST_STREAM(CANCEL)` via `enqueue_rst`, remove the wire-id
  mapping, mark the stream `Recycle` (on `Position::Server`; the `Position::Client`
  reap is finalized through the linked frontend's `end_stream`), notify the linked
  endpoint. The access-log reason discriminates them — `"H2::IdleTimeout"` vs
  `"H2::WindowStall"` — and is emitted once, frontend-side.
- Run from the top of every `readable()` call **and** from the connection-level
  `MuxState::timeout`, so a fully-silent peer — which never triggers a read
  event — still has its window-stalled stream reaped; the timeout path consults
  `ConnectionH2::has_pending_control_write` and sets `should_write` so the queued
  `RST_STREAM(CANCEL)` is flushed to the peer before close (`has_pending_write`
  intentionally ignores `pending_rst_streams` because it gates connection close).
- **Does** feed `total_rst_streams_queued` and the MadeYouReset emitted-lifetime
  cap (via `enqueue_rst` → `account_emitted_rst`, since `Cancel != NoError`):
  proxy-emitted reaps deliberately count against the caps so an attacker cannot
  use proxy-forced resets to bypass the ceiling (security review LISA-001; see
  §8.2).
- All three per-stream maps (`stream_last_activity_at`, `stream_fc_stalled_since`,
  `stream_fc_stalled_progress`) are evicted in `remove_dead_stream` (with
  `debug_assert`s guarding against a leaked entry on a new per-stream cache).
- Observability: `cancel_timed_out_streams` emits
  `h2.streams.reaped.{idle_timeout,window_stall}` per guard, plus
  `h2.streams.reaped.stall_budget` (a subset of `window_stall`) when the reaped
  stream had dribbled progress below the floor.

### 7.3 Backend timeout

- Tracker: the backend connection's own `timeout_deadline`, reflected onto the
  wheel by `Mux.timeouts[&back_token]` (§7.7).
- Set to `configured_backend_timeout` after successful connect
  (`Connection::set_timeout_duration`, called from `Mux::ready_inner`).
- Fired by: timer wheel → `Mux::timeout` with the backend token.
- Action: for each stream linked to that backend, either send 504, or forcefully
  terminate, or keep draining — see `mod.rs:2135-2189`. The timeout is re-armed
  if the session stays alive (`mod.rs:2262`) to avoid the "immortal zombie"
  state.
- Access-log discriminator: the "response not started" arm sets
  `stream.context.access_log_message = Some("backend_timeout")`; the "response
  in progress" `forcefully_terminate_answer(InternalError)` arm sets it to
  `Some("backend_response_timeout")`. Both tokens map the same
  `MuxState::timeout` branch onto stable, HAProxy-aligned strings on the
  access-log `message` field — see `doc/configure.md` § "Access log message
  field".

### 7.4 Interaction order on a loaded connection

1. `readable()` entry runs `cancel_timed_out_streams` first (§7.2).
2. Then it optionally fires `goaway(SettingsTimeout)` if the SETTINGS ACK is
   overdue (`h2.rs:1740-1749`).
3. Then it consumes the frame / payload.
4. `writable()` mirrors this check, via `flush_pending_control_frames`
   (`h2.rs:2928-2938`).
5. If the frontend timer fires while streams are linked, the timeout logic in
   `Mux::timeout` (`mod.rs`) decides per-stream; backend timer fires independently.
6. Loop budget (`MAX_LOOP_ITERATIONS = 10_000`, `mod.rs`) is a hard backstop
   at `mod.rs:1768`. `counter` is declared at `mod.rs:1341`, above BOTH loops, so
   the budget is shared across every outer iteration of one `ready()` call.

Steps 1-4 all run inside one `readable()`/`writable()` call and therefore all
read the same `ConnectionH2.now` — see §7.5.

### 7.5 Clock sampling — one snapshot per pass

Every deadline above is evaluated against a snapshot, not against a fresh
`Instant::now()`. **`Mux` is the only clock sampler in the mux.** It writes
`Context.now` (`mod.rs:528`) at three points:

- once per **outer** `Mux::ready` pass (`mod.rs:1389`), so that the inner loop
  deliberately shares one instant — that is the property the snapshot exists
  for, not a claim about how long a sweep takes. `MAX_LOOP_ITERATIONS` is a
  count and bounds iterations, not wall clock, and `counter` (`mod.rs:1341`)
  sits above both loops, so one `ready()` call can spend the whole budget
  under a single snapshot;
- at the top of `Mux::timeout` (`mod.rs:1987`);
- at the top of `Mux::shutting_down` (`mod.rs:2360`), which runs outside
  `ready()` entirely. That line is load-bearing, not belt-and-braces:
  `drive_frontend_shutdown_io` (`mod.rs`) always reaches `readable()` for
  an H2 frontend — `force_h2_read` is unconditionally true, so the early
  return above it cannot fire — and `readable` mirrors `context.now` into
  `ConnectionH2::now` before the forced-close check runs. Drop it and a
  silent draining session propagates its last `ready()` snapshot forever.
  Pinned by `shutting_down_refreshes_the_snapshot_so_the_drain_budget_expires`.

The H2 core reads `ConnectionH2.now` (`h2.rs:995`), a mirror assigned from
`context.now` at each public entry point — `readable`, `writable`,
`cancel_timed_out_streams` and `start_stream` (`h2.rs`, one
`self.now = context.now;` at the top of each) — from the `now` parameter of `graceful_goaway`
(`h2.rs`), and directly by `Mux::shutting_down`. It is a field rather than
a threaded parameter because the read sites are unreachable from a `context`:
`handle_ping_frame` takes no context at all, and the ten
`check_flood_or_return!` sites are spread across six frame handlers.
`H2FloodDetector` likewise takes `now` as a parameter
(`check_flood` and `maybe_reset_window`, `h2_flood_detector.rs`) and keeps
`window_start` (`h2_flood_detector.rs`) private, so nothing can advance the
rate window against a clock the connection is not reading.

**Consequence.** Every deadline armed or evaluated inside a pass is accurate to
within that pass, **in either direction**. The error is not one-sided, and the
asymmetry that produces it is architectural:

- An **arm** site runs at an arbitrary depth into its pass — the liveness
  refreshes at `h2.rs:1768` (DATA) and `h2.rs:4769` (HEADERS), the
  outbound-byte refreshes at `h2.rs:2149` / `h2.rs:2455-2457`, the
  `FcStallAction::Arm` branch of `write_streams` (`h2.rs`) — and stamps the
  snapshot taken at the START of that pass. The
  stored instant is therefore OLDER than the event it records.
- An **eval** site runs near the top of a pass: `cancel_timed_out_streams` is
  the first thing `readable` does (§7.4 step 1), and the SETTINGS-ACK check
  (`h2.rs:1741` in `readable`, `h2.rs:2929` in `flush_pending_control_frames`)
  is step 2.
- The measured age is therefore inflated by the arm site's depth, so a deadline
  can fire up to one pass **early** as well as one pass late. Worked against the
  strict `>` predicate in `H2StreamTable::collect_timed_out`: pass P has snapshot `T`;
  at real `T+Δ` a DATA frame stores `T`; pass Q has snapshot `T+deadline+δ` and
  computes `deadline+δ > deadline`, so it reaps — while true elapsed is
  `deadline+δ−Δ < deadline` whenever `Δ > δ`. At base both ends read the real
  clock and the comparison was exact. This is the cost of the snapshot, and it
  is bounded by one pass.

The flood window (`maybe_reset_window`, `h2_flood_detector.rs`) and the
RFC 9113 §5.1.2 back-pressure window (`h2.rs:3832`) are the one asymmetric case,
and they
**fail closed**: `now` is
constant for the whole pass, so a window cannot decay part-way through one. A
burst arriving during a pass is weighed in full against the window that was open
when the pass started, where before the change a long pass could halve the
counters under the burst. The window BOUNDARY still carries the same one-pass
error in either direction as every other deadline.

Two clocks are deliberately left alone. `SessionMetrics` stays on the real
clock — metrics want true elapsed time, not a quantised one. `TimeoutContainer`
and the thread-local timer wheel stay on the real clock too: they are the
embedder's timer, they live in the `Mux` adapter (§7.7), and no core touches
them. A core's `timeout_deadline` is stamped from the pass snapshot like every
other deadline here.

### 7.6 Early wheel delivery, re-validation, and why the two are one operation

The wheel behind all four surfaces above is duration-based and rounds a
requested delay to the NEAREST tick (`timer.rs` `duration_to_tick`:
`(elapsed_ms + tick_ms / 2) / tick_ms`), then `poll` recomputes `current_tick`
from the real clock and fires everything whose tick has come. An entry armed
for deadline `D` lands in tick `round(D / tick)` and is delivered by the first
poll at or after `tick * round(D / tick) - tick/2`, and the delivery CONSUMES
the slab entry either way. Every consumer must read a wakeup as "my entry is
gone", never as "my deadline has arrived".
`timer::test::test_timeout_fires_up_to_half_a_tick_early` is the wheel-level
statement of the grid half of that property.

**The earliness bound is a full tick minus a millisecond, not half a tick.** It
is `(D_ms + tick_ms/2) mod tick_ms`, spanning `[0, 99]` ms at the worker's
default 100 ms tick. The `<= 50 ms` half is unconditional: `next_poll_date`
returns the grid point `start + tick_ms * tick`, so a loop that sleeps until it
wakes on the grid. The further `<= 49 ms` needs the loop to poll inside
`[100T - 50, 100T)`, which it reaches through another session's earlier wheel
entry plus loop latency, or through the `Token(1)` arm — both ordinary. Reason
with 99 ms.

The same bound applies identically on the backend branch. It does NOT apply to
`Mux::shutting_down`, which `shut_down_sessions()` drives directly and which
compares `context.now` against `drain.started_at` rather than consuming a wheel
delivery.

`Mux::timeout` used to take every delivery at face value, so a 60 s
`front_timeout` could close a live session at 59.901 s. It now re-validates:
`TimeoutContainer` records the absolute instant each armed entry is meant to
fire at (`TimeoutContainer::deadline()`, the mirror of the wheel entry the
wheel itself does not keep) and `consume_timer_entry` in `mod.rs` is the single
gate both the frontend and the backend branch pass through.

That gate does three things, in order, and **they are one operation**:

1. mark the entry consumed (`TimeoutContainer::triggered`) — the wheel has
   already handed it over and the container must not try to cancel it later;
2. compare the deadline captured BEFORE step 1 against the pass's clock
   snapshot (§7.5);
3. on an early delivery, put the entry back **at the same absolute deadline**
   (`TimeoutContainer::set_at`), then return `StateResult::Continue` without
   running the timeout body.

Step 3 is the counter-intuitive one, and it is why re-validation is never
shipped on its own:

- re-arming with `set` instead of `set_at` would push the deadline out by a
  fresh full duration, so an entry delivered a few tens of milliseconds before
  a 60 s timeout would next fire at ~120 s — silently doubling the operator's
  configured timeout;
- **not re-arming at all is a lost wakeup.** The wheel entry is already gone.
  Rejecting the firing without putting it back leaves the session with no timer,
  and nothing else arms one. That is exactly the bug fixed for the UDP shell in
  `UdpManager::handle_timeout` (`lib/src/protocol/udp/manager.rs`), where
  `reschedule` emitted `ArmTimer` only when the minimum deadline had MOVED, and
  the fix was clearing `armed_deadline` on entry — consume-then-reschedule.

When the re-validation above first landed, the mux could not have the UDP bug
at all: `TimeoutContainer` had no memoization — `set` / `reset` unconditionally
cancelled and re-armed, and every `StateResult::Continue` exit from
`Mux::timeout` re-armed. The hazard belonged to any design that ADDED
memoization on top, which is why the re-validation had to land first.

**That design is now here** (§7.7): `Mux::sync_timeout` touches the wheel only
when what a handle holds differs from what its core wants. What keeps that safe
is the rule this section exists for — the firing handler clears what it believes
the wheel holds before anything reschedules. `Mux::consume_timer_entry` does it
by calling `TimeoutContainer::triggered`, which clears the handle's deadline;
`UdpManager::handle_timeout` does it by setting `armed_deadline = None`. Whoever
moves either must move all three steps together.

A firing for a core that wants no timer fails **open** and is reported due.
That is the behaviour of every release before the re-validation existed, and it
keeps a session whose deadline was dropped somewhere from becoming immortal.

### 7.7 Who owns the timer: cores publish, the adapter arms

No H1/H2 core owns a `TimeoutContainer` any more, and neither `h1.rs` nor
`h2.rs` references `crate::timer` at all. Each core carries two plain fields —
`timeout_duration` (configured) and `timeout_deadline: Option<Instant>` (the
instant it wants `timeout()` called at) — and publishes the second through
`poll_timeout()`. `Connection::{arm_timeout, clear_timeout,
set_timeout_duration}` are the whole write surface, and they replace the old
`timeout_container().{reset, cancel, set, set_duration}` one for one.

The `Mux` adapter owns every wheel handle, in `Mux.timeouts: HashMap<Token,
TimeoutContainer>` — one entry for the frontend token plus one per backend in
`router.backends`. This is the only place in the mux that talks to the timer
wheel. `Mux::reschedule` walks the cores and calls `Mux::sync_timeout`, which
arms, re-arms or cancels an entry only when what the handle holds differs from
what the core wants, then drops handles for departed tokens (`retain`;
`TimeoutContainer::drop` cancels). This is the same shape as
`UdpManager::{poll_timeout, reschedule}` (`lib/src/protocol/udp/manager.rs`),
which is the worked example the split is modelled on.

Three rules keep it honest, and each has a test that fails when it is dropped:

- **The reschedule is structural, not hand-placed.** `SessionState::{ready,
  timeout, shutting_down}` are thin wrappers around `*_inner` bodies whose only
  job is to run `reschedule` on the way out, so none of the dozen early
  `return`s inside them can skip it. `cancel_timeouts` calls it directly and
  `close` drops the whole map.
- **Consume before you reschedule.** `Mux::consume_timer_entry` calls
  `TimeoutContainer::triggered` on entry, which clears the handle's deadline, so
  the memo reads "the wheel holds nothing" and re-arms even when the core's
  deadline has not moved. Without that clearing, an early delivery is memoized
  away and the session is left with no entry at all — the lost wakeup of §7.6,
  and the reason the stored deadline had to land before the memoized reschedule.
  Pinned by `an_early_wheel_delivery_leaves_a_live_entry_at_the_same_deadline`.
- **A real expiry clears the core's deadline.** The core asked to be called at
  `D` and has been; leaving `D` in place has `reschedule` re-arm an elapsed
  instant, which the wheel re-delivers on the next tick — a busy loop. Every
  branch that keeps the session alive re-arms explicitly with `arm_timeout`.
  Pinned by `a_real_expiry_clears_the_elapsed_deadline_instead_of_re_arming_it`
  and by the strict-advance `debug_assert!` in `SessionState::timeout`, carried
  over from `UdpManager::handle_timeout`.

Two `debug_assert`s come across from the UDP manager: that strict-advance guard,
and `Mux::debug_assert_timer_coherence` (the analogue of
`UdpManager::check_invariants` (6)) which checks, after every reschedule, that
each handle is armed iff its core wants a timer, holds exactly the core's
instant, and that no handle outlived its connection.

The WebSocket upgrade (`http.rs` / `https.rs` `upgrade_mux`) takes the frontend
and backend handles out of `Mux.timeouts` and hands them to `Pipe`, which is why
`sync_timeout` keeps each handle's `duration()` current through
`TimeoutContainer::retune` — `Pipe` re-arms from it.

One deliberate semantic shift comes with the split, and it is not literally "no
change": the old `TimeoutContainer::reset()` computed its deadline from a fresh
`Instant::now()` at the call site, whereas `arm_timeout(now)` computes it from
the pass snapshot (§7.5). A deadline therefore lands earlier by however long the
pass had been running when the re-arm ran — bounded by one pass, in the same
direction as every other deadline the snapshot governs, and required by
invariant 20. The wheel's own rounding (up to 99 ms, §7.6) dominates it.

---

## 8. Termination Paths

### 8.1 Graceful GOAWAY (double-GOAWAY per RFC 9113 §6.8)

Two GOAWAY frames in `ConnectionH2::graceful_goaway` (`h2.rs`):

1. **Initial GOAWAY** — send GOAWAY with `last_stream_id = 0x7FFFFFFF`
   (`STREAM_ID_MAX`, `h2.rs`); keep `READABLE` so in-flight request bodies
   can still arrive. Called first time from `Mux::shutting_down` at
   `mod.rs:2367`. Draining flag set.
2. **Final GOAWAY** — on the second invocation (draining already true), call
   `goaway(NoError)` (`h2.rs:4078`) with the actual `highest_peer_stream_id`,
   remove `READABLE` interest (`h2.rs:4034`), transition to `H2State::GoAway`.
   Caller is `finalize_write` when all streams drain (`h2.rs:2795`).

`peer_gone_after_final_goaway` (`h2.rs`) guards against deadlock on a
peer-side HUP after the final GOAWAY.

### 8.2 RST_STREAM

Three directions:

- **Peer-initiated** — `handle_rst_stream_frame` (`h2.rs` — reaches
  `self.remove_dead_stream(rst_stream.stream_id, global_stream_id)`, which
  purges the wire-id via `H2StreamTable::remove`). Also runs flood counters
  (CVE-2023-44487 Rapid Reset).
- **Proxy-initiated, error response** — `reset_stream` (`h2.rs`).
  Transitions to `Unlinked`, calls `forcefully_terminate_answer` for kawa
  cleanup, queues the outgoing RST via
  [`enqueue_rst`](#proxy-rst-emission-path), and counts against
  `record_rst_emitted` (CVE-2025-8671 MadeYouReset) unless `NoError`.
- **Proxy-initiated, idle cancel** — `cancel_timed_out_streams` (§7.2); the
  per-stream `forcefully_terminate_answer` call in the `mod.rs:2089` timeout
  path now arms `Ready::WRITABLE` via `arm_writable()` so the pair with
  `event::WRITABLE` actually schedules the next `writable()` tick under
  edge-triggered epoll.

All three paths eventually purge the wire-id from the wire map
(`self.stream_table`).

#### Proxy-RST emission path

`ConnectionH2::enqueue_rst(wire_stream_id, error)` (`h2.rs`) is the
canonical entry point for every proxy-emitted stream reset. It delegates to the
free-function primitive `enqueue_rst_into` (`h2.rs`) and the free function
is unit-tested without a full `ConnectionH2` fixture (see the five
`test_enqueue_rst_into_*` tests and the `prop_pending_rst_queue_stays_within_its_bound`
property). Four invariants are kept in lock-step:

- **Dedupe** via `H2StreamTable`'s private `rst_sent: HashSet<StreamId>`
  (reached through `self.stream_table.rst_sent_contains()`/`rst_sent_mut()`):
  at most one RST per wire stream id. `HashSet::insert` returns `false` when
  the id is already present;
  the helper short-circuits on that branch so `pending_rst_streams` and
  `total_rst_streams_queued` stay consistent even when a cascading error path
  re-enters the reset flow for the same stream.
- **MadeYouReset queued cap** via `self.total_rst_streams_queued` (capped at
  `MAX_PENDING_RST_STREAMS = 200`). Each freshly queued RST bumps the counter;
  `flush_pending_control_frames` escalates to `GOAWAY(ENHANCE_YOUR_CALM)` when
  the cap is exceeded. Orthogonal to `record_rst_emitted` (the 500-emitted
  MadeYouReset lifetime cap) — a RST can be queued-but-not-yet-emitted.
- **Per-insert queue bound**, the same `MAX_PENDING_RST_STREAMS`, applied to
  `pending_rst_streams.len()` at the insert rather than at the drain. An
  insert at the cap returns `EnqueueRstOutcome::Dropped`: nothing is queued,
  nothing is recorded in `rst_sent`, `WRITABLE` is not re-armed, and
  `enqueue_rst` answers with a `h2.rst_stream_dropped` counter plus a
  session-context `error!` line. The drain-side counter check bounds only what
  is *written*, and `cancel_timed_out_streams` queues one RST per timed-out
  stream in a single sweep with no flush in between — so with an
  operator-raised `max_concurrent_streams` a mass idle-timeout reap used to
  push the queue past the bound invariant 3 asserts (sozu-proxy/sozu#1413).
  That setting bounds the live set one sweep walks, not the queue, which
  holds what every caller queued since the last successful drain; the
  DATA-on-closed-stream reset in `handle_header_state` is a second insert
  path `check_invariants` never inspects, rate-limited by `record_glitch` +
  `check_flood_or_return!` rather than by the queue bound.
  Nothing that would have reached the wire is lost: invariant 3 keeps
  `total_rst_streams_queued >= pending_rst_streams.len()`, so a full queue
  implies `total_rst_streams_queued >= MAX_PENDING_RST_STREAMS`, the counter
  half of what `flush_pending_control_frames` tests *before* its drain loop —
  it emits `GOAWAY(ENHANCE_YOUR_CALM)` instead of serialising anything. The
  other half is a state gate,
  `!matches!(self.state, H2State::GoAway | H2State::Error)`, which the drain
  below it does not share, so the implication holds only until the first
  GOAWAY: `goaway()` enters `H2State::GoAway` without clearing
  `pending_rst_streams`, and a `Mux::timeout` reap landing in that window is
  dropped with no second escalation. That window is bounded — the peer
  already holds the GOAWAY and `writable()`'s `GoAway` arm force-disconnects
  on the same pass. The peer is told to back off and the connection is torn
  down; it is not left believing a refused stream is still live.
- **Invariant 15** via `Readiness::arm_writable()` (`lib/src/lib.rs`):
  pairs `Ready::WRITABLE` interest with the matching event bit so `writable()`
  is scheduled on the next epoll tick.

The three RST push sites retrofit to `enqueue_rst`:

- DATA-on-closed-stream (`h2.rs:1551` — `H2Error::StreamClosed`).
- `refuse_stream_and_discard` (`h2.rs` — MCS / pool exhaustion).
- `reset_stream` (`h2.rs` — per-stream error paths: malformed HEADERS,
  content-length mismatch, WINDOW_UPDATE zero-increment or overflow,
  unauthorised priority updates, self-dependent HEADERS).

Serialisation happens in `flush_pending_control_frames` (`h2.rs` → the
RST_STREAM cap-check-and-drain stage at the pending-RST-streams check, which
drains `self.pending_rst_streams` into
`self.zero` via `serializer::gen_rst_stream`). This path is independent of the
owning `Stream` still being present in the wire map (`self.stream_table`), so
it survives the immediate `remove_dead_stream` call that every `reset_stream`
caller performs synchronously after return.

`finalize_write` (`h2.rs`) retains `Ready::WRITABLE` when the pass
completes but `pending_rst_streams` or `flow_control.pending_window_updates` are
still non-empty — otherwise a partial write that deferred the RST_STREAM drain
stage (gated on `expect_write.is_none()`) would strand a queued RST until an
unrelated event re-raised the writable bit. This guard, together with the
invariant-15 arm
inside `enqueue_rst`, closes the 18-check h2spec 2.0 gap previously reported in
RFC 9113 §§5.3, 6.9, 8.1.2.

### 8.3 Session drain

`Mux::shutting_down` (`mod.rs`) is called by the server loop during process
shutdown or listener reload. It:

1. Initiates the double-GOAWAY (`mod.rs:2367`).
2. Drives frontend I/O outside the epoll loop (`drive_frontend_shutdown_io`,
   `mod.rs`) — H2 needs extra passes for the peer's END_STREAM and final TLS
   flush.
3. Checks the graceful-shutdown forced-close deadline: when
   `ConnectionH2::graceful_shutdown_deadline_elapsed` (`h2.rs`) returns
   `true` (i.e. `drain.started_at + drain.graceful_shutdown_deadline <=
   ConnectionH2.now`) the session returns `true` immediately so the server loop
   can tear the connection down even with Linked streams still in flight. The
   comparison is against the connection's snapshot, which `shutting_down`
   refreshes unconditionally at its top (§7.5) — including on the
   already-draining path, which never reaches `graceful_goaway`. Without that
   unconditional refresh a silent draining session would freeze `now` at its
   last `ready()` pass and the budget would never expire.
4. Marks `front_received_end_of_stream` on streams whose request is already
   complete and consumed.
5. Returns `true` when no `Linked` or non-quiesced `Unlinked` streams remain.

The forced-close deadline is armed inside
[`H2DrainState::begin_graceful_drain`](h2_drain.rs), which
`ConnectionH2::graceful_goaway` (`h2.rs`) delegates the decision to —
`graceful_goaway` itself reads and writes none of `H2DrainState`'s five
private fields. `begin_graceful_drain` arms `started_at` from the `now` it is
handed, and the `debug_assert!` guarding that assignment states the invariant
the budget rests on:

```rust lib/src/protocol/mux/h2_drain.rs:234-237
debug_assert!(
    self.started_at.is_none(),
    "begin_graceful_drain must arm started_at exactly once, on the first call"
);
```

A later drain returns `GracefulDrainDecision::AlreadyDraining` before reaching
the assignment, so it can neither re-arm nor extend the budget. `now` is
`graceful_goaway`'s one parameter precisely because its caller,
`Mux::shutting_down`, runs outside the pass that last refreshed the mirror. The budget itself comes from the
listener knob `h2_graceful_shutdown_deadline_seconds` (proto field
`h2_graceful_shutdown_deadline_seconds`, defaulting to 5 s). Setting the knob to
`0` maps to `graceful_shutdown_deadline = None`, which disables the forced-close
branch entirely — shutdown then reverts to "wait for every stream to drain"
semantics. Peer-initiated drains received via `handle_goaway_frame` deliberately
do **not** arm `started_at`: the budget only applies to the proxy's own
soft-stop.

### 8.4 `Connection::end_stream` (backend-side retirement)

`ConnectionH2::end_stream` (`h2.rs`) is the server-side wiper for a single
stream that has completed on the backend. Behavior depends on `Position`:

- **Client** position (i.e. the backend's view) — `h2.rs:5931-5980`. Sends
  RST_STREAM(CANCEL) unless both request and response have terminated, removes
  the wire mapping, marks the stream `Unlinked` if not already `Recycle`.
- **Server** position — the `Position::Server` arm of `ConnectionH2::end_stream`
  (`h2.rs:5981-6088`; the range start is one of eight identical
  `Position::Server => {` lines, hence the symbol). Dispatches on
  `end_stream_decision` (`shared.rs`): either `ForwardTerminated`,
  `CloseDelimited`, `ForwardUnterminated`, `SendDefault(status)`, `Reconnect`,
  or `ReplayOnFreshBackend` — each path sets the appropriate `StreamState` and
  schedules the frontend write.

### 8.5 Stale-upstream replay (`ReplayOnFreshBackend`)

`end_stream_decision` splits "the backend closed without answering" in three,
on two questions: did the request reach the upstream, and is it replayable?

| Request state | Upstream answered | Action |
|---|---|---|
| untouched (`!front.consumed`) | no | `Reconnect` — re-link and route from the intact front kawa |
| written, replayable | no | `ReplayOnFreshBackend` — re-link and re-serialize the captured bytes |
| written, not replayable | no | `SendDefault(502)` |

`ReplayOnFreshBackend` exists because a *pooled* H1 keep-alive upstream can be
closed by its peer while idle, with no event sozu has processed yet. The next
request is written onto that socket, the read returns EOF, and before
sozu-proxy/sozu#1442 the session answered `502 Bad Gateway` in well under a
millisecond — a fast wrong answer no latency budget can see. Measured
2026-09-22 with only this decision reverted: 25 failures over 576 pooled
`test_issue_806` trials, 4.3% per trial, 25 red runs of 25; 25 green runs of
25 with the replay in place. (`repeat_until_error_or` stops at the first
failure, so a run performs between 1 and `n` trials, not `n`.)

The boundary is **no response byte has been received**, because nothing was
observed by the client, so re-issuing is unobservable. It is narrowed by three
further conditions, each carrying its precedent:

- **Pooled connections only.** `ConnectionH1::reused_from_pool` is set on the
  `KeepAlive -> Connected` transition in `start_stream` and nowhere else, and
  only that path arms the capture. This is pingora's `RetryType::ReusedOnly`.
  It is a POLICY, not a diagnosis: sozu cannot tell a stale pool socket from
  an origin that half-closed after processing the request, or from one that
  crashed mid-request — `ConnectionH1::readable` treats every `size == 0`
  alike, and an empty back buffer proves only that no response byte arrived.
  Restricting the replay to pooled connections narrows it to the case where
  an unobserved idle close is plausible; the idempotence condition below is
  what makes re-issuing permissible at all.
- **Idempotent methods only** (`Method::is_idempotent`, RFC 9110 §9.2.2).
  nginx keeps `non_idempotent` out of the `proxy_next_upstream` default;
  pingora's default `error_while_proxy` vetoes `!method.is_idempotent()`.
  `Method::Custom` — which is how sozu parses `PATCH` — counts as
  non-idempotent.
- **One front buffer.** `ConnectionH1::writable` copies the bytes it hands the
  socket into `Stream::retry_buffer` before `kawa::Kawa::consume` drops them,
  and truncates the capture to `None` past `front.storage.capacity()`. HAProxy
  states the same trade: retrying past `conn-failure` "requires to allocate a
  buffer and copy the whole request into it", and "Requests not fitting in a
  single buffer will never be retried".

The replay is bounded by `CONN_RETRIES` alone. The capture is *taken*, not
cloned, so it does not survive its own replay — but that does not make one
request one replay: `start_stream` re-arms a fresh capture on every
`KeepAlive -> Connected` transition and `reused_from_pool` is never cleared,
so a replay that lands on another pooled connection may itself be replayed.
The bound is the re-link going back through `Router::connect`, whose
`stream.attempts >= CONN_RETRIES` gate (3, `server.rs`) answers 503 once the
budget is spent. `backend.retry.stale_upstream` can therefore increment more
than once for one client request.

`Router::connect` skips `route_from_request` on a replay — `front.consumed` is
the discriminator — because routing already ran and its rewrites are baked
into the captured bytes. Re-running it against a drained front kawa cannot
reproduce them, and whatever the next `prepare` emitted would land BEHIND the
replayed bytes (`prepare` appends; `queue_upstream_replay` prepends) as a
second partial copy of the request. A replay whose `cluster_id` did not
survive, and one whose cluster switched to `http2` between attempts, are both
refused through `BackendConnectionError::ReplayRefused` and answered 502 —
what the stream would have received before the replay existed.

The captured bytes are PREPENDED to `front.out`, because `out` need not be
empty: a partial `socket_write_vectored` leaves the refused remainder queued
(`kawa::Kawa::consume` pushes the partially consumed store back to the front),
and appending behind it would put `[tail][head]` on the wire.

Replaying the serialized form rather than re-running the block converter makes
the retried request byte-identical to the first attempt: same `Sozu-Id`, same
`X-Forwarded-*`, so one access-log line still describes one client request.
`backend.retry.stale_upstream` counts each replay, labelled with the cluster
and the stale backend.

---

## 9. Known Invariants Checklist

Mechanical list of invariants. A reviewer can check each one on a PR that
touches `h2.rs`, `mod.rs`, or `stream.rs`.

1. **Wire-map validity.** For every entry `(sid → gid)` in the wire map
   (`H2StreamTable.streams`, `h2_stream_table.rs:125`), `gid < context.streams.len()`.
2. **Backend index consistency.** If
   `context.streams[gid].state == StreamState::Linked(token)`, then
   `context.backend_streams[&token]` contains `gid`. Asserted under
   `debug_assertions` in `Mux::ready` at `mod.rs:1948-1978`.
3. **`expect_write` validity.** If
   `expect_write == Some(H2StreamId::Other { gid, .. })`, then
   `gid < context.streams.len()` and the slot at `gid` is not `Recycle`. Every
   stream-removal site MUST null `expect_write` out before returning; the helper
   is `remove_dead_stream` (`h2.rs`). Manual-removal sites (§5.4) must
   replicate the check inline.
4. **`expect_read` validity.** Same as (3) for `expect_read`.
5. **Recycled slot cleanliness.** A `StreamState::Recycle` slot has cleared
   `front`, `back`, `front.storage`, `back.storage`, reset metrics
   (`mod.rs:668-673`).
6. **No stale `Linked` after backend close.** Before transitioning a stream away
   from `Linked(token)`, call `unlink_stream` (`mod.rs`) or
   `remove_backend_stream` (`mod.rs`) to keep the reverse index honest.
7. **No duplicate RST_STREAM on the wire.** Check
   `self.stream_table.rst_sent_contains(sid)` (`h2.rs:710`) before queuing
   another.
8. **No new streams during drain.** `create_stream` and `start_stream` both
   short-circuit when `self.drain.draining()` (`h2.rs:4285-4292`,
   `h2.rs:6102-6109`).
9. **Connection-level timer resets only on application activity.** H2 control
   frames (PING / WINDOW_UPDATE / SETTINGS) do **not** push
   `ConnectionH2.timeout_deadline` out. `arm_timeout()` has exactly three call
   sites: DATA payload (the `H2StreamId::Other` arm of `readable`), HEADERS
   (inside `handle_headers_frame`), and the top of `write_streams` — outbound
   application data is activity too, and that third site is why the READ path
   alone does not describe the invariant. A control frame reaches none of the
   three.
10. **Single `graceful_goaway` per session outside the final GOAWAY.**
    `Mux::shutting_down` (`mod.rs:2366-2367`) only calls it if
    `!self.frontend.is_draining()`; a second unconditional call would
    collapse the initial GOAWAY into the final one and disconnect
    in-flight streams.
11. **Frontend HUP defers close when output is pending.**
    `delay_close_for_frontend_flush` (`mod.rs`) must be consulted before
    returning `SessionResult::Close` so that unflushed TLS/GOAWAY records are
    not lost.
12. **Loop budget.** Every inner loop in `Mux::ready` and
    `drive_frontend_shutdown_io` bounds iterations at
    `MAX_LOOP_ITERATIONS = 10_000` (`mod.rs`).
13. **`shrink_trailing_recycle` runs only from `create_stream`.** Calling it
    from elsewhere can invalidate cached `GlobalStreamId` values (including
    `expect_write`/`expect_read`) that the caller is not prepared to re-check.
    Keep the single call site, in `Mux::create_stream` (`mod.rs`).
14. **Backend `active_requests` balance.** Every site that removes a stream
    outside `Connection::end_stream` must decrement `backend.active_requests`
    itself (`ConnectionH2::cancel_timed_out_streams`,
    `ConnectionH2::handle_rst_stream_frame` and
    `ConnectionH2::handle_goaway_frame`; all `h2.rs`).
    Otherwise load-balancing counters drift monotonically.
15. **Incremental scheduler — solo-bucket non-yield.** The converter
    (`converter.rs` `H2BlockConverter::call` DATA arm) must only yield after a
    DATA frame when `incremental_mode == true` _and_
    `incremental_peer_count > 1`. A solo incremental stream with no peer to
    interleave with must drain sequentially — otherwise `finalize_write`
    (`h2.rs` `fn finalize_write`) would withdraw `Ready::WRITABLE` on a clean
    pass (no `expect_write` set by the converter) and edge-triggered epoll would
    never re-fire. `ConnectionH2::write_streams` populates both fields per
    `kawa.prepare` from the pass census that
    [`H2Scheduler::begin_pass`](h2_scheduler.rs) built — `is_incremental` from
    `H2Scheduler::priority`, the peer count from
    `ReadyIncrementalCensus::incremental_peer_count`. The `> 1` comparison
    itself stays in the converter. Only the `next_closes_stream` term of that
    expression is immovable — it reads `kawa.blocks.front()`; the first two
    conjuncts could collapse into one scheduler-computed `may_interleave`,
    leaving `may_interleave && !next_closes_stream` here with the look-ahead
    untouched. That is deferred because editing `converter.rs` forfeits this
    step's byte-identity with its parent, which is the evidence for its
    no-new-copy claim — not because the collapse is unsound.
16. **Never withdraw `Ready::WRITABLE` with pending back-buffer after a pass
    that made forward progress.** `finalize_write` (`h2.rs` `fn finalize_write`)
    removes `Ready::WRITABLE` only when the pass drained cleanly
    (`!socket_wants_write && expect_write.is_none()`) _and either_
    `bytes_written_this_pass == 0` _or_ no open stream has queued response bytes
    (`back.out`/`back.blocks`). The progress check is load-bearing: a
    zero-progress pass (e.g. all streams flow-control-starved) must relinquish
    `Ready::WRITABLE` so the session dispatcher does not busy-spin — the next
    wake-up arrives from `WINDOW_UPDATE`, backend readable, or a new request.
    Enforced at runtime via the file-private helper
    `any_stream_has_pending_back` (`h2.rs`). A voluntary scheduler yield (RFC
    9218 incremental rotation and any future yield site) can leave bytes
    buffered without `expect_write` being set; stripping `WRITABLE` in that
    state strands the stream because edge-triggered epoll never re-fires for
    sozu-owned buffers. Defence-in-depth for invariant 15. The same helper backs
    `ConnectionH2::has_pending_write_full`, consulted by
    `delay_close_for_frontend_flush` so shutdown-drain does not close before the
    stream bytes land on the socket.
17. **RFC 9218 incremental peer count is bucket-scoped, ready-only, and kept
    live mid-pass.** `converter.incremental_peer_count` (consumed by the
    converter's DATA-arm yield decision) counts incremental streams in the _same
    urgency bucket_ that are _also ready to emit this pass_ (`is_main_phase()` /
    terminated-but-not-completed / error-but-RST-not-yet-sent). It is
    `ReadyIncrementalCensus` (`h2_scheduler.rs`), a fixed `[usize; 8]` — RFC
    9218 §4.1 urgency is `[0, 7]` and `Prioriser::push_priority` clamps to it —
    built once per pass by `H2Scheduler::begin_pass` and read per stream by its
    own urgency through `ReadyIncrementalCensus::incremental_peer_count`. A
    connection-global count would wrongly yield a solo incremental stream when
    an unrelated incremental stream sits in a different urgency bucket —
    regressing the invariant-15 solo-bucket fast path. Readiness is the one
    fact the scheduler does not own: `begin_pass` takes it as a
    `FnMut(StreamId) -> bool` the caller computes over `Context.streams` and
    `H2StreamTable::rst_sent`, and calls it for incremental streams only.
    Any transition to ineligible _mid-pass_ MUST leave the matching bucket
    before subsequent `'outer` iterations read it. Three sites in
    `ConnectionH2::write_streams` call
    `ReadyIncrementalCensus::note_ineligible` so later same-urgency peers do
    not read the stale snapshot: the pre-prepare `rst_sent` insert, the
    post-prepare HPACK over-budget insert, and the mid-loop
    `completed_streams.push` path. That method is where the
    non-incremental guard and the `saturating_sub` live, so a fourth site
    cannot get either wrong by copying them.
    Post-loop the connection stores `ReadyIncrementalCensus::ready_total` in
    `ready_incremental_streams` and publishes it through
    `gauge_connection_state`, which emits
    `h2.streams.ready_incremental.by_urgency` as a signed `gauge_add!` delta so
    the aggregate sums across live connections; `impl Drop for ConnectionH2`
    subtracts the contribution on teardown. The sample is taken at the END of
    `write_streams` because the entry call to `gauge_connection_state` runs
    before the pass census exists. Guarded by e2e test
    `test_h2_rfc9218_incremental_multi_bucket_drains_sequentially` and by the
    unit tests `the_ready_census_is_scoped_to_one_urgency_bucket`,
    `an_unready_incremental_stream_is_not_counted_as_a_peer`,
    `the_readiness_projection_runs_for_incremental_streams_only`,
    `note_ineligible_reduces_only_its_own_bucket`,
    `note_ineligible_saturates_at_zero` and
    `note_ineligible_is_a_no_op_for_a_non_incremental_stream`
    (`h2_scheduler.rs`), which drive the production methods — the three they
    replace re-implemented the arithmetic inline in the test body and could
    not have caught the scheduler getting it wrong.
18. **RFC 9218 §7.1 PRIORITY_UPDATE is parsed and honoured.** Frame type `0x10`
    is recognised at the parser (`parser.rs` `FrameType::PriorityUpdate` +
    `priority_update_frame`) rather than swallowed as `FrameType::Unknown`.
    `frame_header` enforces `stream_id == 0` at receipt (RFC 9218 §7.1), and the
    handler (`h2.rs` `handle_priority_update_frame`) rejects a
    prioritized-stream-id of `0` with GOAWAY(PROTOCOL_ERROR). Valid frames are
    decoded via `pkawa::parse_rfc9218_priority` and pushed through the same
    memory-guarded path as standalone PRIORITY frames
    (`Prioriser::push_priority_guarded`), so a flood of PRIORITY_UPDATEs for
    far-future stream IDs cannot pin more than `MAX_PRIORITIES` entries. Updates
    for streams that are no longer open and outside the idle look-ahead are
    silently dropped. Guarded by e2e tests
    `test_h2_priority_update_on_open_stream_is_accepted` and
    `test_h2_priority_update_on_stream_zero_is_protocol_error`.
19. **Converter close-race: no yield between last DATA and closing Flags.** The
    H2 converter's DATA arm (`converter.rs` `H2BlockConverter::call`) peeks
    `kawa.blocks.front()` and suppresses the RFC 9218 incremental-mode yield
    when the next queued block is a closing `Block::Flags` with
    `end_stream=true`. Yielding between the last DATA and its companion
    END_STREAM marker would strand the closing frame in `kawa.blocks` — the
    invariant-16 guard in `finalize_write` already prevents the wake-up being
    lost, but forcing a round-trip through the event loop for the terminal
    9-byte empty-DATA frame is wasted work. The suppression is scoped narrowly:
    intermediate Flags blocks (e.g. chunked `end_chunk` without `end_stream`)
    still yield, and a trailing `Block::Chunk` also yields. Unit-tested in
    `converter::tests::test_converter_suppresses_yield_before_closing_end_stream_flags`
    / `test_converter_yields_before_trailing_flags_without_end_stream` /
    `test_converter_yields_before_chunk_block`; end-to-end guard
    `test_h2_incremental_round_robin_closes_every_stream`.
20. **Only `Mux` samples the clock.** No code under `ConnectionH2` calls
    `Instant::now()` or `.elapsed()`; every time-based decision reads
    `ConnectionH2.now`, and `H2FloodDetector` takes `now` as a parameter (§7.5).
    The one sanctioned exception is `ConnectionH2::new`, which takes a single
    sample because a connection is constructed at accept / backend-connect time,
    outside any pass — it seeds `now`, `refuse_window_start` and the flood
    detector's `window_start` from that one value. The reviewer check is
    `grep -nE 'Instant::now|SystemTime::now|\.elapsed\(\)' lib/src/protocol/mux/h2.rs`.
    Every hit must be inside `#[cfg(test)] mod tests` (`h2.rs:6170` onward) or
    that one constructor — there is no third carve-out any more.
    `h2_flood_detector.rs` (extracted from `h2.rs`; not covered by the
    `h2.rs`-scoped grep above) used to hold exactly that third exception —
    `impl Default for H2FloodDetector` sampled the clock for test convenience,
    documented here as test-only. The extraction removed it outright rather
    than moving it: every former `H2FloodDetector::default()` call site now
    reads `H2FloodDetector::new(H2FloodConfig::default(), Instant::now())`,
    pushing the same sample to the call site instead of hiding it behind a
    second constructor, and `h2_flood_detector.rs`'s own
    `#[cfg(test)] mod tests` is where its clock reads live now — the same
    "hits confined to `mod tests` or one constructor" shape this invariant
    already required of `h2.rs`. The exact form matters:
    parentheses are dropped after `now` so a bare function reference matches —
    `.or_insert_with(Instant::now)` is a real clock read that
    `grep 'Instant::now()'` does NOT find, and it was one of the 21 sites this
    changeset converted. `SystemTime` is included because it is a clock too,
    and `-n` rather than `-c` because the criterion is about WHERE each hit
    lives, which a bare count cannot show. One missed call site leaves a dual
    clock, which is invisible in production because both clocks are correct.

    A grep alone is a weak guard, so the property is also pinned by tests that
    advance the snapshot WITHOUT advancing the real clock and observe the
    decision move:
    `graceful_shutdown_deadline_is_evaluated_against_the_connection_snapshot`,
    `backpressure_window_rolls_over_on_the_connection_snapshot`,
    `settings_ack_deadline_is_evaluated_against_the_connection_snapshot`, and
    `per_stream_liveness_reaping_is_evaluated_against_the_connection_snapshot`
    (which also covers the `cancel_timed_out_streams` entry-point mirror).
    `Mux::shutting_down`'s own sampling point is pinned at the `Mux` level by
    `shutting_down_refreshes_the_snapshot_so_the_drain_budget_expires`, because
    no `ConnectionH2`-level test enters that handler. The flood-window
    half-decay is pinned on an injected instant rather than a sleep in
    `test_flood_detector_half_decay_on_window_expiry`.

21. **A delivered wheel entry is consumed, re-validated and put back as one
    operation.** `Mux::timeout` never acts on a delivery without passing it
    through `consume_timer_entry` (`mod.rs`), which calls
    `TimeoutContainer::triggered`, compares the recorded
    `TimeoutContainer::deadline()` against `context.now`, and on an early
    delivery re-arms with `set_at` at the SAME absolute deadline before
    returning `StateResult::Continue`. Dropping any one of the three is a
    distinct bug: no re-validation closes a session up to a full tick minus a
    millisecond (99 ms at the default tick) before its configured timeout;
    re-arming with `set` silently doubles that timeout; not re-arming is a LOST
    WAKEUP, because the wheel entry is already gone (§7.6). Pinned by
    `an_early_wheel_delivery_does_not_run_the_timeout_body`,
    `an_early_wheel_delivery_leaves_a_live_entry_at_the_same_deadline`,
    `a_delivery_past_the_deadline_runs_the_timeout_body` and
    `a_firing_with_no_deadline_is_treated_as_due` in `mod.rs`, and at the
    container level by
    `timeout_container_mirrors_the_deadline_of_its_armed_entry` in `timer.rs`.
    Coverage note: the early-delivery half is exercised on the FRONTEND token
    only; the backend tests park an already-elapsed deadline. The gate is
    shared — `consume_timer_entry` runs before the branch split — so the
    behaviour is common, but no test drives an early backend delivery.

22. **A timed-out backend holds no linked stream when the shared tail runs.**
    The backend-token branch of `Mux::timeout` re-arms the backend container
    only when `!should_close`, yet the shared tail below it can still return
    `Continue` — through the `should_write` writable loop, or through
    `delay_close_for_frontend_flush("timeout")` — with only the FRONTEND
    container re-armed. That is safe only because the branch calls
    `Connection::end_stream` for every id in `context.backend_streams[&token]`,
    unconditionally at the bottom of the loop. `ConnectionH2::end_stream` then
    begins with `Context::unlink_stream`; `ConnectionH1::end_stream` does NOT —
    it guards `self.stream != Some(stream)` and returns early first. That guard
    cannot reject a stream this loop passes it, because an H1 connection carries
    at most one linked stream and it equals `self.stream`: `self.stream =
    Some(..)` occurs only in `start_stream` (paired with `link_stream`) and
    `self.stream = None` only inside `end_stream`, after the unlink. That
    one-active-stream invariant is asserted, not merely written down:
    `debug_assert_h1_owns_stream` runs immediately before every `end_stream`
    call in `Mux::timeout`, in every debug / test / e2e build. Invariant 2 pins
    the other direction. `Context::unlink_stream` is the eviction point
    THIS path relies on, not the only one in the module: `remove_backend_stream`
    also has direct callers in `h1.rs` and `h2.rs`, and extra eviction only
    strengthens the conclusion. Work attached to the backend afterwards does
    not depend on this site either: a pooled backend stays in
    `router.backends`, so `Mux::reschedule` keeps its handle armed at whatever
    deadline the core holds, on every pass. (`arm_timeout()` at the top of
    `ConnectionH1::{readable,writable}` and in
    `ConnectionH2::{write_streams,handle_headers_frame}` pushes that deadline
    out on the first pass, but nothing hangs on it.) That is what covers the
    pool-reuse branch of `Router::connect` never arming a timeout, unlike the
    fresh-dial branch. Asserted under `debug_assertions` at the
    re-arm site and pinned by
    `a_backend_timeout_leaves_no_linked_stream_behind_on_the_close_path` and
    `a_completed_backend_response_timeout_unlinks_through_end_stream_alone`.
    Both tests set `h1.stream = Some(0)` by hand, so they exercise
    `ConnectionH1::end_stream`'s matched path only; the mismatch arm is covered
    by the one-active-stream invariant above, not by a test.

23. **Only the `Mux` adapter touches the timer wheel.** No code under
    `ConnectionH1` or `ConnectionH2` references `crate::timer`; the cores carry
    `timeout_duration` + `timeout_deadline` and publish the latter through
    `poll_timeout()`. Every `TimeoutContainer` lives in `Mux.timeouts`, is
    reconciled by `Mux::reschedule` (memoized on handle-vs-core), and is
    released by that function's `retain` when its token leaves
    `router.backends`. The reviewer check is
    `grep -n 'timer::\|TimeoutContainer' lib/src/protocol/mux/h1.rs lib/src/protocol/mux/h2.rs lib/src/protocol/mux/router.rs lib/src/protocol/mux/connection.rs`.
    Every hit must be inside a comment — `///` on the replacement accessors and
    a plain `//` on the constructor note in `h2.rs` — and none may be code. The
    reconciliation is placed
    in wrappers around `ready_inner` / `timeout_inner` / `shutting_down_inner`
    precisely so it cannot be forgotten on an early return (§7.7). Pinned by
    `a_departed_backend_releases_its_wheel_handle`,
    `a_real_expiry_clears_the_elapsed_deadline_instead_of_re_arming_it` and
    `Mux::debug_assert_timer_coherence`, which runs after every reschedule in
    debug builds — including every e2e and simulation run.

24. **HEADERS+CONTINUATION reassembly is a dedicated, owned accumulator.
    The ACCUMULATED, multi-frame history of a block is now unrepresentable
    to write-side control-frame code — not merely guarded — but two
    narrower hazards survived the first version of this design and needed
    fixing in review before it could land.** Until the accumulator-
    extraction step below, `self.zero` (`h2.rs`) played BOTH the read-side
    accumulator a HEADERS/CONTINUATION field block was reassembled into —
    `headers.header_block_fragment` a `(start, len)` window over
    `self.zero.storage` that had to survive multiple `readable()` passes
    until `END_HEADERS` (§2.2) — AND the write-side scratch buffer every
    control-frame flush (WINDOW_UPDATE, RST_STREAM, GOAWAY) clears and
    reuses. That single shared buffer is not what ships today; it is kept
    below as provenance, because four separate bugs shipped from the
    unnamed hazard before it was closed stage by stage:

    - **sozu-proxy/sozu#1396** (merge `42ac35bfcb52`) — a refused stream's
      field block was dropped without decoding it into the connection-level
      HPACK decoder (the decoder is connection-scoped, not stream-scoped —
      §2.2), permanently desyncing it from the peer's encoder.
    - **sozu-proxy/sozu#1397** (merge `610eaa048ead`) — an unrelated
      WINDOW_UPDATE/RST_STREAM flush inside `flush_pending_control_frames`
      cleared `zero.storage` mid-reassembly, silently corrupting the
      accumulating header-block window. The bytes still decode (HPACK carries
      no self-check), so the failure mode is wrong header *values*, not a
      crash or a decode error — no adversarial peer required, an ordinary
      connection-level WINDOW_UPDATE housekeeping flush was enough.
    - **sozu-proxy/sozu#1401** (`e0e74097f547`) — the same class in
      `graceful_goaway`, firing unconditionally on every drain landing
      mid-reassembly. An operator-triggered soft-stop (hot reload,
      `Mux::shutting_down`) was enough to trip it every time, not just under
      adversarial timing.
    - **sozu-proxy/sozu#1423** — `flush_pending_control_frames`'s
      `frontend_hung_up_while_draining()` stage, ahead of every other
      guarded stage, cleared `zero.storage` with no guard at all. Found
      while landing the #1401 fix, deliberately left for its own changeset
      (this one, resolved below) rather than patched with a fourth ad hoc
      guard.

    **The design (this step, sozu-proxy/sozu#1425 successor).** The
    reassembly accumulator is now [`HeaderBlockAccumulator`](h2_header_reassembly.rs)
    — `ConnectionH2::header_reassembly`, a private field with a real owned
    `Vec<u8>` lifetime, encapsulated the same way
    [`H2DrainState`](h2_drain.rs), [`H2FlowControl`](h2_flow_control.rs),
    [`H2StreamTable`](h2_stream_table.rs) and
    [`H2FloodDetector`](h2_flood_detector.rs) already are. `self.zero`
    itself is now purely control-frame write scratch plus the transient read
    landing zone for whichever ONE stream-0 frame is currently being read —
    it never holds the accumulated history of a multi-frame block across a
    pass boundary:
    - The initiating HEADERS frame's own fragment is copied out of
      `zero.storage` into the accumulator (`HeaderBlockAccumulator::begin`)
      and `zero.storage` is cleared, in the same step that transitions to
      `H2State::ContinuationHeader` (`ConnectionH2::handle_headers_frame`).
    - Each CONTINUATION frame's payload is folded in
      (`HeaderBlockAccumulator::append`) and `zero.storage` is cleared again,
      in the same step that transitions out of `H2State::ContinuationFrame`
      (the match arm in `ConnectionH2::readable`) — before `handle_frame`
      is even re-entered.
    - The block is retired via `HeaderBlockAccumulator::finish`, either
      decoded (`handle_headers_frame`'s `END_HEADERS` branch) or handed
      unread to `DiscardedFieldBlock::Continuation` on a CVE-2024-27316
      refusal (`handle_continuation_header_state`) — the single exit point
      matching the single entry point `begin`. **Every early-return path in
      `handle_headers_frame` between the `!end_headers` check and the
      normal `finish()` call must retire an in-progress accumulator before
      returning, or `is_in_progress()` leaks `true` into the NEXT HEADERS
      frame processed on this connection** — see the review finding below;
      this is a real, load-bearing requirement, not a decorative note.
    - The old `self.zero.storage.end -= 9` trick — which kept a
      CONTINUATION frame's incoming header from disturbing the
      already-accumulated fragment's *physical position* inside
      `zero.storage`, so the next payload read would land contiguously
      after it — no longer has anything to stay contiguous with, and is
      gone; every zero-stream frame (HEADERS included) now frees
      `zero.storage` the same way every other frame type already did.

    Because no write-side code holds a field named `header_reassembly`,
    `flush_pending_control_frames` and `graceful_goaway` cannot reach the
    ACCUMULATED bytes of a reassembly in progress: not one write-side site
    names the field, in any build profile. That closes the #1396/#1397/#1401
    class outright — the same clobber can no longer happen the same way.

    **Two hazards this design did NOT automatically close, both found and
    fixed by review of this changeset's first version (`e1c3c2fb`) before
    it landed:**

    - **The accumulator can still leak `is_in_progress() == true` across
      streams if a READ-side early return skips retiring it.**
      `handle_headers_frame`'s RFC 9113 §5.3.1 PRIORITY self-dependency
      branch (`reset_stream` + `remove_dead_stream`, then `return`) used to
      do exactly that: when the aborted stream's block had gone through
      CONTINUATION reassembly, the flag stayed `true`, and the NEXT HEADERS
      frame on the connection — possibly an entirely different, well-formed
      stream — had its own `!self.header_reassembly.is_in_progress()`
      fast-path check see the stale flag as "still reassembling" and decode
      the PREVIOUS stream's stale, already-discarded bytes instead of its
      own. No `debug_assert!` fired: `HeaderBlockAccumulator::begin`'s own
      assertion only runs on a call that happens, and the bug is precisely
      that the call — the seeding `begin()` for the new stream's fragment —
      was *skipped*. Fixed by retiring the accumulator on that early return
      (and every such return); a `debug_assert!(!self.header_reassembly
      .is_in_progress())` was also added to the fast-path branch as a
      general invariant check, though — being inside the branch that is
      only reached when the flag already reads `false` — it could not by
      itself have caught this specific leak, which took the OTHER branch
      instead. Pinned by
      `a_priority_self_dependency_reset_does_not_leak_the_reassembly_accumulator`
      (`h2.rs`), red on `e1c3c2fb` (an unrelated stream 3 decoded from
      stream 1's stale 22-byte fragment, `context.path` corrupted) and
      green after. This is a debug/test-time and human-review discipline,
      not a compile-time guarantee: `begin`/`append`/`finish` are
      `pub(super)` methods on a private field of the very struct whose
      methods must call them correctly — nothing in the type system stops
      a future early return from repeating this mistake, and their
      `debug_assert!`s compile out entirely in release.
    - **`flush_pending_control_frames`'s frontend-hung-up-while-draining
      stage's justification for staying unguarded was factually wrong.**
      The first version of this step reasoned that `Ready::HUP` means "no
      further bytes can ever arrive on this socket", so the only thing that
      stage could still clobber — a single CONTINUATION frame's own
      not-yet-fully-read payload — could never have completed into a
      decodable block regardless. That reasoning does not hold:
      `Ready::HUP` is `is_read_closed() || is_write_closed()`
      (`command/src/ready.rs`), and mio's own documentation states
      `is_read_closed()` is true not only on a full close but also "the
      peer stream has shutdown the write half of its socket" — a TCP
      half-close, where data the peer already sent can still be sitting,
      unread, in the kernel receive queue. `drive_frontend_shutdown_io`
      (`mod.rs`) force-calls `readable()` for H2 on every `shutting_down()`
      poll, so a CONTINUATION frame split across TCP segments landing a HUP
      event alongside its first segment is the ordinary soft-stop path, not
      a contrived corner case. This stage now carries the same
      `header_block_reassembly_in_progress()` guard as its three siblings.
      Pinned by
      `a_continuation_frame_split_by_tcp_segmentation_survives_a_hup_while_draining`
      (`h2.rs`), red on `e1c3c2fb` with the exact
      `StringDecodingError(NotEnoughOctets)` / decoder-desync failure shape
      #1397/#1401 already showed, green after.

    Both findings mean **#1423 is closed by this changeset, but not by the
    mechanism originally claimed** ("resolved without adding a guard"): the
    frontend-hung-up-while-draining stage DOES now carry a guard, matching
    its siblings, because the argument for leaving it unguarded turned out
    to be wrong. What genuinely IS true, and is the real structural
    contribution of this step, is that the ACCUMULATED multi-frame history
    of a block — the thing #1396/#1397/#1401 actually clobbered — is
    unrepresentable to write-side code by construction; #1423's narrower
    residual (a single in-flight frame's partial bytes) needed, and now
    has, the ordinary guard.

    **All six `header_block_reassembly_in_progress()` call sites are
    load-bearing.** `ConnectionH2::header_block_reassembly_in_progress()`
    (true while `self.state` is `ContinuationHeader`/`ContinuationFrame`) is
    checked before the frontend-hung-up-while-draining, WINDOW_UPDATE-drain
    and RST_STREAM-drain stages and the deferred-initial-GOAWAY-readiness
    check of `flush_pending_control_frames`, and inside
    `ConnectionH2::graceful_goaway` itself before it decides whether to
    defer or send immediately (via `h2_drain::H2DrainState::begin_graceful_drain`'s
    `reassembly_in_progress` parameter and
    `H2DrainState::take_deferred_initial_goaway`'s `ready_to_flush`
    parameter — `h2_drain.rs` has neither `self.zero` nor `H2State` to
    compute the guard itself), and inside `ConnectionH2::flush_zero_buffer`
    as a no-op guard. What all six protect is the same narrow window: a
    single CONTINUATION frame's payload can be *mid-flight* in
    `zero.storage` — a `socket_read()` that has only partially filled this
    one frame — when a write pass runs in the same event-loop sweep
    (`mod.rs`'s inner loop dispatches frontend `readable()` then
    `writable()` together, §2.2). On an ordinary, healthy, still-connected
    session more CONTINUATION bytes genuinely are still coming, so losing
    that in-flight chunk would still truncate or corrupt an otherwise-
    completable block — and, per the finding above, "the frontend already
    hung up" does not reliably rule this out either.
    `h2_drain::H2DrainState::initial_goaway_pending` stays: it is the flag
    `begin_graceful_drain` sets so a `graceful_goaway` landing
    mid-reassembly defers its advisory GOAWAY, and
    `flush_pending_control_frames` drains it via `take_deferred_initial_goaway`
    once reassembly completes. Pinned by
    `a_legitimate_continuation_survives_an_unrelated_window_update_flush`
    (the WINDOW_UPDATE-stage case, #1397),
    `a_legitimate_continuation_survives_a_graceful_goaway` (the GOAWAY case,
    #1401), `a_legitimate_continuation_survives_a_hup_while_draining` (the
    inter-frame HUP case) and
    `a_continuation_frame_split_by_tcp_segmentation_survives_a_hup_while_draining`
    (the mid-frame HUP case, #1423's actual residual); the RST_STREAM drain
    stage shares the identical guard expression but has no dedicated
    deterministic regression test of its own. A `quickcheck` property,
    `reassembly_property::qc_h2_header_reassembly_survives_interleaved_control_frame_flushes`
    (`h2.rs`), generalises all four deterministic triggers (unrelated
    WINDOW_UPDATE flush, `graceful_goaway`, HUP-while-draining) composed in
    every order and count across a 2..=5-way split block, independently
    choosing per interleave point whether it lands between two frames or
    inside one frame's own TCP-level write (the two shapes are not
    interchangeable — only the latter reaches the narrower guard above; a
    first version of the property that only produced the former could not
    have caught the frontend-hung-up-while-draining finding).
25. **One write pass, one encoder, one HPACK size-update prefix.**
    `ConnectionH2::write_streams` builds an `H2BlockConverter` for exactly ONE
    `kawa.prepare` call, not one per pass, so its `&mut self.hpack` borrow
    never spans the per-stream loop and every `&self` / `&mut self` method
    stays callable inside it. Two properties that the single long-lived
    converter used to hold structurally are now carried across the streams of
    a pass by `converter::H2ConverterPass` and MUST stay true:
    - **One encoder.** Every converter of a pass borrows the SAME
      `loona_hpack::Encoder` through `HpackState::encoder_mut`, so the dynamic
      table is continuous across the pass and a peer replaying the pass's
      blocks in order with one decoder stays in sync. A per-stream encoder
      would emit blocks that are individually decodable and collectively
      desynced.
    - **One size-update prefix.** The RFC 7541 §6.3 dynamic-table-size-update
      goes on the FIRST header block of the pass and on no other.
      `H2ConverterPass::converter` hands the pending value to each converter
      and `H2ConverterPass::reclaim` takes back whatever
      `emit_pending_size_update_if_new_block` left of it — the converter
      `take()`s the value the moment it writes the prefix, so threading it back
      is what stops a second stream re-emitting it. `write_streams` clears
      `ConnectionH2::pending_table_size_update` only when
      `H2ConverterPass::size_update_emitted` reports a block actually carried
      it, so a DATA-only pass keeps the signal queued.

    Neither property is visible to `converter.rs`'s own unit tests, which
    drive one converter over one kawa. Both are pinned on the wire by
    `one_write_pass_prefixes_its_first_header_block_only_and_shares_one_encoder`
    (`h2.rs`), whose last assertion is the negative half — a peer decoder that
    never saw the first block must FAIL on the second — and generalised over
    2..=5 streams and the advertised table size by
    `write_pass_property::qc_one_write_pass_prefixes_its_first_header_block_only`
    (`h2.rs`).

26. **RFC 9218 §4 incremental leadership rotates inside the bucket that
    leads the pass — rotate AND commit, or a stream starves — and no
    further.** Scheduler fairness has two halves and neither is sufficient
    alone. `Prioriser::apply_incremental_rotation` (`h2_scheduler.rs`),
    called from `H2Scheduler::begin_pass`, rotates each urgency bucket's
    incremental tail to start at the first stream id *strictly greater* than
    `Prioriser::incremental_cursor`, wrapping; `H2Scheduler::end_pass` then
    commits that cursor to `ReadyIncrementalCensus::first_incremental_fired`
    — the first incremental stream of the pass that actually **consumed send
    window**. Together they advance leadership exactly one position per pass,
    so K ready incremental peers sharing the leading bucket each lead once
    every K passes and none waits longer than K-1.

    Each half fails differently and both failures are starvation, not a
    slowdown. Rotating without committing re-reads the same cursor forever
    and the smallest id in the bucket leads every pass. Committing without
    rotating leaves the ascending order untouched and does the same. And
    `consumed > 0` is load-bearing in its own right: a permanently
    window-blocked stream that "led" without sending would hand the cursor
    onward every pass while its peers never actually got a turn either.

    **The bound covers ONE bucket per connection, and the limitation is
    real.** `incremental_cursor` is a single connection-global stream id.
    `apply_incremental_rotation` applies it to every bucket's tail, but
    `end_pass` can only ever commit an id drawn from the lowest-numbered
    urgency bucket that had a ready incremental stream fire — the pass order
    is ascending by urgency, so the first stream to consume window comes from
    there. Any other bucket is rotated by a cursor from a foreign id range,
    and when that cursor sits entirely below or entirely above the bucket's
    own ids `partition_point` returns a constant and the bucket never rotates
    at all. Measured: u=0 incremental `{1, 3}` beside u=3 incremental
    `{5, 7}`, all ready and all consuming, six passes — u=0 alternates
    `1, 3, 1, 3, …` while u=3 is `5, 7` in every pass and stream 7 never
    leads. That is positional starvation, and a `FlushOutcome::Stalled` at
    stream 5 turns it into byte starvation for 7, pass after pass.

    This is **not a regression**: `Prioriser` is byte-identical to the commit
    that extracted it, and the field's own doc has always scoped the cursor
    to "the incremental tail of the lowest-urgency bucket that contained at
    least one incremental stream". What this invariant adds is the name and
    the boundary, so no later change inherits the wider reading. Per-bucket
    rotation means `incremental_cursor: [StreamId; 8]`, a per-bucket leader
    in `ReadyIncrementalCensus` (`note_fired` is not given the urgency
    today), and a matching `end_pass`; it changes wire ordering on
    multi-bucket connections and needs its own changeset with its own e2e
    evidence.

    The census of invariant 17 must stay bucket-scoped for an unrelated
    reason: a connection-global peer count starves nothing, but it makes a
    solo incremental stream yield to a peer it can never interleave with,
    which is the invariant-15 strand.

    Pinned deterministically by `incremental_leadership_rotates_one_position_per_pass`
    (four peers, eight passes, the exact leader sequence),
    `no_incremental_peer_is_starved_over_a_full_cycle` (five peers, ten
    passes, each leading exactly twice — deliberately five rather than two,
    because a rotation bug that swaps a pair still looks fair on two streams
    and starves the fifth), `a_window_blocked_stream_does_not_take_the_lead`
    and `a_pass_with_no_incremental_progress_leaves_the_cursor_alone`; and
    generalised over 2..=8 peers, their ids and gaps, the shared urgency
    bucket, 1..=4 full cycles and a distractor set by
    `fairness_property::qc_incremental_leadership_visits_every_peer_once_per_cycle`,
    whose distractors sit in strictly lower-priority buckets precisely so its
    main bucket is always the leader — the scope this invariant claims. The
    boundary itself is pinned by
    `the_round_robin_cursor_is_connection_global_so_only_the_leading_bucket_rotates`,
    which asserts the frozen trailing bucket as observed behaviour. All of
    them are in `h2_scheduler.rs`; each carries a `TO SEE THIS RED` recipe
    naming the exact statement to delete and the panic it produces, except
    the last, which has no one-line red and says so.

    The composition is what this step's tests add. At the parent commit,
    deleting the `advance_incremental_cursor` call site from
    `ConnectionH2::write_streams` leaves the entire `protocol::mux` suite
    green — 402 passed, 0 failed. The rotation primitive was pinned; the
    rotate-and-commit pair was not.

---

## 10. Pointers for First Steps

If you are fixing a bug in this module:

- **Panic on `context.streams[gid]`** — start at §5, then walk every
  stream-removal site listed in §5.4. The fix is usually a missed invalidation
  of `expect_write`/`expect_read`.
- **Hung session** — check §7. Verify the timer in question is being reset only
  on application activity, not on control frames.
- **Doubled metrics / gauge drift** — check §9 item 14 and the backend-stream
  accounting paths in `mod.rs:1299-1300` and `mod.rs:1633-1634`.
- **Truncated response / TLS decode error** — check `has_pending_write`
  (`h2.rs`), `delay_close_for_frontend_flush` (`mod.rs`), and the TLS
  drain logic in the `Position::Server` arm of `ConnectionH2::close`
  (`h2.rs:5725-5823`; the range start is one of eight identical
  `Position::Server => {` lines, hence the symbol).
- **Stream count underflows `max_concurrent_streams`** — see
  `prune_inactive_streams_while_closing` (`h2.rs`) and make sure every
  removal increments nothing and decrements what it should.

Last revision date of the surviving line anchors: 2026-09-22, against
`refactor/h2-header-reassembly` (this step, based on `refactor/h2-goaway-drain`
PR #1425 at `f76c8eb3`) after the review of its first version (`e1c3c2fb`)
re-anchored the citations this changeset's own edits shifted, plus several
that had already drifted from earlier changesets. Keep this line _actually_
current when you touch the file — greppable as "Last revision date".

---

## 11. Close-path log severity tiers

The TLS-drain warning at `ConnectionH2::close` (`h2.rs:5762-5822`, `Position::Server`
arm) fires when `socket_wants_write()` is still true after `MAX_DRAIN_ROUNDS`
empty `socket_write_vectored(&[])` calls. The severity is tiered along
**stream-count + close-state**, not peer-vs-operator. The tier is intentionally
orthogonal to — and composes with — the send-side `H2Error`-variant tier in
`goaway()` (`h2.rs:4016-4020`); both rules demote benign paths and keep
loss-bearing paths loud.

| Stream count   | `H2State`           | Severity | Rationale                                                                                                                                                                                                                                                                         |
| -------------- | ------------------- | -------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `streams != 0` | any                 | `error!` | Live streams at close time. Response bytes may have been queued in the TLS write buffer and stranded by the kernel RST. Real data loss is possible.                                                                                                                               |
| `streams == 0` | `GoAway` or `Error` | `warn!`  | Idle close after a GOAWAY exchange (peer-initiated abort or our own graceful drain). What is stranded is best-effort GOAWAY/close_notify; no application data was queued. Covers both the production HAProxy-chain RST race and operator soft-stop on an already-idle connection. |
| `streams == 0` | any other state     | `error!` | Idle close from an unexpected state (no GOAWAY exchange, e.g. `Header` or `Settings`). Worth keeping loud so unknown teardown paths surface.                                                                                                                                      |

The format string is prefixed with `"{}"` and `log_context!(self)` so the line
carries the canonical `MUX-H2 Session(...)` envelope. This matches every other
log site on `ConnectionH2`, which is required for operators to grep-correlate
against the preceding `WARN MUX-H2` GOAWAY-receipt line for the same session.

Regression coverage:

- `e2e/src/tests/h2_correctness_tests.rs::test_h2_peer_goaway_protocol_error_then_rst_clean_drain`
  — peer abort with stream count zero (the production scenario; new `warn!`
  path).
- `test_h2_peer_goaway_no_error_clean_close` — happy-abort baseline.
- `test_h2_peer_goaway_during_response_body` — in-flight stream at close time
  (`error!` path; asserts the stream is torn down).
- `lib/tests/log_layout.rs` — static check that this site uses
  `log_context!(self)` and that no protocol/runtime log call drifts away from
  the canonical envelope.
