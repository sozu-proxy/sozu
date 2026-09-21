# UDP Load Balancing — Flow Workflow

Reference document for maintainers of `lib/src/protocol/udp/` (the pure sans-io
core) and `lib/src/udp.rs` (the I/O shell). Companion to
`lib/src/protocol/mux/LIFECYCLE.md` (H2), `lib/src/protocol/kawa_h1/LIFECYCLE.md`
(H1) and `lib/src/protocol/proxy_protocol/LIFECYCLE.md` (PROXY ingress).

Every claim is anchored to code. Where the prose names an item — a function, a
method, a struct, an enum — the anchor is that item plus its file, with no line
number, because a line number does not survive an edit above it. A `file.rs:LINE`
or `file.rs:LINE-LINE` anchor is kept only where the claim is about a specific
statement or branch inside an item; those were refreshed against `main` at
`0cb1e2a7` on 2026-09-20. Implements issue #1273; the design rationale lives in
the `tasks/udp-lb/MASTER-PLAN.md` referenced by the PR.

---

## 1. Why UDP Is Different

The H1/H2/TCP datapaths are **one-`accept()`-one-session**: a readable event on
a listener means "a new connection", the proxy calls `accept()` then
`create_session()`, and a per-session state machine owns one socket for its
lifetime.

UDP is **connectionless** and therefore **one-listener-many-flows**. A single
bound `mio::net::UdpSocket` (`UdpListener`, `udp.rs`) serves every client.
A readable event means "datagrams are waiting", **not** "a new connection"
(`udp.rs:11-13`). There is no accept loop and no `create_session()` call: a
`Protocol::UDP` listener falls through `Server::ready`'s generic arm into
`ProxySession::ready` (`udp.rs:25-27`, `Server::ready`'s generic session arm,
`lib/src/server.rs`). Because there is no
kernel-level connection, Sōzu reconstructs the notion of a "connection" itself —
a **virtual flow** keyed on the client source address. That flow table is, in
effect, a **userland UDP conntrack** living inside the single-threaded worker
(there is no dependency on `nf_conntrack`).

`UdpProxy` (`udp.rs`) does **not** implement `ProxyConfiguration` / `L7Proxy`
— those signatures are `TcpStream`-bound. The server calls its inherent `notify`
(`udp.rs`) directly (`Server::notify_proxys`, `lib/src/server.rs`).

---

## 2. Two-Level Sans-IO Split

The datapath mirrors the H2 `ConnectionH2` / `Context` split: a pure core that
holds protocol state and an impure shell that holds the syscalls.

| Half | Module | Owns |
|------|--------|------|
| **Pure core** | `lib/src/protocol/udp/` | flow table, admission, cap/shedding, flow-key extraction, LB *request*, timer *scheduling*, teardown state machine, PPv2 header build |
| **I/O shell** | `lib/src/udp.rs` (+ `lib/src/udp/health.rs`) | every syscall, buffer pool, per-flow connected upstream sockets, the actual `TIMER` wheel, the `BackendMap`, health checks, metrics, slab/token bookkeeping |

The core performs **no I/O**: no socket, no `Instant::now()`/`SystemTime`, no
`rand`, no `Arc<Mutex>` (`mod.rs:6-7`). Time is injected as `now: Instant` on
every time-dependent entry point; the hash seed is injected once at construction
(`UdpManager::new` / `UdpManager::with_extractor`, `manager.rs`). This purity is
exactly what the deterministic-simulation test in `sim/tests/udp_simulation.rs`
(the `sozu-sim` crate, moonpool-driven) exploits (see `doc/udp_simulation.md`).

The two halves talk over a narrow message contract in `mod.rs`:

- **`ManagerInput<'a>`** (`mod.rs`) — what the shell feeds in:
  `ClientDatagram { src, payload }` (`mod.rs`), `BackendDatagram { flow, payload }`
  (`mod.rs`, already tagged with the owning flow), `Config(ConfigEvent)`
  (`mod.rs`), `BackendResolved { flow, backend, addr }` (`mod.rs`).
  Inputs **borrow** the recv buffer.
- **`Output`** (`mod.rs`) — what the core emits, drained by the shell via
  `poll_output()` until `None` (`manager.rs`, `udp.rs` `drain_outputs`):
  `SelectBackend` (`mod.rs`), `OpenUpstream` (`mod.rs`), `SendToBackend`
  (`mod.rs`), `SendToClient` (`mod.rs`), `ArmTimer` (`mod.rs`),
  `Metric` (`mod.rs`), `CloseFlow` (`mod.rs`), `Drop` (`mod.rs`).

The **single owned copy** the design permits is the admission copy: the borrowed
recv buffer is materialised into an owned `Transmit::payload: Vec<u8>`
(`mod.rs`) only when a datagram is actually buffered or forwarded.

The two structs inside the core:

- **`UdpManager<E>`** (`manager.rs`) — the flow table (`HashMap<FlowKey, FlowId>`
  over a `slab::Slab<UdpFlow>`), admission, the `max_flows` cap, the pluggable
  `FlowKeyExtractor`, the LB request, and the single armed deadline.
- **`UdpFlow`** (`flow.rs`) — per-flow teardown counters, idle deadline,
  `timer_gen`, PPv2 bookkeeping, the real (pre-NAT) client address, and the
  chosen backend. It is the slab slot payload; the manager owns the slab.

---

## 3. The Flow Table = Userland Conntrack

A **flow** is a virtual client identified by a `FlowKey` (`mod.rs`):

```rust
pub struct FlowKey { pub src: SocketAddr }
```

`FlowKey::from_src` (`mod.rs`) keys on the **4-tuple** (source IP + port) when
`affinity_with_port` is set, or the **2-tuple** (source IP, port normalised to
`0`) otherwise — a per-cluster knob (`ClusterConfig::affinity_with_port`,
`mod.rs`). The extractor is the only seam (`FlowKeyExtractor` trait,
`manager.rs`); the in-scope impl is `SourceTupleExtractor` (`manager.rs`),
which also enforces "an empty datagram is not a valid flow trigger" (`manager.rs:50-53`).

**Two-tier selection** on a client datagram (`UdpManager::on_client_datagram`, `manager.rs`):

1. Oversize → `Drop(Truncated)` before any allocation (`manager.rs:248-252`).
2. No cluster configured → `Drop(NoBackend)` (`manager.rs:253-257`).
3. Extract the key; rejection → `Drop(Invalid)`, allocates nothing (`manager.rs:258-265`).
4. **Key already in the table** → reuse its flow & backend (`manager.rs:267-271`,
   `forward_on_existing_flow`). This is what makes affinity sticky: the same
   client always reaches the same backend for the life of the flow.
5. **New key**, draining or at cap → shed (`manager.rs:273-299`), allocate nothing.
6. Otherwise **admit**: one slab slot + one payload copy, parked `AwaitingBackend`
   with the first datagram buffered (`manager.rs:315-325`), then emit
   `FlowCreated` + `SelectBackend` (`manager.rs:340-346`).

`FlowId` (`mod.rs`) is the slab index; it is stable for the flow's lifetime
and reused after close. The shell additionally keeps `upstream_token -> FlowId`
(`upstream_to_flow`) and `FlowId -> upstream_token` (`flow_to_upstream`), both
`UdpListenerSession` fields in `udp.rs`, so a backend datagram can be tagged with its owning flow.

---

## 4. Flow Lifecycle State Machine

Three phases (`FlowPhase`, `flow.rs`): `AwaitingBackend → Established → Closing`.
The transition is strictly forward; `UdpFlow::set_phase` (`flow.rs`) `debug_assert!`s
the legal edges (only `→ Closing` may be reached from either live phase — a flow
can be aborted before it establishes).

```
   client datagram, key unknown, room under cap
                    │  admit: slab slot + buffer 1st datagram
                    ▼
        ┌───────────────────────┐   FlowCreated + SelectBackend  (manager.rs:340-346)
        │     AwaitingBackend     │   - extra client dgrams: newest-wins buffer,
        │  (FlowPhase, flow.rs)   │     idle refresh only, NOT counted as request
        └───────────┬───────────┘     (forward_on_existing_flow, manager.rs)
                    │  BackendResolved (on_backend_resolved, manager.rs)
                    │  → OpenUpstream + flush buffered dgram (counted now)
                    ▼
        ┌───────────────────────┐   client dgram  → SendToBackend (+PPv2 1st)
        │      Established         │   backend dgram → SendToClient (NAT return)
        │ (FlowPhase, flow.rs)    │   each touch() refreshes idle + bumps timer_gen
        └───────────┬───────────┘
                    │  teardown trigger:
                    │   • idle deadline elapsed   (UdpManager::handle_timeout, manager.rs)
                    │   • responses cap reached    (DNS = 1)
                    │   • requests cap reached
                    │   • Drain / listener remove / soft-stop / abort
                    ▼
        ┌───────────────────────┐   FlowEvicted + CloseFlow  (UdpManager::close_flow, manager.rs)
        │       Closing           │   shell: close upstream socket, free slot,
        │  (FlowPhase, flow.rs)   │          udp.active_flows -= 1  (on_close_flow, udp.rs)
        └───────────────────────┘   racing client dgram here → Drop(Shed)
```

Key subtlety — **`requests` counts real forwards, not buffered datagrams**. A
datagram that arrives while `AwaitingBackend` is buffered (one slot, newest
wins, `manager.rs:359-367`) and only counted toward `requests` when it is
actually flushed in `on_backend_resolved` (`manager.rs:426-444`,
`UdpFlow::on_client_datagram`, `flow.rs`). Otherwise a burst during the await window
could trip the `requests` cap having delivered fewer than `requests` datagrams
(`flow.rs:136-140`).

---

## 5. End-to-End Datapath

### Forward (client → backend)

```
client ──dgram──▶ front UDP socket ──▶ ingest_client (udp.rs)
                                         │  recv_from into recv_buf
                                         ▼
                       UdpManager::handle_input(ClientDatagram)  (manager.rs)
                                         │  admit / reuse / drop
                                         ▼   Output::SelectBackend (new flow)
                       drain_outputs (udp.rs) → on_select_backend (udp.rs)
                                         │  BackendMap::next_available_backend_with_key
                                         ▼   ManagerInput::BackendResolved (on_backend_resolved, manager.rs)
                       Output::OpenUpstream  → on_open_upstream (udp.rs)
                                         │  udp_connect(backend)  (socket.rs)
                                         │  register upstream_token → flow
                                         ▼
                       Output::SendToBackend → on_send_to_backend (udp.rs)
                                         │  (PPv2 prefix already applied in core)
                                         ▼
                              connected upstream socket ──dgram──▶ backend
```

### Return (backend → client) — the symmetric NAT path

```
backend ──dgram──▶ connected upstream socket ──▶ ingest_upstream (udp.rs)
                          │  upstream_token → FlowId  (ingest_upstream, udp.rs)
                          ▼
              UdpManager::handle_input(BackendDatagram { flow })  (on_backend_datagram, manager.rs)
                          │  on_backend_datagram: count response, refresh idle
                          ▼   Output::SendToClient (manager.rs:480)
              on_send_to_client (udp.rs) ──dgram──▶ front socket ──▶ real client
```

---

## 6. Symmetric NAT Return — the Connected Socket Is the Demux Key

This is the load-bearing trick and the answer to "how do you track UDP return
traffic without kernel conntrack".

Each flow gets its **own connected upstream socket** opened by `udp_connect`
(`socket.rs`). The socket is bound on an ephemeral local port and
`connect()`-ed to the backend address (the `bind` / `connect` pair in
`udp_connect`). A connected DGRAM socket "only receives from the connected
address" (`connect(2)`), so its fd **is** the return-4-tuple demux key (the
`udp_connect` doc comment states the same invariant): the kernel delivers the
backend's reply onto exactly that socket. The shell registers it with mio
under a fresh `upstream_token` and records `upstream_token -> FlowId`
(`on_open_upstream`, `udp.rs`). When the socket becomes readable, `ingest_upstream`
(`udp.rs`) resolves the owning flow from the token and re-emits to the real
client via the front socket — restoring the pre-NAT client address that
`UdpFlow::client` (`flow.rs`) preserved at admission.

No shared state, no kernel conntrack entry, no source-port rewriting bookkeeping:
one connected fd per flow does it all. The trade-off is one fd per active flow,
which is exactly why `max_flows` exists (§9).

---

## 7. Teardown: Three Knobs + the Idle Timer Wheel

A flow is reaped on the **first** of these (`CloseReason`, `flow.rs`):

| Knob | Config | Semantics | Check |
|------|--------|-----------|-------|
| **idle** | `front_timeout` / `back_timeout` (default 30 s, `mod.rs:233-234`) | no datagram in that direction within the window | `UdpManager::handle_timeout` (`manager.rs`) |
| **responses** | `responses` (`0` = unlimited) | close after N backend replies — **DNS uses 1** | `UdpFlow::responses_exhausted` (`flow.rs`) |
| **requests** | `requests` (`0` = unlimited) | close after N client forwards | `UdpFlow::requests_exhausted` (`flow.rs`) |
| drain / admin | — | listener drain, remove, soft/hard-stop, abort | `UdpManager::close_all` (`manager.rs`), `UdpManager::abort_flow` (`manager.rs`) |

`responses`/`requests` are checked synchronously at each forward/return via
`UdpFlow::teardown_reason` (`flow.rs`), whose `debug_assert` proves the boundary
(a reason is returned **iff** a cap is truly exhausted).

**Idle is a single armed deadline + generation tokens, not a per-flow timer.**
The manager only ever asks the shell to arm **one** deadline (`armed_deadline`,
`ArmTimer`, `UdpManager::reschedule`, `manager.rs`); the shell owns the actual `TIMER`
wheel (`arm_timer`, `udp.rs`; `server::TIMER`). Each flow carries a
`timer_gen` token (`flow.rs`) bumped on every `UdpFlow::touch` (`flow.rs`).
A wheel expiry only closes a flow whose deadline is still `<= now`
(`UdpManager::handle_timeout`, `manager.rs:546-551`); a flow that saw traffic has been
rescheduled, so it survives the expiry. A debug **strict-advance guard**
(at the end of `UdpManager::handle_timeout`) asserts the next armed deadline is strictly `> now` after
a firing — this is the canonical sans-io busy-loop defence and the real reason
the generation tokens exist. (`prop_generation_token_defeats_stale_close`,
`manager.rs`, fuzzes this.)

**Consume-then-reschedule: an expiry that closes nothing is NOT a no-op.**
`crate::timer` rounds a requested delay to the *nearest* tick
(`duration_to_tick` in `timer.rs`), not up: with the 100 ms default tick an
entry whose deadline lies in `[100N-50, 100N+50)` is delivered at tick `N`, so
the shell can be woken as much as **50 ms early** when the poll lands on the
tick grid — which is what `Timer::next_poll_date` schedules, and what
`test_timeout_fires_up_to_half_a_tick_early` (`timer.rs`) measures. `Timer::poll`
recomputes `current_tick` from the real clock, so a poll anywhere in
`[100N-50, 100N)` already sees tick `N`: the bound a consumer must tolerate is
`(delay_ms + 50) mod 100`, i.e. up to **99 ms** — a full tick minus a
millisecond. Either figure is enough for what follows; the hazard is any
earliness at all. `Timer::poll` then *removes* that entry from its slab. So on every expiry, whether or not a flow
was due:

- the manager clears `armed_deadline` (`manager.rs:535`) **before** any
  `reschedule`, so a recomputed deadline equal to the old one is still emitted
  as a fresh `ArmTimer` instead of being memoized away — this is the
  load-bearing half;
- the shell drops its `timer_handle` (`UdpListenerSession::close`, `udp.rs`), which is hygiene rather
  than a fix: a delivered handle is already inert, because `set_timeout_at`
  clamps every new entry past `self.tick` while a delivered one sat at or below
  it, so `cancel_timeout`'s tick guard can never match the successor that reuses
  its slab slot
  (`test_a_delivered_timeout_handle_cannot_cancel_its_slot_successor`,
  `timer.rs`). Clearing it keeps the field's meaning local instead of resting on
  a two-hop argument about clamping and monotonicity in another module.

Skipping the first step is a **lost wakeup**: an early expiry finds nothing due,
`reschedule` sees an unchanged minimum and emits nothing, the wheel is empty,
and the flow is never reaped — it pins its `max_flows` slot, upstream socket,
slab slot and `udp.active_flows` count until some *other* flow's deadline
happens to move the minimum. The cost of the rule is one extra wheel wakeup per
early-fired expiry, which is the right trade: a wheel entry is cheap, a flow
that never dies is not.

An expiry that closes **several** flows at once now also emits an intermediate
`ArmTimer` per `close_flow` whose recomputed minimum differs from the last
(two flows due together plus a later third emit two where HEAD emitted one).
`drain_outputs` is synchronous and `arm_timer` cancels before it arms, so these
coalesce into wheel insert/cancel churn inside the one drain and never reach the
event loop as extra wakeups.
(`early_expiry_that_finds_nothing_due_still_rearms` and
`repeated_early_expiries_each_rearm`, `manager.rs`;
`an_early_wheel_fire_still_evicts_the_idle_flow`, `udp.rs`, drives the real
wheel end to end.)

Every close path emits `FlowEvicted` then `CloseFlow` (`UdpManager::close_flow`,
`manager.rs`); the shell's `on_close_flow` (`udp.rs`) closes the
upstream socket, drops the token maps, frees the slab slot and decrements
`udp.active_flows` **exactly once**. `abort_flow` / `close_all` reuse the same
path, so the gauge cannot leak on listener remove / deactivate / soft-stop
(`UdpProxy::remove_listener` / `give_back_listener` / `notify`'s stop arms, `udp.rs`,
all through `close_all_flows`). Idempotent:
a missing or already-`Closing` flow is a no-op — no double-evict, no underflow.

---

## 8. PROXY Protocol v2 (DGRAM)

Sōzu defines PPv2-over-UDP — **no reference proxy ships it**
(`proxy_protocol.rs:3`). The v2 spec carries a DGRAM encoding: the
version+command byte (offset 12) is `0x21` and the family+transport byte
(offset 13) is `0x12` for UDP-over-IPv4 / `0x22` for UDP-over-IPv6 (low nibble
`0x2` = DGRAM, vs `0x1` = STREAM used by the TCP serializer)
(`proxy_protocol.rs:4-7`, `26-37`).

`dgram_header(client, backend)` (`proxy_protocol.rs`) builds the header with
the **real (pre-NAT) client** as the PPv2 source and the backend as the
destination. `prepend_dgram_header` (`proxy_protocol.rs`) splices it in front
of the owned payload. Mixed-family (a v4/v6 mismatch the datapath never produces,
since the connected socket matches the backend family) falls back to an
`AF_UNSPEC` zero-length block per the spec (`proxy_protocol.rs:73-82`).

Policy is per-cluster: `send_proxy_protocol` gates it; `proxy_protocol_every_datagram`
chooses **first-datagram-only** (default) vs **every-datagram**. `UdpFlow::take_proxy_protocol`
(`flow.rs`) consumes the first-datagram bookkeeping (`first_upstream_pending`,
`flow.rs`) so the prefix is applied exactly once when first-only. The prefix
is applied in the **core**, so the shell writes `Transmit::payload` verbatim
(`mod.rs`); metric byte counts exclude the prefix (`MetricEvent::DatagramIn`
carries the payload length, `mod.rs`). Byte-exact tests:
`proxy_protocol.rs:108-174`.

---

## 9. Load Balancing

Selection is **requested** by the core and **performed** by the shell. On a new
flow the core emits `SelectBackend { flow, cluster, key }` (`mod.rs`), where
`key` is an affinity hash computed from the flow key (`affinity_hash`,
`manager.rs`). The shell's `on_select_backend` (`udp.rs`) consults
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
black-holing (`lib/src/udp/health.rs:19-22`).

---

## 10. Health Checks

UDP has no reliable liveness signal, so health is bound to the **endpoint**, not
the listener (`lib/src/udp/health.rs:1-26`, MASTER-PLAN §7):

- **Primary — companion TCP probe**: non-blocking mio TCP `connect` to a
  configurable `tcp_port` (default = the data port). Established ⇒ healthy;
  refused/timeout ⇒ unhealthy. Industry-standard (IPVS/Keepalived, HAProxy
  `check port`, Envoy, nginx). A *hint*, not proof of UDP reachability.
- **Secondary — app UDP probe**: send a payload, expect any reply within a
  timeout; silence ⇒ unhealthy.

Results feed `Backend::health` through rise/fall hysteresis
(`HealthState`), steering only **new** selections — a flow already pinned to a
now-unhealthy backend stays until idle-timeout (the flow table pins it). Probes
run **non-blocking in the event loop** (`UdpProxy::health_poll` `udp.rs`,
driven from `Server::run`'s health tick, `lib/src/server.rs`; `health_owns_token`/`health_ready` route readiness,
`udp.rs`, `Server::ready`'s UDP health arm, `lib/src/server.rs`). No background threads — consistent with the
single-threaded worker model.

---

## 11. Control Plane: Config, Hot-Reconfig, SCM, Upgrade

Worker requests reach UDP via `UdpProxy::notify` (`udp.rs`), routed from
`Server::notify_proxys` (`lib/src/server.rs`). The relevant edges:

- **Add / activate listener**: `UdpProxy::add_listener` (`udp.rs`) →
  `notify_add_udp_listener` (`lib/src/server.rs`); `UdpProxy::activate_listener` (`udp.rs`)
  then `UdpProxy::build_session` (`udp.rs`) — UDP replaces the accept/create-session
  step with a single long-lived `UdpListenerSession` per listener
  (`notify_activate_listener`'s UDP arm, `lib/src/server.rs`).
  That install happens once per *activation*, not once per request: `activate`
  short-circuits on the listener's own `active` flag and answers with the same
  token for a listener that is already up, and `ConfigState` accepts the repeat
  and replays one `ActivateListener` per active listener, so the server arm calls
  `build_session` only when `UdpProxy::has_listener_session` says none is
  installed. Rebuilding on a repeat would displace the live session while the
  proxy kept the shared `UdpManager` and its flow table, leaving every in-flight
  flow unforwardable and its upstream slab slot unreleasable.
- **Front add/remove**: `UdpProxy::add_udp_front` (`udp.rs`) / `UdpProxy::remove_udp_front`
  (`udp.rs`).
- **Cluster knobs**: `UdpProxy::apply_cluster` (`udp.rs`) → `apply_udp_knobs`
  (`udp.rs`) fold the `UdpClusterConfig` proto into the core `ClusterConfig`,
  delivered as `ConfigEvent::SetCluster` (`mod.rs`). A mid-flow reconfig
  applies to **new** flows only — existing flows keep the config captured at
  admission (`UdpFlow::config`, `flow.rs`; the
  `reconfig_midflow_preserves_existing_flow_contract` test, `manager.rs`), so a
  live flow's teardown contract and affinity are stable.
- **Listener update**: `UdpProxy::update_listener` (`udp.rs`) keeps three things in
  agreement — the listener config, the manager (`SetMaxFlows` / `SetMaxRxDatagramSize`),
  and the session's `recv_buf`, which is re-sized via `resize_recv_buf`
  (`udp.rs`, called at `UdpProxy::update_listener`, `udp.rs`) so `recv_from` cannot truncate after a
  size bump.
- **SCM fd hand-off across re-exec**: `give_back_listeners` / `give_back_listener`
  (`udp.rs`) return the raw `UdpSocket`; the hand-off is fd-type-agnostic
  (only `UdpSocket::from_raw_fd` on the receiving side is UDP-specific). The
  re-exec'd worker adopts the passed fd and rebuilds its session. `Server::new`
  (`lib/src/server.rs`) takes the SCM listeners **before** it applies the initial
  state, which is what makes the adoption happen: that state carries one
  `ActivateListener` per active listener, and until 2026-09-20 it ran first, so
  `UdpListener::activate` took the `udp_bind` branch — succeeding only because of
  `SO_REUSEPORT` — and the inherited descriptor, with everything already queued in
  its receive buffer, was dropped unused (sozu#1342). `Server::notify_activate_listener`
  additionally asks `UdpProxy::inherited_socket_fate` (`udp.rs`) before
  `Listeners::get_udp` (`command/src/scm_socket.rs`), so a descriptor leaves the SCM
  table only when a listener will actually take it. With no listener yet at the
  address it stays in the table for a later `AddListener` + `ActivateListener`.
  With one already active it is closed deliberately instead of by a dropped
  wrapper — a deliberate trade, not an impossibility: a deactivate plus
  reactivate would adopt it, but its queued datagrams decay while an unaccepted
  socket in the address's `SO_REUSEPORT` group is expected to go on taking a
  share of new datagrams for as long as it stays open. That expectation follows
  from `SO_REUSEPORT` load-balancing semantics; nothing here measures it.
- **Hot-upgrade resets flows.** Only the **listener fd** is handed off — flow
  state is **not** migrated (`UdpProxy::give_back_listener`'s `close_all_flows` on
  hand-off, `udp.rs`). In-flight flows reset on upgrade. This is a deliberate
  phase-1 limitation and the key difference from a kernel conntrack, which would
  survive. Adopting the inherited fd preserves only what the kernel holds on it,
  i.e. datagrams already queued in its receive buffer; it migrates no flow state
  and does not change this limitation.

---

## 12. Hardening Notes

The UDP datapath is a security-sensitive surface: UDP is an amplification / DoS
vector with no backpressure and a trivially spoofable source address. These
rules are load-bearing and follow the repo `CLAUDE.md` no-panic-on-network-input
model.

1. **Silence is a virtue — drop before you allocate.** Unknown / empty /
   oversized / no-cluster / over-cap datagrams are dropped + metered **before**
   any flow, buffer, or socket is allocated (`UdpManager::on_client_datagram`,
   `manager.rs:248-299`). Each early return pair-asserts `flows.len()` is
   unchanged (`manager.rs:279-298`). `DropReason` (`mod.rs`) carries the
   reason into `udp.datagrams.dropped`.
2. **Bounded flow table = bounded fds.** `max_flows` (`UdpManager::max_flows`, `manager.rs`;
   `effective_max_flows` `udp.rs`) defaults to ~70 % of the soft
   `RLIMIT_NOFILE`, **clamped against shared-slab headroom** so a UDP listener
   cannot starve HTTP/TCP. Beyond the cap, new flows are **shed** (`FlowShed`,
   `manager.rs:290-291`); existing flows are protected. This is the bounded
   analog of kernel conntrack-table exhaustion.
3. **Bounded rx.** `max_rx_datagram_size` is clamped to `buffer_size`
   (`clamp_max_rx`, `udp.rs`); the `recv_buf` is sized `max_rx + 1`
   (`UdpListenerSession::new`'s `recv_buf` sizing, `udp.rs`) so an over-size datagram is detected (read longer than
   `max_rx`) and dropped as `Truncated` rather than silently cut.
4. **Bounded egress write queue.** A stalled backend cannot balloon memory: the
   per-flow upstream write queue (`WriteQueue`, `udp.rs`) has a small cap;
   past it the datagram is dropped (`udp.datagrams.dropped.wq_full`,
   `udp.rs:121-123`) — UDP is best-effort, the client retries. Writable re-arm
   (`arm_upstream_writable` / `drain_upstream_queue`; client side
   `arm_client_writable` / `drain_client_queue`; all `udp.rs`) is the UDP analog
   of `signal_pending_write` on the edge-triggered loop.
5. **No busy-loop, no stale close, no lost wakeup.** Generation tokens + the
   strict-advance guard (§7, in `UdpManager::handle_timeout`) guarantee the timer always
   advances past a firing; consume-then-reschedule (§7) guarantees an expiry
   that closed nothing still re-arms, so an early wheel fire cannot strand a
   flow that nothing will ever reap.
6. **Gauge correctness on every path.** `udp.active_flows` is balanced on every
   close — idle, responses/requests reached, drain, soft/hard-stop, listener
   remove/deactivate, and upstream-open failure (`UdpManager::abort_flow`, `manager.rs`;
   `on_close_flow`, `udp.rs`). Gauge underflow is a correctness bug, not a
   rounding issue (repo `CLAUDE.md`).
7. **`EMFILE`/`ENFILE` → shed, never panic.** `udp_connect` errors bubble up
   (`socket.rs`); the shell aborts the just-admitted flow
   (`on_open_upstream` failure path, `udp.rs`) rather than panicking,
   freeing the slab slot it would otherwise pin for the idle timeout.
8. **Debug invariants everywhere.** `UdpManager::check_invariants` (`manager.rs`) runs
   as a post-condition after every public mutating method
   (`handle_input`, `manager.rs:190`); the deterministic simulator
   (`sim/tests/udp_simulation.rs`, the moonpool-driven `sozu-sim` crate) and the
   property tests (`prop_flow_invariants`, `manager.rs`) drive the core hard
   enough to trip any regression.

---

## 13. Cross-References

- `lib/src/protocol/udp/mod.rs` — the `ManagerInput`/`Output` contract, `FlowKey`,
  `ClusterConfig`, `MetricEvent`, `DropReason`.
- `lib/src/protocol/udp/{manager,flow}.rs` — the pure core.
- `lib/src/protocol/udp/proxy_protocol.rs` — the PPv2 DGRAM header builder.
- `lib/src/udp.rs` (+ `lib/src/udp/health.rs`) — the I/O shell, event-loop
  wiring, health checks.
- `lib/src/socket.rs` — `udp_bind` / `udp_connect`.
- `lib/src/server.rs` — generic-readiness integration, SCM activate, health drive.
- `doc/udp_simulation.md` — the FoundationDB/VOPR-style deterministic simulator
  for the core.
- `doc/configure.md` — operator-facing UDP listener / cluster / health config and
  the `udp.*` metrics.
- `doc/architecture.md` §"UDP" — the one-listener-many-flows summary.
- `lib/src/protocol/mux/LIFECYCLE.md` — the H2 `Connection`/`Context` split this
  two-level design mirrors.
