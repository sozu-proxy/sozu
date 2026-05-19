# Lifetime of a session

## 1. Audience and purpose

Operator-and-new-contributor entry point for "how a request flows
through Sōzu". It explains the single-threaded mio worker, where the
per-protocol state machines sit, where the HTTP/1.1 vs HTTP/2
boundary is drawn, and which files to read next.

For deep per-protocol detail, follow the `LIFECYCLE.md` siblings:

- HTTP/2 mux: [`lib/src/protocol/mux/LIFECYCLE.md`](../lib/src/protocol/mux/LIFECYCLE.md)
- HTTP/1.1 (Kawa-backed): [`lib/src/protocol/kawa_h1/LIFECYCLE.md`](../lib/src/protocol/kawa_h1/LIFECYCLE.md)
- PROXY-protocol pre-flight: [`lib/src/protocol/proxy_protocol/LIFECYCLE.md`](../lib/src/protocol/proxy_protocol/LIFECYCLE.md)
- Master/worker supervisor: [`bin/src/command/LIFECYCLE.md`](../bin/src/command/LIFECYCLE.md)

This file stays narrative. Cited paths are repo-relative; SHA-pinned
permalinks have been removed because they rot.

## 2. Conceptual primitives

### 2.1 The mio event loop

A Sōzu worker is a single OS thread that owns one `mio::Poll`
(`lib/src/server.rs:323, 342`). On Linux this is a thin wrapper around
`epoll(7)`; on the BSDs and macOS it is `kqueue(2)`. The worker
registers every socket — listen sockets, frontend, backend, metrics,
unix command-channel pair — with that single poller, then loops
reading events out of `Events` and dispatching them to the correct
session. Loop time is observable via the `epoll_time` time! metric
(`lib/src/server.rs:593-595`).

### 2.2 Edge-triggered readiness and the writable invariant

mio runs in edge-triggered mode: the kernel notifies the worker only
once when a socket transitions to readable or writable. If Sōzu does
not drain the kernel buffer fully on that wake-up, it gets no other
event until the *next* edge.

To survive that contract, every protocol module routes its readiness
through a `Readiness` tracker (`lib/src/protocol/mux/connection.rs:200,
203`) and uses two helpers:

- `signal_pending_write` — set by code that has produced bytes that
  must eventually go out, even though the writable epoll edge may have
  been consumed already.
- `arm_writable` — set when bytes are queued from a *readable* code
  path so the next pump iteration writes them out without waiting for
  another kernel wake-up.

If new code queues output bytes from a readable path and forgets to
call `arm_writable` (mux) or `signal_pending_write` (kawa_h1 / pipe),
the session "stalls" — bytes sit in the buffer and the next epoll
event never arrives. Past truncation bugs on this branch all
originated here. The `mux::answers` module documents this as the
"invariant-15 pair" (`lib/src/protocol/mux/answers.rs:280, 298`); the
home for the invariant is `mux::connection`
(`lib/src/protocol/mux/connection.rs:14`).

### 2.3 Tokens, the SessionManager, and the slab

Every mio registration carries a `Token` (a `usize`). The
`SessionManager` (`lib/src/server.rs:166`) owns a
`Slab<Rc<RefCell<dyn ProxySession>>>` (`lib/src/server.rs:170`) that
maps each token back to the session that owns the registration. This
slab is also the unit of bookkeeping that enforces `max_connections`
(`lib/src/server.rs:188, 194`) and gauges
`client.connections`, `client.connections_max`,
`client.connections_percent`, `slab.{entries,capacity,usage_percent,
accept_threshold_percent}` and `buffer.{in_use,capacity,usage_percent}`,
all sampled once per run-loop iteration in
`Server::run` (`lib/src/server.rs`).

A single session typically occupies *two* slab entries while it is
forwarding traffic: one for the frontend token (registered when the
client connection was accepted) and one for the backend token
(registered after `connect_to_backend` succeeded). This is the
single biggest mental-model adjustment for new contributors: the same
session is reachable through two different keys.

Listen sockets themselves are stored as `ListenSession` entries in the
same slab (`lib/src/server.rs:1920`), which is what allows the same
event loop to multiplex accept events alongside data events.

### 2.4 The three proxies

A worker hosts three proxy types, one per supported listener protocol:

- `HttpProxy` (`lib/src/http.rs:762`)
- `HttpsProxy` (`lib/src/https.rs:1319`)
- `TcpProxy` (`lib/src/tcp.rs`)

Each proxy owns its listeners, its known frontends and clusters, the
per-protocol configuration (TLS material, ALPN list, H2 knobs, etc.),
and the upgrade paths that promote a session from one protocol layer
to the next.

## 3. Accepting a connection

### 3.1 Listeners and SO_REUSEPORT

Listen sockets are created via `lib/src/socket.rs` with
`SO_REUSEPORT` enabled (`lib/src/socket.rs:1023`); multiple workers
in the same Sōzu process share each listener address and the kernel
distributes accept events across them. Each listener is registered
with mio and tracked through a `ListenSession` slab entry. Hot
reconfig adds and removes listeners at runtime via the master-to-
worker channel (`lib/src/server.rs:1255, 1286, 1313, 1477, 1524,
1565`).

### 3.2 The accept queue

When a listener becomes readable, the proxy accepts every pending
connection in a single batch and parks each `TcpStream` on an internal
`accept_queue: VecDeque<…>` (`lib/src/server.rs:294`). Sessions are
*not* created synchronously inside the accept loop. The queue is
drained later, newest-first, so connections that have been waiting
too long are dropped before they are turned into a session. The
cut-off is `accept_queue_timeout` (`lib/src/server.rs:287`).

### 3.3 Backpressure and `max_connections`

If the slab is at capacity (`SessionManager::can_accept` is `false`,
`lib/src/server.rs:188`) the proxy stops draining the accept queue and
the kernel's listen backlog absorbs the surplus. The
`accept_queue.backpressure` gauge flips to 1 in that state
(`lib/src/server.rs:196, 207`); a 1 Hz ticker also bumps
`accept_queue.saturated_seconds` so dashboards can plot how long the
worker spent backpressured (`lib/src/server.rs:104-107, 682-699`). The
system unwinds at 90% of `max_connections` to avoid flapping
(`lib/src/server.rs:244-249`).

### 3.4 Zombie detection

A periodic "zombie checker" pass walks the slab and forcibly closes
sessions that look stuck — typically because of a logic bug elsewhere.
This is a safety net, not a primary lifecycle mechanism.

## 4. TLS handshake (HTTPS only)

For `HttpsProxy` sessions, the first protocol layer above raw TCP is
TLS. Sōzu uses [rustls](https://docs.rs/rustls) and instantiates one
`rustls::ServerConnection` per session
(`lib/src/protocol/rustls.rs:12, 76, 94`). The handshake itself is
driven from `lib/src/protocol/rustls.rs`; the listener-level config
(certificate stores, ALPN list, SNI binding policy) lives in
`lib/src/https.rs` and `lib/src/tls.rs`.

### 4.1 SNI / `:authority` binding

If `strict_sni_binding` is enabled on a listener
(`command/src/config.rs:388`), Sōzu rejects any HTTP request whose
`:authority` (H2) or `Host` (H1) is not covered by a SAN of the
certificate served on this TLS session, with RFC 6125 §6.4.3 wildcard
handling. This matches Firefox / Chrome connection-coalescing
semantics (RFC 7540 §9.1.1 / RFC 9113 §9.1.1) — browsers reuse a
single H2 connection for any origin covered by the served
certificate's SubjectAlternativeName dNSName entries (RFC 6125
§6.4.4: when SAN dNSName is present, the CN is ignored).
Misses are answered with 421 Misdirected Request (RFC 9110 §15.5.20),
which both browsers handle by opening a fresh connection on the right
SNI. The SAN dNSName snapshot is captured once at handshake (mirroring
browser cache semantics) and stored on the mux `Context` as
`tls_cert_names`; it is frozen for the connection lifetime even if the
operator swaps the underlying certificate mid-flight. Plaintext
listeners have no SNI / cert to compare against and bypass the check.
This protects multi-tenant HTTPS deployments from an attacker reaching
tenant B via a TLS session keyed for tenant A while staying compatible
with browser-driven coalescing on legitimate wildcard certs.

### 4.2 ALPN and `disable_http11`

After the handshake completes, Sōzu inspects the negotiated ALPN
protocol (`lib/src/https.rs:321-322, 339`) and decides which mux
flavour to instantiate:

- ALPN `h2` → HTTP/2 mux.
- ALPN `http/1.1` → HTTP/1.1 path.
- No ALPN selected → HTTP/1.1 by default.

Listener-level `disable_http11` (`command/src/config.rs:393`) lets an
operator force H2-only on a per-listener basis. ALPN rejections are
counted with two distinct keys so dashboards can split refusals by
cause:

- `https.alpn.rejected.unsupported` — peer offered an ALPN that Sōzu
  does not implement (e.g. `h3`) (`lib/src/https.rs:359`).
- `https.alpn.rejected.http11_disabled` — peer wanted `http/1.1` but
  the listener has `disable_http11 = true`
  (`lib/src/https.rs:342, 372`).

The startup-time validator at `command/src/config.rs:253-262` catches
the obvious operator mistake of pairing `disable_http11 = true` with
`alpn_protocols` that still contains `"http/1.1"`.

### 4.3 Handshake telemetry

Successful handshakes report `tls.handshake_ms` as a histogram
(`lib/src/protocol/rustls.rs:78, 119, 193, 254, 261`). Failures are
tagged with a constant key per rustls error variant
(`tls.handshake.failed.alert_received`,
`tls.handshake.failed.no_alpn`, …) so statsd cardinality stays bounded
even when a misbehaving client is hammering the handshake
(`lib/src/protocol/rustls.rs:328-345, 503`).

## 5. PROXY-protocol pre-flight

When a frontend is configured to expect a HAProxy PROXY-protocol
header (typically because Sōzu sits behind a Layer-4 load balancer)
the session starts in a small `ExpectProxyProtocol` state
(`lib/src/http.rs:141`,
`lib/src/protocol/proxy_protocol/expect.rs:117`). That state reads the
v1 / v2 header off the front socket, extracts the real client address,
and then transitions the session into the downstream protocol
(HTTP/1.1, HTTP/2, or raw TCP relay).

The full lifecycle of the three sub-state-machines (`expect`, `relay`,
`send`) is documented in
[`lib/src/protocol/proxy_protocol/LIFECYCLE.md`](../lib/src/protocol/proxy_protocol/LIFECYCLE.md);
read it before changing anything in
`lib/src/protocol/proxy_protocol/`.

## 6. Per-protocol session lifecycle

Once the session has gone through any TLS and PROXY-protocol
pre-flight, control transfers to one of three protocol state machines
that own the rest of the session.

### 6.1 HTTP/1.1 (Kawa-backed)

The HTTP/1.1 path is the historical core of Sōzu and is now backed by
the [Kawa](https://github.com/CleverCloud/kawa) HTTP parser.
Conceptually the lifecycle is:

1. **Parse the request** out of the front buffer using Kawa, in
   `lib/src/protocol/kawa_h1/mod.rs`.
2. **Route the request** to a cluster via
   `cluster_id_from_request` (`lib/src/protocol/kawa_h1/mod.rs:1340`).
3. **Pick a backend** via `backend_from_request`
   (`lib/src/protocol/kawa_h1/mod.rs:1397`) and
   **connect to it** via `connect_to_backend`
   (`lib/src/protocol/kawa_h1/mod.rs:1462`). A previously-opened
   keep-alive socket may be reused after a liveness probe
   (`check_backend_connection`,
   `lib/src/protocol/kawa_h1/mod.rs:1288`).
4. **Forward bytes** in both directions through the front/back Kawa
   buffer pair, registering writable interest as needed
   (`lib/src/protocol/kawa_h1/mod.rs:1545, 1566`).
5. **Close or reset** when the response completes. If the request and
   response both indicate keep-alive, the session is "reset" rather
   than destroyed and waits for the next request on the same front
   socket; the back socket may be released or kept depending on the
   cluster decision (`close_backend`,
   `lib/src/protocol/kawa_h1/mod.rs:1198`).

The full state diagram, including the parser back-pressure rules,
the H1 → WebSocket upgrade path, the H1 → H2 mux transition, and the
keep-alive vs close attribution, lives in
[`lib/src/protocol/kawa_h1/LIFECYCLE.md`](../lib/src/protocol/kawa_h1/LIFECYCLE.md).

### 6.2 HTTP/2 (mux)

The H2 multiplexer is the largest single piece of Sōzu and lives under
`lib/src/protocol/mux/`. The high-level data model:

- A single `ConnectionH2<Front>` per TCP connection
  (`lib/src/protocol/mux/h2.rs`) owns the wire state: HPACK encoder
  and decoder, connection-level flow window, GOAWAY state, and the
  per-connection `H2FloodDetector`.
- A `Context<L>` (`lib/src/protocol/mux/mod.rs`) owns the
  `Vec<Stream>` that backs every individual H2 stream's request /
  response buffers. Streams are referenced across the two through a
  `GlobalStreamId = usize`.
- Each `Stream` carries a `StreamState`
  (`lib/src/protocol/mux/stream.rs:35`) that walks through the
  lifecycle `Idle` → `Link` → `Linked(Token)` → `Unlinked` →
  `Recycle`. Only `Idle` and `Recycle` are "free" slots; the
  intermediate states pin the stream to a backend connection.

The H2 mux owns a few invariants that are easy to break by accident:

- **Flow control.** Per-stream and per-connection windows must be
  topped up with `WINDOW_UPDATE` frames or the peer stalls.
- **HPACK stateful coding.** Decoder and encoder state must stay in
  lock-step with the wire — silently dropping a size update or
  skipping a dynamic-table eviction de-syncs the peer for the rest of
  the connection.
- **RFC 9218 priorities.** Priorities are extracted from the
  `priority` request header and from `PRIORITY_UPDATE` frames
  (`lib/src/protocol/mux/pkawa.rs:498-520`) and feed the writable
  scheduler so a slow priority-7 download cannot starve a priority-0
  interactive request.
- **GOAWAY and graceful drain.** After GOAWAY(NO_ERROR) the connection
  enters draining mode (`lib/src/protocol/mux/h2.rs:1223-1226`); new
  peer streams must be refused (RFC 9113 §6.8) and existing streams
  must complete. The graceful-shutdown deadline is driven from the
  listener config (`lib/src/https.rs:448-449, 458, 977-978`).
- **Flood mitigation.** The `H2FloodDetector` sits inline in the read
  path and backs the published mitigations for CVE-2023-44487 (Rapid
  Reset), CVE-2024-27316 (CONTINUATION flood), and CVE-2025-8671
  (MadeYouReset), plus PING / SETTINGS / WINDOW_UPDATE / glitch flood
  thresholds. Every trip emits a distinct
  `h2.flood.violation.<kind>` counter (kinds include
  `rst_stream_{lifetime,pre_response_lifetime,emitted_lifetime,window}`,
  `ping_{window,lifetime}`, `settings_{window,lifetime}`,
  `empty_data_window`, `continuation_per_block`,
  `window_update_stream0_window`, `header_size_per_block`,
  `glitch_window`; see `lib/src/protocol/mux/h2.rs:779-930, 3656`).
  GOAWAY and RST_STREAM sends/receives are attributed by error code
  via `h2.{goaway,rst_stream}.{sent,received}.<code>`
  (`lib/src/protocol/mux/h2.rs:3706`).
- **Edge-triggered writes.** The mux is the most common offender for
  the "queued bytes, no writable wake-up" stall described in §2.2;
  `arm_writable` calls are scattered across `mux::answers`, `mux::h1`,
  and `mux::h2` and must not be removed without an equivalent kick.

The full per-stream state diagram and the connection-level handler
catalogue are in
[`lib/src/protocol/mux/LIFECYCLE.md`](../lib/src/protocol/mux/LIFECYCLE.md).

### 6.3 WebSocket and TCP pass-through (pipe)

Once an HTTP/1.1 session has successfully negotiated a WebSocket
upgrade (or once a `TcpProxy` accepts a connection that is pure
byte-stream pass-through), the session promotes to a `Pipe` state
(`lib/src/protocol/pipe.rs:82, 118`). The pipe holds no protocol
parser; it shuttles bytes between the front and back sockets and
relies on the standard readiness-pumping discipline. WebSocket
metadata (`WebSocketContext`, `lib/src/protocol/pipe.rs:71`) is
inherited from the H1 mux at upgrade time so logging and metrics
keep their context.

## 7. Connect to the backend cluster

Routing happens after the request headers are parsed. The router asks
"which cluster does this `(host, path, method)` match?" and returns a
`cluster_id`. From there the load-balancer picks a backend.

The available algorithms live in `lib/src/load_balancing.rs`:

- `RoundRobin` (`lib/src/load_balancing.rs:20-24`)
- `Random`
- `LeastLoaded` (`lib/src/load_balancing.rs:86`)
- `PowerOfTwo` (`lib/src/load_balancing.rs:127`)

Sticky sessions are implemented as an opt-in cookie-based override:
when a request carries a sticky cookie that names a still-healthy
backend, the load balancer skips its normal selection and pins the
request to that backend.

Backend health is tracked in `lib/src/backends.rs`. A request that
fails to reach its first chosen backend retries up to three attempts
(within the same cluster) before Sōzu serves a default 503; if no
cluster matches the request at all, Sōzu serves a default 404.

## 8. Forwarding bytes both ways

After the backend connection is established the session enters its
steady-state. The H1 path holds a `front`/`back` pair of Kawa buffers;
the H2 path holds a per-stream pair driven by the mux scheduler; the
pipe path passes bytes through verbatim. In all three cases the loop
shape is the same:

1. mio reports a readable edge on either socket.
2. The session reads as much as the kernel buffer holds.
3. The bytes are processed (parsed, scheduled, or copied) and queued
   for the opposite socket.
4. The session arms writable interest if it just produced new output
   from a readable code path (§2.2).
5. mio reports a writable edge on the destination socket.
6. The session drains as much as the kernel will accept and loops.

The two pitfalls that bite repeatedly:

- **Forgetting `arm_writable` / `signal_pending_write` after queueing
  bytes from a readable handler.** The session looks alive but never
  flushes the last frame.
- **Asymmetric scalar vs vectored write paths.** `socket_write` and
  `socket_write_vectored` must retry under partial writes the same
  way; past divergence here caused the multi-megabyte response
  truncation bug on `feat/h2-mux`.

Per-cluster traffic is observed via `requests`, `bytes_in`,
`bytes_out`, and `backend_response_time`
(`lib/src/metrics/mod.rs:446-453`).

## 9. Closing the session

A session ends when:

- The H1 response completes and either side closed the connection, or
- The H2 stream pool drains after GOAWAY and the connection is
  destroyed, or
- A protocol error or flood violation forces a hard close, or
- The zombie checker decides the session is wedged.

For TLS frontends specifically, the close path uses **write-only
shutdown** on the front socket
(`lib/src/https.rs:661-668`, mirrored in
`lib/src/http.rs:448-452`):

```rust
front_socket.shutdown(Shutdown::Write)
```

`Shutdown::Both` is forbidden on a TLS frontend. It includes
`SHUT_RD`, which discards any unread data in the kernel receive
buffer (the client's GOAWAY, ACKs, or trailing TLS records). On
Linux the subsequent `close()` then sends a TCP RST instead of a FIN,
destroying any data still in the send buffer — including the TLS
records the drain loop just flushed. `Shutdown::Write` sends FIN only
after the send buffer drains, preserving the response. The plaintext
TCP path (`lib/src/tcp.rs:867-870, 1011-1016`) keeps `Shutdown::Both`
because it has no encrypted send-buffer to truncate; the comment
flags that a future TLS upgrade on TCP would need to switch modes.

After shutdown, `state.close(...)` closes the backend, flushes any
close-notify, and releases buffers; the proxy removes the session
from the slab under both front and back tokens, mio deregisters the
sockets, and the slab entries return to the free list. Half-closed
H2 streams unwind the same way — per-stream cleanup in `mux::mod` and
`mux::router` decrements `backend.pool.size`
(`lib/src/protocol/mux/mod.rs:1641-1650`,
`lib/src/protocol/mux/router.rs:388-394, 428`).

## 10. Hot reconfig and upgrades

Everything above describes a worker forwarding live traffic. That
data path is decoupled from the **control plane**: master and workers
communicate through a unix command channel, the master validates
incoming requests, and changes fan out to workers through
SCM_RIGHTS-passing pairs. Hot reconfiguration (add a frontend, remove
a backend, swap a certificate) flows through this channel without
touching live sessions. The hot **upgrade** path additionally re-execs
the master with the listener file descriptors handed off across
`execve` (`bin/src/upgrade.rs`), so a new binary takes over the same
listening sockets without dropping accepted connections.

Detailed master/worker lifecycle, the SoftStop / HardStop verbs
(`lib/src/server.rs:788, 799`), and the audit-log envelope live in
[`bin/src/command/LIFECYCLE.md`](../bin/src/command/LIFECYCLE.md).

**Scope clarification.** Data-plane sessions never emit audit-log
lines. The audit log is bound to control-plane mutations (frontends,
backends, certificates, listener config) over the unix command
socket. To answer "where did this 502 come from", read the per-cluster
metrics and the protocol log macros — not the audit log.

## 11. Where to look in the code

Use this map as the entry point when you want to read source.

| Concern | Files |
|---|---|
| mio loop, slab, accept queue, max_connections, soft/hard stop | `lib/src/server.rs` |
| `SO_REUSEPORT`, socket helpers | `lib/src/socket.rs` |
| `HttpProxy`, H1 listener, upgrade transitions | `lib/src/http.rs` |
| `HttpsProxy`, TLS listener, ALPN dispatch, write-only shutdown | `lib/src/https.rs`, `lib/src/tls.rs` |
| `TcpProxy` (plaintext byte relay) | `lib/src/tcp.rs` |
| TLS handshake (rustls glue), handshake metrics | `lib/src/protocol/rustls.rs` |
| HTTP/1.1 session (parser, editor, router, backend connect, keep-alive) | `lib/src/protocol/kawa_h1/` |
| HTTP/2 mux (connection, frames, HPACK, priorities, scheduler, flood detector) | `lib/src/protocol/mux/` |
| WebSocket / TCP pass-through after upgrade | `lib/src/protocol/pipe.rs` |
| PROXY-protocol pre-flight (expect / relay / send) | `lib/src/protocol/proxy_protocol/` |
| Routing and load balancing | `lib/src/router/`, `lib/src/load_balancing.rs`, `lib/src/backends.rs` |
| Metrics emission | `lib/src/metrics/mod.rs` |
| Master/worker supervisor, command socket, hot upgrade | `bin/src/command/`, `bin/src/upgrade.rs` |
| Config knobs (buffer_size, ALPN, H2 timeouts, flood thresholds, sticky sessions) | `command/src/config.rs` |
| Per-protocol log macros (`MUX-H2`, `RUSTLS`, `KAWA-H1`, `PIPE`, `TCP`, `HTTPS`, …) | each module's `log_context!` family |

## 11.5 Where metrics fire along the path

Full taxonomy: `doc/configure.md` + `lib/src/metrics/`. The minimum
set to read a session's life from a dashboard:

- `tls.handshake_ms`, `tls.handshake.failed.<reason>` — handshake
  latency + per-rustls-variant failure attribution
  (`lib/src/protocol/rustls.rs:193, 254, 261, 328-345`).
- `https.alpn.rejected.{unsupported,http11_disabled}` — ALPN refusal
  causes (`lib/src/https.rs:342, 359, 372`).
- `client.connections`, `client.connections_max`,
  `client.connections_percent` — slab-backed lifecycle gauges
  (`client.connections` is sampled per increment/decrement in
  `SessionManager::incr/decr`; `_max` and `_percent` are sampled in the
  run loop alongside `slab.*` and `buffer.*`).
- `accept_queue.backpressure`, `accept_queue.saturated_seconds` —
  binary backpressure + time-integrated saturation
  (`lib/src/server.rs:196, 207, 682-699`).
- `backend.pool.size` — long-lived gauge mirroring open backend
  connections (`lib/src/protocol/mux/mod.rs:1650`,
  `lib/src/protocol/mux/router.rs:394, 428`,
  `lib/src/protocol/mux/connection.rs:379`).
- `requests`, `bytes_in`, `bytes_out`, `backend_response_time` —
  per-cluster + per-backend counters and timing
  (`lib/src/metrics/mod.rs:446-453`).
- `h2.flood.violation.<kind>` — H2 flood-detector trips
  (`lib/src/protocol/mux/h2.rs:779-930`).
- `h2.{goaway,rst_stream}.{sent,received}.<code>` — H2 error
  attribution (`lib/src/protocol/mux/h2.rs:3706`).
- `epoll_time` — `Poll::poll` wall-clock, useful for worker saturation
  (`lib/src/server.rs:594`).

## 12. Removed and migrated APIs

Earlier revisions of this document (and the `e4e7488…` permalinks they
embedded) cited two modules that no longer exist on `feat/h2-mux`:

- `lib/src/https_openssl.rs` — the OpenSSL-backed HTTPS path. Sōzu
  has been rustls-only for several releases; the canonical
  replacements are `lib/src/https.rs` (proxy + listener) and
  `lib/src/protocol/rustls.rs` (per-session handshake state machine).
- `lib/src/protocol/http/mod.rs` — the pre-Kawa HTTP/1.1 state
  machine; the canonical replacement is `lib/src/protocol/kawa_h1/`
  (with its sibling
  [`LIFECYCLE.md`](../lib/src/protocol/kawa_h1/LIFECYCLE.md)).

A stale reference to either path elsewhere in `doc/` is a defect —
update it against current sources rather than copying the obsolete
name into new docs.
