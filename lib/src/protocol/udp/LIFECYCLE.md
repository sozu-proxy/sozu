# UDP Load Balancing — Flow Workflow

Reference document for maintainers of `lib/src/protocol/udp/` (the pure sans-io
core) and `lib/src/udp.rs` (the I/O shell). Companion to
`lib/src/protocol/mux/LIFECYCLE.md` (H2), `lib/src/protocol/kawa_h1/LIFECYCLE.md`
(H1) and `lib/src/protocol/proxy_protocol/LIFECYCLE.md` (PROXY ingress).

Every claim is anchored to a concrete `file.rs:LINE`; line numbers were last
refreshed against the `feat/udp-load-balancing` branch tip (`259f60a2`) on
2026-06-04. Implements issue #1273; the design rationale lives in the
`tasks/udp-lb/MASTER-PLAN.md` referenced by the PR.

---

## 1. Why UDP Is Different

The H1/H2/TCP datapaths are **one-`accept()`-one-session**: a readable event on
a listener means "a new connection", the proxy calls `accept()` then
`create_session()`, and a per-session state machine owns one socket for its
lifetime.

UDP is **connectionless** and therefore **one-listener-many-flows**. A single
bound `mio::net::UdpSocket` (`udp.rs:213`, `UdpListener`) serves every client.
A readable event means "datagrams are waiting", **not** "a new connection"
(`udp.rs:11-13`). There is no accept loop and no `create_session()` call: a
`Protocol::UDP` listener falls through `Server::ready`'s generic arm into
`ProxySession::ready` (`udp.rs:24-27`, `server.rs:2266`). Because there is no
kernel-level connection, Sōzu reconstructs the notion of a "connection" itself —
a **virtual flow** keyed on the client source address. That flow table is, in
effect, a **userland UDP conntrack** living inside the single-threaded worker
(there is no dependency on `nf_conntrack`).

`UdpProxy` (`udp.rs:318`) does **not** implement `ProxyConfiguration` / `L7Proxy`
— those signatures are `TcpStream`-bound. The server calls its inherent `notify`
(`udp.rs:790`) directly (`server.rs:1876`).

---

## 2. Two-Level Sans-IO Split

The datapath mirrors the H2 `ConnectionH2` / `Context` split: a pure core that
holds protocol state and an impure shell that holds the syscalls.

| Half | Module | Owns |
|------|--------|------|
| **Pure core** | `lib/src/protocol/udp/` | flow table, admission, cap/shedding, flow-key extraction, LB *request*, timer *scheduling*, teardown state machine, PPv2 header build |
| **I/O shell** | `lib/src/udp.rs` (+ `udp/health.rs`) | every syscall, buffer pool, per-flow connected upstream sockets, the actual `TIMER` wheel, the `BackendMap`, health checks, metrics, slab/token bookkeeping |

The core performs **no I/O**: no socket, no `Instant::now()`/`SystemTime`, no
`rand`, no `Arc<Mutex>` (`mod.rs:5-10`). Time is injected as `now: Instant` on
every time-dependent entry point; the hash seed is injected once at construction
(`manager.rs:98`/`116`). This purity is exactly what the deterministic-simulation
test in `lib/tests/udp_simulation.rs` exploits (see `doc/udp_simulation.md`).

The two halves talk over a narrow message contract in `mod.rs`:

- **`ManagerInput<'a>`** (`mod.rs:89`) — what the shell feeds in:
  `ClientDatagram { src, payload }` (`mod.rs:91`), `BackendDatagram { flow, payload }`
  (`mod.rs:99`, already tagged with the owning flow), `Config(ConfigEvent)`
  (`mod.rs:107`), `BackendResolved { flow, backend, addr }` (`mod.rs:110`).
  Inputs **borrow** the recv buffer.
- **`Output`** (`mod.rs:123`) — what the core emits, drained by the shell via
  `poll_output()` until `None` (`manager.rs:572`, `udp.rs:1198` `drain_outputs`):
  `SelectBackend` (`mod.rs:126`), `OpenUpstream` (`mod.rs:136`), `SendToBackend`
  (`mod.rs:143`), `SendToClient` (`mod.rs:145`), `ArmTimer` (`mod.rs:148`),
  `Metric` (`mod.rs:150`), `CloseFlow` (`mod.rs:154`), `Drop` (`mod.rs:157`).

The **single owned copy** the design permits is the admission copy: the borrowed
recv buffer is materialised into an owned `Transmit::payload: Vec<u8>`
(`mod.rs:71-84`) only when a datagram is actually buffered or forwarded.

The two structs inside the core:

- **`UdpManager<E>`** (`manager.rs:60`) — the flow table (`HashMap<FlowKey, FlowId>`
  over a `slab::Slab<UdpFlow>`), admission, the `max_flows` cap, the pluggable
  `FlowKeyExtractor`, the LB request, and the single armed deadline.
- **`UdpFlow`** (`flow.rs:53`) — per-flow teardown counters, idle deadline,
  `timer_gen`, PPv2 bookkeeping, the real (pre-NAT) client address, and the
  chosen backend. It is the slab slot payload; the manager owns the slab.

---

## 3. The Flow Table = Userland Conntrack

A **flow** is a virtual client identified by a `FlowKey` (`mod.rs:50`):

```rust
pub struct FlowKey { pub src: SocketAddr }
```

`FlowKey::from_src` (`mod.rs:60`) keys on the **4-tuple** (source IP + port) when
`affinity_with_port` is set, or the **2-tuple** (source IP, port normalised to
`0`) otherwise — a per-cluster knob (`ClusterConfig::affinity_with_port`,
`mod.rs:208`). The extractor is the only seam (`FlowKeyExtractor` trait,
`manager.rs:36`); the in-scope impl is `SourceTupleExtractor` (`manager.rs:46`),
which also enforces "an empty datagram is not a valid flow trigger" (`manager.rs:50-53`).

**Two-tier selection** on a client datagram (`on_client_datagram`, `manager.rs:245`):

1. Oversize → `Drop(Truncated)` before any allocation (`manager.rs:247-250`).
2. No cluster configured → `Drop(NoBackend)` (`manager.rs:252-255`).
3. Extract the key; rejection → `Drop(Invalid)`, allocates nothing (`manager.rs:257-263`).
4. **Key already in the table** → reuse its flow & backend (`manager.rs:266-269`,
   `forward_on_existing_flow`). This is what makes affinity sticky: the same
   client always reaches the same backend for the life of the flow.
5. **New key**, draining or at cap → shed (`manager.rs:284-303`), allocate nothing.
6. Otherwise **admit**: one slab slot + one payload copy, parked `AwaitingBackend`
   with the first datagram buffered (`manager.rs:319-333`), then emit
   `FlowCreated` + `SelectBackend` (`manager.rs:339-340`).

`FlowId` (`mod.rs:39`) is the slab index; it is stable for the flow's lifetime
and reused after close. The shell additionally keeps `upstream_token -> FlowId`
(`udp.rs:1015`) and `FlowId -> upstream_token` (`udp.rs:1017`) maps so a backend
datagram can be tagged with its owning flow.

---

## 4. Flow Lifecycle State Machine

Three phases (`FlowPhase`, `flow.rs:40`): `AwaitingBackend → Established → Closing`.
The transition is strictly forward; `set_phase` (`flow.rs:197`) `debug_assert!`s
the legal edges (only `→ Closing` may be reached from either live phase — a flow
can be aborted before it establishes).

```
   client datagram, key unknown, room under cap
                    │  admit: slab slot + buffer 1st datagram
                    ▼
        ┌───────────────────────┐   FlowCreated + SelectBackend  (manager.rs:339)
        │     AwaitingBackend     │   - extra client dgrams: newest-wins buffer,
        │  (mod.rs:42, flow.rs:42)│     idle refresh only, NOT counted as request
        └───────────┬───────────┘     (forward_on_existing_flow, manager.rs:354)
                    │  BackendResolved (manager.rs:397)
                    │  → OpenUpstream + flush buffered dgram (counted now)
                    ▼
        ┌───────────────────────┐   client dgram  → SendToBackend (+PPv2 1st)
        │      Established         │   backend dgram → SendToClient (NAT return)
        │ (mod.rs:43, flow.rs:45) │   each touch() refreshes idle + bumps timer_gen
        └───────────┬───────────┘
                    │  teardown trigger:
                    │   • idle deadline elapsed   (handle_timeout, manager.rs:514)
                    │   • responses cap reached    (DNS = 1)
                    │   • requests cap reached
                    │   • Drain / listener remove / soft-stop / abort
                    ▼
        ┌───────────────────────┐   FlowEvicted + CloseFlow  (close_flow, manager.rs:598)
        │       Closing           │   shell: close upstream socket, free slot,
        │  (mod.rs:44, flow.rs:48)│          udp.active_flows -= 1  (on_close_flow, udp.rs:1588)
        └───────────────────────┘   racing client dgram here → Drop(Shed)
```

Key subtlety — **`requests` counts real forwards, not buffered datagrams**. A
datagram that arrives while `AwaitingBackend` is buffered (one slot, newest
wins, `manager.rs:355-360`) and only counted toward `requests` when it is
actually flushed in `on_backend_resolved` (`manager.rs:420-438`,
`flow.rs:141` `on_client_datagram`). Otherwise a burst during the await window
could trip the `requests` cap having delivered fewer than `requests` datagrams
(`flow.rs:146-152`).

---

## 5. End-to-End Datapath

### Forward (client → backend)

```
client ──dgram──▶ front UDP socket ──▶ ingest_client (udp.rs:1116)
                                         │  recv_from into recv_buf
                                         ▼
                       UdpManager::handle_input(ClientDatagram)  (manager.rs:171)
                                         │  admit / reuse / drop
                                         ▼   Output::SelectBackend (new flow)
                       drain_outputs (udp.rs:1198) → on_select_backend (udp.rs:1218)
                                         │  BackendMap::next_available_backend_with_key
                                         ▼   ManagerInput::BackendResolved (manager.rs:397)
                       Output::OpenUpstream  → on_open_upstream (udp.rs:1257)
                                         │  udp_connect(backend)  (socket.rs:1083)
                                         │  register upstream_token → flow
                                         ▼
                       Output::SendToBackend → on_send_to_backend (udp.rs:1349)
                                         │  (PPv2 prefix already applied in core)
                                         ▼
                              connected upstream socket ──dgram──▶ backend
```

### Return (backend → client) — the symmetric NAT path

```
backend ──dgram──▶ connected upstream socket ──▶ ingest_upstream (udp.rs:1163)
                          │  upstream_token → FlowId  (udp.rs:1164)
                          ▼
              UdpManager::handle_input(BackendDatagram { flow })  (manager.rs:460)
                          │  on_backend_datagram: count response, refresh idle
                          ▼   Output::SendToClient (manager.rs:478)
              on_send_to_client (udp.rs:1466) ──dgram──▶ front socket ──▶ real client
```

---

## 6. Symmetric NAT Return — the Connected Socket Is the Demux Key

This is the load-bearing trick and the answer to "how do you track UDP return
traffic without kernel conntrack".

Each flow gets its **own connected upstream socket** opened by `udp_connect`
(`socket.rs:1083`). The socket is bound on an ephemeral local port and
`connect()`-ed to the backend address (`socket.rs:1099-1101`). A connected
DGRAM socket "only receives from the connected address" (`connect(2)`), so its
fd **is** the return-4-tuple demux key (`socket.rs:1076-1080`): the kernel
delivers the backend's reply onto exactly that socket. The shell registers it
with mio under a fresh `upstream_token` and records `upstream_token -> FlowId`
(`udp.rs:1314-1316`). When the socket becomes readable, `ingest_upstream`
(`udp.rs:1163`) resolves the owning flow from the token and re-emits to the real
client via the front socket — restoring the pre-NAT client address that
`UdpFlow::client` (`flow.rs:55`) preserved at admission.

No shared state, no kernel conntrack entry, no source-port rewriting bookkeeping:
one connected fd per flow does it all. The trade-off is one fd per active flow,
which is exactly why `max_flows` exists (§9).

---

## 7. Teardown: Three Knobs + the Idle Timer Wheel

A flow is reaped on the **first** of these (`CloseReason`, `flow.rs:19`):

| Knob | Config | Semantics | Check |
|------|--------|-----------|-------|
| **idle** | `front_timeout` / `back_timeout` (default 30 s, `mod.rs:230-231`) | no datagram in that direction within the window | `handle_timeout` (`manager.rs:514`) |
| **responses** | `responses` (`0` = unlimited) | close after N backend replies — **DNS uses 1** | `responses_exhausted` (`flow.rs:226`) |
| **requests** | `requests` (`0` = unlimited) | close after N client forwards | `requests_exhausted` (`flow.rs:220`) |
| drain / admin | — | listener drain, remove, soft/hard-stop, abort | `close_all` (`manager.rs:221`), `abort_flow` (`manager.rs:205`) |

`responses`/`requests` are checked synchronously at each forward/return via
`teardown_reason` (`flow.rs:232`), whose `debug_assert` proves the boundary
(a reason is returned **iff** a cap is truly exhausted).

**Idle is a single armed deadline + generation tokens, not a per-flow timer.**
The manager only ever asks the shell to arm **one** deadline (`armed_deadline`,
`ArmTimer`, `reschedule` at `manager.rs:580`); the shell owns the actual `TIMER`
wheel (`udp.rs:1574` `arm_timer`, `server::TIMER`). Each flow carries a
`timer_gen` token (`flow.rs:84`) bumped on every `touch()` (`flow.rs:114-138`).
A wheel expiry only closes a flow whose deadline is still `<= now`
(`handle_timeout`, `manager.rs:518-530`); a flow that saw traffic has been
rescheduled, so the stale expiry is a no-op. A debug **strict-advance guard**
(`manager.rs:540-548`) asserts the next armed deadline is strictly `> now` after
a firing — this is the canonical sans-io busy-loop defence and the real reason
the generation tokens exist. (`prop_generation_token_defeats_stale_close`,
`manager.rs:1686`, fuzzes this.)

Every close path emits `FlowEvicted` then `CloseFlow` (`close_flow`,
`manager.rs:598`); the shell's `on_close_flow` (`udp.rs:1588`) closes the
upstream socket, drops the token maps, frees the slab slot and decrements
`udp.active_flows` **exactly once**. `abort_flow` / `close_all` reuse the same
path, so the gauge cannot leak on listener remove / deactivate / soft-stop
(`udp.rs:476`, `udp.rs:848`/`867`, `close_all_flows` `udp.rs:1688`). Idempotent:
a missing or already-`Closing` flow is a no-op — no double-evict, no underflow.

---

## 8. PROXY Protocol v2 (DGRAM)

Sōzu defines PPv2-over-UDP — **no reference proxy ships it**
(`proxy_protocol.rs:3`). The v2 spec carries a DGRAM encoding: the
version+command byte (offset 12) is `0x21` and the family+transport byte
(offset 13) is `0x12` for UDP-over-IPv4 / `0x22` for UDP-over-IPv6 (low nibble
`0x2` = DGRAM, vs `0x1` = STREAM used by the TCP serializer)
(`proxy_protocol.rs:6-7`, `26-37`).

`dgram_header(client, backend)` (`proxy_protocol.rs:47`) builds the header with
the **real (pre-NAT) client** as the PPv2 source and the backend as the
destination. `prepend_dgram_header` (`proxy_protocol.rs:87`) splices it in front
of the owned payload. Mixed-family (a v4/v6 mismatch the datapath never produces,
since the connected socket matches the backend family) falls back to an
`AF_UNSPEC` zero-length block per the spec (`proxy_protocol.rs:73-82`).

Policy is per-cluster: `send_proxy_protocol` gates it; `proxy_protocol_every_datagram`
chooses **first-datagram-only** (default) vs **every-datagram**. `take_proxy_protocol`
(`flow.rs:266`) consumes the first-datagram bookkeeping (`first_upstream_pending`,
`flow.rs:80`) so the prefix is applied exactly once when first-only. The prefix
is applied in the **core**, so the shell writes `Transmit::payload` verbatim
(`mod.rs:71-75`); metric byte counts exclude the prefix (`MetricEvent::DatagramIn`
carries the payload length, `mod.rs:188-191`). Byte-exact tests:
`proxy_protocol.rs:108-174`.

---

## 9. Load Balancing

Selection is **requested** by the core and **performed** by the shell. On a new
flow the core emits `SelectBackend { flow, cluster, key }` (`mod.rs:126`), where
`key` is an affinity hash computed from the flow key (`affinity_hash`,
`manager.rs:663`). The shell's `on_select_backend` (`udp.rs:1218`) consults
`BackendMap::next_available_backend_with_key`, then replies with `BackendResolved`.

A single `key: Option<u64>` threaded through the LB trait selects the algorithm
per cluster:

- **Round-robin** — `key = None`.
- **HRW / rendezvous (default)** — health-aware highest-random-weight; stable
  under backend-set churn.
- **Maglev (opt-in)** — lookup table rebuilt **only on backend-set change**,
  never per datagram (a per-packet rebuild was a DoS amplifier caught in review
  — see the PR description and `CHANGELOG.md`).

Health-aware selection skips unhealthy backends but **fails open**: if every
backend reads unhealthy, selection routes over the full set rather than
black-holing (`udp/health.rs:18-22`).

---

## 10. Health Checks

UDP has no reliable liveness signal, so health is bound to the **endpoint**, not
the listener (`udp/health.rs:1-28`, MASTER-PLAN §7):

- **Primary — companion TCP probe**: non-blocking mio TCP `connect` to a
  configurable `tcp_port` (default = the data port). Established ⇒ healthy;
  refused/timeout ⇒ unhealthy. Industry-standard (IPVS/Keepalived, HAProxy
  `check port`, Envoy, nginx). A *hint*, not proof of UDP reachability.
- **Secondary — app UDP probe**: send a payload, expect any reply within a
  timeout; silence ⇒ unhealthy.

Results feed `Backend::health` through rise/fall hysteresis
(`HealthState`), steering only **new** selections — a flow already pinned to a
now-unhealthy backend stays until idle-timeout (the flow table pins it). Probes
run **non-blocking in the event loop** (`UdpProxy::health_poll` `udp.rs:398`,
driven from `server.rs:981`; `health_owns_token`/`health_ready` route readiness,
`udp.rs:406`/`411`, `server.rs:955`). No background threads — consistent with the
single-threaded worker model.

---

## 11. Control Plane: Config, Hot-Reconfig, SCM, Upgrade

Worker requests reach UDP via `UdpProxy::notify` (`udp.rs:790`), routed from
`Server::notify_proxys` (`server.rs:1876`). The relevant edges:

- **Add / activate listener**: `add_listener` (`udp.rs:415`) →
  `notify_add_udp_listener` (`server.rs:2087`); `activate_listener` (`udp.rs:483`)
  then `build_session` (`udp.rs:502`) — UDP replaces the accept/create-session
  step with a single long-lived `UdpListenerSession` per listener (`server.rs:2266-2271`).
- **Front add/remove**: `add_udp_front` (`udp.rs:628`) / `remove_udp_front`
  (`udp.rs:662`).
- **Cluster knobs**: `apply_cluster` (`udp.rs:717`) → `apply_udp_knobs`
  (`udp.rs:911`) fold the `UdpClusterConfig` proto into the core `ClusterConfig`,
  delivered as `ConfigEvent::SetCluster` (`mod.rs:243`). A mid-flow reconfig
  applies to **new** flows only — existing flows keep the config captured at
  admission (`flow.rs:config`, `manager.rs:1218` test), so a live flow's teardown
  contract and affinity are stable.
- **Listener update**: `update_listener` (`udp.rs:561`) keeps three things in
  agreement — the listener config, the manager (`SetMaxFlows` / `SetMaxRxDatagramSize`),
  and the session's `recv_buf`, which is re-sized via `resize_recv_buf`
  (`udp.rs:1097`, called at `udp.rs:622`) so `recv_from` cannot truncate after a
  size bump.
- **SCM fd hand-off across re-exec**: `give_back_listeners` / `give_back_listener`
  (`udp.rs:519`/`533`) return the raw `UdpSocket`; the hand-off is fd-type-agnostic
  (only `UdpSocket::from_raw_fd` on the receiving side is UDP-specific). The
  re-exec'd worker re-binds via the passed fd and rebuilds its session.
- **Hot-upgrade resets flows.** Only the **listener fd** is handed off — flow
  state is **not** migrated (`udp.rs:553-556`, `close_all_flows` on hand-off).
  In-flight flows reset on upgrade. This is a deliberate phase-1 limitation and
  the key difference from a kernel conntrack, which would survive.

---

## 12. Hardening Notes

The UDP datapath is a security-sensitive surface: UDP is an amplification / DoS
vector with no backpressure and a trivially spoofable source address. These
rules are load-bearing and follow the repo `CLAUDE.md` no-panic-on-network-input
model.

1. **Silence is a virtue — drop before you allocate.** Unknown / empty /
   oversized / no-cluster / over-cap datagrams are dropped + metered **before**
   any flow, buffer, or socket is allocated (`on_client_datagram`,
   `manager.rs:245-303`). Each early return pair-asserts `flows.len()` is
   unchanged (`manager.rs:288-302`). `DropReason` (`mod.rs:161`) carries the
   reason into `udp.datagrams.dropped`.
2. **Bounded flow table = bounded fds.** `max_flows` (`manager.rs:60`,
   `effective_max_flows` `udp.rs:937`) defaults to ~70 % of the soft
   `RLIMIT_NOFILE`, **clamped against shared-slab headroom** so a UDP listener
   cannot starve HTTP/TCP. Beyond the cap, new flows are **shed** (`FlowShed`,
   `manager.rs:289`); existing flows are protected. This is the bounded
   analog of kernel conntrack-table exhaustion.
3. **Bounded rx.** `max_rx_datagram_size` is clamped to `buffer_size`
   (`clamp_max_rx`, `udp.rs:975`); the `recv_buf` is sized `max_rx + 1`
   (`udp.rs:1084`) so an over-size datagram is detected (read longer than
   `max_rx`) and dropped as `Truncated` rather than silently cut.
4. **Bounded egress write queue.** A stalled backend cannot balloon memory: the
   per-flow upstream write queue (`WriteQueue`, `udp.rs:149`) has a small cap;
   past it the datagram is dropped (`udp.datagrams.dropped.wq_full`,
   `udp.rs:115-120`) — UDP is best-effort, the client retries. Writable re-arm
   (`arm_upstream_writable` `udp.rs:1413`, `drain_upstream_queue` `udp.rs:1444`;
   client side `udp.rs:1506`/`1547`) is the UDP analog of `signal_pending_write`
   on the edge-triggered loop.
5. **No busy-loop, no stale close.** Generation tokens + the strict-advance
   guard (§7, `manager.rs:540-548`) guarantee the timer always advances past a
   firing.
6. **Gauge correctness on every path.** `udp.active_flows` is balanced on every
   close — idle, responses/requests reached, drain, soft/hard-stop, listener
   remove/deactivate, and upstream-open failure (`abort_flow`, `manager.rs:205`;
   `on_close_flow`, `udp.rs:1588`). Gauge underflow is a correctness bug, not a
   rounding issue (repo `CLAUDE.md`).
7. **`EMFILE`/`ENFILE` → shed, never panic.** `udp_connect` errors bubble up
   (`socket.rs:1081-1083`); the shell aborts the just-admitted flow
   (`on_open_upstream` failure path, `udp.rs:1300-1316`) rather than panicking,
   freeing the slab slot it would otherwise pin for the idle timeout.
8. **Debug invariants everywhere.** `check_invariants` (`manager.rs:685`) runs
   as a post-condition after every public mutating method
   (`handle_input`, `manager.rs:189`); the deterministic simulator
   (`lib/tests/udp_simulation.rs`) and the property tests (`prop_flow_invariants`
   `manager.rs:1632`) drive the core hard enough to trip any regression.

---

## 13. Cross-References

- `lib/src/protocol/udp/mod.rs` — the `ManagerInput`/`Output` contract, `FlowKey`,
  `ClusterConfig`, `MetricEvent`, `DropReason`.
- `lib/src/protocol/udp/{manager,flow}.rs` — the pure core.
- `lib/src/protocol/udp/proxy_protocol.rs` — the PPv2 DGRAM header builder.
- `lib/src/udp.rs` (+ `lib/src/udp/health.rs`) — the I/O shell, event-loop
  wiring, health checks.
- `lib/src/socket.rs` — `udp_bind` (`:1049`) / `udp_connect` (`:1083`).
- `lib/src/server.rs` — generic-readiness integration, SCM activate, health drive.
- `doc/udp_simulation.md` — the FoundationDB/VOPR-style deterministic simulator
  for the core.
- `doc/configure.md` — operator-facing UDP listener / cluster / health config and
  the `udp.*` metrics.
- `doc/architecture.md` §"UDP" — the one-listener-many-flows summary.
- `lib/src/protocol/mux/LIFECYCLE.md` — the H2 `Connection`/`Context` split this
  two-level design mirrors.
