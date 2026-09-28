# Hot-path zero-copy and syscall reduction

This document records the work merged between 2026-09-26 and 2026-09-28 to cut
the system calls and heap allocations Sōzu pays per proxied request: what
changed, how each change works, why it was made, what it measured before and
after, and which invariants now keep the result. The request lifecycles it
refers to are in [`lifetime_of_a_session.md`](./lifetime_of_a_session.md) and
the `LIFECYCLE.md` files it links.

## 1. Goal and rules

The goal: every system call and every heap allocation a worker pays per
request on the hot path (routing, header editing, access log) and on the data
path (reading, writing, relaying bytes) is either needed by that request or
removed. Three constraints came with it:

- **Zero-copy stays the default.** A reduction that adds an allocation per
  request is not a win, whatever it saves in system calls; the trade is judged,
  not assumed. Two allocations per access-log line was named as too costly
  from the start.
- **A gain counts only when it is measured.** Each reduction carries a before
  and an after count per request, on an identical scenario (same
  configuration, backend, client command, request count and protocol), with the
  tool that counted it, and lands in a merged pull request whose tests were
  seen red before the fix.
- **False gains are rejected.** `Instant::now()` and `std::env::var` are not
  system calls (the first goes through the vDSO, the second reads process
  memory), so removing them saves nothing and was never counted.

## 2. Method and overall result

### 2.1 The fixed scenario

- A release build with `--no-default-features --features
  crypto-ring,opentelemetry,splice,simd` (glibc malloc, so an `LD_PRELOAD`
  allocation counter sees every allocation), `worker_count = 1`, a `file://`
  access log, run from a configuration outside the tree.
- A `python3 -m http.server` backend on loopback. It speaks HTTP/1.0 and closes
  after each response; the keep-alive scenarios use a variant with
  `protocol_version = "HTTP/1.1"`.
- `curl` as the client, `N = 20` requests: one per connection, all on one
  keep-alive connection, or 20 H2 streams multiplexed on one connection. H2 runs
  over an HTTPS listener with `command/assets/{certificate,key}.pem`, since the
  cleartext listener does not serve h2c.
- Only the worker is traced — the main process serves no traffic — and only
  the window between the first `accept4` and the last `close` of the load.

### 2.2 Instruments

- **`intentrace -p <worker>`** is the primary system-call counter. Its output
  carries ANSI colour codes that must be stripped, and it is counted on the
  system-call name field only: counting a name as free text also matches it in
  decoded arguments. Under `ptrace` the worker is slow enough that a client's
  next flight often lands during the pass, so the timing-dependent families
  (`epoll_wait`, `recvfrom`, `writev`) come out lower than without a tracer.
- **An `LD_PRELOAD` descriptor tracer** is the cross-check. Its real calls go
  through raw `syscall(2)`, it resolves every descriptor through
  `/proc/self/fd` before a `close`, and it was validated on known cases (three
  writes of known sizes, a `fork`) before it was trusted. It does not interpose
  `getsockopt`, `setsockopt`, `getpeername` or `getrandom`, so its totals are
  lower than intentrace's by those families. Where the two agree, the count is
  evidence; where they disagree, one of them miscounts.
- **An `LD_PRELOAD` allocation counter** (malloc, calloc, realloc, memalign and
  bytes requested) on a non-jemalloc build, read before and after the load.
- **In-process allocation counts** in unit tests: `sozu-lib`'s unit-test binary
  has one counting global allocator, `crate::test_allocations`
  (`lib/src/lib.rs`), thread-local so the other test threads cannot pollute a
  measurement ([`testing.md`](./testing.md) §2, "Allocation budgets").

### 2.3 Where the campaign ends

Measured on 2026-09-28 with the rig above, one binary built at `b5169d44` and
one at `c086b456` (`main` on 2026-09-28), interleaved pass by pass, two
repetitions each. `b5169d44` is `main` on 2026-09-26 at 18:17 UTC, the first
release-build baseline of the work: #1552, #1553, #1557, #1558, #1561, #1565,
#1568, #1578 and #1581 had already merged, so their gains are not in this
table and are given in their own sections below. Figures are per request;
ranges are the two repetitions. The one-minute load average, read before and
after each intentrace pass, was 10.7–24.9 during the first repetition of the
scenarios without keep-alive and 2.7–8.5 during the second (2.5–24.9 over all
their passes, descriptor-tracer and allocation passes included), and 1.5–2.9
for every keep-alive pass; system-call counts other than the three
timing-dependent families do not move with it.

| Scenario | System calls, intentrace | System calls, descriptor tracer | Heap operations | Bytes requested |
|---|---|---|---|---|
| H1, one request per connection | 26.25–26.70 → **18.45–18.85** | 22.25–24.85 → 18.65–19.80 | 83.65–83.85 → **39.95–40.35** | 84 327–84 445 → **15 793–16 069** |
| H1 keep-alive, 20 requests on one connection | 14.75 → **9.65** | 12.60–12.80 → 9.65 | 50.80 → **7.80** | 8 362 → **1 322** |
| H1 keep-alive over TLS | 15.75 → **10.70** | 13.05–13.30 → 10.05–10.25 | 63.30 → **20.25–20.30** | 10 644–10 680 → **3 638–3 640** |
| H2 over TLS, one request per connection | 61.25–61.60 → **40.15–40.75** | 47.30–47.70 → 30.80–32.10 | 412.15–413.90 → **240.60–241.75** | 137 094–137 857 → **47 559–47 963** |
| H2 over TLS, 20 streams on one connection | 25.05 → **13.55** | 21.60 → 14.75–16.30 | 83.40–83.90 → **41.15–42.00** | 11 021–11 029 → **4 792–4 825** |
| TCP relay (splice), one connection per request | 39.00–39.10 → **36.05–36.15** | 32.85–33.20 → 32.80–32.95 | 31.45 → **14.45** | 4 068 → **3 778** |

"Heap operations" counts malloc + calloc + realloc + memalign. The TLS rows
include 15 `getrandom(2)` per handshake on both sides (0.75 per request when 20
requests share the connection): that is the `crypto-ring` provider, the one
the default build uses; `crypto-aws-lc-rs` issues none (§3.13). The TCP row moved only through code it
shares with the HTTP path — the accept path of §3.10 and the access-log
renderer of §3.1 among it; its allocations were not attributed further.

Two earlier figures frame this table. The first map of the campaign, taken on
2026-09-26 with the `LD_PRELOAD` interposer alone on a debug build, with a
`stdout` access log, counted about 25.1 system calls per H1 request and 34.8
per H2 request; it did not interpose `fcntl` or `recv`, so it undercounts
(the campaign ledger, "The map"). Separately, the `tcp://` access-log pass of
[#1552](https://github.com/sozu-proxy/sozu/pull/1552), measured on its base
`ca058989`, found the worker at 106.25 system calls per H1 request, 81.70 of
them `send(2)` of the access log; §3.1 is that fix.

What the remaining calls are, per protocol, is in
[`lifetime_of_a_session.md`](./lifetime_of_a_session.md) §6.7 (H1), §7.6 (H2),
§9 (TCP) and §10 (UDP).

## 3. What changed

Each subsection says how the change works, why it was made, what it measured,
and which pull requests carry it. Pull-request numbers link the full
measurement.

### 3.1 Access logs: one system call per record, no allocation per line

**One `send(2)` per record on `tcp://`
([#1552](https://github.com/sozu-proxy/sozu/pull/1552)).** `LoggerBackend::Tcp`
was the only log sink without a buffer of its own: `write_fmt` drove the
formatter piece by piece and every piece reached the socket as its own
`write_all`, so one access-log record was a burst of small `send(2)` — the
timestamp alone arrived as sixteen of them. The record is now rendered into the
reused `LoggerBuffer` the `unix://` and `udp://` sinks already used and written
once with `write_all` (a stream socket may accept fewer bytes than offered).
Measured on H1, N = 20: **81.70 → 1.00 `send` per record** (H2: 81.95 → 1.00),
and the worker's whole budget went from **106.25 to 25.35 system calls per
request**. The buffer is reused, so the change costs no allocation.

**The other targets were measured and left as they are.** `file://` writes
through a `MultiLineWriter` that flushes whole records when its 4 KiB buffer
fills, measured at 0.10 `write(2)` per request — ten times better than one per
line; routing it through the per-record buffer would have been a tenfold
regression. `unix://` costs one call per record, which a datagram sink needs
for framing, and `stdout` one per record too, more for a long line (a
2 764-byte line left in three writes).

**No allocation per line
([#1592](https://github.com/sozu-proxy/sozu/pull/1592)).** Rendering one ASCII
access-log line allocated ten times although every field of the record is a
borrow. Each allocating field now writes straight into the formatter: the socket
addresses through `LogAddress` (`command/src/logging/access_logs.rs`), and, in
`command/src/logging/display.rs`, the session and request ULIDs through
`write_ulid` (a Crockford encoder on a 26-byte stack array), the user agent
through `EscapedUserAgent` (which writes the spans between the escaped bytes),
the status through `write_status`. The output is byte-for-byte unchanged.
Measured: **10.00 → 0 allocations per line** on `file://` and `udp://`, and
about 10 fewer heap operations per H1 request end to end (83.80 / 83.95 → 73.70 / 73.80, two
runs each).

**Records survive a stop ([#1554](https://github.com/sozu-proxy/sozu/pull/1554),
[#1559](https://github.com/sozu-proxy/sozu/pull/1559)).** Batching is only safe if
the batch is flushed. A stopping worker was killed with `SIGKILL` as soon as its
channel closed, before the `MultiLineWriter` could flush: 6 of 27 records were
lost on `sozu shutdown`, on `--hard` and on a worker upgrade (five runs
each), and 6 to 8 under `SIGTERM`, which no process handled (five runs per
signal order, one run for a worker signalled alone). The worker now flushes
both buffering backends after its event loop ends and before its channel
closes; `SIGTERM` to the main process now soft-stops the workers, and a second
one hard-stops them. Measured: 0/27 lost on every path and every run.

### 3.2 Request id: rendered once, fresh per request

**One rendering shared by four headers
([#1628](https://github.com/sozu-proxy/sozu/pull/1628)).** The generated
`X-Request-Id`, its copy for the access log and the `Sozu-Id` on the request
and on the response were four renderings of the same ULID, each through the
allocating `Display` of `rusty_ulid`. The id is now rendered on the stack and
copied once into an `Rc<str>` that all four share through
`kawa::Store::Shared`; the default `Sozu-Id` name is a `'static` literal.
`HttpContext::x_request_id` became `Option<Rc<str>>`, which needs kawa's
`rc-alloc` feature (in its default set; no new crate). Together with a single
exact-size copy for each synthesised forwarding header, H1 keep-alive went from
**24.60 to 15.55 heap operations per request** (TLS: 40.65 → 31.65), system
calls unchanged.

**A fresh id per keep-alive request
([#1635](https://github.com/sozu-proxy/sozu/pull/1635)).** The measurement of
#1628 found every request of an H1 keep-alive connection carrying the first
request's ULID — one id across 20 requests in the access log — although
`doc/configure.md` and `kawa_h1/LIFECYCLE.md` promised a per-request id. The
operator chose to fix the code rather than the documentation.
`HttpContext::reset` now takes the next id as an argument, minted by
`Context::next_request_id` from the pass's wall-clock snapshot and the session's
seeded RNG, the source that already numbered H2 streams: no `getrandom`, no
`clock_gettime`, no allocation. Measured: distinct `Sozu-Id` per connection
1/20 → 20/20, identical system-call set.

### 3.3 `TCP_INFO`: read only for a log line, and once per connection

The access log's `client_rtt` and `server_rtt` are the only consumers of
`getsockopt(TCP_INFO)`. It used to be read far more often.

- **Only when a log line needs it
  ([#1598](https://github.com/sozu-proxy/sozu/pull/1598)).** The H2 frontend was
  sampled at the top of every mux pass, logging or not. The core now asks its
  endpoint for the frontend RTT (`Endpoint::local_rtt`) only when it logs a
  stream, and the H2 shell answers it from its own socket. Measured with the
  interposer: H2 **7.85 → 1.85** reads per request, H1 2.80 → 1.85, multiplexed
  H2 5.60 → 1.80.
- **Once per connection ([#1634](https://github.com/sozu-proxy/sozu/pull/1634)).**
  The operator accepted sampling the RTT once per connection and side.
  `memoized_rtt` (`lib/src/protocol/mux/mod.rs`) keeps the connection's first
  sample, taken at its first logged request, and every later line repeats it; a
  backend connection keeps its sample across keep-alive reuse. The value no
  longer follows the kernel's smoothed RTT during the connection. Measured on H1
  keep-alive, two runs each: **11.55 / 11.55 → 9.65 / 9.65 system calls per
  request** (TLS 11.85 / 11.85 → 9.95 / 9.95), `getsockopt` 2.00 → 0.10;
  multiplexed H2 12.35 / 12.20 → 11.35 / 11.30.

### 3.4 `ClusterId` is an `Arc<str>`

`sozu_command_lib::state::ClusterId` was `String`, so every routed request
cloned its cluster id out of the route table, and the per-(cluster, source-IP)
gate — which every production request crosses, since it always has a source
address — cloned it three more times
([#1629](https://github.com/sozu-proxy/sozu/pull/1629)). `ClusterId` is now
`Arc<str>`: the route table, `HttpContext::cluster_id`, the gate's
`SessionManager` maps, the dial plan and the backend connection share the one
allocation made when the frontend was added. Measured with
`crate::test_allocations`, per routed request on a reused H1 backend: **4 → 0**
through the gate, 1 → 0 without it (H2 backend: 4.28 → 0.28, the rest being the
backend's stream table).

`Arc` rather than `Rc`, by decision: a worker is single-threaded, but
`ConfigState` and the response types are public API that embedders keep behind
`tokio` locks across threads (proxy-manager does), and an `Rc<str>` revision of
the change broke that build. A compile-time assertion in `command/src/state.rs`
now holds `ClusterId`, `ConfigState`, `StateError` and the response frontends and
backends to `Send + Sync`. An `Arc` clone is one uncontended atomic increment.
The protobuf messages keep `String`; the id is converted once when a request
enters `ConfigState` or a proxy.

### 3.5 Routing and backend selection

- **Selection borrows its candidates
  ([#1557](https://github.com/sozu-proxy/sozu/pull/1557)).** Every selection
  collected its candidates into a fresh `Vec` of cloned `Rc<RefCell<Backend>>`,
  once per tier it tried, and `Random` added two allocations of its own.
  `BackendList` now keeps a reusable `Vec<usize>` of candidate positions, and
  `LoadBalancingAlgorithm::next_available_backend` takes a borrowed `Candidates`
  view (a public trait change). Measured: **2 → 0 allocations per selection**
  (`Random`: 4 → 0), one `Rc` increment instead of one per candidate; pick
  sequences pinned identical under all six policies.
- **The backend identity is interned once per session.**
  `BackendRegistry::id_for` copied the backend id into a fresh `Rc<str>` on every
  dial ([#1565](https://github.com/sozu-proxy/sozu/pull/1565): 1 → 0 per
  redial); the mux then turned it back into a `String` twice per request on a
  reused connection and three times per dial
  ([#1581](https://github.com/sozu-proxy/sozu/pull/1581):
  `HttpContext::backend_id` and `SessionMetrics::backend_id` became
  `Option<Rc<str>>`, 2 → 0 per request).
- **A reused backend connection costs no allocation per request
  ([#1584](https://github.com/sozu-proxy/sozu/pull/1584)).** The reverse index
  kept its emptied `Vec` instead of rebuilding it, the delta ledger drains
  instead of being replaced (`BackendRegistry::apply_all`), and the routed
  cluster id moves into the stream's context instead of being copied: 3 → 0 per
  request and 2 → 1 per dial.
- **The router allocates nothing per request past its result
  ([#1594](https://github.com/sozu-proxy/sozu/pull/1594)).** The authority is
  copied only when a host rewrite needs it for `X-Forwarded-Host`, and the trie
  records matched segments in `InlineTrieMatches`, a 16-slot stack array that
  spills only past 16 segments: 3 → 1 per request, the last one being the
  cluster id that §3.4 then removed.
- **The local metrics drain looks up without copying
  ([#1561](https://github.com/sozu-proxy/sozu/pull/1561)).** `LocalDrain` built
  an owned key (`entry(cluster_id.to_owned())`) on every cluster-labelled metric
  and scanned a per-cluster `Vec` of backends. It now tries `get_mut(&str)`
  first and keys backends in a `BTreeMap`: **9 → 0 allocations per request**,
  and the benchmark's end-of-session emission at 1 000 backends went from
  14.7 µs to 753 ns.

### 3.6 The H1 header editor

`HttpContext::on_request_headers` and `on_response_headers`
(`lib/src/protocol/kawa_h1/editor.rs`) edit every request and response, H1 and
H2 alike.

- **One scratch, exact-size copies
  ([#1628](https://github.com/sozu-proxy/sozu/pull/1628)).** Each synthesised
  forwarding header used to take the scratch `Vec` itself, which cost a shrink
  and a regrowth per header; each now takes an exact-size copy of one reused
  scratch (a bare request 14 → 10 heap operations, then 7 with §3.2).
- **Forwarding values rendered once per connection
  ([#1643](https://github.com/sozu-proxy/sozu/pull/1643)).** `X-Forwarded-For`,
  `Forwarded`, `X-Real-IP` and `X-Forwarded-Port` depend only on the protocol,
  the public address (whose `port` is the `X-Forwarded-Port` value) and the
  peer address, all connection-scoped. A
  `ForwardingHop` on `HttpContext` is rendered by the connection's first request
  and shared by the next ones through `kawa::Store::Shared`; it is keyed on its
  three inputs and rendered again when one changes. Each rendering starts with
  `", "`, so the same bytes serve an appended hop and, from offset 2, a
  synthesised value. A client-supplied chain is request-scoped and never cached:
  the hop is appended to it in one exact-size copy. Measured on H1 keep-alive
  (release, `crypto-aws-lc-rs`, glibc): **11.45 → 7.65 heap operations per
  request**, TLS 27.55 → 23.75.
- **A standard reason phrase is not copied
  ([#1644](https://github.com/sozu-proxy/sozu/pull/1644)).**
  `HttpContext::reason` is a `Cow<'static, str>`: a backend that sends exactly
  the phrase RFC 9110 §15 registers for its status (plus 429) is captured as
  that `'static` string (`standard_reason`); any other phrase is copied
  verbatim, never normalised. Measured on the same rig: 11.45 → 10.45 heap
  operations per request.

### 3.7 The write path: descriptor vectors that live with the connection

`writev(2)` needs an `IoSlice` per kawa block, and an H1 header block is dozens
of blocks (45 descriptors in a `curl` request, 32 in the test backend's
response). Both write paths built that vector per pass.

- **H2 ([#1578](https://github.com/sozu-proxy/sozu/pull/1578))** and **H1
  ([#1582](https://github.com/sozu-proxy/sozu/pull/1582))** keep it in an
  `io_slices` field for the connection's lifetime, filled and emptied by the
  shared `h2_transmit::gather` / `confirm` pair: only the capacity survives a
  pass, never a descriptor. Measured with `crate::test_allocations`: a warm H2
  pass 1 → 0, a warm H1 pass 3 → 0; the `writev` count is unchanged.
- **A cold pass sizes the vector once
  ([#1612](https://github.com/sozu-proxy/sozu/pull/1612)):** `gather` reserves
  `kawa.out.len()` descriptors instead of doubling from 4 to 64.

### 3.8 H2: preface, output queue, HPACK

- **The server preface in one `writev`
  ([#1558](https://github.com/sozu-proxy/sozu/pull/1558)).** The stream-0
  WINDOW_UPDATE was queued after the first flush of SETTINGS and ACK, costing a
  second TLS record and a second `writev` on every frontend connection. It is
  now serialised between the two: **7.00 → 6.00 `writev` per connection**.
- **The final GOAWAY and `close_notify` in one write
  ([#1608](https://github.com/sozu-proxy/sozu/pull/1608)).**
  `SocketHandler::socket_write_then_close` lets rustls encrypt the GOAWAY, queue
  the alert behind it and flush both with one `write_tls`: close writes 35–40 →
  20 per 20 connections under the descriptor tracer. The `close_notify` stays
  (RFC 8446 §6.1).
- **An in-tree, sans-io HPACK codec
  ([#1620](https://github.com/sozu-proxy/sozu/pull/1620)).** `loona-hpack`
  rebuilt a `HashMap`-based Huffman decoder for every string, returned owned
  vectors and stored each dynamic-table entry in two allocations.
  `lib/src/protocol/mux/hpack/` is written from RFC 7541: the decoder calls back
  per field with borrowed slices, Huffman decoding runs through a 256-state
  nibble machine a `const fn` derives from Appendix B, and the dynamic table is
  one byte buffer plus a ring of offsets. The wire is unchanged (a differential
  run against `loona-hpack` found no difference over 3 000 encoder sequences and
  20 000 blocks). Measured: **389.4 → 257.2 allocations per H2 request (−34 %)**
  and 69 186 → 47 112 bytes; a Huffman decode and a steady-state block allocate
  nothing.
- **One ordered output queue
  ([#1625](https://github.com/sozu-proxy/sozu/pull/1625)).** `zero` used to be
  both the frame-header input buffer and the control-frame output buffer, with
  stream frames written from each stream's buffer and serialised against it
  only by `expect_write`. `H2Output` (`lib/src/protocol/mux/h2_output.rs`) is now
  the one ordered queue for control frames and for the unsent rest of the one
  stream frame a partial write cut; stream frames stay zero-copy, gathered after
  the queue in the same `writev`. The queue holds 128 bytes inline (the whole
  server preface is 79), so a connection without backpressure allocates nothing
  for it. This was a correctness change (§4.2); it measured no system call added
  (`writev` 120 → 120 per 20 connections under intentrace) and allocations
  within noise.

### 3.9 The read path: no `recv` that can only answer `EAGAIN`

- **TLS plaintext before `read_tls`
  ([#1596](https://github.com/sozu-proxy/sozu/pull/1596)).** The H2 read path
  asks for a 9-byte header, then a payload, and one TLS record usually carries
  several frames; `FrontRustls::socket_read` nevertheless called `read_tls`
  first on every call. `rustls_socket_read` now drains the plaintext rustls
  holds and reads the socket only when the caller's buffer is still short.
  Measured: **H2 62.05 → 51.90–52.05 system calls per request**, `recvfrom`
  16 → 6;
  multiplexed 25.05 → 19.80. A fatal TLS error is now sticky, so plaintext
  decrypted before a corrupt record is never served afterwards.
- **Stop on a short read
  ([#1606](https://github.com/sozu-proxy/sozu/pull/1606)).** A `recv` that
  returns fewer bytes than offered proves the stream socket's receive queue is
  empty, so every read path now stops there instead of reading on to an
  `EAGAIN` (HAProxy `raw_sock.c` and tokio do the same). A FIN already folded
  into HUP keeps READABLE so the EOF is still read. Measured: H1 `recv` 4.45 →
  2.30 per request and 31 → 0 `EAGAIN` per 20 requests; H2 total 35.25 → 34.35
  under the descriptor tracer. TCP urgent data is the one known hole
  ([`lifetime_of_a_session.md`](./lifetime_of_a_session.md) §2.2).
- **No `EAGAIN` left on TLS
  ([#1611](https://github.com/sozu-proxy/sozu/pull/1611)).** The handshake
  stops on a short read (`handshake_read` wraps the socket in the same
  `ShortReadProbe`), the upgrade arms READABLE only when a read is due
  (`upgraded_frontend_events`), and `RecvMemory` carries the proof of an empty
  queue from one read call to the next until an event is delivered. Measured:
  `EAGAIN` per 20 H2 connections **56–60 → 0**, multiplexed 21–22 → 0.

### 3.10 Accept and close

- **The accept path keeps what `accept(2)` returned
  ([#1593](https://github.com/sozu-proxy/sozu/pull/1593)).** The listeners
  dropped the peer address `accept4` returned and read it back twice with
  `getpeername(2)`; `TCP_NODELAY` was set on every accepted socket. The address
  now rides the accept queue into `create_session`, and the flag is set once on
  the listener (with a per-connection fallback for a backlog that predates the
  flag). Measured per connection: `getpeername` 2 → 0, `setsockopt` 2 → 1 (the
  remaining one is the backend socket's); H1 26.95 → 23.75 system calls.
- **No `EPOLL_CTL_DEL` before `close`
  ([#1568](https://github.com/sozu-proxy/sozu/pull/1568)).** Linux removes a file
  from every epoll set on its last close, and nothing duplicates a session
  socket: `epoll_ctl` **4.00 → 2.00** per request, H1 and H2; descriptors stable
  over 1 000 requests. `L7Proxy::deregister_socket`, left without a caller, was
  then removed ([#1618](https://github.com/sozu-proxy/sozu/pull/1618)).
- **No `shutdown(2)` on a closed peer, no `epoll_wait` for a known EOF
  ([#1605](https://github.com/sozu-proxy/sozu/pull/1605)).** `shutdown_write`
  skips the call once the peer's HUP, EOF or `close_notify` has been seen (the
  following `close` sends the same FIN), and an H1 backend read that met the
  EOF with the last bytes recorded HUP at once, which saved an `epoll_wait`
  round. #1606 (§3.9) later made a short read stop the read, so the EOF now
  comes from a later read, which still records HUP. Measured on #1605:
  `shutdown` **2.00 → 0.00** per request under intentrace, H1 and H2; under the
  descriptor tracer the H2 `ENOTCONN` failures went from 15 to 1 per 20
  requests. The TLS `close_notify` is kept.
- **The event loop no longer clones the registry
  ([#1553](https://github.com/sozu-proxy/sozu/pull/1553)).**
  `UdpProxy::health_poll`, called once per loop turn whether or not a UDP
  cluster exists, cloned the mio `Registry` — an `fcntl(F_DUPFD_CLOEXEC)` and a
  `close` of the epoll descriptor per turn. It now borrows it: `close` **7.00 →
  2.00** and `fcntl` 5.00 → 0 per H1 request (H2: 8.10 → 2.00 and 6.10 → 0). It
  also stopped skipping UDP health probing when the worker was out of
  descriptors.

### 3.11 Session setup

A one-request connection pays the session's setup on every request, so its
allocations count as per-request ones.

- **The debug history is empty in release
  ([#1591](https://github.com/sozu-proxy/sozu/pull/1591)).** `DebugHistory`
  reserved its 512-event ring (48 KiB) in every session although only a
  `debug_assertions` build records into it: **about 49 200 bytes and one
  allocation less per connection**, 58 % of the bytes an H1 request
  allocated.
- **The first backend connection is inline
  ([#1612](https://github.com/sozu-proxy/sozu/pull/1612)).** Inserting the first
  backend into `Router::backends` allocated a whole `BTreeMap` leaf, eleven
  1.7 KiB `Connection` slots, for one backend. `BackendConnections` keeps the
  lowest-token connection inline and spills the rest: H1 bytes per request
  **35 108–35 415 → 16 218–16 297**, and the session object grows once, by
  1 728 bytes.
- **Timer handles and the reverse index inline
  ([#1614](https://github.com/sozu-proxy/sozu/pull/1614)).** `MuxTimeouts` keeps
  the frontend's timer handle in a named field and the backends' in an
  `InlineTokenMap` (first entry inline), and `Context::backend_streams` uses the
  same map with `LinkedStreams`, which holds the first linked stream inline:
  **−3 allocations per request** on a one-request connection.

### 3.12 Soft stop without a busy loop

During a soft stop, `Mux::drive_frontend_shutdown_io` forced one write pass on
every draining H2 session, then looped until the WRITABLE event bit cleared —
which the forced pass had set itself and which only a `WouldBlock` cleared. An
idle draining session with a writable socket therefore ran all
`MAX_LOOP_ITERATIONS` (10 000) empty passes per call, once per loop turn, for
every draining session ([#1645](https://github.com/sozu-proxy/sozu/pull/1645)).
The loop now stops once nothing is queued or the socket blocks. Measured, one
`shut_down_sessions` call on a release build: **3.94 ms → 7 µs**. The same
change turned a truncated 1 MiB transfer during a drain into a complete one and
removed most of the e2e flakes under CPU contention
([#1623](https://github.com/sozu-proxy/sozu/issues/1623)).

### 3.13 `getrandom(2)` depends on the crypto provider

The H2 maps showed 15 `getrandom(2)` per TLS handshake. rustls draws its
randomness through the provider: `crypto-ring` maps every draw to a raw
`getrandom` call, while `crypto-aws-lc-rs` keeps a thread-local CTR-DRBG
reseeded every 4 096 draws. Measured on `a0785505`, one H2 request per TLS
connection: aws-lc-rs **0 `getrandom`, 25.9 system calls per connection**; ring
15 and 40.9. `crypto-ring` is the default provider (`bin/Cargo.toml`,
`lib/Cargo.toml`, the Dockerfile, the RPM and Arch packages; the release
workflow builds `crypto-ring` artefacts beside `crypto-aws-lc-rs` ones), so
default builds pay the 15 calls per TLS handshake; a
build with `crypto-aws-lc-rs` avoids them. Changing the default provider is a
decision outside this work and was not taken. The campaign's own measurements
keep `crypto-ring` so that they compare with each other.

### 3.14 Measured and left as they are

- **`file://`, `stdout` and `unix://` access logs** (§3.1).
- **TLS 1.3 session tickets:** one `writev` per connection with the default
  `send_tls13_tickets = 4`; `0` removes it. A configuration trade, not a defect.
- **The HTTP/1.1 replay capture** of a pooled upstream connection reserves one
  buffer per request (`ConnectionH1::writable`), and rustls allocates about five
  buffers per TLS request for its own records; both were measured and left
  ([#1628](https://github.com/sozu-proxy/sozu/pull/1628)).
- **The TCP relay** (`lib/src/tcp.rs`) still deregisters and shuts down both
  sockets before closing them, reads the backend address back with
  `getpeername(2)` for its log line, and allocates its splice pipes per session
  ([`lifetime_of_a_session.md`](./lifetime_of_a_session.md) §9). No change
  targeted the TCP data path; its figures in §2.3 moved only through code it
  shares with the HTTP path.
- **UDP** was not traced beyond the event-loop fix of §3.10. Its per-datagram
  costs are read from the code in
  [`udp/LIFECYCLE.md`](../lib/src/protocol/udp/LIFECYCLE.md) §13.

## 4. Correctness and safety fixes found along the way

Measuring the hot path meant reading it closely, and adversarial reviews of the
performance changes found defects that predated them. Each fix below carries its
own red test; none of them is a performance change.

### 4.1 HTTP/1.1 request smuggling (CWE-444)

- **A request without `Content-Length` or `Transfer-Encoding` swallowed the
  requests pipelined behind it** ([#1650](https://github.com/sozu-proxy/sozu/issues/1650),
  [#1651](https://github.com/sozu-proxy/sozu/pull/1651)). kawa 0.7.1 parsed such
  a request as close-delimited, like a response (CleverCloud/kawa#23), so a
  second request written in the same segment behind a plain `GET` was forwarded
  raw to the first request's backend: never routed, never checked against the
  frontend's Basic auth, without `Sozu-Id` or `X-Forwarded-*`. RFC 9112 §6.3
  rule 7 gives such a request no body; `HttpContext::on_request_headers` ended
  it after its headers. All 24 e2e cases were red before the fix.
- **A `Content-Length` with a leading `+` was accepted and forwarded verbatim**
  ([#1652](https://github.com/sozu-proxy/sozu/issues/1652),
  [#1653](https://github.com/sozu-proxy/sozu/pull/1653)). kawa 0.7.1 read the
  value with `usize::from_str` (CleverCloud/kawa#25). A backend that refuses or
  re-reads `+5` takes the body for the next request. A request whose
  `Content-Length` is not `1*DIGIT` (RFC 9110 §8.6) has been answered 400 since,
  and such a response 502.

Both guards sit in Sōzu's header editor, and both were live when they merged.
kawa 0.7.2, published on 2026-09-28 and adopted by
[#1655](https://github.com/sozu-proxy/sozu/pull/1655), fixed the two parser
bugs upstream: it refuses a `Content-Length` that is not `1*DIGIT` before the
header callback (CleverCloud/kawa#26), and ends a request with neither
`Content-Length` nor `Transfer-Encoding` after its headers (CleverCloud/kawa#27).
The guards were kept unchanged as defense in depth against a kawa regression:
under 0.7.2 neither fires on traffic kawa accepts, so their counters
`http.frontend.content_length_invalid` and `http.backend.content_length_invalid`
stay at zero unless kawa regresses, and a non-`1*DIGIT` value is counted in
`http.frontend_parse_errors` / `http.backend_parse_errors` instead. The
answers are unchanged (400 on a request, 502 on a response).
[`kawa_h1/LIFECYCLE.md`](../lib/src/protocol/kawa_h1/LIFECYCLE.md) §2.1–§2.2
describe both layers and how to reproduce the regression each guard covers.

### 4.2 HTTP/2 framing and HPACK

- **Half-written frames.** A stream reaped in the middle of a DATA payload
  stopped the connection from reading
  ([#1597](https://github.com/sozu-proxy/sozu/issues/1597),
  [#1599](https://github.com/sozu-proxy/sozu/pull/1599)); a PING or SETTINGS
  ACK could be written inside a half-written stream frame
  ([#1600](https://github.com/sozu-proxy/sozu/issues/1600),
  [#1601](https://github.com/sozu-proxy/sozu/pull/1601)); and removing a stream
  parked mid-frame left a truncated frame on the wire
  ([#1604](https://github.com/sozu-proxy/sozu/issues/1604)). The last one needed
  the ordered output queue of §3.8
  ([#1625](https://github.com/sozu-proxy/sozu/pull/1625)): every committed frame
  now leaves whole.
- **HPACK table size updates.** Two `SETTINGS_HEADER_TABLE_SIZE` changes between
  two header blocks signalled only the last size, where RFC 7541 §4.2 requires
  the smallest, then the last
  ([#1622](https://github.com/sozu-proxy/sozu/issues/1622),
  [#1626](https://github.com/sozu-proxy/sozu/pull/1626)).
- **An HPACK block encoded and dropped unsent**
  ([#1627](https://github.com/sozu-proxy/sozu/issues/1627),
  [#1630](https://github.com/sozu-proxy/sozu/pull/1630)). The encoder changes its
  dynamic table when it encodes a block, not when the block is sent. A block
  dropped unsent (its stream removed while parked, an oversized block, H1
  trailers split across reads) shifted every older entry in Sōzu's table only,
  so a later block could make the peer decode a different field with no error.
  The next block now opens with the size updates `0`, then the maximum, which
  empty both tables (`Encoder::reset_table`).
- **A request encoded for one H2 backend connection was re-linked to another**
  ([#1632](https://github.com/sozu-proxy/sozu/issues/1632),
  [#1639](https://github.com/sozu-proxy/sozu/pull/1639)) with frames carrying
  the first connection's stream id and encoded against its table. It is now
  answered 502 (lost connection) or 503 (refused by GOAWAY) instead.
- **A backend stream whose HEADERS had not left was reset**
  ([#1631](https://github.com/sozu-proxy/sozu/issues/1631),
  [#1638](https://github.com/sozu-proxy/sozu/pull/1638)): a RST_STREAM on an idle
  stream is a connection error, and one could even precede the connection
  preface. Such a stream is now retired with no frame.

**Scope.** The dynamic-table desync, the re-link and the idle-stream reset all
act on one H2 **backend** connection. A backend connection belongs to one `Mux`
session — `Router::backends` is a field of `Mux::router` — and is never shared
with another client connection: it carries the concurrent streams of one
frontend connection only. The damage therefore stays within one frontend
connection: a request of that connection could be rebuilt with fields another
request of the same connection had inserted, or all of its streams could be
lost. It reaches a different user only behind an intermediary that multiplexes
several users' requests on one frontend connection to Sōzu. Some pull-request
descriptions and issues of this work called it "cross-client"; that overstated
it, and the CHANGELOG entries now say it as above.

### 4.3 HTTP/2 flow control and draining

- **DATA prepared and dropped unsent kept its send credit**
  ([#1641](https://github.com/sozu-proxy/sozu/issues/1641),
  [#1646](https://github.com/sozu-proxy/sozu/pull/1646)). The window is debited
  when DATA is prepared; a parked stream removed before the socket took it never
  gave the credit back, and the peer never returns credit for DATA it did not
  receive, so each drop shrank the connection window for the rest of its life.
  `H2FlowControl::refund_send_window` gives it back; a refund the peer pushed
  past 2^31-1 is a FLOW_CONTROL_ERROR, never a panic.
- **A soft stop mid header block closed without any GOAWAY**
  ([#1637](https://github.com/sozu-proxy/sozu/issues/1637),
  [#1649](https://github.com/sozu-proxy/sozu/pull/1649)): the advisory GOAWAY was
  deferred while a peer header block was being reassembled, a deferral that
  protected no buffer since §3.8. It is now sent at once.
- **A soft stop dropped a request still on its way to a backend, and a forced
  close sent no final GOAWAY**
  ([#1647](https://github.com/sozu-proxy/sozu/issues/1647),
  [#1654](https://github.com/sozu-proxy/sozu/pull/1654)). A stream whose header
  block is still being reassembled is `StreamState::Idle`, and one whose
  complete request awaits its backend link is `StreamState::Link`;
  `Mux::shutting_down_inner` waited for neither, so a soft stop landing then
  closed the session once the advisory GOAWAY was flushed and the request was
  lost. The session now waits for both, within
  `h2_graceful_shutdown_deadline_seconds`; when that budget elapses,
  `ConnectionH2::goaway_before_forced_close` sends a final `GOAWAY(NO_ERROR)`
  before every forced close of a draining H2 session, whose `last_stream_id`
  excludes a stream whose opening block never completed, so the client may
  retry it. A client that never finishes its block now holds the drain until
  the budget instead of being closed at once.

### 4.4 HTTP/1.1 response framing

- **A `Content-Length` response truncated by a backend close was forwarded as
  complete** ([#1633](https://github.com/sozu-proxy/sozu/issues/1633),
  [#1640](https://github.com/sozu-proxy/sozu/pull/1640)): it now ends in error
  (RST_STREAM on H2, a closed connection on H1), and a failed response is never
  followed by a default answer once part of it has left.
- **A backend's `Connection: close` kept the H1 client connection open**
  ([#1642](https://github.com/sozu-proxy/sozu/issues/1642),
  [#1648](https://github.com/sozu-proxy/sozu/pull/1648)), so a close-delimited
  body never ended and a pipelined response could be appended to it.

### 4.5 Worker supervision

`Channel::readable` cleared HUP and ERROR on a read error, so a main process
soft-stopping a worker that died hung forever
([#1560](https://github.com/sozu-proxy/sozu/issues/1560),
[#1562](https://github.com/sozu-proxy/sozu/pull/1562)); channel write errors
were swallowed or reported as read errors
([#1570](https://github.com/sozu-proxy/sozu/pull/1570)).

## 5. Invariants to preserve

Each property below is held by a test that was seen red before its fix. Run the
unit tests with `cargo test -p sozu-lib --lib` (the allocation tests are
compiled into that binary) and `cargo test -p sozu-command-lib`.

| Property | Held by |
|---|---|
| A steady-state backend selection allocates nothing | `backend_selection_allocates_nothing_in_steady_state` (`lib/tests/backend_selection.rs`) |
| A redial, a reuse, a routed request and a dial copy no backend id or cluster id | `a_redial_of_an_interned_backend_allocates_nothing`, `a_request_on_a_reused_backend_connection_allocates_nothing`, `stamping_a_dialled_backend_allocates_nothing`, `a_routed_request_on_a_reused_backend_connection_copies_no_cluster_id`, `a_dial_moves_the_planned_cluster_id_into_the_backend_connection` |
| A route lookup allocates nothing past its result | `a_tree_lookup_allocates_nothing_past_its_route_result` |
| The local metrics drain allocates nothing per request | `steady_state_emission_does_not_allocate` |
| An access-log line allocates nothing, and a `tcp://` record is one write | `a_file_access_log_line_does_not_allocate`, `a_udp_access_log_line_does_not_allocate` (`command/tests/access_log_allocations.rs`), `a_fragmented_record_reaches_the_sink_in_exactly_one_call` |
| The header editor's per-request budget | `a_bare_request_costs_one_scratch_and_one_copy_per_forwarding_header`, `a_response_shares_the_request_id_rendering`, `keep_alive_requests_reuse_the_connection_forwarding_hop`, `a_response_reason_is_borrowed_when_standard_and_copied_otherwise` |
| A warm write pass allocates nothing | `a_warm_write_pass_allocates_nothing` (H2), `a_warm_h1_response_write_pass_allocates_nothing`, `a_warm_h1_request_write_pass_allocates_nothing` |
| Session setup: no debug ring, first backend and timer handles inline | `creating_a_debug_history_allocates_nothing`, `the_first_backend_connection_of_a_session_allocates_nothing`, `a_session_holds_its_frontend_and_first_backend_wheel_handles_without_allocating` |
| HPACK decoding allocates nothing per field | `huffman_decoding_allocates_nothing`, `a_steady_state_block_allocates_nothing` |
| One `TCP_INFO` read per connection and side | `keep_alive_requests_read_tcp_info_once_per_connection`, `snapshot_rtts_samples_the_frontend_once_per_connection`, `a_pass_that_logs_nothing_reads_no_tcp_info` |
| No `recv` that can only answer `EAGAIN` | `frames_sharing_one_record_cost_one_recv`, `a_short_handshake_read_stops_without_an_eagain`, `the_upgraded_frontend_reads_only_when_a_read_is_due`, `after_a_short_read_new_bytes_raise_a_new_edge`, `data_and_fin_in_one_edge_keep_readable_until_the_eof_is_read` |
| The server preface is one write, the final GOAWAY and `close_notify` one more | `server_preface_settings_window_update_and_ack_share_one_write`, `the_final_goaway_and_close_notify_leave_in_one_tls_write` |
| No `EPOLL_CTL_DEL` before a session socket's last close | `closed_sessions_leave_their_sockets_to_close`, `close_leaves_backend_sockets_to_their_last_close` |
| No `shutdown(2)` once the peer has closed | `mux_close_skips_the_backend_shutdown_once_the_backend_closed`, `https_close_skips_the_frontend_shutdown_once_the_client_closed` |
| No per-connection `getpeername` or `TCP_NODELAY` | `a_steady_state_accepted_socket_inherits_nodelay_and_its_peer_address` |
| A draining session makes one write pass per soft-stop call | `an_idle_draining_h2_session_makes_one_writable_pass_per_shutdown_tick` |
| A draining H2 session waits for a request still arriving or awaiting its link, and a forced close sends a final GOAWAY | `a_draining_h2_session_waits_for_a_stream_awaiting_its_link`, `test_h2_graceful_drain_deadline_mid_header_block_sends_final_goaway` |
| A fresh request id per keep-alive request and per stream | `header_editing_output_is_byte_exact_across_keep_alive_requests`, `test_keep_alive_rotates_request_id`, `test_h2_streams_carry_distinct_request_ids` |
| Every committed H2 frame leaves whole | `a_removed_half_written_stream_still_completes_its_frame`, `a_queue_that_never_drains_stays_bounded` |

Unit tests cannot count the system calls a running worker makes for its
sockets, so a change on the data path is also measured end to end with the rig
of §2.1: build both sides with the same features, trace the worker only, run the
identical load, and report both counts, the tool and the load average. A
reduction that adds an allocation per request must say so and justify the trade.

## 6. Pull requests

Merged between 2026-09-26 and 2026-09-28, in number order. The campaign's
changes are described above; the others merged in the same window and are
listed for completeness.

| Pull request | Merge | Date | Title |
|---|---|---|---|
| [#1541](https://github.com/sozu-proxy/sozu/pull/1541) | `7fb6500e` | 2026-09-26 | test(mux): read the gauge the three active_requests e2e tests are named after |
| [#1542](https://github.com/sozu-proxy/sozu/pull/1542) | `4cbad821` | 2026-09-26 | refactor(mux-h1): render the peer address in ConnectionH1's Debug |
| [#1543](https://github.com/sozu-proxy/sozu/pull/1543) | `1b1b77ff` | 2026-09-26 | docs(mux): complete the §1.1 module-layout table with the fifteen missing modules |
| [#1545](https://github.com/sozu-proxy/sozu/pull/1545) | `747bc3a8` | 2026-09-26 | docs(mux,e2e): inline the reasoning nine comments deferred to a missing note |
| [#1546](https://github.com/sozu-proxy/sozu/pull/1546) | `f7491559` | 2026-09-26 | feat(metrics): add backend_header_time, the backend time-to-first-header-byte |
| [#1550](https://github.com/sozu-proxy/sozu/pull/1550) | `e2fd5f0d` | 2026-09-26 | refactor(mux): select and dial through a BackendDialer lent for one call |
| [#1551](https://github.com/sozu-proxy/sozu/pull/1551) | `ca058989` | 2026-09-26 | fix(mux-h1): log the request line of a request its parse rejected |
| [#1552](https://github.com/sozu-proxy/sozu/pull/1552) | `d8880795` | 2026-09-26 | perf(logging): send one syscall per record on the tcp log backend |
| [#1553](https://github.com/sozu-proxy/sozu/pull/1553) | `9a687884` | 2026-09-26 | perf(udp): stop cloning the mio registry on every event-loop turn |
| [#1554](https://github.com/sozu-proxy/sozu/pull/1554) | `bdedc5fb` | 2026-09-26 | fix(logging): flush a stopping worker's log buffers before its channel closes |
| [#1556](https://github.com/sozu-proxy/sozu/pull/1556) | `be93fde3` | 2026-09-26 | docs(mux-h1): state what the 1xx arm_writable actually covers |
| [#1557](https://github.com/sozu-proxy/sozu/pull/1557) | `d2d4bbd0` | 2026-09-26 | perf(backends): select a backend without allocating or cloning candidates |
| [#1558](https://github.com/sozu-proxy/sozu/pull/1558) | `e123d736` | 2026-09-26 | perf(mux-h2): send the server preface in one writev |
| [#1559](https://github.com/sozu-proxy/sozu/pull/1559) | `57c92699` | 2026-09-26 | fix(bin): soft stop the workers on SIGTERM |
| [#1561](https://github.com/sozu-proxy/sozu/pull/1561) | `f0b4af26` | 2026-09-26 | perf(metrics): make the local drain's per-request path allocation-free and index backends by id |
| [#1562](https://github.com/sozu-proxy/sozu/pull/1562) | `9affad1f` | 2026-09-26 | fix(command): keep HUP and ERROR set when a channel socket call fails |
| [#1565](https://github.com/sozu-proxy/sozu/pull/1565) | `a3e72a34` | 2026-09-26 | perf(mux): stop allocating the backend id on every backend dial |
| [#1568](https://github.com/sozu-proxy/sozu/pull/1568) | `c3679fe4` | 2026-09-26 | perf(mux): leave closing session sockets to close(2), not EPOLL_CTL_DEL |
| [#1569](https://github.com/sozu-proxy/sozu/pull/1569) | `598929a5` | 2026-09-26 | fix(mux-h2): log the pseudo-headers of a request whose field block was refused |
| [#1570](https://github.com/sozu-proxy/sozu/pull/1570) | `5ffd4096` | 2026-09-26 | fix(command): report channel write errors instead of swallowing or misnaming them |
| [#1573](https://github.com/sozu-proxy/sozu/pull/1573) | `bbb78cb3` | 2026-09-26 | fix(top): leave saved_state unset in the sozu top e2e config |
| [#1575](https://github.com/sozu-proxy/sozu/pull/1575) | `ba2aeac9` | 2026-09-26 | fix(top): render --snapshot frames to stdout without terminal control |
| [#1577](https://github.com/sozu-proxy/sozu/pull/1577) | `b5169d44` | 2026-09-26 | fix(top): track rst_stream_dropped in the H2 trend and run the tui tests in CI |
| [#1578](https://github.com/sozu-proxy/sozu/pull/1578) | `83bc9216` | 2026-09-26 | perf(mux-h2): reuse the write pass IoSlice vector across passes |
| [#1581](https://github.com/sozu-proxy/sozu/pull/1581) | `39dcc9c3` | 2026-09-26 | perf(mux): stop copying the backend id into a String per request |
| [#1582](https://github.com/sozu-proxy/sozu/pull/1582) | `4d104b3a` | 2026-09-26 | perf(mux-h1): reuse the write pass IoSlice vector across passes |
| [#1584](https://github.com/sozu-proxy/sozu/pull/1584) | `e49d78ab` | 2026-09-26 | perf(mux): stop allocating per request on a reused backend connection |
| [#1591](https://github.com/sozu-proxy/sozu/pull/1591) | `a1d314f9` | 2026-09-26 | perf(mux): stop preallocating the debug history in release builds |
| [#1592](https://github.com/sozu-proxy/sozu/pull/1592) | `02597013` | 2026-09-26 | perf(logging): render an access-log line without allocating |
| [#1593](https://github.com/sozu-proxy/sozu/pull/1593) | `761fab2a` | 2026-09-26 | perf(server): reuse the accept(2) peer address and set TCP_NODELAY on the listener |
| [#1594](https://github.com/sozu-proxy/sozu/pull/1594) | `bbceca82` | 2026-09-26 | perf(router): copy no authority and allocate no trie path per request |
| [#1596](https://github.com/sozu-proxy/sozu/pull/1596) | `97a97994` | 2026-09-26 | perf(socket): serve buffered TLS plaintext before calling read_tls |
| [#1598](https://github.com/sozu-proxy/sozu/pull/1598) | `f165a3dc` | 2026-09-26 | perf(mux): read TCP_INFO only when an access log needs it |
| [#1599](https://github.com/sozu-proxy/sozu/pull/1599) | `b19d94f8` | 2026-09-27 | fix(mux-h2): keep reading after a stream is reaped mid DATA payload |
| [#1601](https://github.com/sozu-proxy/sozu/pull/1601) | `b03ffa2b` | 2026-09-26 | fix(mux-h2): send a PING or SETTINGS ACK after a half-written stream frame |
| [#1605](https://github.com/sozu-proxy/sozu/pull/1605) | `13269c5e` | 2026-09-27 | perf(mux): stop waiting for a read EOF and shutting down closed peers |
| [#1606](https://github.com/sozu-proxy/sozu/pull/1606) | `7c820e57` | 2026-09-27 | perf(socket): stop reading on a short read instead of reading until EAGAIN |
| [#1608](https://github.com/sozu-proxy/sozu/pull/1608) | `b74d5d97` | 2026-09-27 | perf(mux-h2): send the final GOAWAY and the TLS close_notify in one write |
| [#1611](https://github.com/sozu-proxy/sozu/pull/1611) | `6b1db0c4` | 2026-09-27 | perf(tls): drop the EAGAIN recv of the handshake, its upgrade and split record reads |
| [#1612](https://github.com/sozu-proxy/sozu/pull/1612) | `addef7b4` | 2026-09-27 | perf(mux): cut the allocations of a session's setup |
| [#1614](https://github.com/sozu-proxy/sozu/pull/1614) | `a0785505` | 2026-09-27 | perf(mux): keep a session's wheel handles and backend reverse index inline |
| [#1617](https://github.com/sozu-proxy/sozu/pull/1617) | `46204814` | 2026-09-27 | fix(router): number tree-rule $HOST[n] captures left to right |
| [#1618](https://github.com/sozu-proxy/sozu/pull/1618) | `2d4a0dc4` | 2026-09-27 | refactor(lib): remove L7Proxy::deregister_socket |
| [#1620](https://github.com/sozu-proxy/sozu/pull/1620) | `bdc74ecd` | 2026-09-27 | perf(mux-h2): replace loona-hpack with a sans-io HPACK module |
| [#1621](https://github.com/sozu-proxy/sozu/pull/1621) | `771c9e90` | 2026-09-27 | test(udp): drive the early-wheel-fire eviction test from a simulated clock |
| [#1625](https://github.com/sozu-proxy/sozu/pull/1625) | `58550dd4` | 2026-09-27 | fix(mux-h2): send every committed frame whole through one ordered output queue |
| [#1626](https://github.com/sozu-proxy/sozu/pull/1626) | `0155d40b` | 2026-09-27 | fix(hpack): signal the smallest table size before the last one |
| [#1628](https://github.com/sozu-proxy/sozu/pull/1628) | `c974237c` | 2026-09-27 | perf(h1): 24.6 → 15.6 heap operations per keep-alive request |
| [#1629](https://github.com/sozu-proxy/sozu/pull/1629) | `3a79685e` | 2026-09-27 | refactor(command): share cluster ids as Arc<str> instead of copying Strings |
| [#1630](https://github.com/sozu-proxy/sozu/pull/1630) | `d5161919` | 2026-09-27 | fix(mux-h2): reset the HPACK encoder table when an encoded block is dropped unsent |
| [#1634](https://github.com/sozu-proxy/sozu/pull/1634) | `41ccc1b7` | 2026-09-28 | perf(mux): sample the access-log RTT once per connection |
| [#1635](https://github.com/sozu-proxy/sozu/pull/1635) | `6eef0ba1` | 2026-09-28 | fix(http): give every keep-alive request its own request id |
| [#1636](https://github.com/sozu-proxy/sozu/pull/1636) | `5db53c60` | 2026-09-28 | test(e2e): finish the TLS handshake before the SETTINGS-ACK test polls |
| [#1638](https://github.com/sozu-proxy/sozu/pull/1638) | `cea53d67` | 2026-09-28 | fix(mux-h2): never reset a backend stream whose HEADERS have not left |
| [#1639](https://github.com/sozu-proxy/sozu/pull/1639) | `44c18422` | 2026-09-28 | fix(mux-h2): never re-link a request encoded for another H2 backend connection |
| [#1640](https://github.com/sozu-proxy/sozu/pull/1640) | `96b4bca9` | 2026-09-28 | fix(mux): end a Content-Length response truncated by a backend close in error |
| [#1643](https://github.com/sozu-proxy/sozu/pull/1643) | `a3cb11ae` | 2026-09-28 | perf(h1): render a keep-alive connection's forwarding values once |
| [#1644](https://github.com/sozu-proxy/sozu/pull/1644) | `85f0ae33` | 2026-09-28 | perf(h1): capture a standard reason phrase without a copy |
| [#1645](https://github.com/sozu-proxy/sozu/pull/1645) | `3ee0b6d5` | 2026-09-28 | perf(mux): stop spinning empty writes on every draining H2 session |
| [#1646](https://github.com/sozu-proxy/sozu/pull/1646) | `a3d3dd47` | 2026-09-28 | fix(mux-h2): give back the send credit of DATA prepared and dropped unsent |
| [#1648](https://github.com/sozu-proxy/sozu/pull/1648) | `eadfa4a7` | 2026-09-28 | fix(mux): close an H1 client connection after the backend's Connection: close |
| [#1649](https://github.com/sozu-proxy/sozu/pull/1649) | `ca42a2ea` | 2026-09-28 | fix(mux-h2): send the advisory GOAWAY even while a peer header block is incomplete |
| [#1651](https://github.com/sozu-proxy/sozu/pull/1651) | `2a1c488f` | 2026-09-28 | fix(h1): end a request without Content-Length or Transfer-Encoding after its headers |
| [#1653](https://github.com/sozu-proxy/sozu/pull/1653) | `c086b456` | 2026-09-28 | fix(h1): reject a Content-Length that is not 1*DIGIT instead of forwarding it verbatim |
| [#1654](https://github.com/sozu-proxy/sozu/pull/1654) | `d729cef5` | 2026-09-28 | fix(mux-h2): wait for an incomplete header block on soft-stop and close with a final GOAWAY |
| [#1655](https://github.com/sozu-proxy/sozu/pull/1655) | `f5136d67` | 2026-09-28 | chore(deps): bump kawa to 0.7.2 |
