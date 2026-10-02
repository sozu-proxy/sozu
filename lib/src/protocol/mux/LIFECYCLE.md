# H2 Mux — Session Workflow and Stream Lifecycle

Reference document for maintainers of the HTTP/1.1 + HTTP/2 multiplexing layer
under `lib/src/protocol/mux/`.

Every claim is anchored to code. Where the prose names an item — a function, a
method, a struct, an enum — the anchor is that item plus its file, with no line
number, because a line number does not survive an edit above it. A `file.rs:LINE`
or `file.rs:LINE-LINE` anchor is kept only where the claim is about a specific
statement or branch inside an item; those were refreshed against `main` at
`0cb1e2a7` on 2026-09-20. A second site in the same file may continue from the
first as a bare `` `:LINE` `` on the SAME line, which this document does once;
see doc/README.md#continuing-a-citation-without-repeating-the-path.

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
| `h2_close.rs`                 | `lib/src/protocol/mux/h2_close.rs` | Close and write-pass finalization decisions under TLS backpressure |
| `h2_control_tx.rs`            | `lib/src/protocol/mux/h2_control_tx.rs` | Proxy-emitted `RST_STREAM` queue and its lifetime cap |
| `h2_drain.rs`                 | `lib/src/protocol/mux/h2_drain.rs` | RFC 9113 §6.8 double-GOAWAY drain state and its transitions |
| `h2_flood_detector.rs`        | `lib/src/protocol/mux/h2_flood_detector.rs` | CVE-2023-44487 / CVE-2024-27316 / CVE-2025-8671 mitigations, frame rate limits |
| `h2_flow_control.rs`          | `lib/src/protocol/mux/h2_flow_control.rs` | Connection-level send window, queued `WINDOW_UPDATE`s; the advertised receive window is not enforced |
| `h2_header_reassembly.rs`     | `lib/src/protocol/mux/h2_header_reassembly.rs` | Owned accumulator for an in-progress HEADERS + CONTINUATION block |
| `h2_output.rs`                | `lib/src/protocol/mux/h2_output.rs` | The connection's ordered output queue: control frames and the adopted rest of a cut stream frame (invariant 28) |
| `h2_stream_table.rs`          | `lib/src/protocol/mux/h2_stream_table.rs` | Wire `StreamId` → `GlobalStreamId` map, RST dedupe, liveness and stall caches |
| `h2_transmit.rs`              | `lib/src/protocol/mux/h2_transmit.rs` | Vectored `gather`/`confirm` of one stream's pending output, behind the output queue (`gather_after`), and `frame_tail` |
| `h2_write_pass.rs`            | `lib/src/protocol/mux/h2_write_pass.rs` | Owned state and phase of one `write_streams` pass |
| `hpack/`                      | `lib/src/protocol/mux/hpack/mod.rs` | Sans-io RFC 7541 codec: decoder, encoder, dynamic table, `const` Huffman machine |
| `hpack_state.rs`              | `lib/src/protocol/mux/hpack_state.rs` | RFC 7541 codec pair and its reusable scratch buffers |
| `buffer_source.rs`            | `lib/src/protocol/mux/buffer_source.rs` | Where the core takes every buffer; a refusal is not an error |
| `answers.rs`                  | `lib/src/protocol/mux/answers.rs` | Materialises a configured default answer into a stream's kawa |
| `auth.rs`                     | `lib/src/protocol/mux/auth.rs` | HTTP Basic extraction, SHA-256 canonicalisation, constant-time compare |
| `shared.rs`                   | `lib/src/protocol/mux/shared.rs` | Routines H1 and H2 share, including TLS `close_notify` ordering |
| `debug.rs`                    | `lib/src/protocol/mux/debug.rs` | Bounded per-session event ring; a no-op without `debug_assertions` |

### 1.2 The split between `ConnectionH2` and `Context`

Two orthogonal structs hold H2 session state:

- [`ConnectionH2`] — `lib/src/protocol/mux/h2.rs` —
  **per-connection** wire-level state: HPACK coders, frame-parser state
  (`H2State`), flow control window, flood counters, priority map, and a
  `BTreeMap<StreamId, GlobalStreamId>` wire map — private to
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
        streams: BTreeMap            streams: Vec<Stream>    streams: BTreeMap
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
| `ConnectionH2`                       | `h2.rs`                         | H2 connection state (no socket)                                      |
| `H2Shell<Front>`                     | `h2.rs`                         | `ConnectionH2` + the socket it speaks through                        |
| `ConnectionH1<Front>`                | `h1.rs` (struct `ConnectionH1`) | H1 connection state                                                  |
| `Stream`                             | `stream.rs`                     | Per-request buffers + HTTP context                                   |
| `StreamState`                        | `stream.rs`                     | `Idle`/`Link`/`Linked`/`Unlinked`/`Recycle`                          |
| `H2State`                            | `h2.rs`                         | H2 connection-level state machine                                    |
| `H2StreamId`                         | `h2.rs`                         | `Zero` or `Other { id, gid }` — used by `expect_read`/`expect_write` |
| `StreamId` (alias for `u32`)         | `mod.rs`                        | On-the-wire H2 stream identifier                                     |
| `GlobalStreamId` (alias for `usize`) | `mod.rs`                        | Index into `context.streams`                                         |
| `H2FlowControl`                      | `h2_flow_control.rs`            | Connection-level send window + receive-side byte accounting          |
| `H2DrainState`                       | `h2_drain.rs`                   | Graceful-shutdown bookkeeping                                        |
| `H2ByteAccounting`                   | `h2.rs`                         | Overhead byte attribution                                            |
| `H2ConnectionConfig`                 | `h2.rs`                         | Per-listener tuning                                                  |
| `H2FloodConfig`                      | `h2_flood_detector.rs`          | CVE-mitigation thresholds                                            |
| `H2FloodViolation`                   | `h2_flood_detector.rs`          | A tripped threshold's (reason, count, threshold)                     |
| `H2FloodDetector`                    | `h2_flood_detector.rs`          | CVE-mitigation rate/lifetime counters                                |

`H2FlowControl`'s row says "receive-side byte accounting" and not "receive
window" on purpose. Sōzu advertises a connection-level receive window — RFC
9113 §6.9.2's fixed 65535 octets plus every stream-0 `WINDOW_UPDATE` it sends,
which is how `H2ConnectionConfig::initial_connection_window` reaches the peer —
and then enforces nothing against it. `H2FlowControl::window` is the SEND
window, peer-granted credit for our own writes, and
`H2FlowControl::account_received_bytes` is a counter whose only job is deciding
when to hand credit back: no state is decremented by an inbound DATA frame, so
none can go negative and no connection-level `FLOW_CONTROL_ERROR` is raised.
Measured: 106496 octets of DATA accepted against an advertised 98303, with no
GOAWAY (sozu-proxy/sozu#1488). The real boundary is memory, bounded by the
buffer pool: a `BufferSource` refusal degrades one stream with
`RST_STREAM(REFUSED_STREAM)` and never the connection — `buffer_source.rs`'s
module doc is that contract, and §8.2's `refuse_stream_and_discard` is where
the H2 core spends it. Unsoftened: a peer that trusts the advertisement has no
way to discover the real limit, and RFC 9113 §6.9.1 makes enforcement a MUST,
so a conformance suite will flag this as a §6.9.1 violation. The decision was
to document the gap rather than close it, so do not describe this window as
enforced. The full statement lives in `h2_flow_control.rs`'s module doc and in
doc/h2_mux_internals.md's "The advertised connection-level receive window is
not enforced".

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
4. A read that comes back SHORT of that request re-arms `expect_read` with the
   remainder and runs an early guard, so a client that is plainly not speaking
   H2 is dropped (`ConnectionH2::force_disconnect`, logged as `EARLY INVALID
   PREFACE`) without waiting for the whole window. The guard compares only the
   octets the magic string can cover — `serializer::H2_PRI` is 24 octets while
   the request is `CLIENT_PREFACE_SIZE`, so between the two lies a window the
   guard must accept rather than refuse. Nothing about a TCP segment or a TLS
   record makes a read land on the 24-octet boundary, and a byte-perfect
   preface split anywhere inside the request is a conforming client:
   `a_byte_perfect_client_preface_survives_every_read_fragmentation` sweeps
   every fixed chunk size in `1..=40` (not every partition of the window),
   `an_invalid_client_preface_is_still_refused_before_the_window_is_filled`
   holds the other side.

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

- On a backend (`Position::Client`) the same names run the other way: the
  `ClientPreface` writable arm queues the preface and SETTINGS, which must be
  the first bytes on the wire (RFC 9113 §3.4, debug-asserted: nothing is queued
  before them), `ClientSettings` flushes them and turns READABLE on, and
  `ServerSettings` waits for the server's SETTINGS. A write readiness in
  `ServerSettings` is withdrawn, not an error: `ConnectionH2::start_stream`
  arms one for every stream the router links, and `Router::plan_connect` links onto
  a `Connected` connection whatever its handshake state; the SETTINGS ACK the
  read then queues re-arms it (sozu-proxy/sozu#1631,
  `a_backend_stream_ended_during_the_handshake_keeps_the_connection`,
  `a_request_linked_during_the_handshake_goes_out_after_the_settings`).
- `ClientPreface` → `ClientSettings` in the `(H2State::ClientPreface,
  Position::Server)` arm of `ConnectionH2::handle_read` (`h2.rs`), once the
  client preface parses.
- `ClientSettings` → `ServerSettings` in the `H2State::ClientSettings` arm of
  `ConnectionH2::handle_read` (`h2.rs`), right after the SETTINGS frame is
  serialized. By symbol, not line: `self.state = H2State::ServerSettings;` is
  one of two identical lines in the file (the other is in `writable`), and the
  line number this bullet would otherwise carry is one the bullet above it
  used at an earlier revision — a collision the drift rule cannot tell from
  staleness, because it keys on the citation and not on where it sits. Do not
  convert it back.
- `Discard` (stream refused, a frame dropped for a stream reset by a stream
  error or earlier by us — RFC 9113 §5.1 —, or an orphaned DATA remainder
  skipped — see §5.4) is set in `refuse_stream_and_discard`,
  `discard_field_block` or `skip_orphaned_data_payload` (`h2.rs`) and exited by the `H2State::Discard` arm of `ConnectionH2::handle_read`
  (`lib/src/protocol/mux/h2.rs`). By symbol, not line, for the same reason as
  the bullet above: `self.expect_header();` is one of four identical lines in
  the file, and the line number this bullet would otherwise carry is one the
  `ClientPreface` bullet above used at an earlier revision — a collision the
  drift rule cannot tell from staleness, because it keys on the citation text
  rather than on where it sits (sozu-proxy/sozu#1447). Do not convert it back.
- `Continuation*` states handle multi-frame HEADERS per RFC 9113 §4.3 — enter in
  `ConnectionH2::handle_headers_frame` (`h2.rs`).
- **Discard does not skip HPACK.** HPACK field-compression state is scoped to
  the *connection* (RFC 9113 §4.3), not the stream, so the bytes `Discard`
  drops on a refused stream are still a field block the peer's encoder has
  already applied to its own dynamic table. Before `H2State::Discard`'s
  `handle_read()` arm clears `zero.storage`, it decodes that field block into
  `ConnectionH2::decoder` (result discarded, callback is a no-op) via the
  free function `decode_discarded_field_block`, using the
  `ConnectionH2::discarded_field_block` value `discard_field_block`
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
  (`flush_pending_control_frames`) serialises the RST_STREAM before the next
  `readable()` pass reads this frame's own payload, into the separate output
  queue, which shares nothing with `zero` (#1604) — see invariants 24 and 28. When END_HEADERS was not set on the refused frame, decoding
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
  CONSTRUCTION rather than merely guarded. The narrower window that
  remained — `zero.storage` holding a single CONTINUATION frame's own
  not-yet-fully-read payload (a partial `socket_read()`) while
  `flush_pending_control_frames`'s WINDOW_UPDATE and RST_STREAM stages
  reused it as write scratch, which `Mux::ready`'s readable-then-writable
  sweep made an ordinary case, not an attack — is closed by construction
  too since #1604: control frames are serialised into the separate output
  queue (invariant 28), `zero` is input only, and those stages no longer
  consult `ConnectionH2::header_block_reassembly_in_progress` nor defer.
- **`graceful_goaway` sends its advisory GOAWAY even mid-block.** Before
  #1604 its first GOAWAY cleared and reused `zero.storage` to serialize
  `GOAWAY(NO_ERROR, last_stream_id=MAX)` — the third instance of this bug
  class (#1401) — so `H2DrainState::begin_graceful_drain` (`h2_drain.rs`)
  deferred it while a block was in progress, through a `DeferInitial`
  decision and an `initial_goaway_pending` flag that
  `flush_pending_control_frames` drained once the block completed. Since
  #1604 the GOAWAY is queued in the separate output queue and can clobber
  nothing, and #1637 removed the deferral: it protected no buffer, and a
  drain landing while the only stream was mid-block let
  `Mux::shutting_down_inner` (`mod.rs`) close the session with nothing
  queued — the peer saw no GOAWAY at all, against RFC 9113 §6.8. RFC 9113
  §4.3/§6.10 constrain the block's sender, not a GOAWAY travelling the other
  way. `begin_graceful_drain` now takes only `now`, marks the connection
  draining, arms the forced-close budget (§9.3 below) and answers
  `SendInitial`; `ConnectionH2::graceful_goaway` queues the frame through
  `ConnectionH2::send_initial_goaway`, and `H2Shell::flush_output_buffer` —
  the direct, `writable()`-bypassing flush `Mux::shutting_down` calls right
  after `graceful_goaway` — sends it in the same pass.
- **A drain waits for a block in progress (#1647).** Sending the advisory
  GOAWAY mid-block was not enough: the block's stream is still
  `StreamState::Idle`, and `Mux::shutting_down_inner` then waited only for
  `Linked` and non-quiesced `Unlinked` streams, so it closed the session as
  soon as the advisory GOAWAY was flushed — the request was lost and no final
  GOAWAY followed. `ConnectionH2::peer_header_block_in_progress` now holds the
  session in either reassembly state, within the forced-close budget
  (invariant 31).

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

- Frontend HUP — detected by the `readiness().event.is_hup()` branch of
  `Mux::ready` (`lib/src/protocol/mux/mod.rs`), subject to
  `delay_close_for_frontend_flush` (`mod.rs`) to avoid truncating TLS. The
  HUP also stands for a client's half-close (`EPOLLRDHUP`, which
  `Ready::from(&Event)` in `command/src/ready.rs` maps to HUP): while
  `Mux::frontend_exchange_in_flight` (`mod.rs`) holds — input is still
  unread (`Connection::has_unread_input`), or a stream whose request was
  received whole is open — the session serves the exchange and closes once
  the response is complete
  ([#1779](https://github.com/sozu-proxy/sozu/issues/1779)). A request left
  incomplete at the client's EOF holds nothing, and a full hang-up is never in
  flight: ERROR, or WRITE_CLOSED (mio's
  `is_write_closed`, `EPOLLHUP` or `EPOLLERR`, which a half-close never
  raises), except on a lingering frontend whose write side sozu shut itself.
  ERROR alone is not enough: a reset whose error sozu's own `read` or `write`
  consumed first is reported as `EPOLLHUP` without `EPOLLERR`. A write to the
  client that fails closes the session too, on H1 (`ConnectionH1::writable`)
  and on H2 (`H2Shell::writable`), and the connection then reports nothing
  left to flush, so the session closes at once
  ([#1792](https://github.com/sozu-proxy/sozu/issues/1792)). Output already
  pending is flushed as a delayed close would, without `close_notify`. A lingering
  frontend counts as unread input from `ConnectionH1::start_linger`, which
  raises READABLE, until its drain reads the EOF. The request reader keeps
  reading an answered stream: `ConnectionH1::readable` never links a request
  whose stream is already `Unlinked`, such as one `Mux::timeout_inner`
  answered 408 while its head was incomplete.
- `MuxResult::CloseSession` from any readable/writable path (the frontend
  readable and writable arms of `Mux::ready_inner`, etc.).
- A loop-iteration budget overrun (`MAX_LOOP_ITERATIONS = 10_000`, `mod.rs`,
  checked in the inner loop of `Mux::ready_inner`).
- Timeout (`Mux::timeout`, `mod.rs`).
- Graceful shutdown initiated by the server (`Mux::shutting_down`,
  `mod.rs`).

### 2.5 Listener configuration: what a request runs on, and for how long

Hot reconfiguration is the reason this proxy exists, so an operator may patch a
listener — `HttpListener::update_config` / `HttpsListener::update_config`
(`lib/src/http.rs`, `lib/src/https.rs`) — at any instant, including while a
connection is mid-request. The mux answers that with a snapshot rather than a
live handle:

> **A request captures the listener configuration it needs when it arrives and
> runs to completion on that capture. A reload landing afterwards does not
> reach it. The next request picks the reload up. The staleness window is
> therefore exactly one request** — stated here rather than left to emerge from
> wherever the next `listener.borrow()` happens to sit (sozu-proxy/sozu#1340,
> Question 7).

Neither extreme in that question is what the mux does. Freezing the listener
into the session at construction would be the simplest core and would break hot
reconfiguration: a long-lived H2 connection would serve every one of its streams
under the configuration that happened to be in force when it was accepted.
Borrowing the listener live on the write, reset and access-log paths — what the
code did before — has the opposite failure: a request already in flight can be
answered under a sticky cookie name, an `X-Real-IP` policy or an error page an
operator installed after it started.

**The single capture point** is `Context::create_stream` (`mod.rs`).
`ConnectionH2::create_stream` (`h2.rs`) calls it, and is itself called from
`ConnectionH2::handle_header_state` (`h2.rs`) as a HEADERS frame opening a new
stream is accepted — so an H2 stream captures at HEADERS. Everything the
capture holds is owned by the request:


| Captured value                    | Lives on                    | Read by |
| --------------------------------- | --------------------------- | ------- |
| `get_sticky_name`                 | `HttpContext::sticky_name`  | the request cookie walk in `HttpContext::on_request_headers` and the `Set-Cookie` edit in `HttpContext::on_response_headers` (`editor.rs`) |

| `get_sozu_id_header`              | `HttpContext::sozu_id_header` | `HttpContext::on_response_headers` (`editor.rs`) |
| `get_strict_sni_binding`          | `HttpContext::strict_sni_binding` | `Router::route_from_request` (`router.rs`) |
| `get_elide_x_real_ip`             | `HttpContext::elide_x_real_ip` | `HttpContext::on_request_headers` (`editor.rs`), `pkawa::handle_trailer` |
| `get_send_x_real_ip`              | `HttpContext::send_x_real_ip` | idem |
| `get_forwarded_headers`           | `HttpContext::forwarded_headers` | `HttpContext::on_request_headers` (`editor.rs`), `apply_request_rewrites_and_headers` (`router.rs`) |
| `get_answers`                     | `Stream::answers` (`stream.rs`) | every `set_default_answer` / `forcefully_terminate_answer` site |

`Stream::answers` is a handle, not a copy — `Template` owns a `kawa::Kawa` and
cannot be cloned — so the snapshot only holds if a reload **publishes** a new
registry instead of rewriting the captured one. It does: `update_config` builds
a fresh `HttpAnswers` and installs it under a new `Rc`, copying the per-cluster
overrides across (`HttpAnswers::cluster_answers` holds `Rc<Template>` for
exactly that copy). Migrating those overrides by `std::mem::take` instead would
empty the registry the in-flight requests are still holding and strip their
custom pages mid-response.

That publication boundary is the listener reload and nothing else.
`HttpProxy::add_cluster` / `HttpProxy::remove_cluster` (`lib/src/http.rs`) and
their HTTPS counterparts still reach `HttpAnswers::add_cluster_answers` /
`HttpAnswers::remove_cluster_answers` through a `borrow_mut` on whatever
registry the listener currently holds, so a cluster-level answer added or
removed without a listener reload is visible to requests already in flight.
Closing that too means publishing on those paths as well; it is not what
Question 7 asked and is left as it was.


**Captured once per connection, not per request**, because no patch can change
them: `Context::protocol` (the listener's HTTP/HTTPS kind, which decides the H2
`:scheme` in `ConnectionH2::begin_scheduler_pass`) and
`Context::h2_stream_shrink_ratio`, which governs the connection's own slot
array. The TLS fields (`Context::tls_server_name`, `Context::tls_cert_names`,
`Context::tls_version`, `Context::tls_cipher`, `Context::tls_alpn`) are
handshake facts and are frozen for the same reason — see §1.2.

**Two listener reads remain live on the datapath, deliberately:**

- `L7ListenerHandler::frontend_from_request`, called from
  `Router::route_from_request` (`router.rs`). Routing is Question 6 of the same
  issue and is still open; a request must in any case be routed against the
  frontends that exist when it is routed, not against a table captured earlier.
- `ListenerHandler::get_tags`, called from `Stream::generate_access_log`
  (`stream.rs`). This is the *fallback* tag source, reached only for a request
  that never completed routing — malformed, unknown host, SNI/authority
  mismatch — because a routed request carries the tags of the frontend rule that
  matched it in `HttpContext::tags`. Resolving it needs the request authority,
  which does not exist yet at the capture point, and snapshotting the listener's
  whole authority-keyed map per request would cost more than the observability
  it buys.

**H1 is a connection, not a stream.** `http.rs` and `https.rs` call
`Context::create_stream` once per H1 connection, and `HttpContext::reset`
(`editor.rs`) deliberately carries the listener-scoped knobs — along with the
session id — across every keep-alive and pipelined request in that slot. The
request id is not carried: `reset` takes a fresh one from
`Context::next_request_id`, so each request of the connection has its own. An H1 connection's staleness window is therefore the connection, not
one request. Narrowing it means giving the H1 keep-alive reset in
`ConnectionH1::end_stream` (`h1.rs`) a re-capture of its own, which is a
separate change.

Pinned by `a_listener_reload_between_two_streams_moves_only_the_second`
(`mod.rs`) for the mux half and
`a_listener_answers_reload_publishes_a_new_registry` (`lib/src/http.rs`) for the
publication half.

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
  to the client. Transitions: `set_default_answer_with_retry_after` and
  `forcefully_terminate_answer` (`answers.rs`), the `StreamState::Unlinked`
  assignments of `ConnectionH1::end_stream` (`h1.rs`), and in
  `h2.rs` the `StreamState::Unlinked` assignments of `ConnectionH2::reset_stream`
  and `ConnectionH2::end_stream` (the client-side retirement plus the
  `ForwardTerminated` and `CloseDelimited` arms of the server side). By symbol,
  not line: those four assignments move together whenever either function is
  edited, and a `/`-separated number group rots one element at a time — the
  reader then follows the surviving numbers into unrelated code. Do not convert
  it back.
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
- **H1 keep-alive reuse.** An H1 frontend keeps its one slot for the next
  request: the keep-alive branch of `ConnectionH1::writable` (`h1.rs`) resets
  it in place instead of going through `Context::create_stream`. It resets
  `HttpContext` (`HttpContext::reset`, with a fresh request id), clears
  `front`, `back` and the response storage, `front_bound_to_backend`,
  `attempts`, `Stream::front_received_end_of_stream`,
  `Stream::back_received_end_of_stream`, `Stream::front_data_received` and
  `Stream::back_data_received`, forgets the replay capture, and returns the
  slot to `Idle`. Unlike `Context::create_stream`, it keeps the slot's
  `answers` (the connection's listener templates), leaves
  `Stream::front_window` alone (an H1 frontend leg has no window), and does
  not touch `request_counted`, which `Stream::generate_access_log` already
  cleared, nor mark a request start, which `ConnectionH1::readable` marks when
  the next request arrives. `ConnectionH2::start_stream` sizes
  `Stream::back_window` from the backend's SETTINGS_INITIAL_WINDOW_SIZE each
  time the slot opens on an H2 backend, and asserts that the backend leg's
  end-of-stream flag and DATA counter are clear. An H2 backend connection
  reads `Stream::back_received_end_of_stream` to refuse a frame on a closed
  stream (RFC 9113 §5.1): before sozu-proxy/sozu#1781 the stale flag of the
  first response made it refuse the second response's HEADERS with
  GOAWAY(STREAM_CLOSED), and the client got a 502. Pinned by
  `test_h1_to_h2_keep_alive_requests` and
  `test_h1_to_h2_keep_alive_requests_small_backend_window`
  (`e2e/src/tests/tests.rs`).
- **Backend attach.** Two call sites, on the two paths a stream can reach a
  backend, both reached from `Mux::ready_inner`'s `pending_links` drain:
  `Router::plan_connect` calls `Context::link_stream` itself on the pool-reuse
  return, and `Router::commit_dialed` calls it once the embedder has dialled.
  Either way it sets `Linked(token)` and pushes to `context.backend_streams`.
  That index is an `InlineTokenMap<LinkedStreams>`: the lowest backend token's
  entry is held inline, and an entry holds its first stream inline too,
  spilling into a `Vec` only when a second stream links to the same
  connection, so a one-backend H1 session links without allocating (#1613).
  An unlink leaves the emptied entry in place rather than removing the key,
  so the next attach on the same connection — every request on a keep-alive
  socket or an H2 multiplex slot — reuses it instead of allocating
  (#1583). The key leaves with the connection, in the dead-backend sweep of
  `Mux::ready_inner`, and `Mux::close` clears the index.

  The split is Question 6 of
  [#1340](https://github.com/sozu-proxy/sozu/issues/1340): the core decides and
  the embedder performs. `Router::plan_connect` routes, gates and scans the
  pool, then answers a `ConnectPlan` — `Attached` when it completed the attach
  from the pool, `Dial` when it needs a backend opened. It performs no dial, no
  `setsockopt`, no slab insertion and no epoll registration; `Mux::dial_backend`
  and the `Mux::attach_dialed` it calls do all of those, in the order `Router::connect` used, and hands the result
  to `Router::commit_dialed`. `a_dial_plan_performs_no_effect_of_its_own`
  (`router.rs`) pins the purity half: a `Dial` that had already dialled would
  still be a `Dial`, so the test asserts on the state the router and the stream
  are left in rather than on the returned value.

  The pool scan matches a connection by the cluster id in its
  `Position::Client`, and skips one whose backend has left the configuration.
  A cluster id outlives `RemoveCluster` followed by `AddCluster` of the same
  id, and a cluster outlives `RemoveBackend`, so the id alone does not say the
  backend is still configured. `BackendList::remove_backend` and
  `BackendMap::remove_cluster` (`lib/src/backends.rs`) mark every backend they
  drop `Closing`. The session's `BackendRegistry` still holds that `Rc`, and
  `BackendRegistry::is_retired` (`mod.rs`) reads its status for the
  `RoutingView` the embedder builds (`RoutingView::with_backend_retired`,
  `router.rs`). A retired connection takes no new stream and drains the ones it
  carries. A backend added again, even at the same address, is a new `Rc` in a
  new slot, so the next request dials it.

  Once a retired connection carries no stream — an H1 `KeepAlive`, or an H2
  `Connected` connection with an empty stream table — `Mux::ready_inner` drops
  it (`Connection::idle_pooled_backend`, then `Connection::force_disconnect`).
  The HUP that raises sends it through the dead-backend sweep on the next
  iteration, which releases its connection on the backend, so a `Closing`
  backend reaches `Closed` instead of holding a parked socket until the session
  ends. The check runs on every pass of the session, so a connection idle
  before the removal is closed on the session's next event. The
  `RemovedBackendHasNoConnections` event is still tied to the session: it is
  emitted when the last `Rc` of the backend drops, and the session's
  `BackendRegistry` holds one until the session closes.

  A reload that changes a backend's weight, `sticky_id` or `backup` reaches the
  workers as `RemoveBackend` then `AddBackend` (`ConfigState::diff`,
  `command/src/state.rs`). The removal retires the backend, so every session
  drops its pooled connection to it and dials the re-added backend, where an
  in-place update used to keep the connection.

  That is what took `Rc<RefCell<dyn ProxySession>>` out of the router entirely —
  its single use was `L7Proxy::add_session`, which is the embedder's.

  Two of the three reads `plan_connect` made through
  `Rc<RefCell<dyn L7Proxy>>` are gone too. `L7Proxy::clusters` and
  `L7Proxy::kind` are pure, and they now arrive as `RoutingView`, built once
  per decision by `Mux::ready_inner` from a single `proxy.borrow()` — Question
  12's mechanism applied a second time rather than reinvented, with a
  staleness of exactly one routing decision. `Router::route_from_request` no
  longer takes the handle at all. `the_routing_decision_reads_cluster_config_from_the_view`
  (`router.rs`) pins it by handing the decision a view that disagrees with the
  proxy and asserting the plan follows the view.

  The per-(cluster, source-IP) limit gate could not follow them, because it is
  not a read: it calls `SessionManager::cluster_ip_at_limit` **and**
  `SessionManager::track_cluster_ip`, a mutation of worker-global state
  (`connections_per_cluster_ip`, keyed on `(cluster, ip)` across every session
  and read again by `lib/src/tcp.rs`'s own gate). A read-only view cannot carry
  a mutation, so the gate is a **returned step** instead:
  `Router::plan_connect` answers `ConnectStep::CheckIpLimit(ConnectResume)`,
  `consult_ip_gate` (`mod.rs`) performs the consult and the track, and
  `Router::plan_connect_resume` finishes on the verdict. `plan_connect` now
  takes no `Rc<RefCell<dyn L7Proxy>>` at all.

  The cluster the gate is keyed on does not travel in `ConnectResume`.
  `plan_connect` moves the id `route_from_request` returned into the stream's
  `HttpContext::cluster_id`, and `consult_ip_gate` and
  `Router::decide_after_gate` borrow it from there (#1583). `ClusterId` is a
  reference-counted `Arc<str>`, so that id is itself a handle on the route
  table's allocation, and so are the keys `SessionManager::track_cluster_ip`
  stores and the id a dial hands over: `ConnectPlan::Dial` carries a handle
  and `Mux::attach_dialed` moves it into the new connection's
  `Position::Client`. No step of a request copies its cluster id.

  Two things make the split hard to get wrong rather than merely documented.
  `ConnectResume` is taken **by value**, so a parked decision cannot be
  resumed twice — `H2Shell::settled` taking its parked target, applied here.
  And the resumed call returns `ConnectPlan`, not `ConnectStep`, so a second
  gate consultation is unrepresentable rather than just incorrect. Both
  entries share one `Router::decide_after_gate` body, so the gated and ungated
  paths cannot drift.

  **The track happens before the decision resumes, and that ordering is the
  invariant.** An `IpGateVerdict::Admitted` asserts to the core that the slot
  has been taken; resuming as `Admitted` without tracking would let the stream
  through uncounted, and because the counter is worker-global the next session
  from the same IP would find a slot that should already be gone.
  `an_admitted_stream_is_counted_before_the_decision_resumes` (`router.rs`)
  drives the real three-step sequence for two different frontend tokens from
  one source IP against a limit of one — two tokens, because the gate is
  deliberately idempotent within a token.

  Backend selection no longer reaches the handle either (Question 12's second
  part). `Router::backend_from_request` is lent a `BackendSelector`
  (`router.rs`) for one call, whose single method selects a backend **and
  reserves** a connection on it; `Mux::dial_backend` implements it over the
  worker's `BackendMap` and the session's `BackendRegistry`
  (`RegistrySelector`), so the router receives an opaque `BackendId` and
  never an `Rc<RefCell<Backend>>`, and then dials `BackendId::address` itself
  ([#1684](https://github.com/sozu-proxy/sozu/issues/1684)). It is a
  capability rather than a data view like `RoutingView` because selection
  mutates — the round-robin cursor, the fail-open latch, `PeakEWMA`, the
  Maglev table, the availability latch — and a snapshot of N backends each
  behind its own `RefCell` cannot be lent without allocating. Selecting
  reserves under the same borrow as the choice, and a failed `connect(2)`
  releases through the ledger (see "Selection reserves, failure releases"
  under invariant 14 in §9), so no window opens between reading the counters
  and the connection they count.
  `a_second_selection_observes_the_first_selections_reservation`
  (`router.rs`) holds that property. The map itself is `Mux::backends`,
  handed over when the session is built (`HttpSession::new`, or the proxy's
  field at the expect-proxy and TLS-handshake upgrades); `L7Proxy` has no
  `backends` method any more, so a dial borrows neither the proxy nor a
  fresh handle to reach it ([#1684](https://github.com/sozu-proxy/sozu/issues/1684)).
  `BackendSelector::select` also takes the request's client affinity
  key, which `HRW` and `MAGLEV` pin the client with and a shuffle-sharded
  cluster ranks the client's shard by
  ([#524](https://github.com/sozu-proxy/sozu/issues/524)). Sharding itself
  lives below the selector, in `BackendList::select_with_key`
  (`lib/src/backends.rs`): it narrows the candidates to the shard before the
  cluster's policy runs, so the router and the reservation are unchanged.
  `Router::plan_connect` derives it (`affinity_key`, `router.rs`) right after
  `route_from_request`, where both the routed cluster and the request's header
  blocks are in hand, and stores it in `HttpContext::affinity_key` beside
  `cluster_id`. The storage is for the same reason as the cluster's: a replay
  skips routing against a drained front kawa, so it reuses the key the first
  attempt derived rather than recomputing it from blocks that are gone. The key
  is `None` under every other policy, and a sticky cookie naming a live backend
  still wins over it. Connection reuse runs before any dial and does not read
  the key: an H1 keep-alive or H2 connection the session already holds serves
  the next request whatever key it carries, as it serves it whatever sticky
  cookie it carries.
- **Backend detach.** `Context::unlink_stream` (`mod.rs`) — called from the four
  timeout arms of `Mux::timeout_inner` (`mod.rs`), from H1 EOF
  (`ConnectionH1::end_stream`, `h1.rs`),
  and from `ConnectionH2::reset_stream` and `ConnectionH2::end_stream`
  (`h2.rs`), each of which opens with it. By symbol, not line, for the same
  reason as the `Unlinked` bullet above. Do not convert it back.
- **Recycle.** The H2 write path recycles a server stream once both the front
  request and back response are `is_terminated() && is_completed()` — see
  `try_recycle_server_stream` flow in `h2.rs`.

### 3.3 What "Recycle" means and when slots are popped

`StreamState::Recycle` marks a slot as **logically free but still allocated**.
It means:

- The pool buffers are cleared when the slot is taken over (the reuse branch
  of `Context::create_stream`, `mod.rs`).
- Metrics have been reset — the `SessionMetrics::reset` call in
  `Context::create_stream`'s recycled-slot branch (`mod.rs`).
- The slot can be handed back to a new request by `Context::create_stream`
  which, on entry, searches for a `Recycle` slot — the `recycle_slot` binding
  at the top of that function (`mod.rs`). By symbol, not line: `recycle_slot`
  is the one binding in `create_stream` that performs this search, so the
  symbol resolves unambiguously, while the range it would otherwise carry
  spans a `position` closure whose lines move whenever the predicate is
  edited. Do not convert it back.

A `Recycle` slot is **physically popped** only when `shrink_trailing_recycle`
runs — see §6.

---

## 4. The Two Stream Maps

### 4.1 `ConnectionH2.streams` (per-connection wire map)

- Type: `BTreeMap<StreamId, GlobalStreamId>` — private field of
  [`H2StreamTable`](h2_stream_table.rs), named `streams`, since the
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

- Type: `Vec<Stream>` — the `Context::streams` field (`mod.rs`).
- Scope: one `Vec` per `Mux` session. **Both** H1 and H2 frontends use it, and
  **every** backend `ConnectionH2` attached to this session indexes into it.
- Index: `GlobalStreamId = usize` (`mod.rs`).
- Mutated by:
  - push — `create_stream` when no `Recycle` slot is available (`Context::create_stream`,
  `mod.rs`).
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
   the end of `Mux::ready_inner`, after every `ready()` pass.

The H1 side keeps its single `stream: Option<GlobalStreamId>` in `ConnectionH1`
— there is no hashmap because H1 multiplexing is limited to request pipelining
with at most one live stream at a time.

---

## 5. Index-Caching Invariants (`expect_write` / `expect_read`)

### 5.1 The fields

Both private fields of [`H2StreamTable`](h2_stream_table.rs)
(`H2StreamTable::expect_read`, `H2StreamTable::expect_write`) since the step-3 extraction, reached from
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
- `expect_write = Some(sid)` — a write on `sid` stalled and it is parked;
  `write_streams` resumes it first on the next writable pass. It is only
  ever `Other` since #1604: the park keeps the stream's whole remaining
  frames first in line and says nothing about framing. The rest of the frame
  the stall cut is no longer the stream's: the ordered output queue adopted
  it (invariant 28), so a PING or SETTINGS ACK, a GOAWAY or a RST_STREAM
  queued meanwhile follows it on the wire (#1600), and removing the parked
  stream drops only frames the wire has not started (#1604) — which keeps
  the framing whole. When one of those frames is an already-encoded header
  block, `ConnectionH2::parked_header_block` records it at the park, and the
  next pass resets the HPACK encoder's table if the block was dropped
  (#1627, invariant 28). The DATA payload the park still holds whole is
  recorded beside it, `ConnectionH2::parked_data`, because the prepare
  already debited both send windows for it: dropped unsent, it is given back
  (#1641, invariant 28). In
  `H2State::GoAway` a parked stream is still written, since a GOAWAY does not
  close the streams at or below its last stream id.

### 5.2 Load-bearing invariant

> If `expect_write` or `expect_read` holds an `H2StreamId::Other { gid, .. }`,
> then `gid` **MUST** be a valid in-bounds index into `context.streams` at the
> moment it is dereferenced.

Dereference sites:

- write resume path — reads `context.streams[global_stream_id]` in the
  `H2WritePhase::Resume` arm of `ConnectionH2::poll_write_target`, whose
  `H2StreamId::Other` payload comes straight out of `expect_write` (`h2.rs`).
  By symbol, not line: that read is one of fifteen identical
  `let stream = &mut context.streams[global_stream_id];` lines.
- `try_resume_reading` — reads `context.streams[global_stream_id]` in its
  `expect_read` block (`h2.rs`). By symbol, not line: that read is the file's
  only `let stream = &context.streams[global_stream_id];`, so the symbol
  resolves unambiguously and cannot drift, which a line number here would.
- `read_buffer` — indexes `context.streams[global_stream_id]` on behalf of
  both halves of the read protocol: `ConnectionH2::poll_read_target` and
  `ConnectionH2::handle_read` each hand it `&mut context.streams`, and its
  `H2StreamId::Other` arm does the dereference (the free function
  `read_buffer`, `h2.rs`). `readable`
  itself only sits between the two halves — it reaches this slice through
  `read_space`, which calls `read_buffer` for it.

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
recycled slot to reuse and then crosses the shrink ratio (the `h2_stream_shrink_ratio` test at the end of
`Context::create_stream`, `mod.rs`).
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
routing discipline is compiler-enforced, not just documented). An entry whose
method holds exactly one `remove_dead_stream` call is cited by symbol — the
method name is the whole address there, and a line only rots; do not convert
those back. The entries that keep a line mean one specific branch inside a
method that calls it more than once, or — for the `close` exception below —
one block inside a method that does several unrelated things:

- `poll_write_target` after end-of-stream — the `H2WritePhase::Resume` retirement
  and `H2WritePhase::End`'s deferred `completed_streams` loop, both in
  `ConnectionH2::poll_write_target` (`h2.rs`).
- `ConnectionH2::prune_inactive_streams_while_closing` (`h2.rs`).
- `ConnectionH2::handle_window_update_frame` zero-increment path (`h2.rs`).
- `ConnectionH2::cancel_timed_out_streams` slow-multiplex guard (`h2.rs`).
- `ConnectionH2::handle_continuation_header_state` CONTINUATION oversize
  (`h2.rs`).
- `ConnectionH2::handle_rst_stream_frame` peer RST (`h2.rs`).
- `ConnectionH2::handle_goaway_frame` retry loop (`h2.rs`), `Position::Client`
  only: on a frontend connection `last_stream_id` bounds server-initiated
  streams (RFC 9113 §6.8), which sozu never opens, so nothing is retired.
- `ConnectionH2::end_stream` client-side retirement (`h2.rs`).

No call site in this file performs `self.streams.remove(...)` inline: it
cannot — `streams` is a private field of `h2_stream_table.rs`, so that
expression does not even compile from `h2.rs` (verified: a deliberately
reintroduced inline `self.streams.remove(...)` inside `ConnectionH2` fails
with `E0609: no field 'streams'`). The one exception below does not remove at
all:

- `close` backend-stream teardown — the final loop of `H2Shell::close` (`h2.rs`) (does not remove, only
  notifies the endpoint — the surrounding `close` path drops the whole
  connection, and every entry in the wire map (`self.stream_table`) with it,
  shortly after).

The two-check pattern — one for `expect_write`, one for `expect_read` — is
mechanical work `remove_dead_stream` now performs for every removal above; a
reviewer's job on a new removal site is to confirm it calls the helper rather
than reintroducing an inline `self.streams.remove(...)`.

Nulling `expect_read` is only half of the job when the removed stream owns
the DATA payload being read (`H2State::Frame` with
`expect_read == Some((Other { gid, .. }, n))`). Left at that, the connection
has nothing to read and never reads again (sozu-proxy/sozu#1597). So
`remove_dead_stream` reads that pair before `H2StreamTable::remove` clears it,
and hands the unread remainder `n` to
`ConnectionH2::skip_orphaned_data_payload` (`h2.rs`). That function:

- moves to `H2State::Discard` with `expect_read = Some((Zero, n))` and no
  `discarded_field_block`, which marks the remainder as skipped rather than
  refused. `poll_read_target` then caps each read at the free space in
  `zero`, and `handle_read` drops the bytes instead of filling `zero`, so the
  payload is consumed piece by piece and `zero`'s content is never touched;
- credits the frame's whole `payload_len` to the connection receive window.
  `handle_data_frame` never sees this frame, and RFC 9113 §6.9 counts it
  anyway;
- restores READABLE interest, since a stream parked on buffer pressure is
  resumed only by `try_resume_reading`, which needs the stream to exist. It
  does so even while output is queued: `zero` is input only (invariant 28).

The backend-side `ConnectionH2::end_stream` RST_STREAM(CANCEL) once went
into `zero` with neither `expect_write` nor READABLE touched. A backend
connection that skipped an orphaned DATA remainder then read the next frame
header in behind it and took the RST for one of the backend's own frames
(sozu-proxy/sozu#1597). It is queued in the ordered output queue now, like a
PING or SETTINGS ACK, behind any half-written stream frame's rest.

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
by a ratio threshold so we don't thrash on every request (the `h2_stream_shrink_ratio` test at the
end of `Context::create_stream`, `mod.rs`):

```rust
if total > 1 && active > 0 && total > active * self.h2_stream_shrink_ratio {
    self.shrink_trailing_recycle();
}
```

The default ratio is 2 (`h2.rs`, `DEFAULT_STREAM_SHRINK_RATIO`),
overrideable per listener via `H2ConnectionConfig::stream_shrink_ratio`
(`h2.rs`). In short: if more than `2×active` slots are held, trim trailing
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
  embedder to call back at. The wheel handle is `Mux.timeouts.frontend` — see
  §7.7.
- Fired when: no traffic observed for `configured_frontend_timeout`
  (the `Mux::configured_frontend_timeout` field, `mod.rs`) while any stream is live, or the shorter `request_timeout`
  until the first `Link` transition (`Mux::ready_inner` switches the frontend
  to `configured_frontend_timeout` there).
- Reset: on meaningful activity — HEADERS for an existing stream, DATA bytes
  read, and a write-pass transmit that moved a stream's bytes — see
  `ConnectionH2::handle_headers_frame` (`h2.rs`), the `arm_timeout()` call in
  the DATA-payload arm of `ConnectionH2::poll_read_target` (`h2.rs`), and the
  `H2StreamId::Other` arm of `ConnectionH2::handle_write` (`h2.rs`). All three
  are by symbol, not line: the number the second would carry is one the
  `context.streams[global_stream_id]` dereference bullet in §5.2 used at the
  base revision, and the drift rule keys on the citation rather than on where
  it sits, so it cannot tell that collision from staleness. Do not convert them
  back. Control frames (PING, WINDOW_UPDATE, SETTINGS) deliberately do **not**
  reset it so a misbehaving peer cannot pin the session with keepalive noise —
  the comment block that opens `ConnectionH2::poll_read_target`, also by symbol
  rather than by line. That last part was prose only until
  sozu-proxy/sozu#1489: `ConnectionH2::write_streams` armed at pass entry and
  `writable()` reaches it on every proxying-state pass, so the pass that
  flushed a PING ACK reset the deadline the read side had just refused to
  reset. The write site is now per transmit and gated on the bytes, so an
  acknowledgement-only pass arms nothing.
- Handling: `Mux::timeout` inspects each stream's state (`Mux::timeout_inner`) and
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

- **Bidirectional-silence guard** — `H2StreamTable.stream_last_activity_at:
  BTreeMap<StreamId, Instant>`. Refreshed on every non-empty inbound DATA frame,
  on HEADERS for an existing stream (trailers), and on outbound bytes written.
  Catches a stream making no forward progress in either direction
  (slow-multiplex Slowloris: the connection-level timer resets on every frame,
  so without this per-stream guard a peer could hold `max_concurrent_streams`
  slots for the full session timeout).
- **Outbound-flow-control-stall guard** — `H2StreamTable.stream_fc_stalled_since:
  BTreeMap<StreamId, Instant>`, paired with
  `H2StreamTable.stream_fc_stalled_progress: BTreeMap<StreamId, usize>` (the
  cumulative-stall budget). Armed (in `ConnectionH2::poll_write_target`) whenever a stream holds
  sendable buffered data it cannot send because its effective send window
  `min(stream window, connection.window)` is exhausted (the stream window is
  this connection's leg: `Stream::front_window` on a frontend connection,
  `Stream::back_window` on a backend one). This is
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
  resume path (the `resume_bytes > 0` block of `H2WritePhase::Resume` in
  `ConnectionH2::poll_write_target`, `h2.rs`) — therefore never reaches the floor, so the deadline
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
- **Does** go through `enqueue_rst` (dedupe, pending-queue bound, `WRITABLE`
  arming — security review LISA-001; see §8.2) and feeds
  `total_rst_streams_queued`, but as `RstOrigin::Local`: the reap is Sōzu's
  decision, so `account_emitted_rst` does not charge it to the peer's
  MadeYouReset count.
- All three per-stream maps (`stream_last_activity_at`, `stream_fc_stalled_since`,
  `stream_fc_stalled_progress`) are evicted in `remove_dead_stream` (with
  `debug_assert`s guarding against a leaked entry on a new per-stream cache).
- Observability: `cancel_timed_out_streams` returns
  `h2.streams.reaped.{idle_timeout,window_stall}` per guard as `MetricEvent`s
  the shell records, plus `h2.streams.reaped.stall_budget` (a subset of
  `window_stall`) when the reaped stream had dribbled progress below the floor.

### 7.3 Backend timeout

- Tracker: the backend connection's own `timeout_deadline`, reflected onto the
  wheel by the handle under `back_token` in `Mux.timeouts.backends` (§7.7).
- Set to `configured_backend_timeout` after successful connect
  (`Connection::set_timeout_duration`, called from `Mux::ready_inner`).
- Fired by: timer wheel → `Mux::timeout` with the backend token.
- Action: for each stream linked to that backend, either send 504, or forcefully
  terminate, or keep draining — see the backend-token branch of `Mux::timeout_inner`. The
  timeout is re-armed there if the session stays alive to avoid the "immortal zombie"
  state.
- Access-log discriminator: the "response not started" arm sets
  `stream.context.access_log_message = Some("backend_timeout")`; the "response
  in progress" `forcefully_terminate_answer(InternalError)` arm sets it to
  `Some("backend_response_timeout")`. Both tokens map the same
  `MuxState::timeout` branch onto stable, HAProxy-aligned strings on the
  access-log `message` field — see `doc/configure.md` § "Access log message
  field".

### 7.4 Interaction order on a loaded connection

1. `readable()` entry runs `cancel_timed_out_streams` first, inside
   `ConnectionH2::poll_read_target` (§7.2).
2. Then that same call optionally fires `goaway(SettingsTimeout)` if the
   SETTINGS ACK is overdue (`ConnectionH2::poll_read_target`, `h2.rs`) — before
   any read is offered.
3. Then `ConnectionH2::handle_read` consumes the frame / payload.
4. `writable()` mirrors this check, via `flush_pending_control_frames`
   (`ConnectionH2::flush_pending_control_frames`, `h2.rs`).
5. If the frontend timer fires while streams are linked, the timeout logic in
   `Mux::timeout` (`mod.rs`) decides per-stream; backend timer fires independently.
6. Loop budget (`MAX_LOOP_ITERATIONS = 10_000`, `mod.rs`) is a hard backstop
   in `Mux::ready_inner`, whose `counter` is declared above BOTH of its loops,
   so the budget is shared across every outer iteration of one `ready()` call.
   A frontend HUP never counts as work against it: the inner loop closes the
   session on one once no output is left to flush and no exchange is in
   flight (`Mux::frontend_exchange_in_flight`). A lingering frontend (§8.4)
   is in flight until its drain reads the client's EOF, as unread input, so
   its last bytes are drained first. Its exit check counts only frontend
   READABLE, WRITABLE and ERROR interest; a full hang-up (ERROR or
   WRITE_CLOSED) is never in flight, and closes the session at once.

Steps 1-4 all run inside one `readable()`/`writable()` call and therefore all
read the same `ConnectionH2.now` — see §7.5.

### 7.5 Clock sampling — one snapshot per pass

Every deadline above is evaluated against a snapshot, not against a fresh
`Instant::now()`. **`Mux` is the only clock sampler in the mux.** It writes
`Context.now` (the `Context::now` field, `mod.rs`) at three points:

- once per **outer** `Mux::ready` pass (the top of `Mux::ready_inner`'s outer
  loop), so that the inner loop
  deliberately shares one instant — that is the property the snapshot exists
  for, not a claim about how long a sweep takes. `MAX_LOOP_ITERATIONS` is a
  count and bounds iterations, not wall clock, and `counter` (declared at the top of
  `Mux::ready_inner`) sits above both loops, so one `ready()` call can spend the whole budget
  under a single snapshot;
- at the top of `Mux::timeout_inner` (`mod.rs`);
- at the top of `Mux::shutting_down_inner` (`mod.rs`), which runs outside
  `ready()` entirely. That line is load-bearing, not belt-and-braces:
  `drive_frontend_shutdown_io` (`mod.rs`) always reaches `readable()` for
  an H2 frontend — `force_h2_read` is unconditionally true, so the early
  return above it cannot fire — and `readable` mirrors `context.now` into
  `ConnectionH2::now` before the forced-close check runs. Drop it and a
  silent draining session propagates its last `ready()` snapshot forever.
  Pinned by `shutting_down_refreshes_the_snapshot_so_the_drain_budget_expires`.

The H2 core reads `ConnectionH2.now` (`h2.rs`), a mirror assigned from
`context.now` once per pass — `writable`, `cancel_timed_out_streams` and
`start_stream` carry `self.now = context.now;` at their top, and the read pass
carries it at the top of `poll_read_target`, which `readable` calls first, not
in `readable` itself (`h2.rs`, four sites) — from the `now` parameter of `graceful_goaway`
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
  refreshes in the DATA-payload arm of `ConnectionH2::poll_read_target` (`h2.rs`, by
  symbol for the collision the liveness `Reset:` bullet above records — do not
  convert it back) and in `ConnectionH2::handle_headers_frame` (`h2.rs`, HEADERS), the
  outbound-byte refreshes in the `H2WritePhase::Resume` branch of
  `ConnectionH2::poll_write_target` (`h2.rs`, by symbol because the `writable`
  lift moved another citation onto the number this one used to carry — do not
  convert it back) and at
  the `H2WritePhase::Flush` arm of `ConnectionH2::poll_write_target` (`h2.rs`), the `FcStallAction::Arm` branch of
  `ConnectionH2::poll_write_target` (`h2.rs`) — and stamps the
  snapshot taken at the START of that pass. The
  stored instant is therefore OLDER than the event it records.
- An **eval** site runs near the top of a pass: `cancel_timed_out_streams` is
  the first thing `readable` does (§7.4 step 1), and the SETTINGS-ACK check
  (in `ConnectionH2::poll_read_target` and in `ConnectionH2::flush_pending_control_frames`, `h2.rs`)
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
RFC 9113 §5.1.2 back-pressure window
(`ConnectionH2::record_refusal_for_backpressure`) are the one asymmetric case,
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

**The frontend RTT is sampled on the same discipline, at a different cadence.**
No core reaches a socket for it: `ConnectionH2::snapshot_rtts` asks its
`Endpoint` through `Endpoint::local_rtt`, and `H2Shell` — the owner of the
socket — answers through `ShellEndpoint`, which it builds around the endpoint
at each core call from disjoint borrows of its own fields. `H2Shell::local_rtt`
memoizes the answer through `memoized_rtt` (`lib/src/protocol/mux/mod.rs`),
the one function every mux RTT read goes through. It differs from the clock in
three ways that matter when changing either:

- **Laziness.** `Context::now` is written before anything reads it. The RTT is
  read only when a stream is logged: the first `snapshot_rtts` of the
  connection takes the `getsockopt(TCP_INFO)` and stores it, every later one
  reuses it, and a connection that logs nothing reads nothing (issue #1590).
  Before that, `Mux::refresh_client_rtt` sampled at the top of every pass
  whether or not the pass logged, about five reads for the one an H2 request
  prints.
- **Cadence.** `Context::now` is refreshed once per *outer loop iteration* of
  `Mux::ready`; the RTT memo is never refreshed at all. It is one sample per
  connection, taken at the connection's first access log. Until the
  per-connection sample, `Mux::expire_client_rtt` forgot it at the top of every
  `ready`, `timeout_inner` and `shutting_down_inner` pass, so a keep-alive
  connection paid one read per request; that function is gone. The sample is
  not taken at `accept`: Linux already has a handshake RTT there (27–34 µs on
  loopback straight out of `accept`, measured), so it would be valid, but a
  lazy read is free on a connection that never logs.
- **Visibility.** A stale clock snapshot costs at most a one-pass deadline
  error, discussed above. The RTT sample is *published*: it lands in the
  access log's `client_rtt` cell, so every stream of one connection reports
  one identical number, the frontend SRTT when the connection's first stream
  was logged. Issue #1339 question 11 made that value per pass, #1590 made it
  lazy, and the per-connection sample made it per connection —
  `doc/configure.md` states it for operators and `doc/h2_mux_internals.md`
  carries the syscall measurements.

Pinned by `snapshot_rtts_samples_the_frontend_once_per_connection`,
`a_pass_that_logs_nothing_reads_no_tcp_info` and
`streams_reaped_in_one_pass_log_one_present_client_rtt` (`h2.rs`), by
`a_mux_pass_keeps_the_connection_sample_and_reads_none_without_a_log` and
`mux_close_reads_the_frontend_rtt_only_for_a_stream_it_logs` (`mod.rs`), and by
`keep_alive_requests_read_tcp_info_once_per_connection` (`h1.rs`).
`server_rtt` is memoized the same way, on the backend `Connection`:
`Endpoint::peer_rtt` answers through `Connection::rtt`, which samples the
backend socket at the first response logged through it and keeps that value
for the connection's life, keep-alive pool reuse included. The two cells of a
log line therefore come from two different instants, each the first access log
of its own connection.

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

The `Mux` adapter owns every wheel handle, in `Mux.timeouts: MuxTimeouts` — a
`frontend` field for the frontend token, whose key never changes, plus one
`backends` entry per connection in `router.backends`, held in an
`InlineTokenMap` whose lowest-token entry is inline, so the usual session keeps
both handles without allocating (#1613). This is the only place in the mux that talks to the timer
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
  `close` drops every backend handle and replaces the frontend's with an
  unarmed one.
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
   can still arrive. Called first time from `Mux::shutting_down_inner`
   (`lib/src/protocol/mux/mod.rs`). Draining flag set.
2. **Final GOAWAY** — on the second invocation (draining already true), call
   `goaway(NoError)` (`ConnectionH2::goaway`, `h2.rs`) with the actual `highest_peer_stream_id`,
   remove `READABLE` interest (same function), transition to `H2State::GoAway`.
   Caller is `finalize_write` when all streams drain (`h2.rs`). When the
   forced-close budget elapses first (§8.3), `Mux::shutting_down_inner` sends
   it through `ConnectionH2::goaway_before_forced_close` instead, which
   reports a stream whose opening block never completed as unprocessed
   (invariant 31).

`peer_gone_after_final_goaway` (`h2.rs`) guards against deadlock on a
peer-side HUP after the final GOAWAY.

**The close after a final GOAWAY(NO_ERROR) lingers.** The client may still be
sending frames it wrote before it read the GOAWAY (RFC 9113 §6.8 lets it), and
reading stops at the GOAWAY; a close with those frames unread, or followed by
more, makes the kernel reset the connection and discard response bytes still
queued in the socket. So when a `writable` pass that began in `H2State::GoAway`
after a `NO_ERROR` GOAWAY (`ConnectionH2::graceful_goaway` set by
`goaway_with_last_stream_id`) decides `CloseSession`, and the client has not
hung up, `H2Shell::linger_instead_of_closing` stages the close the way §8.4
does for H1, through the same `Linger` state and `shared::drain_discard`:
`close_notify` first — when it is still pending the linger is
`Linger::Pending` and the next pass, whose state is `Error` by then, resumes
it — then `shutdown_write`, then the raw socket is read and dropped until the
client's EOF, `LINGER_MAX_BYTES`, or the listener's `request_timeout`.
`ConnectionH2::arm_timeout` and `set_timeout_duration` leave that deadline in
place. A GOAWAY carrying an error (a protocol violation, a flood) does not
linger: sozu does not keep reading from that peer. The lingering `readable`
and `writable` passes adopt the pass's clock (`ConnectionH2::adopt_now`)
before they return, so the bounds are: the linger deadline,
`request_timeout` from the linger's start, which `Mux::timeout_inner`
enforces; and, during a soft stop, the H2 listener's graceful-shutdown
budget (§8.3) whenever it is armed and elapses first — `graceful_goaway`
arms it when the soft stop sends its first GOAWAY. A drain that the client's
own GOAWAY started does not arm that budget, so its linger runs to the linger
deadline even during a soft stop. Pinned by
`a_soft_stop_closes_a_lingering_h2_session_once_its_budget_elapses` (`mod.rs`),
`an_h2_frontend_lingers_after_its_final_goaway_so_the_client_reads_everything`
(`mod.rs`), `a_linger_deferred_behind_close_notify_starts_on_the_next_pass`
and `the_final_goaway_and_close_notify_leave_in_one_tls_write` (`h2.rs`).

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
- **Never for a backend stream still idle** — on `Position::Client` a stream id
  is allocated by `ConnectionH2::start_stream` when the router links the
  request, but its HEADERS leave with a later write pass, possibly never (the
  preface is not out, the pass has not run, or the socket took nothing and the
  block is parked in `front.out`). `front.consumed` is exactly "a request byte
  reached this socket", and a stream's first frame is its HEADERS, so until it
  is set the backend has not seen the stream: a RST_STREAM for it is a
  connection error there (RFC 9113 §5.1, §6.4), and one queued before the
  preface breaks the connection preface (§3.4). The `Position::Client` arm of
  `ConnectionH2::end_stream` and `ConnectionH2::cancel_timed_out_streams`
  therefore retire such a stream with no frame, and the `H2WritePhase::Prepare`
  arm of `ConnectionH2::poll_write_target` does not hand a request in error
  whose HEADERS never left to the converter, whose `initialize` would queue
  the RST (an H1 frontend that rejects a malformed body answers 400 itself and
  leaves the backend stream registered), as hyperium/h2 drops a stream
  still pending open; the burnt id is closed implicitly by the next HEADERS on
  a higher id (§5.1.1), and a HEADERS block parked unsent is dropped with the
  stream and resets the HPACK encoder's table on the next pass (§9 invariant
  28). The connection is shared by the concurrent streams of the session's
  one frontend connection (`Router::plan_connect`; a backend connection
  belongs to one `Mux` session and is never shared with another client
  connection), so a RST here used to cost all of them
  (sozu-proxy/sozu#1631). Pinned by
  `a_backend_stream_ended_before_its_headers_left_is_not_reset`,
  `a_backend_stream_ended_before_the_preface_leaves_the_preface_first`,
  `a_backend_stream_reaped_before_its_headers_left_is_not_reset`,
  `a_backend_stream_ended_with_its_headers_parked_is_not_reset` (`h2.rs`, red
  on `d5161919`) and
  `a_backend_stream_whose_request_failed_before_its_headers_left_is_not_reset`
  (red on `6eef0ba1`).
- **Proxy-initiated, idle cancel** — `cancel_timed_out_streams` (§7.2); the
  per-stream `forcefully_terminate_answer` call in the `Mux::timeout_inner` timeout
  path now arms `Ready::WRITABLE` via `arm_writable()` so the pair with
  `event::WRITABLE` actually schedules the next `writable()` tick under
  edge-triggered epoll.

All three paths eventually purge the wire-id from the wire map
(`self.stream_table`).

#### Proxy-RST emission path

`ConnectionH2::enqueue_rst(wire_stream_id, error)` (`h2.rs`) is the
canonical entry point for every proxy-emitted stream reset. It delegates the
queueing to `H2ControlTx::enqueue_rst` (`h2_control_tx.rs`), which owns the
queue and is unit-tested without a full `ConnectionH2` fixture (see the five
`test_enqueue_rst_into_*` tests and the `prop_pending_rst_queue_stays_within_its_bound`
property, which moved there with the state they drive). Four invariants are
kept in lock-step:

- **Dedupe** via `H2StreamTable`'s private `rst_sent: HashSet<StreamId>`
  (reached through `self.stream_table.rst_sent_contains()`/`rst_sent_mut()`):
  at most one RST per wire stream id. `HashSet::insert` returns `false` when
  the id is already present;
  `H2ControlTx::enqueue_rst` short-circuits on that branch so its queue and
  its lifetime counter stay consistent even when a cascading error path
  re-enters the reset flow for the same stream. Only a registered stream's id is recorded:
  `ConnectionH2::enqueue_rst` passes no set for a refused stream or a closed
  one, because only the stream table's eviction removes an id again, and an
  unregistered id would stay for the connection's lifetime.
  When `H2StreamTable::remove`
  evicts an id from `rst_sent`, it keeps it among the last
  `RECENTLY_RESET_CAPACITY` reset streams, so
  `H2StreamTable::was_reset_locally` still recognises it, and one read-side
  branch of `ConnectionH2::handle_header_state` ignores the DATA and HEADERS
  frames (with their CONTINUATION frames) the peer sent before reading the
  reset (RFC 9113 §5.1), on a tracked or a retired stream alike
  (sozu-proxy/sozu#1751, sozu-proxy/sozu#1783). A header block is still
  decoded whole for HPACK. Two header blocks per stream and DATA within the
  stream's initial receive window count no glitch; each block beyond two, and
  each DATA frame beyond the window, count one, and an empty DATA frame
  without END_STREAM counts toward the empty-DATA flood limit
  (CVE-2019-9518). The allowances are held for at most
  `RESET_ALLOWANCES_CAPACITY` streams.
  Pinned by `frames_on_a_backend_stream_sozu_reset_are_ignored`,
  `frames_on_a_client_stream_sozu_reset_are_ignored`,
  `data_on_a_reset_backend_stream_counts_a_glitch_beyond_its_window_only`,
  `data_on_a_tracked_backend_stream_sozu_reset_is_ignored`,
  `header_blocks_on_a_reset_backend_stream_beyond_two_count_glitches`,
  `a_continuation_of_a_refused_header_block_is_discarded_with_it`,
  `a_refused_header_block_counts_its_first_fragment_toward_its_size`,
  `empty_data_frames_on_a_reset_backend_stream_count_toward_the_flood_limit`
  (`h2.rs`), `was_reset_locally_survives_removal_within_the_bound` and
  `reset_stream_allowances_stay_bounded` (`h2_stream_table.rs`).
- **Lifetime count** via `H2ControlTx`'s `total_rst_streams_queued`: each
  freshly queued RST bumps it and nothing rewinds it. It is reported by the
  session log line and bounds nothing. The CVE-2025-8671 MadeYouReset cap is
  `H2FloodDetector::record_rst_emitted`
  (`lib/src/protocol/mux/h2_flood_detector.rs`), reached from
  `ConnectionH2::account_emitted_rst` only for a reset whose `RstOrigin` is
  `PeerProvoked`.
- **Per-insert queue bound**, `max_pending` — `pending_rst_bound` of the
  connection's `max_concurrent_streams`: `MIN_PENDING_RST_STREAMS` (4000), or
  four resets per advertised concurrent stream when that is larger
  (`h2_control_tx.rs`). It bounds what is *pending*, not the connection's
  lifetime: the queue drains on every `writable()`. `H2ControlTx::enqueue_rst`
  applies it to its own queue length at the insert rather than at the drain.
  An insert at the bound returns `EnqueueRstOutcome::Dropped` and sets
  `H2ControlTx::overflowed`: nothing is queued, nothing is recorded in
  `rst_sent`, `WRITABLE` is not re-armed, and `ConnectionH2::enqueue_rst`
  answers with a `h2.rst_stream_dropped` counter plus a session-context
  `error!` line. The order is load-bearing and is what
  `test_enqueue_rst_into_refuses_at_capacity_without_side_effects` pins: the
  bound is tested BEFORE `rst_sent.insert`, because recording an id whose frame
  was never queued would make a later, legitimate reset for that stream dedupe
  against a frame that does not exist. `cancel_timed_out_streams` queues one
  RST per timed-out stream in a single sweep with no flush in between; a mass
  idle-timeout reap used to push the queue past the bound invariant 3 asserts
  (sozu-proxy/sozu#1413). Four per concurrent stream keeps one sweep below the
  bound, but the queue holds what every caller queued since the last
  successful drain; the DATA-on-closed-stream reset in `handle_header_state`
  and the refusals are insert paths `ConnectionH2::check_invariants` never
  inspects, rate-limited by `record_glitch` + `check_flood_or_return!`
  rather than by the queue bound. Nothing that would have reached the wire is
  lost: `flush_pending_control_frames` tests `H2ControlTx::overflowed`
  *before* its drain loop and emits `GOAWAY(ENHANCE_YOUR_CALM)` instead of
  serialising anything. That test also carries a state gate,
  `!matches!(self.state, H2State::GoAway | H2State::Error)`, which the drain
  below it does not share, so after the first GOAWAY an overflow does not
  re-escalate: `goaway()` enters `H2State::GoAway` without calling
  `H2ControlTx::clear_pending`, and a `Mux::timeout` reap landing in that window is
  dropped with no second escalation. That window is bounded — the peer
  already holds the GOAWAY and `writable()`'s `GoAway` arm force-disconnects
  on the same pass. The peer is told to back off and the connection is torn
  down; it is not left believing a refused stream is still live.
- **Invariant 15** via `Readiness::arm_writable()` (`lib/src/lib.rs`):
  pairs `Ready::WRITABLE` interest with the matching event bit so `writable()`
  is scheduled on the next epoll tick.

The three RST push sites retrofit to `enqueue_rst`:

- DATA-on-closed-stream (the closed-stream DATA branch of
  `ConnectionH2::handle_header_state`, `h2.rs` — `H2Error::StreamClosed`).
- `refuse_stream_and_discard` (`h2.rs` — MCS / pool exhaustion).
- `reset_stream` (`h2.rs` — per-stream error paths: malformed HEADERS,
  content-length mismatch, WINDOW_UPDATE zero-increment or overflow,
  unauthorised priority updates, self-dependent HEADERS).

Serialisation happens in `H2ControlTx::drain_rst_streams_into`
(`h2_control_tx.rs`), which walks the module's own `pending_rst_streams` and
writes each frame with `serializer::gen_rst_stream`.
`flush_pending_control_frames` (`h2.rs`) owns only the decision to drain and
the buffer it hands over — its RST_STREAM cap-check-and-drain stage calls that
method with room reserved at the end of the ordered output queue. This path is independent of the
owning `Stream` still being present in the wire map (`self.stream_table`), so
it survives the immediate `remove_dead_stream` call that every `reset_stream`
caller performs synchronously after return.

`finalize_write` (`h2.rs`) retains `Ready::WRITABLE` when the pass
completes but `pending_rst_streams` or `flow_control.pending_window_updates` are
still non-empty — otherwise a pass that ended before the RST_STREAM drain
stage (a stalled output flush, a GOAWAY) would strand a queued RST until an
unrelated event re-raised the writable bit. The decision is
`h2_close::finalize_action`'s `FinalizeAction::ArmControlQueue`;
`finalize_write` owns the bits and calls `Readiness::arm_writable`. This guard, together with the
invariant-15 arm
inside `enqueue_rst`, closes the 18-check h2spec 2.0 gap previously reported in
RFC 9113 §§5.3, 6.9, 8.1.2.

### 8.3 Session drain

`Mux::shutting_down` (`mod.rs`) is called by the server loop during process
shutdown or listener reload. It:

1. Initiates the double-GOAWAY (`Mux::shutting_down_inner`).
2. Drives frontend I/O outside the epoll loop (`drive_frontend_shutdown_io`,
   `mod.rs`) — H2 needs extra passes for the peer's END_STREAM and final TLS
   flush.
3. Checks the graceful-shutdown forced-close deadline: when
   `ConnectionH2::graceful_shutdown_deadline_elapsed` (`h2.rs`) returns
   `true` (i.e. `drain.started_at + drain.graceful_shutdown_deadline <=
   ConnectionH2.now`) the session queues the final GOAWAY
   (`ConnectionH2::goaway_before_forced_close`), flushes it once, best effort,
   and returns `true` so the server loop can tear the connection down even
   with Linked streams still in flight. The
   comparison is against the connection's snapshot, which `shutting_down`
   refreshes unconditionally at its top (§7.5) — including on the
   already-draining path, which never reaches `graceful_goaway`. Without that
   unconditional refresh a silent draining session would freeze `now` at its
   last `ready()` pass and the budget would never expire.
4. Marks `front_received_end_of_stream` on streams whose request is already
   complete and consumed.
5. Returns `true` when no `Linked` or non-quiesced `Unlinked` streams remain,
   and, on an H2 frontend, no peer header block is in progress and no stream
   awaits its link (`StreamState::Link`) — invariant 31.

The forced-close deadline is armed inside
[`H2DrainState::begin_graceful_drain`](h2_drain.rs), which
`ConnectionH2::graceful_goaway` (`h2.rs`) delegates the decision to —
`graceful_goaway` itself reads and writes none of `H2DrainState`'s five
private fields. `begin_graceful_drain` arms `started_at` from the `now` it is
handed, and the `debug_assert!` guarding that assignment states the invariant
the budget rests on:

```rust lib/src/protocol/mux/h2_drain.rs:207-210
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
do **not** arm `started_at` when they happen: the budget only applies to the
proxy's own soft-stop. A soft-stop that finds the connection already draining
skips `graceful_goaway`, so `Mux::shutting_down` arms the budget itself through
`H2DrainState::arm_deadline_if_unarmed`, once, from that pass's `now`
(`test_h2_client_goaway_then_soft_stop_honors_deadline`). A client GOAWAY on a
frontend connection leaves every in-flight stream running until it completes
(RFC 9113 §6.8: the sender's own streams are unaffected); `draining` refuses
new streams and the final GOAWAY follows once the stream table is empty
(`test_h2_client_goaway_keeps_in_flight_response`). Each received GOAWAY counts
toward the glitch budget, which bounds a peer that repeats it
(`test_h2_repeated_client_goaway_is_bounded`).

### 8.4 `Connection::end_stream` (backend-side retirement)

`ConnectionH2::end_stream` (`h2.rs`) is the server-side wiper for a single
stream that has completed on the backend. Behavior depends on `Position`:

- **Client** position (i.e. the backend's view) — the `Position::Client` arm
  of `ConnectionH2::end_stream` (`h2.rs`). Sends
  RST_STREAM(CANCEL) unless both request and response have terminated, or the
  request's HEADERS never left (`front.consumed` false), removes the wire
  mapping, marks the stream `Unlinked` if not already `Recycle`. A stream the
  backend has never seen gets no frame at all (sozu-proxy/sozu#1631): see §8.2.
- **Server** position — the `Position::Server` arm of `ConnectionH2::end_stream`
  (`h2.rs`; by symbol because `Position::Server => {` is one of eight
  identical lines). Dispatches on
  `end_stream_decision` (`shared.rs`): either `ForwardTerminated`,
  `CloseDelimited`, `ForwardUnterminated`, `SendDefault(status)`, `Reconnect`,
  or `ReplayOnFreshBackend` — each path sets the appropriate `StreamState` and
  schedules the frontend write.

**A backend close before the end of the response** (sozu-proxy/sozu#1633).
With `Connection: close` (`keep_alive_backend == false`) the backend's EOF
reaches `ConnectionH1::terminate_close_delimited` (`h1.rs`) from both EOF
branches of `ConnectionH1::readable`. Only a body with neither
`Content-Length` nor chunked coding (`BodySize::Empty`) is delimited by the
close (RFC 9112 §6.3 rule 8) and ends `Terminated`. A `Content-Length` body
still expecting bytes (rule 5) or a chunked body without its terminating zero
chunk (§7.1) is truncated: `warn!`,
`h1.backend_eof_before_message_complete`, and the kawa goes to `Error`. When
the backend goes away without that read path ending the body (a socket error,
a backend timeout), the `CloseDelimited` arm of `ConnectionH2::end_stream`
applies the same `body_size != BodySize::Empty` test, with the same `warn!`
and metric, and ends the stream in `Error`.

A response in `Error` then ends in error on both frontends, never with a
substitute answer once part of it left: `end_stream_decision` answers
`ForwardUnterminated` for a back kawa in `Error` whose `consumed` is set
(`Kawa::consume`, called by the H1 write path and the H2 converter), where
`SendDefault(502)` would have cleared the kawa and written a 502 behind the
bytes already sent — into the body of the 200 on H1, as DATA of the same
stream on H2. Before any byte left, the clean 502 stands. The H2 converter
turns `Error` into RST_STREAM(INTERNAL_ERROR). H1 has no stream reset, so
`ConnectionH1::writable` closes the frontend (`defer_close_for_tls_flush`,
write-only shutdown) once the errored response is flushed, never keeping it
alive: the client sees a close before `Content-Length`, the incomplete-message
signal of RFC 9112 §6.3, and no pipelined response can follow the truncated
body. `Mux::close` logs the stream as an error. Pinned by
`only_a_close_delimited_body_ends_cleanly_at_the_backend_eof` (`h1.rs`),
`an_errored_response_already_handed_to_the_client_is_not_replaced`
(`shared.rs`), `a_content_length_response_cut_short_ends_with_a_reset`
(`h2.rs`), and the e2e `test_h2_content_length_truncated_*`,
`test_h1_content_length_truncated_*` and `*_chunked_truncated_*` tests.

**The H1 client connection closes after that response even when it ended
cleanly** (sozu-proxy/sozu#1642). `HttpContext::on_response_headers` forwards
the backend's `Connection: close` to the client, so RFC 9112 §9.6 requires
sozu to close the connection after the response and to process no further
request on it; a close-delimited body, moreover, ends for the client only
with the close (§6.3 rule 8). The `Terminated` branch of
`ConnectionH1::writable` therefore keeps the connection only while both
`keep_alive_frontend` and `keep_alive_backend` hold, and otherwise takes the
same `defer_close_for_tls_flush("response-complete")` exit as a client
`Connection: close`: write-only shutdown once the response is flushed, after
its `H1::Complete` access log. A pipelined request already buffered in
`stream.front` is dropped with the connection rather than parsed, so its
response can never follow the close-delimited body; the client retries it on
a new connection (§9.3.2). Before this, the client waited for more body until
the frontend timeout and the next pipelined response was appended to the
body. This is an H1-frontend decision only: `keep_alive_frontend` is not
cleared, because an H2 client neither sees `Connection` (RFC 9113 §8.2.2) nor
needs the close to end the body, which carries END_STREAM. The H2 write pass
does not read `keep_alive_frontend` at all: a default answer whose template
carries `Connection: close` clears it, and on an H2 frontend that answer ends
its own stream and nothing else — no GOAWAY, the other streams continue
(sozu-proxy/sozu#1740). Pinned by the e2e
`test_h2_default_answer_terminates_stream` and
`test_h2_default_answer_502_spares_other_streams`. HAProxy's `h1_set_cli_conn_mode`
(`src/mux_h1.c`) closes the client on a response without a known length as
well; sozu does not re-frame such a body as chunked to keep the connection.
Pinned by the e2e `test_h1_close_delimited_body_closes_client`,
`test_h1_close_delimited_body_read_with_the_eof_closes_client`,
`test_h1_close_delimited_body_is_not_followed_by_a_pipelined_response`,
`test_h1_content_length_complete_then_backend_close_closes_client`,
`test_h1_framed_keep_alive_responses_keep_the_client` and
`test_h2_*_keeps_the_h2_connection`.

**Neither connection outlives a response that completed before its request**
(sozu-proxy/sozu#1721). A backend may answer early — a 413 or a 401 on an
upload — while the client is still sending the body, and so may a default
answer whose template keeps the connection. RFC 9112 §9.3 lets a connection
carry another message only once the previous one is complete, and the rest of
that body belongs to the request already answered. The same `Terminated`
branch of `ConnectionH1::writable` therefore also requires `stream.front` to be
terminated (received whole) before it keeps the client, and otherwise takes
`defer_close_for_tls_flush("request-incomplete")`. It does not require the
request to have been written upstream: a default answer leaves unsent a
request it never forwarded, and a keep-alive template stays keep-alive. The keep-alive reset
`stream.front.clear()` puts the parser back to its status line while
`front.storage` is kept for pipelining, so keeping the connection used to parse
the rest of the body as a new request — routed, edited and forwarded like any
other, a request-smuggling primitive. On the close path the parser is not
reset, so the remaining body bytes are read as the body they are and dropped
with the connection. Symmetrically, `ConnectionH1::end_stream` moves a
`Connected` backend to `BackendStatus::KeepAlive` only when the request is
terminated and completed as well as the response; otherwise it disconnects,
because that backend still expects the rest of the body and would read the
next request pooled onto it as that body. That backend guard serves both
frontends: an H2 frontend already defers `try_recycle_server_stream` until
the client's END_STREAM, but a client RST_STREAM after the whole response
reaches `end_stream` with the request still incomplete. An H2 backend takes a
different path: a stream it carries is never reused, and when one ends the
connection sends RST_STREAM(CANCEL) unless `back_received_end_of_stream &&
front.is_terminated() && front.is_completed()` (`fully_completed` in
`ConnectionH2`): the response's END_STREAM was read and the request's was
handed to the output, which flushes in order. Received whole is not enough: a
request whose last DATA frames or END_STREAM are still queued toward the
backend — a backend that answered early may never open the window for them
(RFC 9113 §8.1) — would otherwise be forgotten without a reset, leaving the
stream half-closed on the backend and holding one of its
MAX_CONCURRENT_STREAMS slots (§5.1.2) for the connection's lifetime. Pinned by
the unit `a_backend_stream_answered_before_its_request_was_written_is_cancelled`. The close costs a client that pipelined behind its upload
that pipelined request, which it retries on a new connection (§9.3.2).
Sozu does not drain the rest of the body to keep the connection, but it drains
it to close it: a socket closed with bytes still unread, or that receives more
after the close, makes the kernel send a reset that discards whatever part of
the response the client has not read yet. RFC 9112 §9.6 asks for a staged
close instead, so whenever this branch closes with `stream.front` not
terminated but started — part of the head or of the body arrived — it sets
`ConnectionH1::linger` to `Linger::Pending`. That covers `request-incomplete`,
a backend `Connection: close` response mid-upload, and the answers sozu
generates itself to a partly received request (400, 401, 413, 503, 504, or a
408 after part of a head): each lingers, bounded by `request_timeout`. A
request of which no byte arrived (the 408 `client_timeout` to a silent client,
whose `stream.front` is still at `StatusLine` with empty storage) has nothing
in flight to drain and closes at once, as before. Pending also ends the
linked backend stream through `endpoint.end_stream`, which disconnects that
backend. Once nothing is left to flush (the response, then any TLS
`close_notify`), `defer_close_for_tls_flush` calls `start_linger` instead of
returning `CloseSession`: it shuts the write side down with `shutdown_write`,
so the FIN follows the response, and leaves only READABLE in the interest.
`readable` then reads the raw socket and drops what it reads
(`drain_linger`), counting it in the frontend bytes-in metric, and `writable`
does nothing. `ConnectionH1::arm_timeout` is a no-op while lingering, so no
caller moves the deadline: not a read, and not the re-arm `Mux::timeout_inner`
runs after its write pass, which can itself start the linger when a timeout
answer (408, 503, 504) or a pending flush completes in it. The session
closes on the client's EOF — a frontend HUP makes `Mux::ready_inner` read to
that EOF rather than close at once — on a socket error, after `LINGER_MAX_BYTES` (4 MiB) have been dropped, or at the
deadline: the listener's `request_timeout` from the response's completion,
which `Mux::timeout_inner` enforces before any per-stream timeout handling.
Nothing is allocated for the drain, and `Mux::shutting_down_inner` leaves a
lingering session to its own deadline. Past either bound the close may still
reset; the bounds cap what one client can make sozu read after its response.
The close sets `stream.context.closing`, as every close this branch takes does:
a TLS close is deferred until `close_notify` is flushed and `writable` runs
again, and that pass then takes the `closing-context` exit instead of
completing the response a second time — which logged the request twice and
re-took the keep-alive decision after `close_notify`. Each close for an
incomplete request increments `http.close.request_incomplete`. Pinned by the
e2e `test_h1_early_response_mid_content_length_upload`,
`test_h1_early_response_mid_chunked_upload` and
`test_h1_early_response_survives_the_rest_of_the_upload`, and the unit
`a_backend_that_answered_before_the_whole_request_is_not_pooled`,
`a_deferred_tls_close_completes_the_response_once`,
`a_lingering_close_half_closes_then_drains_to_the_client_eof`,
`a_lingering_close_stops_at_its_byte_budget_and_its_deadline`,
`a_linger_started_by_a_timeout_write_keeps_its_own_deadline`,
`a_408_to_a_silent_client_closes_without_lingering` and
`a_silent_client_is_closed_at_the_linger_deadline`.

**An H2 trailer block on a `Content-Length`-framed message is dropped towards
an H1 peer only.** HTTP/1.1 carries a trailer section only with chunked coding
(RFC 9112 §7.1), a length-framed message ends with its last body byte (§6.3),
and the framing cannot become chunked once the header section is sent.
`pkawa::handle_trailer` validates, elides and queues the trailer block as for
any message, so `H2BlockConverter` forwards it to an H2 peer as a HEADERS
frame with END_STREAM. `ConnectionH1::writable` (`h1.rs`) calls
`ConnectionH1::drop_length_framed_trailers` before its `kawa.prepare`: for a
`BodySize::Length` message whose queue ends with a trailer block (the
`Header` blocks before a closing `Flags { end_header, end_stream }` that do
not follow a header section's `StatusLine` or `Cookies`), it removes the
fields and clears `end_header`, so kawa's H1 serializer writes nothing after
the body, for a request to an H1 backend and a response to an H1 client.
RFC 9110 §6.5.1 lets a recipient discard trailers; each dropped block logs a
`warn!` and increments `h2.trailers_dropped_content_length`, at the write, so
a stream reset before it is not counted. An H2 trailer block is queued whole
and kawa's H1 serializer drains the queue in one pass, so a block is never
split across writes. Before sozu-proxy/sozu#1730 the fields and a closing
empty line were written after the body. Pinned by
`a_length_framed_message_writes_no_trailer_to_an_h1_peer`,
`a_length_framed_header_section_is_not_taken_for_trailers`,
`a_length_framed_header_section_ending_with_cookies_is_not_taken_for_trailers`,
`a_header_only_head_response_is_not_taken_for_trailers` (`h1.rs`),
`a_length_framed_trailer_block_reaches_an_h2_peer` (`converter.rs`),
`handle_trailer_filters_and_keeps_a_length_framed_block` (`pkawa.rs`) and
`test_h2_length_framed_request_trailers_keep_h1_backend_framing`
(`e2e/src/tests/h2_security_header_injection.rs`).

**No end-of-body framing or trailer section follows the head of a response
without a body towards an H1 client.** A response to HEAD, a 204 or a 304 has no body by definition
(RFC 9110 §9.3.2, §15.3.5, §15.4.5), and an H1 client reads it as ending with
its header section whatever its framing fields say (RFC 9112 §6.3 rule 1), so
any byte after the head is read as the next response on a keep-alive
connection. The end of an H2 backend stream, an empty DATA frame or a trailer
HEADERS frame, queues `Flags` that kawa's H1 serializer writes as the last
chunk `0\r\n` under chunked framing, and as the trailer fields and an empty
line. For a
`Position::Server` pass whose `HttpContext` holds a HEAD method or a 204 or
304 status (`ConnectionH1::response_has_no_body`), `ConnectionH1::writable`
calls `ConnectionH1::drop_bodiless_response_framing` after
`drop_length_framed_trailers` and before its `kawa.prepare`: every block
after the header section (the last queued `StatusLine` up to its first
closing `Flags { end_header }`, or the whole queue once the head is written) loses
its `Header` fields and its `Flags` lose `end_body`, `end_chunk` and
`end_header`, so for a stream ended by a trailer HEADERS frame or an empty
DATA frame nothing is written after the head. Each dropped trailer
block logs a `warn!` and increments `h2.trailers_dropped_no_body`; a
`Content-Length`-framed one is still dropped and counted by
`drop_length_framed_trailers` first.

`pkawa::handle_header` gives a response with no content by definition no
framing: a response with a 1xx, 204 or 304 status, or to HEAD (which
`HttpContext::on_response_headers` has already marked `Terminated`), whose
header section arrives without END_STREAM gains no `Transfer-Encoding:
chunked` field and no chunked framing (RFC 9112 §6.1 forbids the field in a
1xx or 204). A `content-length` the backend sent is removed from a 1xx or a
204, where a server MUST NOT send it, and passed through on a 304 or a
response to HEAD (RFC 9110 §8.6). A 1xx is interim: like kawa's H1 parser,
`handle_header` marks it complete (`ParsingPhase::Terminated`) at its head, and
the final response follows on the same stream once the frontend has written
it (`ConnectionH2::handle_1xx_reset` on an H2 frontend). A 204, a 304 or a
response to HEAD stays open (`ParsingPhase::Body`) until the backend ends its
stream with an empty DATA frame or a trailer HEADERS frame (RFC 9113 §8.1):
released at its head, the backend stream would be reset by
`ConnectionH2::end_stream` while those frames were in flight, and the
closed-stream check of `ConnectionH2::handle_read` would answer the trailer HEADERS
with GOAWAY(STREAM_CLOSED), ending every stream of the backend connection; an
H2 frontend would also have sent the head without END_STREAM and retired the
stream, so the client never saw it end. The backend's END_STREAM is what ends
the response for every frontend: an H1 client reads the head alone, as above,
and an H2 client receives the head without END_STREAM, then an empty DATA
frame or the trailer HEADERS frame carrying it. A `:status 101` is malformed
in HTTP/2 (RFC 9113 §8.6): `handle_header` refuses it as a stream error
(PROTOCOL_ERROR), so the client gets a 502 rather than an upgrade attempt.
Before sozu-proxy/sozu#1776 such a head gained `Transfer-Encoding: chunked`
and chunked framing, and a 1xx never completed, so an H1 client read a chunked
103 and never the final response. Pinned by
`a_bodiless_h2_response_gains_no_transfer_encoding_towards_an_h1_client`
(`h1.rs`), `a_101_response_is_a_stream_protocol_error` (`pkawa.rs`),
`test_h2_bodiless_response_head_has_no_transfer_encoding`,
`test_h2_bodiless_response_ends_the_h2_client_stream`,
`test_h2_bodiless_response_end_keeps_the_backend_connection`,
`test_h2_backend_interim_response_reaches_the_client` and
`test_h2_backend_101_is_a_bad_gateway`
(`e2e/src/tests/h2_security_header_injection.rs`). A known gap remains: DATA
carrying a payload on a 204, a 304 or a response to HEAD is still forwarded,
written after the head to an H1 client and as DATA to an H2 client, because
`ConnectionH2::content_length_exempt` (`h2.rs`) skips the `content-length`
mismatch reset for HEAD, 204 and 304 and the chunks are not removed here. Pinned by
`a_bodiless_response_writes_nothing_after_its_head_to_an_h1_client`,
`a_bodiless_response_trailer_block_queued_after_its_head_is_dropped`,
`a_response_has_no_body_for_head_204_and_304_only` (`h1.rs`) and
`test_h2_bodiless_response_trailers_keep_h1_client_framing`
(`e2e/src/tests/h2_security_header_injection.rs`).

### 8.5 Stale-upstream replay (`ReplayOnFreshBackend`)

`end_stream_decision` splits "the backend closed without answering" in four,
on three questions: did the request reach the upstream, is it replayable, and
was it encoded for an H2 upstream?

| Request state | Upstream answered | Action |
|---|---|---|
| untouched (`!front.consumed`, `!front_bound_to_backend`) | no | `Reconnect` — re-link and route from the intact front kawa |
| encoded for an H2 backend, unsent (`!front.consumed`, `front_bound_to_backend`) | no | `SendDefault(502)` — its frames belong to that connection |
| written, replayable | no | `ReplayOnFreshBackend` — re-link and re-serialize the captured bytes |
| written, not replayable | no | `SendDefault(502)` |

An H2 backend's write pass HPACK-encodes the request into `front.out` and pops
its blocks (`kawa.prepare`) before any byte is written, and the socket may take
none of them. `front.consumed` is then still false, but the request is no
longer untouched: its frames carry that connection's stream id and field blocks
encoded against that connection's encoder table, and the blocks are gone, so it
can be neither sent on nor encoded for another connection. Re-linked, it used
to go out first on the new connection — possibly one `Router::plan_connect`
shares with the other streams of the same frontend connection — where the
block decodes against another table: another request's indexed fields, or a
COMPRESSION_ERROR/PROTOCOL_ERROR that takes the whole connection down
(sozu-proxy/sozu#1632). Both stay within one frontend connection, since a
backend connection belongs to one `Mux` session. `Stream::front_bound_to_backend`
records it: set by the `Position::Client` prepare of
`ConnectionH2::poll_write_target` when it leaves output, cleared with the front
kawa where a slot takes a new request (`Context::create_stream`'s reuse,
`ConnectionH1`'s keep-alive reset). `ConnectionH2::handle_goaway_frame` reads
it too: a stream above `last_stream_id` was not processed (RFC 9113 §6.8), but
a bound one is answered a whole `503` (the backend refused it unprocessed,
RFC 9110 §15.6.4) instead of re-linked. Not REFUSED_STREAM: its answer,
`forcefully_terminate_answer`, is an empty response kawa in `Error`, which an H2
frontend turns into a RST_STREAM but an H1 frontend cannot write at all, and
the backend core cannot tell which frontend it serves
(`an_h1_client_behind_a_goaway_refusing_its_encoded_request_gets_a_response`). As defense in depth,
`ConnectionH2::start_stream` refuses a request whose `front.out` already holds
output, before allocating an id. An H1 backend never sets the flag: its output
is protocol bytes a fresh H1 connection sends as they are, so its untouched
requests keep `Reconnect`. Re-encoding from the kawa is not possible without
keeping a copy of every request's blocks on the nominal path; hyperium/h2
avoids the question by encoding at send time (`FramedWrite::buffer`). Pinned by
`a_request_encoded_for_a_backend_is_refused_on_goaway_not_relinked`,
`a_request_encoded_for_a_lost_backend_is_answered_not_relinked`,
`a_request_encoded_for_one_backend_never_reaches_another` (`h2.rs`, red on
`d5161919`), `a_request_encoded_for_an_h2_backend_is_not_reconnected` and
`an_untouched_request_prepared_for_an_h1_backend_is_still_reconnected`
(`stream.rs`).

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
observed by the client, so re-issuing is unobservable. It is narrowed by four
further conditions, the first three carrying their precedent and the fourth a
memory bound:

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
  `Method::Custom` — which is how sozu parses `PATCH`, and, since
  sozu-proxy/sozu#1451 made `Method::new` case-sensitive, also how it parses a
  lowercase `get` — counts as non-idempotent. A non-canonical spelling is
  therefore NOT replayable: the origin receives those exact bytes and may give
  them semantics of its own. Enforced TWICE since sozu-proxy/sozu#1450:
  `Stream::arm_upstream_replay` refuses to capture a non-idempotent request at
  all, so it spends no capture budget, and `can_replay_on_fresh_upstream`
  keeps its own conjunct as defense in depth. The method is known at arm time
  — `HttpContext::on_request_headers` sets it during the frontend parse, and
  `Router::plan_connect` routes on it before reaching `start_stream`.
- **One front buffer.** `ConnectionH1::writable` copies the bytes it hands the
  socket into `Stream::retry_buffer` before `kawa::Kawa::consume` drops them,
  and truncates the capture to `None` past `front.storage.capacity()`. HAProxy
  states the same trade: retrying past `conn-failure` "requires to allocate a
  buffer and copy the whole request into it", and "Requests not fitting in a
  single buffer will never be retried". That guard bounds the capture's
  *length*, not its capacity: the copy grows through `Vec::reserve`, which
  allocates `max(2 * old_capacity, required)`, so a capture assembled over
  several partial writes can have allocated twice what it carries (measured:
  one 16393-byte write gives `len/cap 16393/16393`, `[8192, 8192, 9]` gives
  `16393/32768`). Every heap figure here says which of the two it is.
- **Capture budget available** (`MAX_ARMED_REPLAY_CAPTURES`, `stream.rs`, 512
  per worker). A capture is a plain allocation on the global allocator,
  outside the buffer `Pool`, so no `max_buffers` accounting sees it. Its only
  bound used to be transitive — a capture exists only on a live `Stream` and
  `Stream::new` takes two `BufferSource::checkout` calls onto the same `Pool`, so `max_buffers / 2` of
  them, ≈8.2 MB carried at the defaults and linear in `max_buffers` from
  there. The ceiling makes it a constant instead: `512 * 16400` ≈ 8.4 MB
  carried, ≈16.8 MB allocated, whatever `max_buffers` becomes. 512 sits just
  above the 500 the defaults can reach, so a stock deployment replays exactly
  what it replayed before. Past the ceiling `arm_upstream_replay` installs no
  buffer and increments `backend.retry.captures_declined`; the request
  proceeds un-replayable and a stale upstream answers the 502 it answered
  before the replay existed. Nothing is refused and no frontend parks — which is
  precisely what pool-allocating the captures would have cost under the same
  memory pressure, and why they are not pooled. The charge is owned by the
  `ReplayCapture` newtype and released by its `impl Drop`, so it is released
  wherever the capture is, including the `Stream` simply being dropped.
  `doc/configure.md`'s "Capture budget" carries the operator-facing version.

`CONN_RETRIES` is what bounds how many times a replay may be RE-ISSUED, but it
is no longer the only thing that decides whether a replay happens at all —
the capture budget above can decline the capture before any of this is
reached. Given a capture, the capture is *taken*, not cloned, so it does not
survive its own replay — but that does not make one request one replay:
`start_stream` re-arms a fresh capture on every `KeepAlive -> Connected`
transition and `reused_from_pool` is never cleared, so a replay that lands on
another pooled connection may itself be replayed, budget permitting. The bound
on that is the re-link going back through `Router::plan_connect`, whose
`stream.attempts >= CONN_RETRIES` gate (3, `server.rs`) answers 503 once the
budget is spent. `backend.retry.stale_upstream` can therefore increment more
than once for one client request.

`Router::plan_connect` skips `route_from_request` on a replay — `front.consumed` is
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
and appending behind it would put `[tail][head]` on the wire. Once queued they
live in `front.out` as an owned `kawa::Store::Alloc` — still off-pool, and no
longer charged to the capture budget, which `queue_upstream_replay` releases
as it takes the capture. That window is bounded by `CONN_RETRIES` and by the
same one-front-buffer limit the bytes were captured under, and it is not
separately accounted.

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
   (`H2StreamTable.streams`), `gid < context.streams.len()`.
2. **Backend index consistency.** If
   `context.streams[gid].state == StreamState::Linked(token)`, then
   `context.backend_streams[&token]` contains `gid`. Asserted under
   `debug_assertions` at the end of `Mux::ready_inner`. The converse
   holds for every non-empty entry; an emptied entry is capacity kept for the
   next link (#1583) and must name a connection still in `router.backends`.
3. **`expect_write` validity.** If
   `expect_write == Some(H2StreamId::Other { gid, .. })`, then
   `gid < context.streams.len()` and the slot at `gid` is not `Recycle`. Every
   stream-removal site MUST null `expect_write` out before returning; the helper
   is `remove_dead_stream` (`h2.rs`). Manual-removal sites (§5.4) must
   replicate the check inline.
4. **`expect_read` validity.** Same as (3) for `expect_read`.
5. **Recycled slot cleanliness.** A `StreamState::Recycle` slot has cleared
   `front`, `back`, `front.storage`, `back.storage`, reset metrics
   (the slot-reuse branch of `Context::create_stream`, `mod.rs`).
6. **No stale `Linked` after backend close.** Before transitioning a stream away
   from `Linked(token)`, call `unlink_stream` (`mod.rs`) or
   `remove_backend_stream` (`mod.rs`) to keep the reverse index honest.
7. **No duplicate QUEUED RST_STREAM.** Scoped to the queued path, which is
   not every RST_STREAM this proxy emits. `converter.rs` serializes one
   straight into `kawa.out` from its `initialize` chokepoint, from the
   trailer-error arm, and from the HPACK over-budget abort; `ConnectionH2::end_stream`
   queues a `Cancel` frame in the ordered output queue directly. Those four sites
   bypass the queue and this invariant says nothing about them — each carries
   its own `rst_sent_contains` guard instead.
   On the queued path, `H2ControlTx::enqueue_rst` (`h2_control_tx.rs`) is the
   chokepoint: it inserts the id into `rst_sent` and short-circuits when the
   insert reports the id was already present, so the same stream cannot be
   queued twice. Callers that must not re-emit consult
   `self.stream_table.rst_sent_contains(sid)` directly.
   Cited by symbol rather than by line on purpose — the enforcement is one
   `HashSet::insert` whose surrounding statements are generic, so a line
   number here would anchor on text that says nothing about the rule. Do not
   convert it back.
8. **No new streams during drain.** `ConnectionH2::create_stream` and
   `ConnectionH2::start_stream` both short-circuit when `self.drain.draining()`
   (`h2.rs`).
   Cited by symbol rather than by line on purpose — in each method the guard
   precedes every stream allocation and is the only `drain.draining()` test in
   the body, so the method name locates it as precisely as a range did while
   surviving every insertion above it. Measured: a 21-line insertion into
   `ConnectionH2::initiate_close_notify` moved the two ranges this replaces onto
   `self.flush_tls_records()` and a bare `context` argument, and both still
   RESOLVED — `--root` flagged neither. Only the drifted-citation rule, which
   `ci.yml` supplies a `--base` for on a pull request, would have caught them,
   and what it then asks for is a re-anchor: exactly what sozu-proxy/sozu#1501
   and #1497 each performed faithfully on this file's HEADERS citation and each
   landed on the wrong statement. Do not convert it back.
9. **Connection-level timer resets only on application activity.** H2 control
   frames (PING / WINDOW_UPDATE / SETTINGS) do **not** push
   `ConnectionH2.timeout_deadline` out. `arm_timeout()` has exactly three call
   sites: DATA payload (the `H2StreamId::Other` arm of `poll_read_target`), HEADERS
   (inside `handle_headers_frame`), and the `H2StreamId::Other` arm of
   `handle_write`, gated on `size > 0` — outbound application data is activity
   too, and that third site is why the READ path alone does not describe the
   invariant.
   The third site used to be the first statement of `write_streams`, and there
   the invariant was false in exactly the case it exists for
   (sozu-proxy/sozu#1489). Reaching `write_streams` is not evidence that
   application data is being written: `writable()` dispatches into it for every
   proxying state, and a connection whose only queued output is the PING or
   SETTINGS acknowledgement `flush_pending_control_frames` just drained is in
   `H2State::Header` like any other. So a PING-only peer renewed the connection
   deadline indefinitely — the Slowloris shape this invariant refuses — while
   WINDOW_UPDATE, which queues no acknowledgement and therefore gave the write
   path nothing to do, correctly did not. Keying the arm on a real stream id
   AND on bytes the socket took is what makes a control frame reach none of the
   three.
   Pinned by `a_ping_only_peer_does_not_postpone_the_connection_deadline`,
   `a_settings_only_peer_does_not_postpone_the_connection_deadline`,
   `a_write_pass_that_moves_stream_bytes_arms_the_connection_deadline` and
   `a_draining_write_pass_that_moves_stream_bytes_still_arms_the_deadline`.
   Each pairs its negative with a positive leg on the same connection: a build
   that never arms is the opposite defect — an unconditional session cap — and
   a "the deadline did not move" assertion alone cannot tell the two apart.
10. **Single `graceful_goaway` per session outside the final GOAWAY.**
    `Mux::shutting_down_inner` (`lib/src/protocol/mux/mod.rs`) only calls it if
    `!self.frontend.is_draining()`; a second unconditional call would
    collapse the initial GOAWAY into the final one and disconnect
    in-flight streams. The one final GOAWAY `Mux::shutting_down_inner` sends
    itself goes through `ConnectionH2::goaway_before_forced_close`, and only
    once the forced-close deadline has elapsed and the session is closing
    anyway (invariant 31).
11. **Frontend HUP defers close when output is pending.**
    `delay_close_for_frontend_flush` (`mod.rs`) must be consulted before
    returning `SessionResult::Close` so that unflushed TLS/GOAWAY records are
    not lost. Neither HUP check of `Mux::ready_inner` consults it while
    `Mux::frontend_exchange_in_flight` holds: it queues `close_notify` on its
    first line, and a half-closed client (RFC 9293 §3.6) is still owed the
    whole response ([#1779](https://github.com/sozu-proxy/sozu/issues/1779)).
12. **Loop budget.** Every inner loop in `Mux::ready` and
    `drive_frontend_shutdown_io` bounds iterations at
    `MAX_LOOP_ITERATIONS = 10_000` (`mod.rs`). In `ConnectionH1::writable`
    (`h1.rs`), a write that did not answer `SocketResult::Continue` never
    re-raises its own WRITABLE event: a `WouldBlock` is resumed by the
    kernel's next edge, and an `Error` or `Closed` leaves nothing to retry.
    It signals a pending write while the TLS socket still holds records only
    after a write that answered `Continue`; re-raising it after a blocked one
    spun the inner loop to the budget when a TLS client stopped reading
    ([#1780](https://github.com/sozu-proxy/sozu/issues/1780)).
    `ConnectionH2` follows the same rule through
    `ConnectionH2::ensure_tls_flushed`: it raises no synthetic WRITABLE when
    the latest socket write of the pass answered `WouldBlock`, and consumes
    the event instead ([#1788](https://github.com/sozu-proxy/sozu/issues/1788);
    see "Keeping the connection open is not re-arming it" below). On a TLS
    frontend an `Error` write also marks the channel dead
    (`FrontRustls::peer_reset`, `lib/src/socket.rs`), so the records rustls
    still holds stop reading as a pending write on both protocols.
13. **`shrink_trailing_recycle` runs only from `create_stream`.** Calling it
    from elsewhere can invalidate cached `GlobalStreamId` values (including
    `expect_write`/`expect_read`) that the caller is not prepared to re-check.
    Keep the single call site, in `Mux::create_stream` (`mod.rs`).
14. **Backend `active_requests` balance.** Every site that removes a stream
    outside `Connection::end_stream` must release the charge that stream's
    creation took (`ConnectionH2::cancel_timed_out_streams`,
    `ConnectionH2::handle_rst_stream_frame` and
    `ConnectionH2::handle_goaway_frame`; all `h2.rs`).
    Otherwise load-balancing counters drift monotonically.

    The core does not perform the release; it **emits** it. Question 12 of
    [#1340](https://github.com/sozu-proxy/sozu/issues/1340) took
    `Rc<RefCell<Backend>>` out of `Position::Client`, which now carries an
    opaque `BackendId`, so no datapath site can reach the worker's registry.
    Each of them records a `BackendChange` on `Context::backend_deltas`
    through `Context::record_backend_delta`, and `Mux::apply_backend_deltas`
    is the single writer that performs them against
    `Mux::backend_registry`. The pair-assertions that used to sit at each
    site live in `BackendRegistry::apply`, where both halves are observable.

    That makes the invariant a property of one observable list rather than of
    six scattered mutations: summing a ledger's `StreamsStarted` against its
    `StreamsEnded` is the balance itself. `h2.rs`'s
    `invariant_14_*` tests reconcile it across the nominal, refused-start,
    `RST_STREAM`, `GOAWAY` and idle-reap paths, and then apply the ledger and
    assert the counter returns to zero.

    Two ordering rules the emit/apply split has to keep, both load-bearing:

    * **Drain before any read.** The load balancer `Router::backend_from_request`
      reaches through its `BackendSelector` is the only reader of
      `active_requests`, `active_connections` and `connection_time` on this
      path. `Mux::ready_inner` drains immediately before each
      `Router::plan_connect`, and the four `Mux` wrappers that owe the timer wheel
      a reschedule (`ready`, `timeout`, `close`, `shutting_down`) drain on the
      way out. FIFO application plus drain-before-read leaves the counters at
      every read point exactly where mutating in place left them, saturation
      included. Both drains go through `BackendRegistry::apply_all`, which
      empties the queue as it performs it, so a second drain is idempotent
      rather than a double charge. It drains in place rather than taking the
      `Vec`, so the ledger keeps its capacity and the charge
      `Connection::start_stream` records on every request does not reallocate
      it (#1583).
    * **Selection reserves, failure releases.** `active_connections` is
      raised by selection itself, not by the dial: `BackendMap::reserve_backend`
      (`lib/src/backends.rs`) takes the reservation under the same borrow as
      the choice, so the next selection weighs it before anything connects —
      the view a select-and-dial in one call gave, without the dial in the
      selector ([#1684](https://github.com/sozu-proxy/sozu/issues/1684)).
      `Mux::dial_backend` then connects to the reserved backend. A
      `connect(2)` that fails pushes `BackendChange::DialFailed`, whose
      application releases the reservation and records the failure
      (`failures += 1`, `retry_policy.fail`); the drain before the next
      selection applies it, so that selection sees the backend exactly where
      a failed `Backend::try_connect` left it. A dial that connects keeps the
      reservation as its connection's count, and the connection's
      `ConnectionClosed` releases it.

      A dial that connects but is then abandoned releases it too
      ([#1713](https://github.com/sozu-proxy/sozu/issues/1713)). Every exit
      past a successful connect lives in `Mux::attach_dialed`:
      `Connection::new_h2_client` returning `None` (buffer pool exhausted),
      a refused `Connection::start_stream`, and the `register_socket`
      rollback, whose `Connection::release_start_stream_charge` pairs any
      `active_requests` charge `start_stream` took — none for a fresh dial,
      which is still `BackendStatus::Connecting` and was never charged.
      `Connection` has no
      `Drop`, so dropping the connection releases nothing; instead
      `Mux::dial_backend` pushes one `ConnectionClosed` for whichever of them
      returned `Err`, a single release point a new exit cannot bypass. It is
      not `DialFailed`: these are local failures, and recording one against
      the backend's `failures` and `retry_policy` would steer traffic away
      from a healthy backend. Before #1713 all three leaked the reservation,
      as they had leaked the count `Backend::try_connect` took before #1684.
      Pinned by
      `a_second_selection_observes_the_first_selections_reservation`,
      `power_of_two_weighs_every_reservation`,
      `a_failed_dial_releases_its_reservation_and_records_the_failure`,
      `a_synchronously_failed_dial_releases_its_reservation`,
      `a_successful_dial_counts_its_connection_once`,
      `an_h2_client_the_pool_cannot_build_releases_its_reservation`,
      `a_refused_stream_start_releases_its_reservation` and
      `a_socket_the_poller_refuses_releases_its_reservation` (`router.rs`).
    * **Charge only a stream that started.** `Connection::start_stream`
      records `StreamsStarted` *after* the inner start returns true. The
      pre-#1340 shape charged first and undid the charge on refusal; a ledger
      holding a `+1` and a compensating `-1` also reconciles to zero, so
      `invariant_14_a_refused_start_stream_charges_nothing` asserts the
      ledger is EMPTY rather than balanced.

      Moving the charge after the inner call **fixes a standing violation of
      this invariant on the H1 keep-alive path**, and is the one behaviour
      change here. The charge is guarded on `BackendStatus::Connected`, and a
      connection coming back out of the pool is `BackendStatus::KeepAlive`
      until `ConnectionH1::start_stream` flips it — so charging beforehand
      skipped the reuse entirely, while the matching `Connection::end_stream`
      released one (it runs before `ConnectionH1::end_stream` flips the status
      back, so its own guard passes). Every keep-alive reuse cycle net `-1`d
      the backend, `saturating_sub` floored it at zero, and a reused H1
      connection reported no in-flight request while it served one — biasing
      least-loaded and PeakEWMA balancing toward backends that are already
      busy. Pinned by `invariant_14_an_h1_keep_alive_reuse_is_charged`.

    The ledger is not core-only, and cannot be: a fresh dial's first streams
    link while the connection is still `BackendStatus::Connecting`, where
    `Connection::start_stream`'s `Connected` guard skips them. `Mux`'s
    `Connecting` -> `Connected` transition is what charges them, so it emits
    `StreamsStarted(n)` onto the same list — as do the dead-backend arm and
    session teardown, each emitting the `StreamsEnded(count)` its
    `Context::backend_streams` entry owes. The health and latency writes
    (`Backend::failures`, `retry_policy`, `set_connection_time`) are NOT part
    of this balance and stay immediate at their embedder-side call sites,
    because the load balancer reads `retry_policy`. The retry policy and the
    connection-time average read no clock or ambient RNG: they take the
    pass's `Context::now`, and a failure draws its backoff jitter from the
    worker's `BackendMap` generator, the one that seeds every load-balancing
    policy. The connection time itself is still measured as
    `start.elapsed()`
    ([#1684](https://github.com/sozu-proxy/sozu/issues/1684)). Selection
    itself judges backoff windows and connection-time decay against the same
    `Context::now`, which `RegistrySelector` carries into
    `BackendMap::reserve_backend`.

    A `BackendId` names a session-scoped slot rather than the backend's id or
    address. A reload that replaces a registry entry mid-session builds a new
    `Rc`, which takes a new slot, so connections dialled before it keep
    charging the handle they were dialled against — the behaviour holding the
    `Rc` inside `Position::Client` used to give. Keying on the backend's name
    would split a balanced pair across two `Backend` values, leaving the
    superseded one charged forever and the fresh one saturating at zero.
    The slot also keeps the two identity fields a `BackendId` carries, read
    once when the session interns the backend, so minting the id for every
    later dial of it bumps a reference count instead of copying the backend's
    name into a fresh `Rc<str>`
    ([#1564](https://github.com/sozu-proxy/sozu/issues/1564)). The copy cannot
    go stale for the reason above: a replaced backend is a new `Rc` in a new
    slot. `a_redial_of_an_interned_backend_allocates_nothing` (`mod.rs`) pins
    the redial at zero allocations.
15. **Incremental scheduler — solo-bucket non-yield.** The converter
    (`converter.rs` `H2BlockConverter::call` DATA arm) must only yield after a
    DATA frame when `incremental_mode == true` _and_
    `incremental_peer_count > 1`. A solo incremental stream with no peer to
    interleave with must drain sequentially — otherwise `finalize_write`
    (`h2.rs` `fn finalize_write`) would withdraw `Ready::WRITABLE` on a clean
    pass (no `expect_write` set by the converter) and edge-triggered epoll would
    never re-fire. `ConnectionH2::poll_write_target` populates both fields per
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
    `any_stream_has_pending_back` (`h2.rs`), which `finalize_write` hands to
    `h2_close::finalize_action` as a closure rather than a `bool` so the walk
    stays behind the same short-circuit it had inline. The decision is that
    function's `FinalizeAction::RetainPendingBack` (keep the bit) versus
    `FinalizeAction::Quiesce` (withdraw it); `finalize_write` performs whichever
    it is handed. The table is
    `h2_close::tests::invariant_16_retains_writable_only_with_progress_and_pending_back`. A voluntary scheduler yield (RFC
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
    before a later `H2WritePhase::Prepare` reads it. Three sites in
    `ConnectionH2::poll_write_target` call
    `ReadyIncrementalCensus::note_ineligible` so later same-urgency peers do
    not read the stale snapshot: the pre-prepare `rst_sent` insert, the
    post-prepare HPACK over-budget insert, and `H2WritePhase::Flush`'s
    `completed_streams.push` path. That method is where the
    non-incremental guard and the `saturating_sub` live, so a fourth site
    cannot get either wrong by copying them.
    Post-loop the connection stores `ReadyIncrementalCensus::ready_total` in
    `ready_incremental_streams` and publishes it through
    `gauge_connection_state`, which returns
    `h2.streams.ready_incremental.by_urgency` as a signed `MetricEvent` delta
    for the shell to record, so the aggregate sums across live connections;
    `impl Drop for H2Shell` subtracts the contribution on teardown. The sample is taken in
    `H2WritePhase::End` because the entry call to `gauge_connection_state`, in
    `ConnectionH2::begin_scheduler_pass`, runs before the pass census exists. Guarded by e2e test
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
    Every hit must be inside `#[cfg(test)] mod tests` (`h2.rs`) or
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

    **A grep over these three spellings cannot see a clock reached through a
    dependency.** `ConnectionH2::create_stream` called `Ulid::generate()` for
    the request id, and `rusty_ulid` 2.0.1 defines that as
    `Ulid::from_timestamp_with_rng(unix_epoch_ms(), &mut rand::rng())` — a
    wall-clock read AND a random draw, both one crate away. All three greps
    returned zero hits for that line throughout, and the invariant above read
    as satisfied while the reach was live. It now calls
    `Context::next_request_id`, which composes the same ULID from
    `Context::now_wall_ms` (the wall-clock sibling of `Context::now`,
    refreshed by `Mux` at the same three sampling points) and
    `Context::request_id_rng` (seeded once per session in `Context::new` from
    the OS-backed `rand::rng()`). The general rule this leaves behind: a
    reference sweep for an ambient reach must enumerate what each EXTERNAL
    call does, not only match the spellings a local reach would use — the
    second half of the criterion the grep alone cannot express. Pinned by
    `h2_request_ids_are_a_pure_function_of_the_seed`
    (`sim/tests/h2_simulation.rs`), which is red the moment the call site goes
    back to `Ulid::generate()`.

    A second consequence worth stating, because it is easy to read the wrong
    way: `Mux` now refreshes TWO fields at each of its three sampling points,
    `Context::now` and `Context::now_wall_ms`. They are siblings. A future
    sampling point that sets one and not the other lets a request id carry the
    timestamp of an older pass — correct-looking, monotone, and wrong.

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
    `ConnectionH2::{handle_write,handle_headers_frame}` pushes that deadline
    out on the first pass a backend actually moves bytes on, but nothing hangs
    on it — invariant 9.) That is what covers the
    pool-reuse branch of `Router::plan_connect` never arming a timeout, unlike the
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
      (the match arm in `ConnectionH2::handle_read`) — before `handle_frame`
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
      `handle_headers_frame`'s RFC 7540 §5.3.1 PRIORITY self-dependency
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

    **Amended by #1604 (invariant 28) and #1637: all six guards below are
    gone, because what they guarded is gone.** Control frames are now
    serialised into the separate ordered output queue, `zero` is input only,
    and no write-side site clears or reuses `zero.storage` any more. #1604
    removed five; the sixth, `graceful_goaway`'s defer-or-send decision, was
    kept as a drain policy until #1637 removed it together with
    `header_block_reassembly_in_progress()` itself (§2.2). The tests listed
    below still pin the outcome, a block that survives every interleaved
    flush. The paragraph that follows is the account the six guards were
    written against.

    **All six `header_block_reassembly_in_progress()` call sites were
    load-bearing (before #1604).** `ConnectionH2::header_block_reassembly_in_progress()`
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
    `h2_drain::H2DrainState::initial_goaway_pending` was the flag
    `begin_graceful_drain` set so a `graceful_goaway` landing
    mid-reassembly deferred its advisory GOAWAY, drained by
    `flush_pending_control_frames` via `take_deferred_initial_goaway` once
    reassembly completed. Pinned by
    `a_legitimate_continuation_survives_an_unrelated_window_update_flush`
    (the WINDOW_UPDATE-stage case, #1397),
    `a_legitimate_continuation_survives_a_graceful_goaway` (the GOAWAY case,
    #1401; since #1637 it also reads the advisory GOAWAY before the block
    completes), `a_legitimate_continuation_survives_a_hup_while_draining` (the
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
    `ConnectionH2::poll_write_target` builds an `H2BlockConverter` for exactly ONE
    `kawa.prepare` call, not one per pass, so its `&mut self.hpack` borrow
    never spans the per-stream phases and every `&self` / `&mut self` method
    stays callable inside it. Two properties that the single long-lived
    converter used to hold structurally are now carried across the streams of
    a pass by `converter::H2ConverterPass` and MUST stay true:
    - **One encoder.** Every converter of a pass borrows the SAME
      `hpack::Encoder` through `HpackState::encoder_mut`, so the dynamic
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
      is what stops a second stream re-emitting it. `H2WritePhase::End` takes
      `H2ConverterPass::pending_table_size_update` back as
      `ConnectionH2::pending_table_size_update`: `None` once a block carried
      the signal, unchanged after a DATA-only pass, which keeps it queued,
      and re-armed when `H2BlockConverter::check_header_capacity` dropped a
      block and reset the encoder's table (#1627, invariant 28). The prefix is one or
      two updates: `Encoder::encode_size_updates_into` puts the smallest size
      the peer set since the last block before the last one when it is lower
      (RFC 7541 §4.2).

    Neither property is visible to `converter.rs`'s own unit tests, which
    drive one converter over one kawa. Both are pinned on the wire by
    `one_write_pass_prefixes_its_first_header_block_only_and_shares_one_encoder`
    (`h2.rs`), whose last assertion is the negative half — a peer decoder that
    never saw the first block must FAIL on the second — and generalised over
    2..=5 streams and the advertised table size by
    `write_pass_property::qc_one_write_pass_prefixes_its_first_header_block_only`
    (`h2.rs`).

26. **RFC 9218 §4 incremental leadership rotates inside EVERY urgency
    bucket — rotate AND commit, per bucket, or a stream starves.** Scheduler
    fairness has two halves and neither is sufficient alone.
    `Prioriser::apply_incremental_rotation` (`h2_scheduler.rs`), called from
    `H2Scheduler::begin_pass`, rotates each urgency bucket's incremental tail
    to start at the first stream id *strictly greater* than **that bucket's
    own entry** in `Prioriser::incremental_cursor`, wrapping;
    `H2Scheduler::end_pass` then commits each of those cursors to the
    matching entry of `ReadyIncrementalCensus::first_incremental_fired` — the
    first incremental stream **of that bucket** which actually **consumed
    send window**. Together they advance leadership exactly one position per
    pass in every populated bucket, so K ready incremental peers sharing a
    bucket each lead once every K passes and none waits longer than K-1,
    whatever the bucket's urgency and whatever the other buckets are doing.

    Each half fails differently and both failures are starvation, not a
    slowdown. Rotating without committing re-reads the same cursor forever
    and the smallest id in the bucket leads every pass. Committing without
    rotating leaves the ascending order untouched and does the same. And
    `consumed > 0` is load-bearing in its own right: a permanently
    window-blocked stream that "led" without sending would hand the cursor
    onward every pass while its peers never actually got a turn either.

    **Per bucket is the scope, and the connection-global shape it replaced is
    named here so no later change restores it** (sozu-proxy/sozu#1456). With
    one cursor for the whole connection, `end_pass` could only ever commit an
    id drawn from the lowest-numbered urgency bucket that had a ready
    incremental stream fire — the pass order is ascending by urgency, so the
    first stream to consume window came from there. Every other bucket was
    rotated by a cursor from a foreign id range, and when that cursor sat
    entirely below or entirely above the bucket's own ids `partition_point`
    returned a constant and the bucket never rotated at all. Measured on that
    shape: u=0 incremental `{1, 3}` beside u=3 incremental `{5, 7}`, all
    ready and all consuming, six passes — u=0 alternated `1, 3, 1, 3, …`
    while u=3 was `5, 7` in every pass and stream 7 never led. That was
    positional starvation, and a stalled flush at stream 5
    (`H2WritePass::stalled`, which ends the pass) turned it into byte
    starvation for 7, pass after pass. The same six passes now read
    `[1,3,5,7]` / `[3,1,7,5]` alternating, and the change is visible on the
    wire: on a connection with two populated incremental buckets the trailing
    bucket's frame order was stable and now rotates.

    Three things carry the scope, and the third is the one that was missing:
    `incremental_cursor: [StreamId; 8]` — exact rather than a cap, because
    RFC 9218 §4.1 urgency is `[0, 7]` and `Prioriser::push_priority` clamps
    into it; a per-bucket `first_incremental_fired` in
    `ReadyIncrementalCensus`; and `ReadyIncrementalCensus::note_fired` taking
    the **urgency**, without which the census cannot attribute the firing
    stream to a bucket at all and can only name one leader per connection.

    The census of invariant 17 must stay bucket-scoped for an unrelated
    reason: a connection-global peer count starves nothing, but it makes a
    solo incremental stream yield to a peer it can never interleave with,
    which is the invariant-15 strand.

    Pinned deterministically by `incremental_leadership_rotates_one_position_per_pass`
    (four peers, eight passes, the exact leader sequence),
    `no_incremental_peer_is_starved_over_a_full_cycle` (five peers, ten
    passes, each leading exactly twice — deliberately five rather than two,
    because a rotation bug that swaps a pair still looks fair on two streams
    and starves the fifth), `a_window_blocked_stream_does_not_take_the_lead`,
    `a_pass_with_no_incremental_progress_leaves_the_cursor_alone`,
    `test_advance_incremental_cursor_is_per_bucket` (one bucket's committed
    leader moves no other bucket's cursor) and
    `every_urgency_bucket_rotates_its_own_incremental_tail` (two buckets of
    two, six passes, both tails alternating); and generalised over 2..=8
    peers, their ids and gaps, the shared urgency bucket, 1..=4 full cycles,
    a second multi-peer incremental bucket at strictly lower priority and a
    distractor set by
    `fairness_property::qc_incremental_leadership_visits_every_peer_once_per_cycle`,
    whose oracle now checks the cyclic successor for **every** bucket it
    generated rather than the main one alone. All of them are in
    `h2_scheduler.rs`; each carries a `TO SEE THIS RED` recipe naming the
    exact statement to mutate and the panic it produces. The wire half is
    `h2_correctness_tests.rs`'s `test_h2_per_bucket_incremental_rotation`,
    which reads the trailing bucket's leader out of the DATA frame trace.

    The composition is what pins the pair. At the commit that extracted the
    scheduler, deleting the `advance_incremental_cursor` call site from
    `ConnectionH2::write_streams` left the entire `protocol::mux` suite green
    — 402 passed, 0 failed. The rotation primitive was pinned; the
    rotate-and-commit pair was not. And restoring the connection-global
    commit — the lowest urgency's leader written to every bucket — reds
    exactly three of the 1093 `sozu-lib` tests today, all three of them the
    coverage this invariant's per-bucket scope added.

27. **A close under TLS backpressure asks TWO questions, not one, and asks
    the second after a flush it performed itself.** Three sites decide
    whether an H2 connection may close while rustls may still hold encrypted
    records — the `H2State::GoAway` arm of
    `ConnectionH2::dispatch_writable_state`, the
    `(H2State::Error, Position::Server)` arm beside it, and
    `ConnectionH2::force_disconnect`'s server arm. Closing with records
    pending sends FIN, destroys them, and the client reads a truncated
    response; the GoAway arm's own comment calls it the primary truncation
    vector under HAProxy chaining.

    Which two questions they are, why the second cannot be read off the first,
    and why the flush and the re-query stay inside one `writable()` call, are
    stated once in `h2_close`'s module doc — see `h2_close::TlsFlushPhase`.
    Do not restate that reasoning here; a second copy drifts.
    `h2_close::tests::the_two_phases_are_not_interchangeable` pins the
    distinction and `a_flush_that_succeeds_closes_within_one_writable_call`
    (`h2.rs`) pins the tick count.

    **Neither close arm performs its own I/O any more.** Both are answered by
    `ConnectionH2::dispatch_writable_state`, which takes the pre-flush answer
    as a `tls_wants_write: bool` and hands its caller a step:
    `H2WritableStateTarget::Flush` for the GOAWAY arm's own flush, and
    `ConnectionH2::dispatch_writable_state_after_flush` for the post-flush
    answer. `ConnectionH2::writable` — now in the shell impl block beside
    `ConnectionH2::readable` and `ConnectionH2::write_streams` — performs the
    preamble flush, both `ConnectionH2::tls_wants_write` reads around it and
    the third read after the GOAWAY flush. Each read is a separate binding,
    which is what keeps the two questions two values: a core holding one
    `bool` cannot answer the other. Same protocol as the fourth site below,
    same reason.

    What is tested, and what is not. `BackpressuredTlsSocket` (`h2.rs`) is the
    first harness in this repository whose `socket_wants_write()` can answer
    `true` — `mio::net::TcpStream` and `SessionTcpStream` take the trait's
    `false` default and only `FrontRustls` overrides it, which no test
    instantiates — so before it every branch named in this invariant was
    statically dead in the suite. It drives a real `ConnectionH2` through the
    `H2State::GoAway` arm and covers both post-flush outcomes. The other two
    sites are now reached over the production handler itself.
    `handshaken_front_rustls` (`h2.rs`) settles a real TLS 1.3 session in
    memory and attaches it to a loopback socket whose kernel queues are pinned
    small, so `socket_wants_write()` answers `true` because rustls really is
    holding records it could not push;
    `a_rustls_frontend_in_error_state_stays_open_until_its_records_drain` drives
    the `(H2State::Error, Position::Server)` arm over it,
    `force_disconnect_over_a_real_rustls_frontend_waits_for_the_records_to_drain`
    drives `force_disconnect`'s server arm, and
    `a_rustls_frontend_in_goaway_stays_open_until_its_records_drain` drives the
    `H2State::GoAway` arm, each asserting both answers on one connection whose
    only change between them is whether the peer read. The GoAway one also
    asserts the state is still `GoAway`, because falling through that arm
    reaches `force_disconnect`, whose own record guard answers
    `MuxResult::Continue` as well — the result alone cannot tell the two
    apart. That is what sozu-proxy/sozu#1454 asked for. `h2_close`'s tables
    remain the exhaustive statement of the decisions; these are their callers.

    **Keeping the connection open is not re-arming it.** Every site that
    keeps a connection for its pending records goes through
    `ConnectionH2::ensure_tls_flushed`, which raises a synthetic WRITABLE
    event only when the latest socket write of the pass was not refused by
    the kernel. A write that answered `WouldBlock` met a full socket: the
    kernel raises the next WRITABLE edge once the peer reads, and that edge
    flushes the records and lets the close proceed. A synthetic edge would
    only repeat the refused write, and `Mux::ready_inner` did so on every
    inner iteration until `MAX_LOOP_ITERATIONS` whenever a TLS client stopped
    reading ([#1788](https://github.com/sozu-proxy/sozu/issues/1788)). A
    refused write also consumes the WRITABLE event, including the empty-buffer
    flushes whose status nothing else reads. The decisions above are
    unchanged: they still read `tls_wants_write`, and a connection with
    records pending keeps WRITABLE interest. The two real-rustls tests above
    assert that the refused pass queues no event and, once the records have
    drained (the test drains them itself), that the next pass closes;
    `a_refused_goaway_flush_waits_for_the_kernel_edge_then_closes` checks over
    `BackpressuredTlsSocket` that the pass the kernel edge triggers flushes
    the records and disconnects, `a_refused_control_frame_drain_leaves_writable_to_the_kernel`
    and `a_refused_stream_write_leaves_writable_to_the_kernel` cover the
    control-frame flush and the stream write, and
    `test_tls_h2_stalled_reader_does_not_exhaust_loop_budget`
    (`e2e/src/tests/h2_tests.rs`) end to end.

    **A fourth site shares the shape without deciding a close.** The end of
    every `ConnectionH2::write_streams` pass runs the same
    query / flush / query triple — the shell performs the three steps and
    `ConnectionH2::finalize_write` decides them, with
    `ConnectionH2::finalize_write_after_flush` told the post-flush answer —
    and the decision is `h2_close::finalize_action`. It is a sibling
    `FinalizeAction` and not four
    more `CloseAction` variants because its non-flush branch is invariant 16's
    readiness policy above, which decides nothing about closing — widening
    `CloseAction` would force a named-impossible arm into every exhaustive
    `match` `dispatch_writable_state` already writes over it. Its middle flush
    is also
    CONDITIONAL (`if !socket_write`), a third input the GOAWAY arm has no
    analogue for; that `if` becomes an input and surfaces as two distinct
    pre-flush answers rather than staying in `h2.rs`. Both points are argued
    once in `h2_close`'s module doc; do not restate them here.
    `a_finalized_write_pass_flushes_once_and_re_arms_while_records_survive`
    (`h2.rs`) drives the flush-and-re-arm path through
    `BackpressuredTlsSocket` in `H2State::Header` and
    `a_write_pass_that_owes_nothing_withdraws_writable_interest` covers the
    withdrawal. The other four answers — `SkipFlush`, `Parked`,
    `RetainPendingBack` and `ArmControlQueue` — are pinned by `h2_close`'s pure
    rows only; no assertion in `h2.rs` reads any of them. Reaching the first
    three through `writable()` needs a stream carrying response bytes through
    the scheduler, which no fixture in `h2.rs` builds.
28. **Every byte committed to the wire is owned by the connection's ordered
    output queue, whole frame by whole frame (sozu-proxy/sozu#1604).**
    `ConnectionH2::output` (`h2_output.rs`) queues, in wire order, every
    control frame the connection serialises and the unsent rest of the one
    stream frame a partial write cut; each entry is a whole frame (or the
    rest of one already started) when it is queued. `zero` is input only.
    Two properties MUST hold:
    - **A stream's `kawa.out` starts on a frame boundary outside a header
      block.** A stream transmit gathers the output queue first, then the
      stream's frames (`h2_transmit::gather_after`). When the socket stops
      inside a stream frame, `h2_transmit::frame_tail` measures the rest of
      that frame — through END_HEADERS when the frame opened or continued an
      unfinished header block, since RFC 9113 §6.10 allows no other frame in
      between — and `ConnectionH2::handle_write` has the queue adopt it
      (`H2Output::adopt_tail`), counted to the stream at once. So a stream
      only ever owns frames the wire has not started, and removing it (peer
      RST, `end_stream`, expiry, `prune_inactive_streams_while_closing`, any
      `remove_dead_stream` caller) drops whole unsent frames, never the
      second half of one the peer is already parsing. `frame_tail`'s debug
      assertion (`a frame runs past the gathered bytes`) is where a
      violation surfaces first.
    - **A header block encoded and dropped unsent resets the HPACK encoder's
      table before the next block is encoded (sozu-proxy/sozu#1627).** The
      framing guarantee above does not cover HPACK: an unsent
      HEADERS/CONTINUATION block was encoded in `kawa.prepare`, so its
      insertions and any size update it carried are already in the
      connection's encoder table. Entries are numbered from the newest
      (RFC 7541 §2.3.3), so a missed insertion shifts every older entry in
      the encoder's table only, and a later block naming one makes the peer
      read ANOTHER field, silently. Only the stream parked in `expect_write`
      can hold unsent frames (a pass flushes each stream before preparing
      the next and ends at the first stall), so where a pass parks a stream
      it records `ConnectionH2::parked_header_block =
      h2_transmit::holds_header_frame(kawa)`, and the next
      `H2WritePhase::Start` calls `ConnectionH2::reset_encoder_table` when
      the park is gone (the stream was removed) or its `kawa.out` is empty
      (`forcefully_terminate_answer` or a default answer cleared it).
      Inside the converter, every path that throws encoded bytes away —
      `H2BlockConverter::check_header_capacity`, the `StatusLine::Unknown`
      abort, `finalize` clearing an unfinished block — goes through
      `H2BlockConverter::discard_encoded_block`, which does the same and
      re-arms the pass's signal; and a field block is never encoded before
      its closing `Flags { end_header }` is queued
      (`header_block_closing`): kawa's H1 parser queues trailer
      lines one by one and the closing flags only with the final CRLF, while
      `ParsingPhase::Trailers` is a main phase, so a pass used to encode a
      trailer still arriving and drop it. A block that the next queued flags
      do not close was cut short: it is dropped unencoded rather than waited
      on, and `ConnectionH2::end_stream`'s close-delimited arm ends a chunked
      response its backend closed mid-body (trailers included), like a
      `Content-Length` one short of its length, with RST_STREAM, as
      `ConnectionH1::terminate_close_delimited` does (§8.4), so no
      stream waits forever on trailers. A frame header that fails to serialise
      (it cannot: 9-byte buffer, length < 2^24) also resets the table.
      `Encoder::reset_table` empties the table and records the
      size updates `0`, then the maximum size, which the next block opens
      with (RFC 7541 §4.2), so the peer's decoder empties its table too. No
      frame is sent for the dropped block. Pinned by
      `a_header_block_dropped_with_its_stream_keeps_the_peer_table_in_sync`
      and `a_header_block_cleared_from_a_parked_stream_keeps_the_peer_table_in_sync`
      (`h2.rs`, red on `58550dd4`),
      `a_header_block_parked_across_two_stalled_passes_keeps_the_peer_table_in_sync`,
      `an_oversized_block_reset_outlives_the_pass_that_dropped_it` (`h2.rs`),
      `an_oversized_block_dropped_after_encoding_keeps_the_peer_table_in_sync`,
      `h1_trailers_split_across_reads_are_encoded_whole` (red on `fedb4304`),
      `a_chunked_response_closed_inside_its_trailers_ends_with_a_reset` (`h2.rs`,
      red on `d12c3299`), `an_unknown_status_line_keeps_the_size_update_signal`,
      `a_field_block_cut_short_never_stalls_its_stream`,
      `test_converter_finalize_clears_remaining_buffer` (`converter.rs`), `a_dropped_block_substitutes_fields_until_the_table_is_reset`
      (`hpack/tests.rs`), `holds_header_frame_finds_a_block_behind_other_frames`
      (`h2_transmit.rs`) and the `fuzz_hpack_roundtrip` target.
    - **DATA prepared and dropped unsent gives its send credit back
      (sozu-proxy/sozu#1641).** `H2WritePhase::Prepare` debits the stream's
      and the connection's send windows (`H2FlowControl::consume_send_window`)
      for the DATA it encodes, before the socket takes any of it, while RFC
      9113 §6.9 counts only DATA that was sent: the peer returns credit for
      what it received and for nothing else. By the same "only the parked
      stream holds unsent frames" argument, where a pass parks a stream it
      records `ConnectionH2::parked_data`, the DATA payload `kawa.out` still
      holds whole (`h2_transmit::queued_data_payload`). The rest of a frame a
      partial write cut is not counted: the output queue adopted it and sends
      it whole (sozu-proxy/sozu#1625). `ConnectionH2::remove_dead_stream`
      gives the parked credit back to the connection window when it removes
      the parked stream, and re-arms WRITABLE when that reopens the window;
      the next `H2WritePhase::Start` gives back, to both windows, whatever a
      live park no longer queues (`forcefully_terminate_answer` or a default
      answer cleared it). Both go through `ConnectionH2::refund_parked_data`
      and `H2FlowControl::refund_send_window`. A refund that would pass
      2^31-1 means the peer's WINDOW_UPDATEs overgranted while the DATA sat
      parked: it is not applied and the connection sends
      GOAWAY(FLOW_CONTROL_ERROR), as for an overflowing increment (RFC 9113
      §6.9.1). Without it every such drop
      shrank the connection window for the life of the connection, until
      every stream's DATA waited on a WINDOW_UPDATE the peer would never
      send. Pinned by
      `data_dropped_with_its_parked_stream_gives_the_connection_window_back`,
      `data_dropped_from_a_live_parked_stream_gives_both_windows_back`,
      `data_a_partial_write_started_is_sent_whole_and_not_given_back`
      (`h2.rs`, red on `5db53c60`),
      `data_still_parked_after_a_second_stalled_pass_is_not_given_back`,
      `a_refund_the_peer_pushed_past_the_window_ceiling_is_a_flow_control_error`
      (`h2.rs`),
      `a_refund_past_the_window_ceiling_is_an_overflow_and_changes_nothing`,
      `queued_data_payload_counts_only_data_payload_octets` (`h2_transmit.rs`)
      and `refund_gives_back_consumed_credit_and_reports_a_reopened_window`
      (`h2_flow_control.rs`).
    - **Queued output goes out before any new stream frame.** It is flushed
      by the control-frame preamble, and a transmit that finds it non-empty
      sends it ahead of the stream's own frames in the same `writev(2)`.
    Reading is independent of the queue but bounded by it: past one buffer of
    queued output (`ConnectionH2::output_read_cap`) `poll_read_target`
    withdraws READABLE until `ConnectionH2::consume_output` drains it below
    the cap, as HAProxy's demux stops on a full `mbuf` (`H2_CF_DEM_MROOM`).
    Before #1604, `zero` was both the frame-header input buffer and the
    control-frame output buffer, stream frames were serialised against it
    only through `expect_write`, and a stream removed while parked mid-frame
    left that frame truncated on the wire; the one-off defences that
    followed (`zero_output_deferred`, the `ResumeZero` flush stage,
    `expect_write = Some(Zero)`, the `zero_holds_output` read guard) are all
    gone with the shared buffer. Pinned by
    `a_removed_half_written_stream_still_completes_its_frame`,
    `a_removed_half_written_stream_does_not_corrupt_the_next_stream_frame`,
    `a_peer_reset_of_a_half_written_stream_still_completes_its_frame` (red
    on `a0785505`), `a_cut_header_block_is_completed_before_the_ack`, the
    #1601 half-written-frame tests, `h2_transmit`'s
    `frame_tail_cases`, `h2_output`'s unit tests and
    `a_full_output_queue_stops_reading_until_it_drains`.

29. **A backend stream the backend has never seen gets no frame.** On
    `Position::Client`, until `front.consumed` is set no byte of the
    request has reached the socket, so its HEADERS have not left and the
    stream is idle on the backend. Retiring it (`ConnectionH2::end_stream`,
    `ConnectionH2::cancel_timed_out_streams`) queues nothing, and a request
    in error is not handed to the converter (`H2WritePhase::Prepare`): a
    RST_STREAM on an idle stream is a connection error (RFC 9113 §5.1,
    §6.4) on a connection the other streams of the same frontend connection
    share. The client preface is the first thing queued on a backend
    connection (debug-asserted in the `ClientPreface` arm
    of `ConnectionH2::dispatch_writable_state`), and a write readiness while
    the server's SETTINGS are awaited is withdrawn, never answered with a
    disconnect (§2.2, §8.2, sozu-proxy/sozu#1631).

30. **Frames a backend connection encoded never leave on another one.**
    Once an H2 backend's write pass leaves a request's frames in `front.out`
    (`Stream::front_bound_to_backend`), that request is bound to the
    connection: `end_stream_decision` answers it `SendDefault(502)` rather
    than `Reconnect`, `ConnectionH2::handle_goaway_frame` answers it `503` rather
    than re-link it (debug-asserted: a re-linked stream holds no frame), and
    `ConnectionH2::start_stream` refuses any request whose `front.out` is not
    empty (§8.5, sozu-proxy/sozu#1632).

31. **A draining H2 session waits for a request still on its way to a
    backend, and a forced close is announced.** `Mux::shutting_down_inner`
    (`lib/src/protocol/mux/mod.rs`) keeps an H2 frontend open while
    `ConnectionH2::peer_header_block_in_progress` holds (the block's stream is
    still `StreamState::Idle`) and while any stream is `StreamState::Link` —
    the state a CONTINUATION read by `Mux::drive_frontend_shutdown_io` leaves,
    since only `Mux::ready` links it. The request is then served and
    `ConnectionH2::finalize_write` sends the final GOAWAY with its id, as for
    any drained stream. Both waits are bounded by
    `h2_graceful_shutdown_deadline_seconds` (§8.3); when it elapses,
    `ConnectionH2::goaway_before_forced_close` queues the final
    `GOAWAY(NO_ERROR)` before the close, for every draining H2 session. Its
    `last_stream_id` is `H2StreamTable::highest_peer_stream_id`, except that a
    stream whose opening block never completed is excluded: every stream
    `ConnectionH2::create_stream` accepted has a lower id (RFC 9113 §5.1.1,
    §6.10), and a higher observed id can only be a refused stream, so the id
    two below it is reported and the client may retry it. With the knob at
    `0` the drain has no bound of its own and the wait ends with the
    connection's own timeouts. Before sozu-proxy/sozu#1647 the
    session closed as soon as the advisory GOAWAY was flushed, and a forced
    close sent no final GOAWAY at all. Pinned by
    `test_h2_graceful_drain_waits_for_an_incomplete_header_block`,
    `test_h2_graceful_drain_deadline_mid_header_block_sends_final_goaway`,
    `test_h2_graceful_drain_deadline_with_a_linked_stream_sends_final_goaway`
    (`e2e/src/tests/h2_tests.rs`, all red before the change) and
    `a_draining_h2_session_waits_for_a_stream_awaiting_its_link`.
32. **An H2 trailer block ends a chunked H1 body with the last chunk.**
    Once it has accepted a trailer block, `pkawa::handle_trailer` queues
    `Flags { end_body: true }` in front of the trailer fields and closes them
    with `Flags { end_header, end_stream }`, the block shape kawa's H1 parser
    gives a chunked trailer section; a refused block adds no marker. kawa's
    H1 serializer writes the last chunk `0\r\n` for the `end_body` block of
    a chunked message, so an H1 peer reads `0\r\n`, the trailer fields and
    the empty line (RFC 9112 §7.1), in both directions: a request from an H2
    client to an H1 backend, and a response from an H2 backend to an H1
    client. A block with no field left (empty, or every field elided) ends
    the body with `0\r\n\r\n`. `H2BlockConverter` ignores `end_body`: an
    H2 peer gets one trailer HEADERS frame with END_STREAM when a field is
    left, and an empty DATA frame with END_STREAM otherwise. Before
    sozu-proxy/sozu#1722 the block had no `end_body` marker: the backend read
    `5\r\nhello\r\ngrpc-status: 0\r\n\r\n` and took the first trailer
    field for a chunk-size line, which breaks the request framing on a
    keep-alive backend connection, and a block with no field left gave
    `5\r\nhello\r\n\r\n`, a body that never ends. Pinned by
    `handle_trailer_follows_the_last_chunk_on_h1`,
    `handle_trailer_with_every_field_elided_ends_the_chunked_body_on_h1`
    (`pkawa.rs`) and `test_h2_request_trailers_follow_last_chunk_h1_backend`
    (`e2e/src/tests/h2_security_header_injection.rs`), all red before the
    change.

---

## 10. Pointers for First Steps

If you are fixing a bug in this module:

- **Panic on `context.streams[gid]`** — start at §5, then walk every
  stream-removal site listed in §5.4. The fix is usually a missed invalidation
  of `expect_write`/`expect_read`.
- **Hung session** — check §7. Verify the timer in question is being reset only
  on application activity, not on control frames.
- **Doubled metrics / gauge drift** — check §9 item 14, then read the ledger:
  every `Backend::active_requests` and `Backend::active_connections` write on
  this path goes through `BackendRegistry::apply`, driven by
  `Mux::apply_backend_deltas`, so the drift is either a missing
  `Context::record_backend_delta` at a stream-removal site or a drain that
  ran after something read the counter.
- **Truncated response / TLS decode error** — check `has_pending_write`
  (`h2.rs`), `delay_close_for_frontend_flush` (`mod.rs`), and the TLS
  drain logic in the `Position::Server` arm of `H2Shell::close`
  (`h2.rs`; the arm is one of eight identical `Position::Server => {`
  lines, hence the symbol).
- **Stream count underflows `max_concurrent_streams`** — see
  `prune_inactive_streams_while_closing` (`h2.rs`) and make sure every
  removal increments nothing and decrements what it should.

Last revision date of the surviving line anchors: 2026-09-28, when the last
five `file.rs:NNN` anchors outside a pinned code block (§3.3, §4.2, §5.1,
§5.2, §7.5) were converted to the symbols their prose names; the only line
numbers left in this file are the ranges of pinned code blocks, which
`check_doc_citations.py` compares line by line. The sweep before that was
2026-09-25, against
`refactor/h2-writable-flush-lift` (issue #1339 Q10, lifting the
`(H2State::Error, Position::Server)` and `H2State::GoAway` flush triples out of
`ConnectionH2::writable` into `ConnectionH2::dispatch_writable_state` and
`ConnectionH2::dispatch_writable_state_after_flush`, and moving `writable`
itself into the shell impl block), which re-anchored the 50 spans its own edits
shifted. Every one was remapped by CONTENT — `difflib.SequenceMatcher`
equal-opcodes from `c5c80909`, never a global offset — and then checked on two
keys: byte-identical line text, and an identically-named enclosing item. The
`H2WritePhase::Resume` outbound-byte refresh in §5.4's timer bullet dropped its
number for the symbol instead, because this changeset's insertions moved it onto
a line number another span already carried at the base revision, and the drift
rule compares a reused number against whatever the base revision put on it
(sozu-proxy/sozu#1447) — the same remedy the
`ConnectionH2::poll_read_target` anchor beside it already carries.
`doc/h2_mux_internals.md`'s `writable()` signature block is the one span content
could not map, because a ~3 350-line move is a delete plus an insert and not an
equal opcode; it was re-anchored by hand and its five quoted lines verified
byte-for-byte. The previous sweep was 2026-09-24, against
`refactor/h2-finalize-write-split` (issue #1339 Q10, splitting
`ConnectionH2::finalize_write` and `ConnectionH2::flush_pending_control_frames`
at their flush points), which re-anchored 23 anchors and dropped §8.1's
`finalize_write` caller pin in favour of the symbol that sentence already names.
Keep this line _actually_
current when you touch the file — greppable as "Last revision date".

---

## 11. Close-path log severity tiers

The TLS-drain warning at `H2Shell::close` (`h2.rs`, `Position::Server`
arm) fires when `socket_wants_write()` is still true after `MAX_DRAIN_ROUNDS`
empty `socket_write_vectored(&[])` calls. The severity is tiered along
**stream-count + close-state**, not peer-vs-operator. The tier is intentionally
orthogonal to — and composes with — the send-side `H2Error`-variant tier in
`ConnectionH2::goaway` (`h2.rs`); both rules demote benign paths and keep
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

---

## 12. What a request costs on the nominal path

The hot-path work of 2026-09-26 to 2026-09-28 (`doc/hot_path_zero_copy.md`)
left the mux with the costs below. Measured figures per protocol are in
`doc/lifetime_of_a_session.md` §6.7 (H1) and §7.6 (H2); this section says where
in the mux each remaining cost sits, so a change that adds one is visible in
review.

**Once per session.** The session object (`Rc<RefCell<HttpSession>>` or
`HttpsSession`) holds the `Mux` inline, including the first backend connection
(`BackendConnections`), the frontend's timer handle (`MuxTimeouts`), the first
entry of `Context::backend_streams` (`InlineTokenMap` of `LinkedStreams`) and an
empty `DebugHistory`. A connection's first stream slot, its first
`BackendRegistry` entry, `Context::pending_links` and its write-descriptor
vectors (`io_slices`) are each allocated once, sized for one. The H2 output
queue (`H2Output`) holds 128 bytes inline and allocates only under
backpressure.

**Once per connection.** The access log's RTT cells cost one
`getsockopt(TCP_INFO)` per connection and side, at its first logged request
(`memoized_rtt`, `Connection::rtt`). The H1 forwarding values are rendered once
per frontend connection (`ForwardingHop`, `kawa_h1/LIFECYCLE.md` §2). A backend
connection costs `socket`, `connect`, `setsockopt(TCP_NODELAY)` and
`epoll_ctl(EPOLL_CTL_ADD)` when it is dialled (`Mux::dial_backend`, `Mux::attach_dialed`), and its
`close(2)` with no `EPOLL_CTL_DEL`; a `shutdown(SHUT_WR)` only while its peer has
not closed (`shutdown_write`).

**Once per request or stream.** The stream's request id is minted without a
system call (`Context::next_request_id`) and rendered once into an `Rc<str>`
that its headers and its log line share. Routing, the per-(cluster,
source-IP) gate and a reused backend copy no identifier: the cluster id is a
`ClusterId = Arc<str>` and the backend id an `Rc<str>` interned per session
(`BackendRegistry::id_for`), both cloned by reference count. One read per
direction and flight (a short read stops it, `update_readiness_after_read`),
one `writev(2)` per direction and write pass (`h2_transmit::gather`, and
`h2_transmit::gather_after` behind the H2 output queue), and one access-log
record rendered into the logger's reused buffer. `Instant::now()` is sampled
once per `Mux` pass through the vDSO (§7.5).

**What is held by tests.** The allocation budgets use `crate::test_allocations`
(`doc/testing.md` §2) and the system-call counts use counters compiled under
`cfg(test)` (`protocol::mux::sample_rtt`'s read count, `shutdown_write`'s
shutdown count, `crate::socket::tls_writes`); the list of tests, one per
property, is `doc/hot_path_zero_copy.md` §5.

