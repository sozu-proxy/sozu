# Sōzu rate limiting

**Status:** shipped in 2.0.0 — closes [#890](https://github.com/sozu-proxy/sozu/issues/890) (generic rate-limiting) and [#1057](https://github.com/sozu-proxy/sozu/issues/1057) (per-source-IP DoS protection).

This document describes the rate-limiting mechanism that landed in PR
[#1193](https://github.com/sozu-proxy/sozu/pull/1193) and the rationale
for the chosen shape. An earlier design draft proposed a two-layer
mechanism (token-bucket request-rate at L7 + per-IP TCP cap at L4); the
team converged on a single mechanism — **per-(cluster, source-IP)
concurrent connection cap with a 429 response** — that covers both
abuse vectors with a much smaller config + code surface. The
token-bucket request-rate option is documented in §6 as deferred future
work.

## 1. Problem statement

Two distinct abuse vectors needed mitigation:

1. **Per-tenant noisy neighbour ([#890](https://github.com/sozu-proxy/sozu/issues/890)).** A single tenant on a multi-tenant listener can saturate a cluster with a connection burst, starving other tenants on the same listener.
2. **Per-source-IP DoS ([#1057](https://github.com/sozu-proxy/sozu/issues/1057)).** Mass connection attempts from one source IP saturate the accept queue long before `max_connections` fires.

The shipped mechanism addresses **both** vectors simultaneously: a
single counter tracks `(cluster_id, source_ip) → concurrent connection
count`, with the operator setting a global default (cluster-isolating,
addresses #890) and an optional per-cluster override (per-tenant
tuning, addresses #1057's noisy-neighbour pattern).

## 2. Non-goals

- Not a replacement for an upstream WAF (Cloudflare, ModSecurity). Sōzu's per-IP cap is a defence-in-depth floor, not a policy engine.
- No cross-worker / cross-process state. Each worker enforces its own counter independently. A 4-worker deployment with cap=`100` effectively allows 400 concurrent connections per (cluster, IP). Documented trade-off; cross-worker sync is deferred (§6).
- Not a request-rate limit. The cap counts **concurrent connections**, not requests-per-second. Token-bucket request-rate limiting is a separate feature (§6).
- Not a path/method-scoped limit. Path/method limits belong in router middleware, not the listener.
- Does not touch the H2 flood detector (`H2FloodDetector` in `lib/src/protocol/mux/h2_flood_detector.rs`). Protocol-layer abuse stays separate.

## 3. Mechanism — per-(cluster, source-IP) concurrent connection cap

### 3.1 Data model

Two configuration knobs, additive across global → per-cluster:

```protobuf
// command/src/command.proto
message Config {
  // Global default. 0 = disabled (no cap).
  optional uint64 max_connections_per_ip = 23 [default = 0];
}

message Cluster {
  // Per-cluster override. None = inherit global; Some(0) = explicit
  // unlimited for this cluster; Some(n) = override.
  optional uint64 max_connections_per_ip = 13;
  // Optional Retry-After hint emitted on 429 (seconds). 0 / unset
  // elides the header — `Retry-After: 0` invites an immediate retry
  // that defeats the cap.
  optional uint32 retry_after            = 14;
  // Per-cluster override of the SUBNET cap. Same three-state
  // semantics. Independent of tag 13: both must admit.
  optional uint64 max_connections_per_subnet = 17;
}
```

#### The second, independent cap: per-(cluster, source-SUBNET)

The per-IP cap above has no concept of a subnet, and that is a hole
rather than a simplification. Someone on an IPv6 `/64` — the standard
residential allocation — bypasses it **entirely** by taking a fresh
address for every connection: the per-address counter never reaches 2.
[#1270](https://github.com/sozu-proxy/sozu/issues/1270) reported it.

The answer is a SECOND counter, not a change to the first. The two
serve different cases, and both need to be expressible at once:

- **per-IP** — the API-gateway case. One client, one address, holding
  too many connections.
- **per-subnet** — the denial-of-service case. One actor, a routed
  prefix, spreading across it.

So `max_connections_per_ip = 10` and `max_connections_per_subnet = 100`
together mean "10 per IP **and** 100 per subnet". Both gates are
consulted on every admission and **both must admit**.

```protobuf
message Config {
  // Global default for the subnet cap. 0 = disabled, and it IS the
  // default: the per-IP behaviour is strictly unchanged out of the box.
  optional uint64 max_connections_per_subnet = 26 [default = 0];
  // Prefix lengths, in bits, defining "subnet". Rejected at
  // config-load time outside 0-32 / 0-128, never clamped. The defaults
  // mask nothing. /24 and /56 are the recommended starting points.
  optional uint32 subnet_ipv4_prefix         = 27 [default = 32];
  optional uint32 subnet_ipv6_prefix         = 28 [default = 128];
}
```

The prefixes are **boot-time only**. Changing a mask would re-key every
live counter, so there is no runtime setter for them; only the limit
itself can be patched at runtime.

**IPv4-mapped IPv6 sources are canonicalised first.** A dual-stack
`[::]` listener reports an IPv4 client as `::ffff:a.b.c.d`. Masking
that with an IPv6 prefix would collapse the entire IPv4 internet into
the single `::/56` bucket, so `SessionManager::subnet_key`
(`lib/src/server.rs`) converts the mapped form to IPv4 and charges it
to `subnet_ipv4_prefix` instead. The per-IP counter is deliberately
**not** canonicalised — it keeps charging the address exactly as it
arrived, which is part of what leaves its behaviour untouched.

Cluster-level override takes precedence; `None` falls back to the
global default. `Some(0)` is an explicit "unlimited" sentinel — the
operator disables the cap on a specific cluster while keeping the
global default on others.

### 3.2 Counter discipline

`SessionManager` (in `lib/src/`) keeps a per-token set of `(cluster_id,
source_ip)` pairs. On session accept + cluster resolution:

1. Resolve the source IP: PROXY-protocol header IP if present
   (`ExpectHeader` and `RelayHeader` modes both preserve the parsed
   source through the pipe phase), else `peer_addr`. A `LOCAL`-command
   header carries no client identity: a TCP listener falls back to
   `peer_addr`, an HTTP / HTTPS listener closes the session before it
   ever reaches this step — see §3.6.
2. Look up `count` for `(cluster_id, source_ip)`.
3. Compare against `cluster.max_connections_per_ip.unwrap_or(global)`.
4. If `count >= cap`, reject; else increment and proceed.
5. On session close, decrement. Empty entries are dropped.

The subnet counter is an exact mirror of this discipline on a second
pair of maps (`connections_per_cluster_subnet` /
`cluster_subnet_tracks`), keyed on the masked address instead of the
raw one, with one deliberate difference: **it is not written at all
while the cap is `0`**. The per-IP index is populated unconditionally,
so mirroring that would make every deployment which never enables the
subnet cap pay a second hash, a second allocation and a second
per-session entry forever. The cost of the difference is that enabling
the cap at runtime does not retroactively count sessions already in
flight; the counter converges as those sessions close.

Both counters are released from the single existing teardown call,
`SessionManager::untrack_all_cluster_ip` — a second release entry point
would be one more thing each of the three protocol close paths could
forget, and a missed release leaks a slot until worker restart.

The counter lives on the SessionManager rather than a global hashmap
because of H2 multiplexing: H2 sessions multiplex many streams over
one frontend connection, so the per-token set keeps multiple streams
to the same cluster from the same source consuming a **single**
connection slot. This matches the per-connection semantics operators
expect when they set `max_connections_per_ip = 1`.

Step 4 increments **before** backend selection and before the dial, and
step 5 is the only decrement: a backend dial that fails does **not**
give the slot back. That is deliberate, not a missing pairing — see
§3.2.1.

#### 3.2.1 A failed backend dial keeps the slot

`Router::plan_connect` (`lib/src/protocol/mux/router.rs`) increments
after the gate check and before it decides anything about a backend.
Six failures can follow that increment, and every one of them returns a
`BackendConnectionError` without decrementing. Since
[#1519](https://github.com/sozu-proxy/sozu/pull/1519) split the connect
path they live in two functions:

- **`Router::plan_connect`** keeps the two pool-reuse refusals — a
  reused connection that vanished from the router's backend map, and a
  reused backend that refuses one more stream.
- **`Mux::dial_backend`** (`lib/src/protocol/mux/mod.rs`) owns the four
  on the fresh-dial path — backend selection with nothing available, a
  buffer pool with nothing left, the freshly-built connection refusing
  the stream, and a `L7Proxy::register_socket` that fails. The last one
  rolls back its own gauges, slab entry and epoll registration; it does
  not roll back the slot.

The split changed no ordering — the increment still precedes all six —
and it does not fork the error path either: `Mux::ready_inner` joins the
plan and the dial into a single `Result` with `and_then`, so both halves
land in one error arm. The decrement happens once, on session teardown.

The claim is cheap because a failed dial **ends the frontend
connection**, so the slot comes back with it:

- That error arm, in `Mux::ready_inner`'s pending-link loop, answers the
  stream from the template registry rather than retrying, and no arm of
  it returns a session close of its own. Every built-in template in
  `lib/src/protocol/kawa_h1/answers.rs` carries `Connection: close`,
  which clears `HttpContext::keep_alive_frontend`.
- **HTTP/1.1** — once the answer flushes, the response-complete branch
  of `ConnectionH1::writable` (`lib/src/protocol/mux/h1.rs`) closes the
  session instead of taking the keep-alive reset. One flush.
- **HTTP/2, the failed stream was the only one** — the write pass sends
  a final `GOAWAY(NO_ERROR)` as that stream retires.
- **HTTP/2, other streams still in flight** — the write pass sends the
  advisory `GOAWAY` of a graceful drain instead, and a draining
  connection **refuses** new peer streams. The window is therefore the
  remaining lifetime of the streams already open, and the client cannot
  extend it. The `h2_graceful_shutdown_deadline_seconds` forced close
  does not apply here: it is armed only by the worker's soft-stop path.
- **Raw TCP** — never reaches an answer. Any error out of
  `TcpSession::connect_to_backend` (`lib/src/tcp.rs`) becomes a session
  close in the same event-loop pass.

A registered, healthy-status backend that is down may not fail on the
dial at all. `Backend::try_connect` (`lib/src/backends.rs`) is
non-blocking: a refusal the kernel reports synchronously — the common
case for a closed port on loopback — surfaces as
`BackendError::ConnectionFailures` and takes the answer path above,
while an `EINPROGRESS` connect returns `Ok` and its refusal, or a
blackholed SYN, arrives later as a backend HUP and the stream retries
instead. The re-increment is idempotent within the session, so **one**
slot — not one per attempt — is held for at most `CONN_RETRIES`
attempts, each bounded by `connect_timeout`.

The one configuration in which the slot is held for the connection's
whole lifetime is a custom answer template that omits
`Connection: close` — the same keep-alive opt-in §3.4 records for the
429 template. An operator who takes it holds one slot per failed dial
until the client goes away, and should size
`max_connections_per_ip` for that.

Releasing the slot on a dial failure would **loosen** the cap under
precisely the conditions a client can induce, and would buy very
little: the connection that provoked the failure is closed by the
answer it provoked, so it cannot accumulate slots either way. Changing
this is its own decision, not part of a refactor
(sozu-proxy/sozu#1521).

### 3.3 Enforcement points

Both caps are checked **after cluster resolution and before backend
connect**, so a 401 / 421 / redirect / answer-template frontend is
never gated. The conjunction lives in exactly one place —
`SessionManager::cluster_connection_at_limit` (`lib/src/server.rs`) —
so the two enforcement points below cannot drift on what "admitted"
means. Two enforcement points:

- **Unified H1+H2 mux** (`lib/src/protocol/mux/router.rs`) — HTTP and
  HTTPS clients hitting the cap receive `429 Too Many Requests` with
  an optional `Retry-After` header.
- **Raw TCP** (`lib/src/tcp.rs`) — TCP clients see a graceful FIN
  before any backend connect — no SO_LINGER trick, no RST.

### 3.4 Reject behaviour (HTTP / HTTPS)

- Status: `429 Too Many Requests` (RFC 6585).
- Header: `Retry-After: <cluster.retry_after>` if `Some(n > 0)`; elided otherwise.
- Body: `Answer429` variant in the unified template engine, alongside
  the existing `Answer4xx` / `Answer5xx`. Operators can override per
  cluster via `CustomHttpAnswers.answer_429` (proto tag 12).
- Counter `connections.rejected_per_cluster_ip` (cluster-labelled)
  increments on every reject.

The `Answer429` template supports `%RETRY_AFTER` substitution. The
listener default emits a one-line response with `Connection: close`;
custom templates can opt into keep-alive by omitting the
`Connection: close` header.

### 3.5 Reject behaviour (TCP)

- Graceful FIN — no `SO_LINGER` reset trick, no `TCP RST`. The client
  sees a normal close before any backend connect attempt.
- Same counter `connections.rejected_per_cluster_ip` increments.

### 3.6 Source IP selection

| PROXY-protocol mode                  | Source IP used                                          |
| ------------------------------------ | ------------------------------------------------------- |
| `ExpectHeader`                       | Parsed from PP-v2                                       |
| `RelayHeader`                        | Parsed from PP-v2 (preserved through the pipe phase)    |
| PP-v2 `LOCAL`, TCP listener          | `peer_addr` (TCP-level) — address block discarded       |
| PP-v2 `LOCAL`, HTTP / HTTPS listener | none — the session is closed before cluster resolution  |
| No PROXY-protocol                    | `peer_addr` (TCP-level)                                 |

The two `LOCAL` rows are a security property, not a convenience: the HAProxy PROXY
protocol specification §2.2 says a `LOCAL` header (ver/cmd `0x20`) describes a
connection the upstream proxy originated itself, typically a health check, and
its address block must be ignored. The wire format does not require that block
to be empty — only HAProxy's own emitter pairs `LOCAL` with `AF_UNSPEC` — so an
attacker may send `LOCAL` with a fully populated `AF_INET` block. `parse_v2_header`
discards it, otherwise any peer able to reach a PROXY-enabled listener could
pick the IP its connections are counted against and evade
`max_connections_per_ip` at will.

Discarding yields `ProxyAddr::AfUnspec`, and the two listener families resolve
that differently, which is why the table needs two `LOCAL` rows. The four TCP
consumers all fall back to `peer_addr`, so the connection is still counted, just
against the socket peer:

- `TcpSession::effective_session_address` (`lib/src/tcp.rs`) — **this is the
  function that resolves step 1 above for raw TCP**. It reads
  `ExpectProxyProtocol::addresses` / `RelayProxyProtocol::addresses` and the
  preread outcome directly, folds the raw socket peer over them with
  `.or(self.frontend_address)` (`lib/src/tcp.rs:385`; `frontend_address =
  socket.peer_addr().ok()`, `lib/src/tcp.rs:183` and `:305`), and is called from
  the per-(cluster, source-IP) gate at `lib/src/tcp.rs:1672`.
- `ExpectProxyProtocol::into_pipe`
  (`lib/src/protocol/proxy_protocol/expect.rs`) and
  `RelayProxyProtocol::into_pipe`
  (`lib/src/protocol/proxy_protocol/relay.rs`) — they set
  `Pipe::session_address`, which `effective_session_address` then returns for
  the post-upgrade `Pipe` state.
- the SNI preread's `proxy_source` (`lib/src/protocol/tcp_preread/mod.rs:284`,
  resolved against the socket at `lib/src/tcp.rs:908`).

The two HTTP consumers do not:
`HttpSession::upgrade_expect` (`lib/src/http.rs`) and
`HttpsSession::upgrade_expect` (`lib/src/https.rs`) need both a source and a
destination, `ProxyAddr::AfUnspec` yields neither (`ProxyAddr::source` /
`ProxyAddr::destination` in `lib/src/protocol/proxy_protocol/header.rs`), so
each returns `None` and `HttpSession::upgrade` reports `SessionIsToBeClosed`
(`lib/src/http.rs`). An HTTP or HTTPS session that presents a `LOCAL` header is
therefore closed at the expect stage and never reaches §3.2's counter at all.

That close is not a regression for legitimate traffic: HAProxy pairs `LOCAL`
with `AF_UNSPEC`, which already parsed to `ProxyAddr::AfUnspec`, so an HTTP or
HTTPS session from a health-checking upstream already closed here before the
change. What the discard alters is only the forged case — a `LOCAL` header
carrying a populated address block — which used to upgrade with attacker-chosen
addresses and now closes, which is the correct outcome.

The ExpectHeader / RelayHeader paths were updated to be
address-aware in this PR — both modes now carry the parsed source IP
through to the SessionManager. Without this, sōzu running behind
another L4 LB with PROXY-v2 enabled would have rate-limited
per-LB-IP, not per-real-client.

### 3.7 Config surface

```toml
# Global default — applies to every cluster unless the cluster overrides.
# `0` (default) disables the cap.
max_connections_per_ip = 100

[clusters."api-strict"]
# Per-cluster override (proto tag 13). `Some(0)` = explicit unlimited;
# omit to inherit the global default.
max_connections_per_ip = 20
# Retry-After hint on the emitted 429 (proto tag 14). `0` elides.
retry_after = 5

[clusters."api-internal"]
# Internal cluster — opt out of the global cap.
max_connections_per_ip = 0
```

Both caps together, which is the shape [#1270](https://github.com/sozu-proxy/sozu/issues/1270)
asks for:

```toml
# "10 per IP AND 100 per /24 (IPv4) or /56 (IPv6)".
max_connections_per_ip     = 10
max_connections_per_subnet = 100
subnet_ipv4_prefix         = 24
subnet_ipv6_prefix         = 56

[clusters."api-internal"]
# Independent opt-outs: this cluster keeps the per-IP cap but drops
# the subnet one.
max_connections_per_subnet = 0
```

### 3.8 CLI / runtime API

Three verbs on `sozu connection-limit`:

| Verb     | Proto request                    | Proto response                  |
| -------- | -------------------------------- | ------------------------------- |
| `set`    | `SetMaxConnectionsPerIp` (50)    | —                               |
| `show`   | `QueryMaxConnectionsPerIp` (51)  | `MaxConnectionsPerIpLimit` (14) |
| `remove` | `SetMaxConnectionsPerIp(0)` (50) | —                               |

Three mirrored verbs on `sozu subnet-connection-limit`:

| Verb     | Proto request                        | Proto response                      |
| -------- | ------------------------------------ | ----------------------------------- |
| `set`    | `SetMaxConnectionsPerSubnet` (60)    | —                                   |
| `show`   | `QueryMaxConnectionsPerSubnet` (61)  | `MaxConnectionsPerSubnetLimit` (18) |
| `remove` | `SetMaxConnectionsPerSubnet(0)` (60) | —                                   |

`show` reports the two prefix lengths alongside the limit. They are
boot-time only, so this reply is the only runtime surface that can tell
an operator what the live cap is a cap *on*; a bare number would not.

The setter is **non-sticky** — it patches the worker's runtime value
but does not write through to TOML. Operators must mirror the change
in their config to make it durable across restarts.

## 4. Metrics

| Key                                   | Type    | Meaning                                                   |
| ------------------------------------- | ------- | --------------------------------------------------------- |
| `connections.rejected_per_cluster_ip` | counter | per-cluster reject count (cluster-labelled)               |
| `client.connect.per_source.bucket_*`  | counter | per-IP accept-queue admission histogram (already shipped) |

The single counter `connections.rejected_per_cluster_ip` (with the
`cluster_id` label) is the durable signal. Operators wanting per-IP
attribution should pair it with the existing
`client.connect.per_source.bucket_*` series.

## 5. Test coverage

End-to-end tests in `e2e/src/tests/cluster_ip_limit_tests.rs`:

- **HTTP/1.1 global limit emits 429**: a global
  `max_connections_per_ip = 1` rejects the second concurrent
  connection from the same source IP with `429` + `Retry-After`.
  Confirms the answer-engine path, the `Answer429` template variant,
  the `connections.rejected_per_cluster_ip` counter, and the
  SessionManager-side accounting (`untrack_all_cluster_ip` on close
  releases the slot for a follow-up request).
- **HTTP/1.1 + per-cluster override**: an unlimited cluster
  (`Some(0)`) coexists with a capped cluster (`Some(1)`). Two
  concurrent connections to the unlimited cluster MUST both succeed;
  the same pattern against the capped cluster MUST 429.
- **HTTP/1.1 + `Retry-After: 0` semantics**: when the resolved
  `retry_after` is `0`, the response MUST omit the header.
- **TCP**: a TCP listener with `max_connections_per_ip = 1` accepts
  the first connection but closes the second one gracefully (FIN, no
  RST) without dialling the backend.

H2 frontend cells are not in this test file — they are tracked under
`e2e/src/tests/protocol_pair_matrix.rs` as a deferred matrix backfill
(see `e2e/COVERAGE.md`). The connection-pinning helpers needed to
sustain a TLS + HTTP/2 connection while a second connection races
aren't present in the current Hyper client pool.

## 6. Deferred future work

### 6.1 Token-bucket request-rate limiting

A request-rate limit (e.g. "100 requests per second per tenant" with
a token bucket and a refill rate) is **not** in 2.0.0. The connection
cap covers the dominant abuse case (one client opening many
connections to a single cluster) without needing a separate counter
for individual requests. If a request-rate limit becomes necessary,
the natural shape is:

- Layered ON TOP of the connection cap: requests through accepted
  connections feed a per-(cluster, tenant_key) bucket.
- Tenant key resolution: configurable order — peer IP, parsed
  `X-Forwarded-For` (only when `trust_forwarded_for = true` is set on
  the listener), or operator-configured stable header (`X-Tenant-Id`).
- Reject behaviour: same `429` + `Retry-After`, no separate code path
  — the answer engine already handles it.
- Storage: `HashMap<(rule_name, tenant_key), TokenBucket>` with LRU
  eviction at `~10k` entries per worker, refill rate driven by
  `Instant::now()` (already used elsewhere in sōzu).

The trade-off: configuring two complementary mechanisms (connections

- requests) is more work for operators than one. A single mechanism
  that covers 80% of the abuse pattern shipped first; the second
  mechanism ships only when an operator hits a real-world case the
  connection cap doesn't cover.

### 6.2 Cross-worker / cross-process sync

Currently each worker's counter is independent — a 4-worker
deployment with `max_connections_per_ip = 100` allows 400 connections
per `(cluster, IP)`. An operator wanting an exact global cap needs:

- Either a shared-state backend (Redis, memcached) the workers
  consult on every accept — high latency cost, single point of
  failure.
- Or the master broadcasting per-IP counts via the command socket —
  a single message per accept multiplied by N workers — also a
  non-trivial hot-path overhead.

Documented as a known limitation; if a real-world deployment hits
it, the chosen mechanism (Redis vs. command-socket broadcast)
becomes a separate design pass.

### 6.3 Adaptive quotas

Auto-scaling `max_connections_per_ip` based on backend health (e.g.
"halve the cap when backend `503` rate exceeds 1%") is out of scope.
Operators should tune statically based on observed load.

### 6.4 Ban-list integration

Integration with an external ban-list API (abuseipdb, internal
allow / deny lists) is a follow-up. The cap-check site is the natural
insertion point: check the ban list AFTER the cap, with the ban-list
rejection producing a different log line + counter.

### 6.5 Per-source-IP TCP-level cap (pre-TLS)

The original L4 design ([#1057](https://github.com/sozu-proxy/sozu/issues/1057))
proposed a global TCP-RST cap at accept time, before TLS handshake.
This was not implemented separately — the per-(cluster, IP) cap that
shipped covers the same abuse vector once cluster resolution
completes (post-TLS for HTTPS). For deployments where pre-TLS
rejection matters (SSL handshake CPU cost from an attacker), a
TCP-level pre-handshake cap is a possible follow-up. The
`accept_queue.saturated_seconds` and
`client.connect.per_source.bucket_*` metrics already shipped
surface the saturation condition; the pre-TLS cap would build on
top.

## 7. Operator guidance

### 7.1 Setting the global default

```toml
# Recommended starting point for a public-facing listener serving
# heterogeneous traffic. Tuning rule of thumb:
#   max_connections_per_ip << max_connections   (typical ratio 1:100)
max_connections        = 10_000
max_connections_per_ip = 100
```

A source IP can hold up to 100 concurrent connections per cluster,
but the listener as a whole admits up to 10 000 — so a single
attacker saturates one cluster but does not starve other clusters
served by the same listener.

### 7.1.1 Choosing the subnet prefixes

**Recommended starting point: `/24` for IPv4 and `/56` for IPv6.**

```toml
max_connections            = 10_000
max_connections_per_ip     = 10
max_connections_per_subnet = 100
subnet_ipv4_prefix         = 24
subnet_ipv6_prefix         = 56
```

These are recommendations, **not defaults**. The shipped defaults are
`/32` and `/128`, which mask nothing, and `max_connections_per_subnet =
0`, which disables the cap outright — so an upgrade changes no
behaviour until an operator opts in. That follows what the reporter of
[#1270](https://github.com/sozu-proxy/sozu/issues/1270) did in the
equivalent Caddy module: leave the default at single IPs, and recommend
`/24` and `/56` in the documentation.

Why these two:

- **`/24` (IPv4)** — the smallest block routinely allocated to a single
  organisation, and the granularity most operators already reason about.
  Shorter (say `/16`) starts merging unrelated networks.
- **`/56` (IPv6)** — a residential subscriber is typically handed a
  `/56`, from which they may carve `/64`s at will. Capping at `/64`
  would still let a `/56` holder mint 256 uncapped buckets; capping at
  `/48` or shorter would merge unrelated subscribers behind one ISP
  allocation. `/56` is the level at which one subscriber maps to one
  counter.

Set the per-subnet cap well above the per-IP cap — a `/24` legitimately
holds many distinct clients. A ratio of roughly 10:1 over
`max_connections_per_ip` is a reasonable place to start; a NAT'd office
or a mobile carrier's CGNAT range can sit far above that (see §7.3).

Changing a prefix requires a **restart**: the mask is part of the
counter key, so patching it at runtime would re-key every live counter.
Only `max_connections_per_subnet` itself has a runtime setter.

### 7.2 Per-cluster overrides

- Internal management API: `max_connections_per_ip = 0` (unlimited).
- Public-facing strict cluster: `max_connections_per_ip = 20`.
- Default: inherit the global value.

### 7.3 Behind a NAT or shared egress

If a real-world tenant fronts traffic through a single NAT IP
(typical in corporate networks), all that traffic shares a single
`(cluster_id, source_ip)` counter. Operators in this situation
should either raise the cap, scope it to a per-cluster value tuned
for the expected NAT shape, or feed sōzu the real client IP via
PROXY-protocol v2 from an upstream LB so the cap operates per real
client.

### 7.4 Behind another L4 load balancer

If sōzu is downstream of another L4 LB (HAProxy, AWS NLB), enable
PROXY-protocol v2 on the upstream LB and configure sōzu's listener
to expect it (`expect_proxy = true`). The parsed source IP from the
PP-v2 header is what the cap uses — without PP-v2, sōzu sees the
upstream LB's IP as the source and rate-limits per-LB-IP rather than
per-real-client.

## 8. Cross-references

- [`doc/configure.md`](configure.md) — operator-facing TOML reference.
- [`doc/upgrade/1.x-to-2.0.md`](upgrade/1.x-to-2.0.md) — the
  `New TOML keys` section documents `max_connections_per_ip` for
  operators upgrading from 1.1.x.
- [`CHANGELOG.md`](../CHANGELOG.md) — the
  `Per-(cluster, source-IP) connection limit (#1193)` entry under
  `🌟 Added`.
- `lib/src/protocol/mux/router.rs::route_from_request` — H1+H2 enforcement point.
- `lib/src/tcp.rs` — TCP enforcement point.
- `e2e/src/tests/cluster_ip_limit_tests.rs` — end-to-end coverage.
