# Lifetime of a session

## 1. Audience and purpose

Operator-and-new-contributor entry point for "how a request flows
through Sōzu". It explains the single-threaded mio worker, where the
per-protocol state machines sit, where the HTTP/1.1 vs HTTP/2
boundary is drawn, and which files to read next.

For deep per-protocol detail, follow the `LIFECYCLE.md` siblings:

- HTTP/1.1 and HTTP/2 mux: [`lib/src/protocol/mux/LIFECYCLE.md`](../lib/src/protocol/mux/LIFECYCLE.md)
- H1 vocabulary shared with the mux (`DefaultAnswer`, answer templates, `HttpContext`, `Method`): [`lib/src/protocol/kawa_h1/LIFECYCLE.md`](../lib/src/protocol/kawa_h1/LIFECYCLE.md)
- PROXY-protocol pre-flight: [`lib/src/protocol/proxy_protocol/LIFECYCLE.md`](../lib/src/protocol/proxy_protocol/LIFECYCLE.md)
- UDP datagram flows (connectionless; sits outside the per-session model): [`lib/src/protocol/udp/LIFECYCLE.md`](../lib/src/protocol/udp/LIFECYCLE.md)
- Master/worker supervisor: [`bin/src/command/LIFECYCLE.md`](../bin/src/command/LIFECYCLE.md)

This file stays narrative. Cited paths are repo-relative; SHA-pinned
permalinks have been removed because they rot. Where the prose names an
item — a function, a method, a struct, an enum, a field — the anchor is that
item plus its file, with no line number, because a line number does not
survive an edit above it. A `file.rs:LINE` or `file.rs:LINE-LINE` anchor is
kept only where the claim is about a specific statement or branch inside an
item; those were refreshed against `main` at `ba7fa5f9` on 2026-09-20.

## 2. Conceptual primitives

### 2.1 The mio event loop

A Sōzu worker is a single OS thread that owns one `mio::Poll`
(`Server::poll` in `lib/src/server.rs`). On Linux this is a thin wrapper
around `epoll(7)`; on the BSDs and macOS it is `kqueue(2)`. The worker
registers every socket — listen sockets, frontend, backend, metrics,
unix command-channel pair — with that single poller, then loops
reading events out of `Events` and dispatching them to the correct
session. Loop time is observable via the `epoll_time` time! metric
(`names::event_loop::EPOLL_TIME` in `Server::run`, `lib/src/server.rs`).

### 2.2 Edge-triggered readiness and the writable invariant

mio runs in edge-triggered mode: the kernel notifies the worker only
once when a socket transitions to readable or writable. If Sōzu does
not drain the kernel buffer fully on that wake-up, it gets no other
event until the *next* edge.

To survive that contract, every protocol module routes its readiness
through a `Readiness` tracker (`Readiness` in `lib/src/lib.rs`, reached in
the mux through `Connection::readiness_mut` in
`lib/src/protocol/mux/connection.rs`) and uses two helpers:

- `signal_pending_write` — set by code that has produced bytes that
  must eventually go out, even though the writable epoll edge may have
  been consumed already.
- `arm_writable` — set when bytes are queued from a *readable* code
  path so the next pump iteration writes them out without waiting for
  another kernel wake-up.

If new code queues output bytes from a readable path and forgets to
call `arm_writable` (mux) or `signal_pending_write` (pipe),
the session "stalls" — bytes sit in the buffer and the next epoll
event never arrives. Past truncation bugs on this branch all
originated here. The `mux::answers` module documents this as the
"invariant-15 pair" (`set_default_answer_arms_writable_and_signals`,
`lib/src/protocol/mux/answers.rs`); the
home for the invariant is `mux::connection`
(`lib/src/protocol/mux/connection.rs:13-16`).

### 2.3 Tokens, the SessionManager, and the slab

Every mio registration carries a `Token` (a `usize`). The
`SessionManager` (`lib/src/server.rs`) owns a
`Slab<Rc<RefCell<dyn ProxySession>>>` (`SessionManager::slab`,
`lib/src/server.rs`) that
maps each token back to the session that owns the registration. This
slab is also the unit of bookkeeping that enforces `max_connections`
(`SessionManager::check_limits` / `SessionManager::at_capacity`,
`lib/src/server.rs`) and gauges
`client.connections`, `client.connections_max`,
`client.connections_percent`, `slab.{entries,capacity,usage_percent,
accept_threshold_percent}` and `buffer.{in_use,capacity,usage_percent}`,
all sampled once per run-loop iteration in
`Server::run` (`lib/src/server.rs`).

Listeners live in that same slab, but on a different clock. A listener
is given one slab slot by `add-listener`, and it keeps that exact slot —
and therefore that exact token — until `remove-listener`. `deactivate-listener`
does *not* give the slot back: it puts the inert `ListenSession` placeholder
back in it, because the proxies keep the token inside the listener and hand
the very same one back out of a later `activate-listener`. A slot released on
deactivation would leave the reactivated socket registered under a token the
slab no longer knows, and `Server::ready` silently drops an event whose token
has no slab entry — the listener would report a successful activation and then
never see traffic again. It would also let the slab hand that key to an
ordinary session, which the UDP activation path would then overwrite.
`Server::reserve_listen_token` (`lib/src/server.rs`) holds this invariant.

Reserving the slot settles where the key may be *handed*, not what the
UDP activation path may *write* into it. UDP is the one protocol that
replaces the placeholder with a real session (`UdpListenerSession`), and
`activate-listener` is not a once-per-listener request: `UdpListener::activate`
short-circuits on its own `active` flag and answers with the same token for a
listener that is already up, `ConfigState::activate_listener` accepts the
repeat, and `load_state` re-emits one `activate-listener` per *active* listener
on every replay. The activation arm therefore installs its session only when
one is not installed already — the proxy's `listener_sessions` map is the
record — and answers `ok` for a repeat without touching anything. Rebuilding on
a repeat would drop the live session while the proxy kept the shared
`UdpManager` and its flow table, so every in-flight flow would stop forwarding
and its upstream slab slot could never be released. `close()` is a
`ProxySession` method, not `Drop`, so a displaced session runs no teardown.

A single session typically occupies *two* slab entries while it is
forwarding traffic: one for the frontend token (registered when the
client connection was accepted) and one for the backend token
(registered after `connect_to_backend` succeeded). This is the
single biggest mental-model adjustment for new contributors: the same
session is reachable through two different keys.

Listen sockets themselves are stored as `ListenSession` entries in the
same slab (`ListenSession`, `lib/src/server.rs`), which is what allows
the same event loop to multiplex accept events alongside data events.

### 2.4 The three proxies

A worker hosts three proxy types, one per supported listener protocol:

- `HttpProxy` (`lib/src/http.rs`)
- `HttpsProxy` (`lib/src/https.rs`)
- `TcpProxy` (`lib/src/tcp.rs`)

Each proxy owns its listeners, its known frontends and clusters, the
per-protocol configuration (TLS material, ALPN list, H2 knobs, etc.),
and the upgrade paths that promote a session from one protocol layer
to the next.

## 3. Accepting a connection

### 3.1 Listeners and SO_REUSEPORT

Listen sockets are created via `lib/src/socket.rs` with
`SO_REUSEPORT` enabled (`server_bind`, `lib/src/socket.rs`); multiple
workers in the same Sōzu process share each listener address and the kernel
distributes accept events across them. Each listener is registered
with mio and tracked through a `ListenSession` slab entry. Hot
reconfig adds and removes listeners at runtime via the master-to-
worker channel (`Server::notify_proxys`, `lib/src/server.rs`).

### 3.2 The accept queue

When a listener becomes readable, the proxy accepts every pending
connection in a single batch and parks each `TcpStream` on an internal
`accept_queue: VecDeque<…>` (`Server::accept_queue`,
`lib/src/server.rs`). Sessions are
*not* created synchronously inside the accept loop. The queue is
drained later, newest-first, so connections that have been waiting
too long are dropped before they are turned into a session. The
cut-off is `accept_queue_timeout` (`Server::accept_queue_timeout`,
`lib/src/server.rs`).

### 3.3 Backpressure and `max_connections`

If the slab is at capacity (`SessionManager::can_accept` is `false`,
`lib/src/server.rs`) the proxy stops draining the accept queue and
the kernel's listen backlog absorbs the surplus. The
`accept_queue.backpressure` gauge flips to 1 in that state
(`SessionManager::check_limits`, `lib/src/server.rs`); a 1 Hz ticker also bumps
`accept_queue.saturated_seconds` so dashboards can plot how long the
worker spent backpressured (`lib/src/server.rs:110-114`, and the
`ACCEPT_SATURATION_TICK` block of `Server::run` in `lib/src/server.rs`). The
system unwinds at 90% of `max_connections` to avoid flapping
(`lib/src/server.rs:1056-1064`).

### 3.4 Zombie detection

A periodic "zombie checker" pass walks the slab and forcibly closes
sessions that look stuck — typically because of a logic bug elsewhere.
This is a safety net, not a primary lifecycle mechanism.

## 4. TLS handshake (HTTPS only)

For `HttpsProxy` sessions, the first protocol layer above raw TCP is
TLS. Sōzu uses [rustls](https://docs.rs/rustls) and instantiates one
`rustls::ServerConnection` per session
(`TlsHandshake::session`, `lib/src/protocol/rustls.rs`). The handshake
itself is driven from `lib/src/protocol/rustls.rs`; the listener-level config
(certificate stores, ALPN list, SNI binding policy) lives in
`lib/src/https.rs` and `lib/src/tls.rs`.

### 4.1 SNI / `:authority` binding

If `strict_sni_binding` is enabled on a listener
(`ListenerBuilder::strict_sni_binding`, `command/src/config.rs`), Sōzu
rejects any HTTP request whose
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
protocol (`lib/src/https.rs:446-505`) and decides which mux
flavour to instantiate:

- ALPN `h2` → HTTP/2 mux.
- ALPN `http/1.1` → HTTP/1.1 path.
- No ALPN selected → HTTP/1.1 by default.

Listener-level `disable_http11` (`ListenerBuilder::disable_http11`,
`command/src/config.rs`) lets an
operator force H2-only on a per-listener basis. ALPN rejections are
counted with two distinct keys so dashboards can split refusals by
cause:

- `https.alpn.rejected.unsupported` — peer offered an ALPN that Sōzu
  does not implement (e.g. `h3`) (`lib/src/https.rs:483`).
- `https.alpn.rejected.http11_disabled` — peer wanted `http/1.1` but
  the listener has `disable_http11 = true`
  (`lib/src/https.rs:466, 496`).

The startup-time validator at `command/src/config.rs:1279-1283, 1301-1307`
catches the obvious operator mistake of pairing `disable_http11 = true` with
`alpn_protocols` that still contains `"http/1.1"`.

### 4.3 Handshake telemetry

Successful handshakes report `tls.handshake_ms` as a histogram
(`TlsHandshake::record_handshake_duration_ms`,
`lib/src/protocol/rustls.rs`). Failures are
tagged with a constant key per rustls error variant
(`tls.handshake.failed.alert_received`,
`tls.handshake.failed.no_alpn`, …) so statsd cardinality stays bounded
even when a misbehaving client is hammering the handshake
(`handshake_failure_reason`, `lib/src/protocol/rustls.rs`).

## 5. PROXY-protocol pre-flight

When a frontend is configured to expect a HAProxy PROXY-protocol
header (typically because Sōzu sits behind a Layer-4 load balancer)
the session starts in a small `ExpectProxyProtocol` state
(`HttpSession::new` in `lib/src/http.rs`, `ExpectProxyProtocol::readable`
in `lib/src/protocol/proxy_protocol/expect.rs`). That state reads the
v1 / v2 header off the front socket, extracts the real client address,
and then transitions the session into the downstream protocol
(HTTP/1.1, HTTP/2, or raw TCP relay). A v2 header carrying the `LOCAL`
command (ver/cmd `0x20`) is the exception: it describes a connection the
upstream proxy originated itself, so its address block is discarded per the
HAProxy PROXY protocol specification §2.2 and no client address is
extracted at all.

What happens next depends on the listener, because the two families
resolve the resulting `ProxyAddr::AfUnspec` differently. A **TCP**
listener keeps the front socket's own `peer_addr`:
`ExpectProxyProtocol::into_pipe`
(`lib/src/protocol/proxy_protocol/expect.rs`) /
`RelayProxyProtocol::into_pipe`
(`lib/src/protocol/proxy_protocol/relay.rs`), the SNI preread
(`TcpSession::build_pipe_from_preread`, `lib/src/tcp.rs`) and
`TcpSession::effective_session_address`
(`lib/src/tcp.rs`, which feeds the raw-TCP `max_connections_per_ip`
gate) all fall back to it. An **HTTP or HTTPS**
listener instead refuses the upgrade: `HttpSession::upgrade_expect`
(`lib/src/http.rs`) / `HttpsSession::upgrade_expect` (`lib/src/https.rs`)
needs both a source and a destination to build the session, `AfUnspec` supplies neither, so it
returns `None` and `HttpSession::upgrade` reports `SessionIsToBeClosed`
(`lib/src/http.rs`) — the session is closed at the expect stage,
before any request is read.

That close is not a regression for legitimate traffic. HAProxy pairs
`LOCAL` with `AF_UNSPEC`, which already parsed to `AfUnspec`, so an
HTTP or HTTPS session from a health-checking upstream already closed
here. The only behaviour the discard changes is the forged case — a
`LOCAL` header carrying a populated address block — which used to
upgrade with attacker-chosen addresses and now closes instead.

The full lifecycle of the three sub-state-machines (`expect`, `relay`,
`send`) is documented in
[`lib/src/protocol/proxy_protocol/LIFECYCLE.md`](../lib/src/protocol/proxy_protocol/LIFECYCLE.md);
read it before changing anything in
`lib/src/protocol/proxy_protocol/`.

## 6. Per-protocol session lifecycle

Once the session has gone through any TLS and PROXY-protocol
pre-flight, control transfers to one of three protocol state machines
that own the rest of the session.

### 6.1 HTTP/1.1 (mux in H1 mode)

The HTTP/1.1 path is the historical core of Sōzu and is backed by the
[Kawa](https://github.com/CleverCloud/kawa) HTTP parser. Since the mux
migration it does **not** run through a protocol module of its own:
`HttpStateMachine` and `HttpsStateMachine` carry a `Mux` variant only, so
H1 and H2 share `lib/src/protocol/mux/` and differ in their connection
type (`ConnectionH1` in `mux/h1.rs`, `ConnectionH2` in `mux/h2.rs`). The
standalone `kawa_h1::Http` session that used to own this path was removed
on 2026-09-20 (sozu#1346) after a planted `panic!` proved no binary
constructed it. Conceptually the lifecycle is:

1. **Parse the request** out of the front buffer using Kawa, in
   `ConnectionH1::readable` (`lib/src/protocol/mux/h1.rs`), driven by the
   `HttpContext` callbacks in `lib/src/protocol/kawa_h1/editor.rs`.
2. **Route the request** to a cluster via `Router::route_from_request`
   (`lib/src/protocol/mux/router.rs`).
3. **Pick a backend and connect to it** via `Router::backend_from_request`
   (same file), which `Mux::dial_backend` (`lib/src/protocol/mux/mod.rs`) calls
   once `Router::plan_connect` has decided to dial. A previously-opened
   keep-alive socket may be reused after a liveness probe.
4. **Forward bytes** in both directions through the per-`Stream`
   front/back Kawa buffer pair (`lib/src/protocol/mux/stream.rs`),
   registering writable interest with `arm_writable` as needed.
5. **Close or reset** when the response completes, emitting the access log
   and the status metrics from `Stream::generate_access_log`.

The full state diagram, including the parser back-pressure rules, the
H1 → WebSocket upgrade path, the H1 → H2 transition, and the keep-alive vs
close attribution, lives in
[`lib/src/protocol/mux/LIFECYCLE.md`](../lib/src/protocol/mux/LIFECYCLE.md).
[`lib/src/protocol/kawa_h1/LIFECYCLE.md`](../lib/src/protocol/kawa_h1/LIFECYCLE.md)
now documents only the H1 vocabulary that module still provides
(`DefaultAnswer`, `HttpAnswers`, `HttpContext`, `Method`).

### 6.2 HTTP/2 (mux)

The H2 multiplexer is the largest single piece of Sōzu and lives under
`lib/src/protocol/mux/`. The high-level data model:

- A single `ConnectionH2` per TCP connection, wrapped in an
  `H2Shell<Front>` that holds the socket
  (`lib/src/protocol/mux/h2.rs`) owns the wire state: HPACK encoder
  and decoder, connection-level flow window, GOAWAY state, and the
  per-connection `H2FloodDetector`.
- A `Context<L>` (`lib/src/protocol/mux/mod.rs`) owns the
  `Vec<Stream>` that backs every individual H2 stream's request /
  response buffers. Streams are referenced across the two through a
  `GlobalStreamId = usize`.
- Each `Stream` carries a `StreamState`
  (`lib/src/protocol/mux/stream.rs`) that walks through the
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
  (`parse_rfc9218_priority`, `lib/src/protocol/mux/pkawa.rs`) and feed the
  writable scheduler so a slow priority-7 download cannot starve a priority-0
  interactive request.
- **GOAWAY and graceful drain.** After GOAWAY(NO_ERROR) the connection
  enters draining mode (`H2DrainState::draining`,
  `lib/src/protocol/mux/h2_drain.rs`); new
  peer streams must be refused (RFC 9113 §6.8) and existing streams
  must complete. The graceful-shutdown deadline is driven from the
  listener config (`HttpsListener::get_h2_graceful_shutdown_deadline`,
  `lib/src/https.rs`).
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
  `glitch_window`; see `H2FloodViolation` in
  `lib/src/protocol/mux/h2_flood_detector.rs` and
  `ConnectionH2::handle_flood_violation`).
  GOAWAY and RST_STREAM sends/receives are attributed by error code
  via `h2.{goaway,rst_stream}.{sent,received}.<code>`
  (`metric_for_goaway_sent` / `metric_for_goaway_received` /
  `metric_for_rst_stream_sent` / `metric_for_rst_stream_received` in
  `lib/src/protocol/mux/h2.rs`).
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
(`Pipe`, `lib/src/protocol/pipe.rs`). The pipe holds no protocol
parser; it shuttles bytes between the front and back sockets and
relies on the standard readiness-pumping discipline. WebSocket
metadata (`WebSocketContext`, `lib/src/protocol/pipe.rs`) is
inherited from the H1 mux at upgrade time so logging and metrics
keep their context.

## 7. Connect to the backend cluster

Routing happens after the request headers are parsed. The router asks
"which cluster does this `(host, path, method)` match?" and returns a
`cluster_id`. From there the load-balancer picks a backend.

The available algorithms all live in `lib/src/load_balancing.rs`:

- `RoundRobin` — the next backend in declaration order. Reads no load.
- `Random` — one backend drawn at random, weighted by its configured
  `weight`. Reads no load.
- `LeastLoaded` — reads the load of every backend and takes the minimum.
  The best balance available, at `O(n)` load reads per request.
- `PowerOfTwo` — power-of-two-choices: draws two backends at random and
  keeps the less loaded of the two, breaking an exact tie with a coin flip.
  It reads two backend loads whatever the cluster size, against
  `LeastLoaded`'s `n`, and stays close to `LeastLoaded`'s balance. Because
  no worker ever computes a global minimum, independently seeded workers
  cannot herd onto the same backend.
- `Rendezvous` (`HRW`) and `Maglev` — flow-affine hashing for UDP clusters;
  the backend is a function of the flow key, so they read no load either.
  Both DO read `weight` on that keyed path — `Rendezvous::score` scales the
  rendezvous score by it, `Maglev::rebuild` hands out table slots in
  proportion to it — and both fall back to `RoundRobin`, which ignores it,
  when they are called with no key.

`LeastLoaded` and `PowerOfTwo` are the two policies that read load, through
the cluster's `load_metric`.

Those load-read counts are not the per-request cost of a policy. Whatever the
policy, `BackendList::next_available_backend_with_key` first builds the
candidate set with `BackendList::available_backends`, which walks the
cluster's backend list and clones every healthy backend into a fresh `Vec`.
Every policy is `O(n)` per request because of that walk; power-of-two-choices
only removes the `n` load reads layered on top of it.

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
(`names::backend`, `lib/src/metrics/names.rs`).

## 9. Closing the session

A session ends when:

- The H1 response completes and either side closed the connection, or
- The H2 stream pool drains after GOAWAY and the connection is
  destroyed, or
- A protocol error or flood violation forces a hard close, or
- The zombie checker decides the session is wedged.

For TLS frontends specifically, the close path uses **write-only
shutdown** on the front socket
(the `Shutdown::Write` block of `HttpsSession::close` in
`lib/src/https.rs`, mirrored in `HttpSession::close` in
`lib/src/http.rs`):

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
TCP path (`lib/src/tcp.rs:1563-1567, 1838-1843`) keeps `Shutdown::Both`
because it has no encrypted send-buffer to truncate; the comment
flags that a future TLS upgrade on TCP would need to switch modes.

After shutdown, `state.close(...)` closes the backend, flushes any
close-notify, and releases buffers; the proxy removes the session
from the slab under both front and back tokens, mio deregisters the
sockets, and the slab entries return to the free list. Half-closed
H2 streams unwind the same way — per-stream cleanup in `mux::mod` and
`mux::router` decrements `backend.pool.size`
(`Mux::close` in `lib/src/protocol/mux/mod.rs`, `Router::plan_connect` in
`lib/src/protocol/mux/router.rs`).

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

Detailed master/worker lifecycle, the `HardStop` / `SoftStop` arms of
`Server::read_channel_messages_and_notify` (`lib/src/server.rs`), and
the audit-log envelope live in
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
| H1 vocabulary (`DefaultAnswer`, answer templates, `HttpContext` editor, `Method`) | `lib/src/protocol/kawa_h1/` |
| HTTP/1.1 and HTTP/2 mux (connection, frames, HPACK, priorities, scheduler, flood detector, router, backend connect, keep-alive) | `lib/src/protocol/mux/` |
| WebSocket / TCP pass-through after upgrade | `lib/src/protocol/pipe.rs` |
| PROXY-protocol pre-flight (expect / relay / send) | `lib/src/protocol/proxy_protocol/` |
| Routing and load balancing | `lib/src/router/`, `lib/src/load_balancing.rs`, `lib/src/backends.rs` |
| Metrics emission | `lib/src/metrics/mod.rs` |
| Master/worker supervisor, command socket, hot upgrade | `bin/src/command/`, `bin/src/upgrade.rs` |
| Config knobs (buffer_size, ALPN, H2 timeouts, flood thresholds, sticky sessions) | `command/src/config.rs` |
| Per-protocol log macros (`MUX-H1`, `MUX-H2`, `RUSTLS`, `PIPE`, `TCP`, `HTTPS`, …) | each module's `log_context!` family |

## 11.5 Where metrics fire along the path

Full taxonomy: `doc/configure.md` + `lib/src/metrics/`. The minimum
set to read a session's life from a dashboard:

- `tls.handshake_ms`, `tls.handshake.failed.<reason>` — handshake
  latency + per-rustls-variant failure attribution
  (`TlsHandshake::record_handshake_duration_ms` /
  `handshake_failure_reason`, `lib/src/protocol/rustls.rs`).
- `https.alpn.rejected.{unsupported,http11_disabled}` — ALPN refusal
  causes (`lib/src/https.rs:466, 483, 496`).
- `client.connections`, `client.connections_max`,
  `client.connections_percent` — slab-backed lifecycle gauges
  (`client.connections` is sampled per increment/decrement in
  `SessionManager::incr/decr`; `_max` and `_percent` are sampled in the
  run loop alongside `slab.*` and `buffer.*`).
- `accept_queue.backpressure`, `accept_queue.saturated_seconds` —
  binary backpressure + time-integrated saturation
  (`SessionManager::check_limits` and `SessionManager::decr` in
  `lib/src/server.rs`, plus the `ACCEPT_SATURATION_TICK` block of
  `Server::run` in the same file).
- `backend.pool.size` — long-lived gauge mirroring open backend
  connections (`Router::plan_connect` in `lib/src/protocol/mux/router.rs`,
  `Mux::close` in `lib/src/protocol/mux/mod.rs`,
  `Connection::pre_close_client_bookkeeping` in
  `lib/src/protocol/mux/connection.rs`).
- `requests`, `bytes_in`, `bytes_out`, `backend_response_time` —
  per-cluster + per-backend counters and timing
  (`names::backend`, `lib/src/metrics/names.rs`).
- `h2.flood.violation.<kind>` — H2 flood-detector trips
  (`ConnectionH2::handle_flood_violation`,
  `lib/src/protocol/mux/h2.rs`).
- `h2.{goaway,rst_stream}.{sent,received}.<code>` — H2 error
  attribution (the `metric_for_goaway_sent` family in
  `lib/src/protocol/mux/h2.rs`).
- `epoll_time` — `Poll::poll` wall-clock, useful for worker saturation
  (`names::event_loop::EPOLL_TIME` in `Server::run`, `lib/src/server.rs`).

## 12. Removed and migrated APIs

Earlier revisions of this document (and the `e4e7488…` permalinks they
embedded) cited two modules that no longer exist on `feat/h2-mux`:

- `lib/src/https_openssl.rs` — the OpenSSL-backed HTTPS path. Sōzu
  has been rustls-only for several releases; the canonical
  replacements are `lib/src/https.rs` (proxy + listener) and
  `lib/src/protocol/rustls.rs` (per-session handshake state machine).
- `lib/src/protocol/http/mod.rs` — the pre-Kawa HTTP/1.1 state
  machine. Its Kawa-backed successor, `kawa_h1::Http`, was itself
  removed on 2026-09-20 (sozu#1346) once it became unreachable; the
  canonical replacement for the H1 datapath is now
  `lib/src/protocol/mux/` (with its sibling
  [`LIFECYCLE.md`](../lib/src/protocol/mux/LIFECYCLE.md)).

A stale reference to either path elsewhere in `doc/` is a defect —
update it against current sources rather than copying the obsolete
name into new docs.
