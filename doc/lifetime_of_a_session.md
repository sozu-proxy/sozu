# Lifetime of a session

## 1. Audience and purpose

Operator-and-new-contributor entry point for "how a request flows through
Sōzu". It follows a connection from `accept(2)` to `close(2)` for every
protocol the worker serves — HTTP/1.1, HTTP/2, raw TCP and UDP — and says, at
each step, which system calls and heap allocations the nominal path still
pays. The figures come from the measurement described in
[`hot_path_zero_copy.md`](./hot_path_zero_copy.md) §2, taken on 2026-09-28 at
`c086b456`; that document also explains why each cost that used to be here is
gone.

For deep per-protocol detail, follow the `LIFECYCLE.md` siblings:

- HTTP/1.1 and HTTP/2 mux: [`lib/src/protocol/mux/LIFECYCLE.md`](../lib/src/protocol/mux/LIFECYCLE.md)
- H1 vocabulary shared with the mux (`HttpContext`, header editing, default answers): [`lib/src/protocol/kawa_h1/LIFECYCLE.md`](../lib/src/protocol/kawa_h1/LIFECYCLE.md)
- PROXY-protocol pre-flight: [`lib/src/protocol/proxy_protocol/LIFECYCLE.md`](../lib/src/protocol/proxy_protocol/LIFECYCLE.md)
- TCP SNI/ALPN preread: [`lib/src/protocol/tcp_preread/LIFECYCLE.md`](../lib/src/protocol/tcp_preread/LIFECYCLE.md)
- UDP datagram flows: [`lib/src/protocol/udp/LIFECYCLE.md`](../lib/src/protocol/udp/LIFECYCLE.md)
- Master/worker supervisor: [`bin/src/command/LIFECYCLE.md`](../bin/src/command/LIFECYCLE.md)

This file stays narrative. Paths are repo-relative. Every anchor is a symbol —
a function, a method, a type or a field, plus its file — never a line number,
because a line number does not survive an edit above it; where the prose means
one branch inside a function, it names the branch in words.

## 2. Conceptual primitives

### 2.1 The mio event loop

A Sōzu worker is a single OS thread that owns one `mio::Poll` (`Server::poll`
in `lib/src/server.rs`). On Linux this is `epoll(7)`; on the BSDs and macOS it
is `kqueue(2)`. The worker registers every socket — listeners, frontend and
backend sockets, the metrics socket, the command channel — with that one
poller, then loops in `Server::run`:

1. one `epoll_wait(2)`, whose timeout is the next timer date capped by
   `poll_timeout` (1 s);
2. the command channel (token 0), the timer wheel (token 1), the metrics socket
   (token 2), the TCP and UDP health checkers, then every other token through
   `Server::ready` to the session that owns it;
3. `handle_remaining_readiness`, `create_sessions` (the accept queue, §3.2),
   `zombie_check`, the health checkers' `poll` and `UdpProxy::health_poll`;
4. the gauges, the metrics flush, and — during a soft stop — one
   `shut_down_sessions` pass (§10).

Nothing in steps 2 to 4 issues a system call per turn unless a socket has work:
`UdpProxy::health_poll` borrows the worker's `Registry` instead of cloning it,
which used to cost an `fcntl(F_DUPFD_CLOEXEC)` and a `close(2)` on every turn
whether or not a UDP cluster existed
([#1553](https://github.com/sozu-proxy/sozu/pull/1553)). Loop time is observable
through the `epoll_time` and `event_loop_time` metrics
(`names::event_loop`, `lib/src/metrics/names.rs`). Clocks are read through the
vDSO and cost no system call: the mux samples `Instant::now()` once per pass
(`lib/src/protocol/mux/LIFECYCLE.md` §7.5).

### 2.2 Edge-triggered readiness and short reads

mio registers every socket edge-triggered (`EPOLLET`): the kernel notifies the
worker once per change, and a socket that is not drained on that wake-up gets
no further event until the next edge. Each session keeps the edge it received
in a `Readiness` (`lib/src/lib.rs`: `event` is what the kernel said, `interest`
is what the state machine wants).

"Drained" is detected without a wasted `recv(2)`
([#1602](https://github.com/sozu-proxy/sozu/issues/1602)). A read that returns
fewer bytes than it was offered has emptied a stream socket's receive queue
(`man 7 epoll`), so `plain_socket_read` and `rustls_socket_read`
(`lib/src/socket.rs`) stop there and answer `WouldBlock` with the bytes instead
of reading on to an `EAGAIN`; any byte that arrives later raises a new edge. A
read that fills its buffer is not a short read and answers `Continue`. The one
exception is a FIN that arrived with the data: its edge is the HUP already
folded into the readiness, and nothing will announce the EOF again, so
`update_readiness_after_read` (`lib/src/protocol/mux/mod.rs`) keeps READABLE
after a short read once HUP was seen, and the next pass reads the EOF. The pipe
and the pre-mux states act on HUP directly (`Pipe::frontend_hup`,
`Pipe::backend_hup`) and need no EOF read.

A TLS frontend carries that proof from one read call to the next
([#1609](https://github.com/sozu-proxy/sozu/issues/1609)). The H2 read path asks
for a 9-byte frame header, then its payload, so the frames of one record are
served by several calls from rustls's plaintext buffer; `rustls_socket_read`
serves that plaintext before it calls `read_tls` at all
([#1588](https://github.com/sozu-proxy/sozu/issues/1588)), and
`FrontRustls::recv_memory` (`RecvMemory`, `lib/src/socket.rs`) remembers that a
`recv` answered short or `EAGAIN`, so the call that empties the plaintext does
not `recv` again for an `EAGAIN`. `HttpsSession::update_readiness`
(`lib/src/https.rs`) clears the memory on every event delivered for the
frontend token, in every session state: a byte that arrived after the proof
raised an edge that is either pending in epoll, and re-arms READABLE when the
next `epoll_wait` delivers it, or already delivered, which cleared the memory.
HUP or ERROR turns the memory off for good.

The stop has a price, and it is HAProxy's (`src/raw_sock.c`: stop when
`ret < try`, let the poller report the EOF). When a backend's FIN lands after
the `epoll_wait` that woke the pass, the event carries READABLE without HUP; the
short read returns the last bytes and does not issue the `recv` that would have
found the EOF, so the backend is closed one `epoll_wait` round later, on the
`EPOLLRDHUP` edge the FIN queued, and no later
(`a_fin_after_epoll_wait_closes_the_backend_on_the_next_round_at_the_latest`).
When the FIN reached the kernel before `epoll_wait` returned, the edge carries
HUP and the backend is closed in the same pass.

The guarantee has one known hole: TCP urgent data. `recv` stops before an
urgent (out-of-band) mark even with bytes queued behind it, so a short read
while such a mark is pending leaves those bytes in the kernel with no further
edge. A stream that carries OOB data (telnet, rlogin, FTP `ABOR`) can stall
until the peer's next send. HAProxy and tokio behave the same, and Sōzu never
read out-of-band data before this change either.

### 2.3 The writable invariant

Output produced from a read path has no epoll edge of its own: the writable
edge may have been consumed long ago. Every protocol module therefore routes
its readiness through the `Readiness` tracker (reached in the mux through
`Connection::readiness_mut`, `lib/src/protocol/mux/connection.rs`) and uses two
helpers:

- `signal_pending_write` — set by code that has produced bytes that must go
  out, even though the writable edge may have been consumed already;
- `arm_writable` — set when bytes are queued from a *readable* code path so the
  next pump iteration writes them without waiting for another kernel wake-up.

Forgetting either stalls the session: the bytes sit in a buffer and no event
ever arrives. The `mux::answers` module documents this as the "invariant-15
pair" (`set_default_answer_arms_writable_and_signals`,
`lib/src/protocol/mux/answers.rs`). The module doc of
`lib/src/protocol/mux/connection.rs` names the canonical home of the invariant:
the edge-trigger discipline of `H2Shell::writable` (`lib/src/protocol/mux/h2.rs`),
to which the `Connection` abstractions delegate through the protocol-specific
writers.

### 2.4 Tokens, the SessionManager, and the slab

Every mio registration carries a `Token` (a `usize`). The `SessionManager`
(`lib/src/server.rs`) owns a `Slab<Rc<RefCell<dyn ProxySession>>>`
(`SessionManager::slab`) that maps each token back to the session that owns
the registration. This slab is also the unit of bookkeeping that enforces
`max_connections` (`SessionManager::check_limits` /
`SessionManager::at_capacity`) and feeds `client.connections`,
`client.connections_max`, `client.connections_percent`,
`slab.{entries,capacity,usage_percent,accept_threshold_percent}` and
`buffer.{in_use,capacity,usage_percent}`, all sampled once per loop turn in
`Server::run`.

Listeners live in the same slab, on a different clock. A listener is given
one slab slot by `add-listener` and keeps that exact slot — and therefore that
exact token — until `remove-listener`. `deactivate-listener` does *not* give the
slot back: it puts the inert `ListenSession` placeholder back in it, because
the proxies keep the token inside the listener and hand the very same one back
out of a later `activate-listener`. A slot released on deactivation would leave
the reactivated socket registered under a token the slab no longer knows, and
`Server::ready` silently drops an event whose token has no slab entry.
`Server::reserve_listen_token` (`lib/src/server.rs`) holds this invariant. UDP
is the one protocol that replaces the placeholder with a real session
(`UdpListenerSession`), and its activation arm installs that session only when
none is installed already (the proxy's `listener_sessions` map is the record),
because `load_state` re-emits one `activate-listener` per active listener on
every replay.

A forwarding session occupies one slab entry per socket: the frontend token,
plus one token per backend connection. The same session is reachable through
several keys.

### 2.5 The four proxies

A worker hosts one proxy per listener protocol:

- `HttpProxy` (`lib/src/http.rs`) and `HttpsProxy` (`lib/src/https.rs`), whose
  sessions run the mux (§6, §7, §8);
- `TcpProxy` (`lib/src/tcp.rs`), whose sessions run a byte pipe (§9);
- `UdpProxy` (`lib/src/udp.rs`), connectionless, one session per listener (§10).

Each proxy owns its listeners, its frontends and clusters, the per-protocol
configuration (TLS material, ALPN list, H2 knobs, …) and the upgrade paths that
promote a session from one protocol layer to the next.

## 3. Accepting a connection

### 3.1 Listeners, `SO_REUSEPORT` and `TCP_NODELAY`

Listen sockets are created with `SO_REUSEPORT` (`server_bind`,
`lib/src/socket.rs`); several workers share each listener address and the
kernel spreads accepts across them. Hot reconfiguration adds and removes
listeners at runtime through the command channel (`Server::notify_proxys`).

`TCP_NODELAY` is set once per listener, in each listener's `activate`
(`HttpListener::activate`, `HttpsListener::activate`, `TcpListener::activate`),
on whichever socket it activates: freshly bound, inherited over SCM_RIGHTS at
an upgrade, or parked by a failed registration. Linux and the BSDs copy the
flag to every socket whose handshake completes afterwards, so an accepted
socket needs no `setsockopt(2)` of its own. A connection that completed its
handshake before the listener had the flag — the backlog of a socket inherited
from a worker that never set it — does not inherit it, so each listener's
`accept` sets it per connection until its first `WouldBlock` after an
activation (`nodelay_backlog`), which is exactly that backlog
([#1586](https://github.com/sozu-proxy/sozu/issues/1586)).

### 3.2 The accept queue

When a listener becomes readable, the proxy accepts every pending connection in
one batch — one `accept4(2)` per connection plus the one that answers `EAGAIN`
and ends the batch — and parks each `TcpStream` on
`Server::accept_queue`, together with the peer address `accept(2)` returned.
That address feeds the `client.connect.per_source.*` counter and is handed to
`create_session`, which seeds the session with it: nothing on the accept path
calls `getpeername(2)` ([#1586](https://github.com/sozu-proxy/sozu/issues/1586)).
On an expect-proxy listener it stays the network peer (the load balancer); the
client address comes from the PROXY header when the session upgrades.

Sessions are *not* created inside the accept loop. `Server::create_sessions`
drains the queue later in the same turn, newest first, and drops connections
that waited longer than `accept_queue_timeout`. Creating a session registers
its frontend socket (`epoll_ctl(EPOLL_CTL_ADD)`) and allocates the session
object once: `Rc<RefCell<HttpSession>>` carries the mux inline, including the
first backend connection slot and the frontend's timer handle (§8.1).

### 3.3 Backpressure and `max_connections`

If the slab is at capacity (`SessionManager::can_accept` is `false`) the proxy
stops draining the accept queue and the kernel's listen backlog absorbs the
surplus. The `accept_queue.backpressure` gauge flips to 1 in that state
(`SessionManager::check_limits`); a 1 Hz ticker also bumps
`accept_queue.saturated_seconds` so dashboards can plot how long the worker
spent backpressured (the `ACCEPT_SATURATION_TICK` block of `Server::run`). The
gate reopens only below 90 % of `max_connections`, to avoid flapping (the
`can_accept` branch of `SessionManager::decr`).

### 3.4 Zombie detection

A periodic "zombie checker" pass (`Server::zombie_check`) walks the slab and
closes sessions that look stuck — typically because of a logic bug elsewhere.
It is a safety net, not a lifecycle mechanism.

## 4. TLS handshake (HTTPS only)

For `HttpsProxy` sessions the first layer above TCP is TLS. Sōzu uses
[rustls](https://docs.rs/rustls) and instantiates one
`rustls::ServerConnection` per session (`TlsHandshake::session`,
`lib/src/protocol/rustls.rs`); the listener-level configuration (certificate
stores, ALPN list, SNI binding policy) lives in `lib/src/https.rs` and
`lib/src/tls.rs`.

The handshake reads the way the established session does (§2.2):
`handshake_read` (`lib/src/protocol/rustls.rs`) stops on a `recv` that answers
fewer bytes than rustls offered, as on `EAGAIN`, and drops READABLE; the next
segment of a ClientHello split across several raises the next edge. When the
handshake completes, `upgraded_frontend_events` (`lib/src/https.rs`) arms the
mux frontend for WRITABLE, and for READABLE only when the handshake still held
a READABLE edge or rustls already holds plaintext (an HTTP/2 preface sharing a
segment with the client `Finished`) or a `close_notify`
([#1609](https://github.com/sozu-proxy/sozu/issues/1609)).

On the wire a TLS 1.3 handshake costs the server one `writev(2)` for its flight
(ServerHello, ChangeCipherSpec, encrypted handshake messages) and one for the
session tickets (`send_tls13_tickets`, four by default; `0` removes that
write). With `crypto-ring`, the provider of the default build (`bin/Cargo.toml`,
`lib/Cargo.toml`, the Dockerfile, the RPM and Arch packages), each handshake also
costs 15 `getrandom(2)`, one per random draw, because ring reads the kernel
directly; a build with `crypto-aws-lc-rs` draws from a thread-local DRBG and
issues none on this path ([`hot_path_zero_copy.md`](./hot_path_zero_copy.md)
§3.13).

### 4.1 SNI / `:authority` binding

If `strict_sni_binding` is enabled on a listener
(`ListenerBuilder::strict_sni_binding`, `command/src/config.rs`), Sōzu rejects
any HTTP request whose `:authority` (H2) or `Host` (H1) is not covered by a SAN
of the certificate served on this TLS session, with RFC 6125 §6.4.3 wildcard
handling. This matches browser connection coalescing (RFC 9113 §9.1.1): a
browser reuses one H2 connection for any origin covered by the served
certificate's dNSName entries (RFC 6125 §6.4.4: when SAN dNSName is present, the
CN is ignored). Misses are answered with 421 Misdirected Request
(RFC 9110 §15.5.20), which browsers handle by opening a fresh connection on the
right SNI. The SAN snapshot is captured once at handshake and stored on the mux
`Context` as `tls_cert_names`, shared by every stream through an `Arc`; it is
frozen for the connection's lifetime even if the operator swaps the certificate
mid-flight. Plaintext listeners have no certificate to compare against and
bypass the check.

### 4.2 ALPN and `disable_http11`

After the handshake, `HttpsSession::upgrade_handshake` (`lib/src/https.rs`)
reads the negotiated ALPN protocol and picks the mux flavour:

- ALPN `h2` → HTTP/2 frontend;
- ALPN `http/1.1`, or no ALPN → HTTP/1.1 frontend.

Listener-level `disable_http11` (`ListenerBuilder::disable_http11`,
`command/src/config.rs`) forces H2-only per listener. ALPN refusals are counted
under two keys so dashboards can split them by cause: `https.alpn.rejected.unsupported`
(the peer offered an ALPN Sōzu does not implement, e.g. `h3`) and
`https.alpn.rejected.http11_disabled` (the peer wanted `http/1.1` on a
`disable_http11` listener), both emitted by `HttpsSession::upgrade_handshake`.
The configuration validator refuses `disable_http11 = true` beside an
`alpn_protocols` list that still contains `"http/1.1"`
(`ConfigError::DisableHttp11WithHttp11Alpn`, returned by `ListenerBuilder::to_tls`,
`command/src/config.rs`).

### 4.3 Handshake telemetry

Successful handshakes report `tls.handshake_ms` as a histogram
(`TlsHandshake::record_handshake_duration_ms`). Failures are tagged with one
constant key per rustls error variant (`tls.handshake.failed.alert_received`,
`tls.handshake.failed.no_alpn`, …) so the metric cardinality stays bounded
under a misbehaving client (`handshake_failure_reason`,
`lib/src/protocol/rustls.rs`).

## 5. PROXY-protocol pre-flight

When a frontend expects a HAProxy PROXY-protocol header (Sōzu behind a layer-4
load balancer), the session starts in `ExpectProxyProtocol`
(`HttpSession::new`, `lib/src/http.rs`; `ExpectProxyProtocol::readable`,
`lib/src/protocol/proxy_protocol/expect.rs`). That state reads the v1 / v2
header off the front socket, extracts the real client address, and transitions
the session into the downstream protocol (HTTP/1.1, HTTP/2 or raw TCP). A v2
header carrying the `LOCAL` command describes a connection the upstream proxy
originated itself: its address block is discarded (PROXY protocol
specification §2.2). A TCP listener then keeps the front socket's own
`peer_addr` (`ExpectProxyProtocol::into_pipe`, `RelayProxyProtocol::into_pipe`,
`TcpSession::build_pipe_from_preread`, `TcpSession::effective_session_address`);
an HTTP or HTTPS listener refuses the upgrade (`HttpSession::upgrade_expect` /
`HttpsSession::upgrade_expect` need both a source and a destination), so the
session closes before any request is read. The full lifecycle of the three
sub-state machines is in
[`lib/src/protocol/proxy_protocol/LIFECYCLE.md`](../lib/src/protocol/proxy_protocol/LIFECYCLE.md).

## 6. HTTP/1.1 request lifecycle

H1 runs on the mux: `HttpStateMachine` and `HttpsStateMachine` carry a `Mux`
variant, whose frontend is a `ConnectionH1` (`lib/src/protocol/mux/h1.rs`)
driving a [kawa](https://github.com/CleverCloud/kawa) parser over a pooled
buffer. The per-request state is an `HttpContext`
(`lib/src/protocol/kawa_h1/editor.rs`) inside a `Stream`
(`lib/src/protocol/mux/stream.rs`). The standalone `kawa_h1::Http` session that
used to own this path was removed on 2026-09-20 (sozu#1346).

### 6.1 Read and parse

`Mux::ready` (`lib/src/protocol/mux/mod.rs`) hands a frontend READABLE edge to
`ConnectionH1::readable`, which reads into the stream's front buffer with one
`recv(2)` (a short read ends the read, §2.2) and runs `kawa::h1::parse` with
the `HttpContext` callbacks. kawa (0.7.2 and later) refuses a `Content-Length`
that is not `1*DIGIT` and ends a request that declares no body after its
headers (RFC 9112 §6.3 rule 7); `HttpContext::on_request_headers` rejects
ambiguous `Transfer-Encoding` framing (the CL.TE guard), keeps the two kawa
checks as defense in depth, then edits the request in place
(`kawa_h1/LIFECYCLE.md` §2):

- the forwarding headers (`X-Forwarded-For`, `Forwarded`, `X-Forwarded-Port`,
  `X-Real-IP` with `send_x_real_ip`) come from a `ForwardingHop` rendered once
  per connection and shared through `kawa::Store::Shared`, a reference-count
  bump per header; a client-supplied chain is extended into one exact-size copy
  ([#1643](https://github.com/sozu-proxy/sozu/pull/1643)). The listener's
  `forwarded_headers` mode picks which family is emitted — both by default,
  `X-Forwarded-*` only, `Forwarded` only, or neither
  ([#322](https://github.com/sozu-proxy/sozu/issues/322));
- the request id is rendered once on the stack and copied once into an
  `Rc<str>` that the generated `X-Request-Id`, the access log and both
  `Sozu-Id` headers share ([#1628](https://github.com/sozu-proxy/sozu/pull/1628)).

A parse error answers a default 400 (`set_default_answer`) before routing, so a
malformed request never reaches a backend.

### 6.2 Route and gate

`Router::plan_connect` (`lib/src/protocol/mux/router.rs`) routes the request
through `Router::route_from_request`: the frontend lookup walks the pattern trie
without allocating (`InlineTrieMatches` keeps up to 16 matched segments on the
stack, `lib/src/router/pattern_trie.rs`), the authority is not copied unless a
host rewrite needs it, and the matched cluster id is a `ClusterId = Arc<str>`
shared with the route table
([#1594](https://github.com/sozu-proxy/sozu/pull/1594),
[#1629](https://github.com/sozu-proxy/sozu/pull/1629)). The per-frontend
Basic-auth check (`check_basic`, `lib/src/protocol/mux/auth.rs`) runs here. The
per-(cluster, source-IP) admission gate, `consult_ip_gate`
(`lib/src/protocol/mux/mod.rs`), tracks the session under that same shared id.
Then `Router::decide_after_gate` either reuses a backend connection the session
already holds or asks for a dial.

### 6.3 Connect, or reuse, a backend

A session's backend connections live in its own `Router::backends`
(`BackendConnections`: the first connection inline, the others in a
`BTreeMap`). A keep-alive H1 backend whose previous response completed is
reused with no system call and no liveness probe; a request that finds the
reused connection already closed by the backend is replayed on a fresh one,
from a copy of the bytes it sent (`ReplayOnFreshBackend`, `mux/LIFECYCLE.md`
§8.5). Keeping that copy costs one allocation per request on a reused
connection (the replay capture of `ConnectionH1::writable`).

Otherwise `Mux::dial_backend` lends the router a `BackendSelector` for one call;
`BackendSelector::select` picks a backend through the cluster's
load-balancing policy over a borrowed `Candidates` view (no candidate `Vec`, no
`Rc` clone but the chosen one,
[#1557](https://github.com/sozu-proxy/sozu/pull/1557)) and reserves a
connection on it, and `Mux::dial_backend` then connects it
([#1684](https://github.com/sozu-proxy/sozu/issues/1684)). Under `HRW` or
`MAGLEV` the policy is handed the client affinity key `Router::plan_connect`
derived when it routed the request: the hash of the cluster's `affinity_header`
or `affinity_cookie` value, else of the client source IP, read and hashed in
place in the request buffer with no allocation
([#524](https://github.com/sozu-proxy/sozu/issues/524)). The connect itself is:
`socket(2)`, a non-blocking `connect(2)`, `setsockopt(TCP_NODELAY)` and
`epoll_ctl(EPOLL_CTL_ADD)` on a new slab token. The backend's identity is
interned once per session in the `BackendRegistry`, so a redial or a reuse
stamps the stream with a reference count, not a copy
([#1565](https://github.com/sozu-proxy/sozu/pull/1565),
[#1581](https://github.com/sozu-proxy/sozu/pull/1581)).

### 6.4 Forward the request, read the response

`ConnectionH1::writable` on the backend position gathers the request's kawa
blocks into one `writev(2)` through `h2_transmit::gather` and `confirm`, over a
descriptor vector the connection keeps for its lifetime (`io_slices`,
[#1582](https://github.com/sozu-proxy/sozu/pull/1582)). The response is read the
way the request was, and `HttpContext::on_response_headers` captures the status
and — for a standard phrase — a `'static` reason, with no copy
([#1644](https://github.com/sozu-proxy/sozu/pull/1644)). The response goes to
the client in one `writev(2)` per pass, over the frontend connection's own
`io_slices`. A backend that ends a response early is handled per framing:
close-delimited bodies end cleanly at the EOF, anything else is truncated and
the client connection is closed (`ConnectionH1::terminate_close_delimited`,
`mux/LIFECYCLE.md` §8.4).

### 6.5 Access log

When the response completes, `Stream::generate_access_log`
(`lib/src/protocol/mux/stream.rs`) records the metrics and emits one access-log
record. The line is rendered into the logger's reused buffer with no
allocation (`LogAddress` in `command/src/logging/access_logs.rs`; `write_ulid`,
`EscapedUserAgent` and `write_status` in `command/src/logging/display.rs`;
[#1592](https://github.com/sozu-proxy/sozu/pull/1592)), and leaves in one system
call per record on `tcp://`, `udp://`, `unix://` and `stdout` targets, or batched
in whole records by a `MultiLineWriter` on `file://` (one `write(2)` per 4 KiB,
about 0.1 per request; [#1552](https://github.com/sozu-proxy/sozu/pull/1552)).
Its `client_rtt` and `server_rtt` cells are read with
`getsockopt(TCP_INFO)` once per connection, at that connection's first logged
request, and repeated on every later line of the connection (`memoized_rtt`,
`lib/src/protocol/mux/mod.rs`,
[#1634](https://github.com/sozu-proxy/sozu/pull/1634)).

### 6.6 Keep-alive or close

`ConnectionH1::writable` keeps the client connection only while both
`keep_alive_frontend` and `keep_alive_backend` hold, so a backend's
`Connection: close` also closes the client connection once the response is
flushed ([#1648](https://github.com/sozu-proxy/sozu/pull/1648)). On keep-alive,
`HttpContext::reset` clears the request-scoped fields and installs the next
request's id, minted by `Context::next_request_id` from the pass's wall-clock
snapshot and the session's seeded RNG — no system call, no allocation
([#1635](https://github.com/sozu-proxy/sozu/pull/1635)) — while the session id,
the TLS state, the forwarding hop and the backend connection stay. A request
already pipelined behind the first is parsed, routed and edited on its own.

### 6.7 What a request still costs

Measured on `c086b456` (release, `crypto-ring`, one worker, `file://` access
log, a `python3 -m http.server` backend on loopback, `curl`, 20 requests;
intentrace and an `LD_PRELOAD` descriptor tracer, method in
[`hot_path_zero_copy.md`](./hot_path_zero_copy.md) §2):

| Per request | One request per connection | Keep-alive (20 requests, one connection) |
|---|---|---|
| system calls (intentrace) | 18.45–18.85 | 9.65 plaintext, 10.70 over TLS |
| of which per connection | `accept4` ×2, `socket`, `connect`, `setsockopt` (backend `TCP_NODELAY`), `epoll_ctl` ADD ×2, `close` ×2, `getsockopt(TCP_INFO)` ×1 (the test backend closes before the second is read) | the same, once per connection (0.05 per request each); over TLS also the handshake (§4) |
| of which per request | `recvfrom` ×2, `writev` ×2, `epoll_wait` ×4–5, `write` ×0.1 (log) | `recvfrom` ×3, `writev` ×3, `epoll_wait` ×3, `write` ×0.1 (the third read/write pair is the test backend sending headers and body separately) |
| heap operations (malloc + calloc + realloc + memalign) | 39.95–40.35, 15 793–16 069 bytes | 7.80 plaintext (1 322 bytes), 20.25–20.30 over TLS (3 638–3 640 bytes) |

There is no `shutdown(2)` when the peer has already closed (§11), no
`EPOLL_CTL_DEL`, no `getpeername(2)`, no per-connection `setsockopt` on the
frontend and no read that ends in `EAGAIN`.

## 7. HTTP/2 request lifecycle

An `h2` ALPN makes the mux frontend a `ConnectionH2`, wrapped in an `H2Shell`
that owns the socket (`lib/src/protocol/mux/h2.rs`). The shell holds the only
OS handles; the connection core holds the wire state: HPACK encoder and decoder
(the in-tree sans-io codec, `lib/src/protocol/mux/hpack/`), flow-control
windows, the GOAWAY/drain state and the flood detector. A `Context`
(`lib/src/protocol/mux/mod.rs`) owns the `Vec<Stream>` whose buffers carry each
request and response; the two are linked by `GlobalStreamId = usize`.

### 7.1 Preface and settings

The server preface — SETTINGS, the stream-0 WINDOW_UPDATE that enlarges the
connection window, and the ACK of the client's SETTINGS — leaves in one
`writev(2)` ([#1558](https://github.com/sozu-proxy/sozu/pull/1558)). Every
control frame goes through the connection's ordered output queue, `H2Output`
(`lib/src/protocol/mux/h2_output.rs`), which holds 128 bytes inline and spills
to the heap only under backpressure
([#1625](https://github.com/sozu-proxy/sozu/pull/1625)).

### 7.2 Frames, HPACK and streams

Reads are exact-sized (a 9-byte header, then the payload) and served from
rustls's plaintext before any `recv` (§2.2). HEADERS are decoded by the in-tree
HPACK decoder, which calls back per field with slices borrowed from the block,
a table or one scratch buffer, and decodes Huffman strings through a
compile-time state machine with no allocation
([#1620](https://github.com/sozu-proxy/sozu/pull/1620)); `pkawa` turns the
fields into a kawa request and validates the pseudo-headers. Each stream gets
its own request id from `Context::next_request_id`. Routing, the gate and the
backend connection are the H1 ones (§6.2, §6.3).

### 7.3 Backends and the write pass

The H2 frontend's streams share the session's backend connections: H1
backends carry one request at a time, an H2 backend multiplexes the
concurrent streams of this one frontend connection. A backend connection
belongs to one `Mux` session (`Mux::router`) and is never shared with another
client connection.

`H2Shell::write_streams` prepares each stream's frames, then writes the output
queue followed by the stream's `kawa.out` blocks as `IoSlice`s into one
`writev(2)` (`h2_transmit::gather_after`), over the shell's reused `io_slices`
([#1578](https://github.com/sozu-proxy/sozu/pull/1578)): a response's HEADERS
and DATA leave together. Stream frames stay zero-copy; only the rest of a frame
a partial write cut is copied into the output queue, bounded to one frame or
one header block, so a removed stream never leaves half a frame on the wire.
Flow control debits the windows when DATA is prepared and gives the credit
back when that DATA is dropped unsent (`H2FlowControl::refund_send_window`,
[#1646](https://github.com/sozu-proxy/sozu/pull/1646)); a header block dropped
after encoding resets both HPACK tables (`Encoder::reset_table`,
[#1630](https://github.com/sozu-proxy/sozu/pull/1630)).

### 7.4 Access log and stream recycle

Each finished stream logs one record as in §6.5; the frontend's `client_rtt`
is sampled at the connection's first logged stream and reused by every later
one (`ShellEndpoint::local_rtt`,
[#1598](https://github.com/sozu-proxy/sozu/pull/1598),
[#1634](https://github.com/sozu-proxy/sozu/pull/1634)). A finished slot becomes
`StreamState::Recycle` and is reused by the next stream; its buffers go back to
the pool (`mux/LIFECYCLE.md` §3.3).

### 7.5 Closing an H2 connection

A GOAWAY from the client, an idle timeout or a soft stop ends the connection
with Sōzu's final GOAWAY. On a TLS frontend that GOAWAY and the `close_notify`
behind it leave in one `writev(2)` (`H2Shell::flush_output_to_socket`,
`ConnectionH2::output_flush_closes_connection`, `SocketHandler::socket_write_then_close`,
[#1608](https://github.com/sozu-proxy/sozu/pull/1608)); the rest of the close is
§11.

### 7.6 What a request still costs

Same rig as §6.7, TLS listener with ALPN `h2`:

| Per request | One request per TLS connection | 20 streams on one connection |
|---|---|---|
| system calls (intentrace) | 40.15–40.75 | 13.55 |
| per-connection share | `accept4` ×2, `getrandom` ×15 (`crypto-ring` only), `writev` ×4 (handshake flight, tickets, preface, GOAWAY with `close_notify`), `getsockopt(TCP_INFO)` ×1, `epoll_ctl` ADD and `close` of the frontend socket | spread over the 20 streams (`getrandom` 0.75, `accept4` 0.10 per request) |
| per request | `recvfrom` ×4 (client flights and the backend response), `writev` ×2 (request to the backend, HEADERS+DATA to the client), `epoll_wait` ×5–6, and the backend's `socket`, `connect`, `setsockopt`, `epoll_ctl` ADD and `close` | `recvfrom` ×2.10, `writev` ×2.20, `epoll_wait` ×3.15, and one backend connection per stream, since `python3 -m http.server` answers one request per connection |
| heap operations | 240.60–241.75, 47 559–47 963 bytes | 41.15–42.00, 4 792–4 825 bytes |

## 8. The mux session

### 8.1 What a session allocates once

A mux session is one heap object. Its setup no longer allocates per request on
a one-request connection: the first backend connection is inline in
`BackendConnections` rather than in an 18 KiB `BTreeMap` leaf
([#1612](https://github.com/sozu-proxy/sozu/pull/1612)); the frontend's timer
handle is a named field of `MuxTimeouts` and the backends' live in an
`InlineTokenMap`, as does the reverse index `Context::backend_streams`
(`LinkedStreams` holds the first stream inline,
[#1614](https://github.com/sozu-proxy/sozu/pull/1614)); the 48 KiB debug-event
ring (`DebugHistory`, `lib/src/protocol/mux/debug.rs`) is reserved on the first
push, which only a `debug_assertions` build makes
([#1591](https://github.com/sozu-proxy/sozu/pull/1591)). What remains per
connection is the session object itself, the stream slot, the registry entry
and the write descriptors, each sized for one.

### 8.2 One pass

`Mux::ready_inner` takes one clock snapshot per iteration of its outer loop
(`mux/LIFECYCLE.md` §7.5), services the frontend and every
backend whose readiness has work, sweeps the backends found dead (their tokens
collect in `Router::dead_backends`, reused across passes) and settles the
per-backend accounting through `BackendRegistry::apply_all`, whose ledger keeps
its capacity. The loop repeats while any connection still has interest and an
event, bounded by `MAX_LOOP_ITERATIONS`.

## 9. TCP (pipe) session lifecycle

`TcpProxy` sessions run `TcpSession` (`lib/src/tcp.rs`), a small state machine:
an optional `SniPreread` on an SNI-routed listener
([`tcp_preread/LIFECYCLE.md`](../lib/src/protocol/tcp_preread/LIFECYCLE.md)),
an optional PROXY-protocol stage (expect, send or relay), then a `Pipe`
(`lib/src/protocol/pipe.rs`) that copies bytes both ways without parsing them.
A WebSocket upgrade on an H1 connection ends in the same `Pipe`.

1. **Accept** as in §3; the listener's `TCP_NODELAY` and the accepted address
   apply unchanged.
2. **Connect.** `TcpSession::connect_to_backend` resolves the cluster (the
   SNI-routed one, else the listener's), passes the per-(cluster, source-IP)
   gate, picks a backend through the same `Candidates` view as the mux — under
   `HRW` or `MAGLEV`, keyed on the client's source IP, the PROXY-v2 source when
   the cluster expects one ([#524](https://github.com/sozu-proxy/sozu/issues/524)) —
   and dials it: `socket(2)`, `connect(2)`, `setsockopt(TCP_NODELAY)`,
   `epoll_ctl(EPOLL_CTL_ADD)`. The session keeps the chosen `Backend`
   (`TcpSession::backend`) until `TcpSession::remove_backend` releases it. A
   connect refused after `EINPROGRESS` shows up as a HUP on the connecting
   socket: `TcpSession::fail_backend_connection` bumps `Backend::failures`,
   arms `Backend::retry_policy` (which keeps the backend out of selection for
   its back-off window) and counts `backend.connections.error`, then the
   connect is retried, up to `CONN_RETRIES` times. A connect that completes
   resets the retry policy (`TcpSession::set_back_connected`).
3. **Relay.** On Linux with the `splice` feature, a `Protocol::TCP` pipe
   takes a `SplicePipe` (`lib/src/splice.rs`) when it is created: an idle pair
   from the worker's pool when there is one, at no system call, otherwise a new
   one, two `pipe2(2)` and four `fcntl(2)` (`F_SETPIPE_SZ` and `F_GETPIPE_SZ`
   on each pipe). The payload then moves socket → pipe → socket with `splice(2)` and
   never enters user space; bytes buffered before the pipe existed drain first
   (`tcp_preread/LIFECYCLE.md` §8). Without splice, or on a WebSocket pipe, the
   `Pipe` copies through its two pooled buffers with `recv`/`send`.
4. **Close.** A HUP on either side drains what is in flight
   (`Pipe::frontend_hup`, `Pipe::backend_hup`). The `Pipe` logs the session
   once, from whichever of its handlers ends it (`Pipe::log_request_success`
   or `Pipe::log_request_error`, both through `Pipe::log_request`:
   `getsockopt(TCP_INFO)` on each side). The backend address it logs is the
   one recorded when the backend was chosen, not a `getpeername(2)`:
   `Pipe::new` takes it from the `Backend` a WebSocket upgrade hands over, and
   `TcpSession::connect_to_backend` passes the address it dials through
   `Pipe::set_backend_address`, overwritten when a failed connect is retried
   on another backend ([#1657](https://github.com/sozu-proxy/sozu/pull/1657)).
   For a connected backend it is the address `getpeername(2)` returned; a
   backend still connecting or already reset, where that call failed with
   `ENOTCONN` and the log showed none, now logs the dialed address, as the
   mux does. `TcpSession::close` then shuts both sockets down with
   `Shutdown::Both` — correct on a plaintext relay, which has no TLS send
   buffer to truncate — and closes them, without an `EPOLL_CTL_DEL` (§11).
   Dropping the `SplicePipe` returns it to the worker's pool when the pool has
   room and both of its pipes are empty: its pending counters must read zero,
   then one `ioctl(FIONREAD)` per pipe must confirm it, so a pair still holding
   a byte of this session never serves another one. A pair holding bytes, or
   one beyond the pool's 32 idle pairs, closes its four descriptors.

Measured on the same rig as §6.7, `curl` through a TCP listener to the same
backend, one connection per request. The `c086b456` column is the closing
measurement; the #1657 columns come from its own pair of release binaries
(`f5136d67` and the branch), 20 sessions, two passes per instrument:

| Per connection | `c086b456` | #1657 before (`f5136d67`) | #1657 after |
|---|---|---|---|
| system calls (intentrace) | 36.05–36.15 | 36.10–36.25 | 33.05–33.10 |
| system calls (descriptor tracer) | — | 37.30–38.30 | 33.85–34.35 |
| `splice` | 6.05–6.15 (6.70–6.85 under the descriptor tracer) | 6.10–6.25 | 6.05–6.10 |
| `epoll_wait` | 3.95 | 3.95 | 3.95 |
| `epoll_ctl` | ×4 (2 ADD, 2 DEL) | ×4 (2 ADD, 2 DEL) | ×2 (ADD only) |
| `getpeername` | ×1 | ×1 | 0 |
| other setup and teardown | `accept4` ×2, `socket`, `connect`, `setsockopt`, `pipe2` ×2, `fcntl` ×4, `shutdown` ×2, `getsockopt` ×2, `close` ×6 | same | same |
| heap operations | 14.45, 3 778 bytes | — | — |

Reusing the pipes changes the setup and teardown row. Measured with the same
rig on `34d5e71f` (#1657 merged) and on the change, release, 20 sequential
`curl`, two interleaved passes each (load1 21.7–22.1): 34.90 / 34.10 →
**26.85 / 25.75** system calls per connection; `pipe2` 2.00 → 0.10, `fcntl`
4.00 → 0.20 (the first connection of the worker still creates its pair),
`close` 6.00 → 2.00, `ioctl` 0 → 2.00, the rest unchanged but for the
timing-dependent `splice` and `epoll_wait`. After 1 000 further connections the
worker holds one idle pair, four pipe descriptors.

## 10. UDP flow lifecycle

UDP has no connection and no accept. One `UdpListenerSession` per listener
(`lib/src/udp.rs`) serves every client; the pure core `UdpManager`
(`lib/src/protocol/udp/manager.rs`) keeps a flow table keyed on the client's
source address. The full workflow — admission, selection inside the core,
symmetric NAT return through one connected upstream socket per flow, teardown
on idle or request/response caps — is in
[`udp/LIFECYCLE.md`](../lib/src/protocol/udp/LIFECYCLE.md). Its costs, read from
the code (only the new-flow cost was traced):

- **per client datagram:** one `recvfrom(2)`, one owned copy of the payload
  into the forwarded `Transmit`, one `send(2)` on the flow's connected socket;
  each readiness burst ends with one `recvfrom` that answers `EAGAIN`, since a
  datagram socket offers no short-read proof;
- **per backend reply:** one `recv(2)`, one copy, one `sendto(2)` on the
  listener socket;
- **per new flow:** `udp_connect` (`lib/src/socket.rs`) issues `socket(2)`,
  born non-blocking and close-on-exec (`SOCK_NONBLOCK | SOCK_CLOEXEC`),
  `bind(2)` and `connect(2)`; the shell then registers the socket
  (`epoll_ctl(EPOLL_CTL_ADD)`) and takes the flow's slab slot. That is four
  calls on a Linux release build, traced with `intentrace -p` (nine before:
  two `fcntl(2)` set `O_NONBLOCK`, and an `fcntl`, a `getsockname(2)` and a
  `getpeername(2)` fed post-condition checks that now run only with
  `debug_assertions`). Other platforms keep the `set_nonblocking` pair.
  **Per closed flow:** `EPOLL_CTL_DEL` and `close(2)`.

## 11. Closing a session

A session ends when:

- an H1 response completes and either side closes the connection;
- an H2 connection ends on GOAWAY, after its streams drain;
- a protocol error, a flood violation or a timeout forces the close;
- the zombie checker decides it is wedged.

On a TLS frontend the close path shuts down the **write side only**
(`mux::shutdown_write` in `HttpsSession::close`, `lib/src/https.rs`, mirrored in
`HttpSession::close`, `lib/src/http.rs`). `Shutdown::Both` is forbidden there: it
includes `SHUT_RD`, which discards unread data in the receive buffer (the
client's GOAWAY, ACKs, trailing TLS records), and on Linux the `close()` that
follows then sends a TCP RST instead of a FIN, destroying the TLS records the
drain loop just flushed. The plaintext TCP relay keeps `Shutdown::Both`
(`TcpSession::close`, `TcpSession::close_backend`, §9).

`shutdown_write` (`lib/src/protocol/mux/mod.rs`) also skips the `shutdown(2)`
once the peer has closed its side, on the front socket and on every backend
socket of `Mux::close` and of the dead-backend sweep
([#1603](https://github.com/sozu-proxy/sozu/issues/1603)). The peer has closed
when its connection's readiness carries HUP — `EPOLLRDHUP`/`EPOLLHUP` from the
event loop, or an H1 backend read that met the EOF — or, on a TLS frontend,
when a read met the client's EOF or `close_notify`
(`FrontRustls::peer_disconnected`); a backend Sōzu drops on its own
(`BackendStatus::Disconnecting`) keeps its shutdown. Nothing changes on the
wire: the `close()` that follows is the descriptor's last, and Linux's
`tcp_close` sends the FIN after the queued bytes itself (or a RST when unread
data remains), with or without a prior `shutdown(SHUT_WR)`. HAProxy skips the
socket shutdown on the same condition (`conn_sock_shutw`,
`include/haproxy/connection.h`). The TLS `close_notify` is kept: RFC 8446 §6.1
requires it before the write side closes, whether or not the peer sent its own.

An H1 backend read that meets the EOF records HUP (`ConnectionH1::readable`,
[#1603](https://github.com/sozu-proxy/sozu/issues/1603)), so the dead-backend
check closes the backend without waiting for the `EPOLLRDHUP` edge. Since
[#1606](https://github.com/sozu-proxy/sozu/issues/1606) a short read stops the
read, so the last bytes and the EOF no longer come back from one `recv`: the
EOF is read by a later `readable`, in the same pass when the event already
carried HUP, one `epoll_wait` round later when the FIN landed after that
`epoll_wait` returned (§2.2). A frontend read does not record HUP: over TLS its
`Closed` can be a `close_notify` on a TCP stream that is still open, and a
frontend HUP closes the whole session.

The sockets of an HTTP or HTTPS session are **not** deregistered from epoll
([#1567](https://github.com/sozu-proxy/sozu/issues/1567)). Each one closes when
its owner drops: a dead backend connection at the end of the dead-backend
sweep of `Mux::ready_inner`, and the front socket plus every remaining backend
when the session itself drops, which `shut_down_sessions_by_frontend_tokens`
(`lib/src/server.rs`) does before the loop's next `epoll_wait`. Linux removes a
file from every epoll set on its last close, so an `EPOLL_CTL_DEL` just before
that close costs a system call and removes nothing. The argument needs the
close to be the *last* one: a duplicated descriptor would keep the file, and
its registration, alive under a slab token that may already belong to a new
session. Nothing duplicates a session socket: there is no `dup` or `try_clone`
of one, a worker never forks, and SCM_RIGHTS (`command/src/scm_socket.rs`)
carries only listeners. mio keeps no per-source state on epoll or kqueue that a
deregister would release, and BSD also drops a descriptor's kevents on close.
`closed_sessions_leave_their_sockets_to_close` (`lib/src/http.rs`) and
`close_leaves_backend_sockets_to_their_last_close`
(`lib/src/protocol/mux/mod.rs`) pin it by reading the kernel's epoll table from
`/proc/self/fdinfo`.

The raw TCP proxy closes the same way since
[#1657](https://github.com/sozu-proxy/sozu/pull/1657): `TcpSession::close` and
`TcpSession::close_backend` free the slab slots and shut both sockets down with
`Shutdown::Both` as before, without an `EPOLL_CTL_DEL`. Nothing changes for the
peer: the deregister is local to the epoll instance, and the `shutdown(2)` and
`close(2)` calls, their order and the socket options are the same, so the FIN,
a RST on unread data and the bytes in flight are unchanged. `close_backend` also
runs when a backend connect fails and is retried (`TcpSession::ready_inner`):
`TcpSession::connect_to_backend` either installs the new socket, which drops
the old one in the same pass, or fails, and every failure closes the session
(`handle_connection_result`), so the old socket's last close still comes before
the next `epoll_wait`. `closed_tcp_sessions_leave_their_sockets_to_close`
(`lib/src/tcp.rs`) pins it the same way.

## 12. Soft stop, hard stop and GOAWAY

A soft stop (`sozu shutdown`, a worker upgrade, or `SIGTERM` to the main
process, [#1555](https://github.com/sozu-proxy/sozu/issues/1555)) stops the
listeners from accepting — each proxy's `soft_stop` (`HttpProxy::soft_stop`
and its siblings) deregisters and drops its listener sockets — then calls
`Server::shut_down_sessions` once per event-loop turn: each session's
`shutting_down` says whether it may close now.

- **H2.** The first call sends the advisory `GOAWAY(NO_ERROR, 2^31-1)` at once
  and flushes it (`ConnectionH2::graceful_goaway`), even while a peer header
  block is still being reassembled
  ([#1637](https://github.com/sozu-proxy/sozu/issues/1637)). The session then
  stays open while streams are in flight — including a request whose header
  block is still arriving (`ConnectionH2::peer_header_block_in_progress`) or
  that awaits its backend link (`StreamState::Link`,
  [#1647](https://github.com/sozu-proxy/sozu/issues/1647)); when they have
  drained, the final GOAWAY carries the real `last_stream_id` (RFC 9113 §6.8).
  The `h2_graceful_shutdown_deadline_seconds` budget (5 s by default, `0` for
  no bound of its own) bounds the wait. When it elapses, the session is not
  closed silently: `ConnectionH2::goaway_before_forced_close` queues a final
  `GOAWAY(NO_ERROR)` first, whose `last_stream_id` excludes a stream whose
  opening block never completed, so the client knows it may retry it
  ([#1654](https://github.com/sozu-proxy/sozu/pull/1654);
  `mux/LIFECYCLE.md` invariant 31).
  `Mux::drive_frontend_shutdown_io` forces one write pass per call and stops as
  soon as nothing is queued or the socket blocks, where it used to spin
  `MAX_LOOP_ITERATIONS` empty writes per draining session and per call
  ([#1645](https://github.com/sozu-proxy/sozu/pull/1645)).
- **H1.** There is no GOAWAY: a session whose stream is still linked to a
  backend is kept until its response is done, and an idle keep-alive
  connection, whose stream the keep-alive branch of `ConnectionH1::writable`
  returned to `StreamState::Idle`, closes on the next call
  (`Mux::shutting_down_inner` waits only for `Linked` and non-quiesced
  `Unlinked` streams).
- **TCP.** `TcpSession::shutting_down` answers `true`, as it has since
  `be5dd44b` (2020-01-10): a TCP session is closed on the first
  `shut_down_sessions` pass after the soft stop, whatever it still has in
  flight, with no drain of its own.
- **UDP.** The listener session is not a connection and answers `false`; the
  soft stop is `UdpProxy::notify`'s `SoftStop` arm, and it is active. It puts
  every flow manager in `Drain` (no new flow is admitted), then closes every
  existing flow at once through `UdpListenerSession::close_all_flows`, so
  `udp.active_flows` returns to zero and a reply still in flight may be lost,
  then clears the listener sessions and deregisters the listener sockets.

When the last session is gone the worker answers the main process and leaves
its event loop, flushing the log backends first so a `file://` access log keeps
its buffered records ([#1554](https://github.com/sozu-proxy/sozu/pull/1554)). A
hard stop (`sozu shutdown --hard`, or a second `SIGTERM` during a soft stop)
closes every session at once.

## 13. Hot reconfiguration and upgrades

Everything above describes a worker forwarding live traffic. The control plane
is separate: the main process and the workers talk over unix channels, the main
process validates each request, and changes fan out to the workers. Hot
reconfiguration (add a frontend, remove a backend, swap a certificate) flows
through this channel without touching live sessions. The hot
**upgrade** re-execs the main process with the listener descriptors handed over
across `execve` (`bin/src/upgrade.rs`), so a new binary takes over the same
listening sockets without dropping accepted connections. The master/worker
lifecycle, the `HardStop` / `SoftStop` arms of
`Server::read_channel_messages_and_notify` and the audit log are in
[`bin/src/command/LIFECYCLE.md`](../bin/src/command/LIFECYCLE.md). Data-plane
sessions never emit audit-log lines; to answer "where did this 502 come from",
read the per-cluster metrics and the protocol logs.

## 14. Where to look in the code

| Concern | Files |
|---|---|
| mio loop, slab, accept queue, `max_connections`, soft/hard stop | `lib/src/server.rs` |
| `SO_REUSEPORT`, socket reads (short-read stop, TLS plaintext first), `udp_connect` | `lib/src/socket.rs` |
| `HttpProxy`, H1 listener, upgrade transitions | `lib/src/http.rs` |
| `HttpsProxy`, TLS listener, ALPN dispatch, write-only shutdown | `lib/src/https.rs`, `lib/src/tls.rs` |
| TLS handshake (rustls glue), handshake metrics | `lib/src/protocol/rustls.rs` |
| H1 vocabulary (`HttpContext` editor, answer templates, `Method`) | `lib/src/protocol/kawa_h1/` |
| HTTP/1.1 and HTTP/2 mux (connections, frames, HPACK, output queue, scheduler, flood detector, router, backend dial, keep-alive) | `lib/src/protocol/mux/` |
| TCP sessions and the pipe (splice, WebSocket) | `lib/src/tcp.rs`, `lib/src/protocol/pipe.rs`, `lib/src/splice.rs` |
| TCP SNI/ALPN preread | `lib/src/protocol/tcp_preread/` |
| PROXY-protocol pre-flight (expect / relay / send) | `lib/src/protocol/proxy_protocol/` |
| UDP shell and sans-io core | `lib/src/udp.rs`, `lib/src/protocol/udp/` |
| Routing and load balancing | `lib/src/router/`, `lib/src/load_balancing.rs`, `lib/src/backends.rs` |
| Access-log rendering and log backends | `command/src/logging/` |
| Metrics emission and the local drain | `lib/src/metrics/` |
| Master/worker supervisor, command socket, hot upgrade | `bin/src/command/`, `bin/src/upgrade.rs` |
| Configuration knobs | `command/src/config.rs`, [`configure.md`](./configure.md) |

### 14.1 Where metrics fire along the path

Full taxonomy: [`configure.md`](./configure.md) and `lib/src/metrics/`. The
minimum set to read a session's life from a dashboard:

- `tls.handshake_ms`, `tls.handshake.failed.<reason>` — handshake latency and
  per-rustls-variant failures (`TlsHandshake::record_handshake_duration_ms`,
  `handshake_failure_reason`).
- `https.alpn.rejected.{unsupported,http11_disabled}` — ALPN refusals
  (`HttpsSession::upgrade_handshake`).
- `client.connections`, `client.connections_max`, `client.connections_percent`
  — slab-backed gauges (`SessionManager::incr` / `SessionManager::decr`, and
  the run loop for `_max` and `_percent`).
- `accept_queue.backpressure`, `accept_queue.saturated_seconds` — binary
  backpressure and time-integrated saturation (`SessionManager::check_limits`,
  `SessionManager::decr`, the `ACCEPT_SATURATION_TICK` block of `Server::run`).
- `backend.pool.size` — open backend connections (`+1` in
  `Mux::attach_dialed`; `-1` in `Mux::close` for the backends still open at
  session teardown, in `Connection::pre_close_client_bookkeeping` for a backend
  closed through `Connection::close` — the dead-backend sweep of
  `Mux::ready_inner` included — and in `Mux::attach_dialed`'s rollback when the
  mio registration of a new backend socket fails).
- `requests`, `bytes_in`, `bytes_out`, `backend_response_time`,
  `backend_header_time` — per-cluster and per-backend counters and timings
  (`names::backend`, `lib/src/metrics/names.rs`).
- `h2.flood.violation.<kind>` — flood-detector trips
  (`ConnectionH2::handle_flood_violation`).
- `h2.{goaway,rst_stream}.{sent,received}.<code>` — H2 error attribution (the
  `metric_for_goaway_sent` family, `lib/src/protocol/mux/h2.rs`).
- `epoll_time`, `event_loop_time` — the loop's wall-clock split
  (`names::event_loop`).
