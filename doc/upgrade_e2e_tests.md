# Worker Upgrade End-to-End Tests

## Context

Sozu supports hot worker upgrades: replacing a running worker with a new one
without dropping in-flight requests. The main process orchestrates this by
transferring listen sockets via SCM_RIGHTS and draining the old worker
gracefully.

Until now, this mechanism had **zero test coverage**. An upgrade test existed in
`e2e/src/tests/tests.rs` since September 2022 but was marked "not working" and
never promoted to a `#[test]`. The root cause was twofold:

1. `Worker::send_proxy_request()` never tracked configuration state — a
   commented-out `dispatch` call left `ConfigState` empty, so the new worker
   received no routing configuration during upgrade.

2. `Worker::upgrade()` passed the full state (with listeners marked active) to
   `start_new_worker`. The worker processes initial state **before** receiving
   SCM listeners, so `ActivateListener` fell back to `server_bind()` instead of
   using the passed file descriptors.

## How the upgrade works in the e2e framework

The e2e framework runs workers as **threads** (not processes), communicating
over `UnixStream` pairs for commands and `ScmSocket` for file descriptor
passing. The `Worker::upgrade()` method in `e2e/src/sozu/worker.rs` reproduces
the real upgrade flow:

```
1. Send ReturnListenSockets to old worker
2. Old worker deregisters listen sockets from its mio poll
3. Old worker sends listen socket FDs back via SCM_RIGHTS
4. Test thread receives the FDs
5. Soft-stop old worker (begins draining sessions)
6. Start new worker thread with:
   - Same ServerConfig
   - Listen socket FDs via SCM_RIGHTS
   - Config state (clusters, frontends, backends) — but listeners
     marked inactive to avoid premature activation
7. Send ActivateListener requests to new worker (now that it has
   received the SCM FDs)
8. New worker activates listeners and starts accepting connections
```

The old worker continues processing in-flight sessions until they complete,
then shuts down. The new worker accepts new connections immediately after
activation.

## Test scenarios

Six tests cover the upgrade mechanism, each validating a different aspect.
All use `repeat_until_error_or(10, ...)` to account for timing sensitivity.

### 1. `try_upgrade` — Original test (preserved)

The original test from 2022, now functional. Validates the basic upgrade flow
with one in-flight request.

```
Client          Old Worker       New Worker       Backend
  │                │                                │
  ├──GET /api─────►│                                │
  │                ├──connect────────────────────────►
  │                │◄─────────────response───────────┤
  │◄──200 OK───────┤                                │
  │                │                                │
  ├──GET /api─────►│               ┌────────┐       │
  │                ├──forward──────►│ in     │──────►│  request received
  │                │               │ flight │       │  by backend
  │          ┌─────┤               └────────┘       │
  │          │ UPGRADE                              │
  │          │  ReturnListenSockets                 │
  │          │  SoftStop old                        │
  │          │  Start new + activate                │
  │          └─────┤               ┌────────┐       │
  │                │               │  ready │       │
  │                │               └────────┘       │
  │                │                                ├──response
  │◄───200 OK──────┤ (old worker still forwards)    │
  │                │                                │
  ├──reconnect────►│               │                │
  ├──GET /api──────┼──────────────►│                │
  │                │               ├──connect──────►│
  │                │               │◄──response─────┤
  │◄──200 OK───────┼───────────────┤                │
  │                │               │                │
  │              stop            stop               │
```

**Validates**: basic in-flight preservation, new connections on new worker.

### 2. `try_upgrade_in_flight_request` — Thorough in-flight

A more structured version of the original test with explicit baseline
verification and clear separation between pre-upgrade, mid-upgrade, and
post-upgrade phases.

```
Client          Old Worker       New Worker       Backend
  │                │                                │
  │  ── baseline round-trip (verify worker works) ──│
  ├──GET /api─────►├──────────────────────────────►│
  │◄──200 OK───────┤◄─────────────────────────────┤
  │                │                                │
  │  ── send request, hold response ────────────────│
  ├──GET /api─────►├──forward──────────────────────►│  backend holds
  │                │                                │
  │          ┌─────┤                                │
  │          │ UPGRADE (200ms grace)                │
  │          └─────┤               ┌────────┐       │
  │                │               │  ready │       │
  │                │               └────────┘       │
  │                │                                │
  │                │                                ├──response (finally)
  │◄──200 OK───────┤  old worker drains in-flight   │
  │                │                                │
  │  ── verify new worker ──────────────────────────│
  ├──reconnect────►│               │                │
  ├──GET /api──────┼──────────────►├───────────────►│
  │◄──200 OK───────┼───────────────┤◄──────────────┤
  │                │               │                │
  │              stop            stop               │
```

**Validates**: in-flight request completes through draining old worker, new
worker serves fresh connections.

### 3. `try_upgrade_new_connections_after` — Post-upgrade routing

Tests that the new worker correctly routes traffic from multiple new clients
and that keep-alive connections remain stable after upgrade.

```
Client₁         Old Worker       New Worker       Backend
  │                │                                │
  │  ── baseline ───────────────────────────────────│
  ├──GET /api─────►├──────────────────────────────►│
  │◄──200 OK───────┤◄─────────────────────────────┤
  │                │                                │
  │          ┌─────┤                                │
  │          │ UPGRADE (no in-flight)               │
  │          └─────┤               ┌────────┐       │
  │                │               │  ready │       │
                                   └────────┘
Client₂ (new)                      │                │
  ├──connect──────────────────────►│                │
  ├──GET /api─────────────────────►├───────────────►│
  │◄──200 OK───────────────────────┤◄──────────────┤
  │                                │                │
  │  ── 3× keep-alive requests ────│                │
  ├──GET──────────────────────────►├───────────────►│
  │◄──200──────────────────────────┤◄──────────────┤  ×3
  │                                │                │
Client₃ (another new)             │                │
  ├──connect──────────────────────►│                │
  ├──GET /api─────────────────────►├───────────────►│
  │◄──200 OK───────────────────────┤◄──────────────┤
  │                                │                │
                                 stop               │
```

**Validates**: multiple new clients, keep-alive stability, routing consistency.

### 4. `try_upgrade_multiple_in_flight` — 3 concurrent in-flight

The most demanding sync test: three clients each have a request in-flight when
the upgrade triggers. All three must complete through the old worker.

```
Client₀         Old Worker       New Worker       Backend
Client₁            │                                │
Client₂            │                                │
  │                │                                │
  │  ── each client: baseline keep-alive ───────────│
  ├──GET──────────►├──────────────────────────────►│
  │◄──200──────────┤◄─────────────────────────────┤  ×3 clients
  │                │                                │
  │  ── all 3 send requests simultaneously ─────────│
  C₀──GET─────────►│                                │
  C₁──GET─────────►├──forward all 3────────────────►│  3 requests
  C₂──GET─────────►│                                │  held by backend
  │                │                                │
  │          ┌─────┤                                │
  │          │ UPGRADE (3 in-flight!)               │
  │          └─────┤               ┌────────┐       │
  │                │               │  ready │       │
  │                │               └────────┘       │
  │                │                                │
  │                │                                ├──3 responses
  C₀◄──200─────────┤                                │
  C₁◄──200─────────┤  old worker drains all 3       │
  C₂◄──200─────────┤                                │
  │                │                                │
  │  ── verify new worker ──────────────────────────│
  C₀──reconnect───►│               │                │
  C₀──GET──────────┼──────────────►├───────────────►│
  C₀◄──200─────────┼───────────────┤◄──────────────┤
  │                │               │                │
  │              stop            stop               │
```

**Validates**: concurrent session draining, no request loss under load.

### 5. `try_upgrade_keepalive_reconnect` — Idle connection behavior

Tests what happens to an idle keep-alive connection (no in-flight request) when
the worker upgrades. The old worker's soft-stop should close idle sessions.

```
Client          Old Worker       New Worker       Backend
  │                │                                │
  │  ── establish keep-alive ───────────────────────│
  ├──GET /api─────►├──────────────────────────────►│
  │◄──200 OK───────┤◄─────────────────────────────┤
  │                │                                │
  │  ── client is IDLE (no in-flight request) ──────│
  │    (keep-alive)│                                │
  │          ┌─────┤                                │
  │          │ UPGRADE                              │
  │          │  SoftStop → closes idle sessions     │
  │          └─────┤               ┌────────┐       │
  │                │               │  ready │       │
  │                │               └────────┘       │
  │                │                                │
  ├──GET /api─────►│  try old connection            │
  │  send ok?──────┤                                │
  │    │ yes: may get response or EOF               │
  │    │ no:  connection already closed              │
  │    └── both outcomes acceptable ────────────────│
  │                │                                │
  │  ── reconnect to new worker ────────────────────│
  ├──reconnect────►│               │                │
  ├──GET /api──────┼──────────────►├───────────────►│
  │◄──200 OK───────┼───────────────┤◄──────────────┤
  │                │               │                │
  │  ── 3× keep-alive on new worker ───────────────│
  ├──GET──────────►┼──────────────►├───────────────►│
  │◄──200──────────┼───────────────┤◄──────────────┤  ×3
  │                │               │                │
  │              stop            stop               │
```

**Validates**: idle session cleanup during drain, reconnect to new worker,
keep-alive stability on new worker.

### 6. `try_upgrade_async` — Concurrent clients with async backends

Uses auto-responding async backends (instead of manually-stepped sync backends)
with two load-balanced backends and three concurrent clients. This is the
closest to real-world traffic patterns.

```
Client₀                                    Backend₀ (auto-reply)
Client₁         Old Worker    New Worker    Backend₁ (auto-reply)
Client₂            │                          │
  │                │                          │
  │  ── 5 rounds: all 3 clients send ─────────│
  ├──GET──────────►├─────────────────────────►│
  │◄──200──────────┤◄────────────────────────┤  ×5 rounds
  │                │        (load-balanced)   │  ×3 clients
  │                │                          │
  │          ┌─────┤                          │
  │          │ UPGRADE (300ms grace)          │
  │          └─────┤          ┌────────┐      │
  │                │          │  ready │      │
  │                │          └────────┘      │
  │                │                          │
  │  ── all 3 reconnect to new worker ────────│
  ├──reconnect────►│          │               │
  │                │          │               │
  │  ── 5 rounds: all 3 clients send ─────────│
  ├──GET──────────►┼─────────►├──────────────►│
  │◄──200──────────┼──────────┤◄─────────────┤  ×5 rounds
  │                │          │               │  ×3 clients
  │                │          │               │
  │              stop       stop              │
  │                                           │
  │  ── assert: each client received ≥5 ──────│
  │  ── assert: backends handled requests ────│
```

**Validates**: load balancing across upgrade boundary, async request handling,
aggregate throughput (each client must receive at least 5 post-upgrade
responses).

## Coverage matrix

| Scenario | In-flight preserved | New connections | Keep-alive | Concurrency |
|----------|:---:|:---:|:---:|:---:|
| `try_upgrade` | 1 request | 1 client | — | — |
| `try_upgrade_in_flight_request` | 1 request | 1 client | — | — |
| `try_upgrade_new_connections_after` | — | 3 clients | 3 rounds | — |
| `try_upgrade_multiple_in_flight` | 3 requests | 1 client | — | 3 clients |
| `try_upgrade_keepalive_reconnect` | — | 1 client | idle + reconnect | — |
| `try_upgrade_async` | — | 3 clients | — | 3 clients × 2 backends |

## Running the tests

```bash
# All upgrade tests
cargo test -p sozu-e2e test_upgrade

# A specific test
cargo test -p sozu-e2e test_upgrade_multiple_in_flight -- --nocapture

# Full e2e suite
cargo test -p sozu-e2e
```

## Key files

| File | Role |
|------|------|
| `e2e/src/sozu/worker.rs` | `Worker::upgrade()` — e2e upgrade orchestration |
| `e2e/src/tests/tests.rs` | All `try_upgrade_*` test functions and `#[test]` wrappers |
| `e2e/src/tests/mod.rs` | `setup_sync_test`, `setup_async_test`, `repeat_until_error_or` |
| `e2e/src/mock/sync_backend.rs` | Manually-stepped backend for precise ordering |
| `e2e/src/mock/async_backend.rs` | Auto-responding backend for throughput tests |
| `e2e/src/mock/chunked_flush_h1_backend.rs` | Keep-alive H1 backend with TCP_NODELAY + per-chunk `flush()` + sleep for the H2 large-asset repro suite (C1/C2/C3) |
| `lib/src/server.rs:1178` | `notify_activate_listener` — SCM FD activation |
| `command/src/state.rs:534` | `generate_requests` — state replay including listeners |
