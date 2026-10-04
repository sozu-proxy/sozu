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
All six use `repeat_until_error_or(10, ...)`, which is a **stability check, not
a retry**: it loops while the inner test keeps succeeding and returns `Fail` on
the first bad trial, so it requires ten *consecutive* clean runs. `n = 10`
therefore does not account for timing sensitivity — it multiplies the exposure
to it by ten. The value is left as it stands because changing it changes what
these tests assert; read `repeat_until_error_or`'s doc comment in
`e2e/src/tests/mod.rs` and issue #1410 before copying it into a new test.

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

## Main-process handoff tests

The `sozu` crate also has three Linux-only process tests for the command-Hub
handoff. They are ignored in the ordinary unit suite because they fork real
main and worker processes and bind local sockets; run them explicitly and
serially. A separate two-direction compatibility matrix needs frozen legacy
and replacement binaries, so it remains a manual gate rather than a CI test.

`upgrade_main_preserves_in_flight_worker_command_and_original_client_response`
starts a real proxy, holds an HTTP request behind a backend barrier, and starts
a worker upgrade through client A. Client B then upgrades the main process.
After the handoff, releasing the backend must produce the HTTP 200 response,
one terminal success on A's original command connection, and a successful
command from client C. This proves the replacement continued the existing
worker task and correlation instead of replaying it.

`rejected_candidate_keeps_old_hub_authoritative_and_reaps_child` atomically
replaces the test executable with a candidate that exits during the pre-commit
probe. The old main PID must remain authoritative, the held command and HTTP
request must complete once, a new command must succeed, the boot generation
must stay unchanged, no successful `main_upgraded` audit may appear, and the
candidate must be reaped with no descendant left behind.

`sigterm_during_prepare_aborts_upgrade_then_stops_the_old_main` holds a V2
replacement before it can send `PREPARED`, sends `SIGTERM` to the old main,
then releases the replacement. The transfer must abort before `COMMIT`, reap
the replacement, return a failure to the upgrade client, and let the old event
loop consume the preserved stop intention and exit cleanly. A signal aimed
only at the old PID after the final pre-commit observation is outside this
contract; service managers should signal the service control group.

`unwritable_pid_file_rolls_back_main_upgrade_and_keeps_serving` replaces the
pid file with a directory before a main upgrade. The replacement must fail
while opening the pid file, before `PREPARED`, so the upgrade client gets a
failure while the old main, its worker and the frontend keep serving. A
replacement that failed only after `COMMIT` would have stopped every process,
because the fenced old main exits and its workers follow their closed command
channels.

`dangling_pid_file_symlink_rolls_back_main_upgrade_and_keeps_serving` replaces
it with a dangling symlink into a missing directory: `O_CREAT` follows the link,
so the pre-`PREPARED` check tests the final target's parent and the upgrade must
roll back the same way. Create failures that cannot be detected without
creating a file (`ENOSPC`, quota, a security-module denial) still surface only
after `COMMIT`, where they are logged and the new main keeps running.

```bash
cargo test -p sozu --test upgrade_keeps_draining_worker_e2e --locked \
  upgrade_main_preserves_in_flight_worker_command_and_original_client_response \
  -- --ignored --exact --nocapture --test-threads=1

cargo test -p sozu --test main_upgrade_transfer_rejection_e2e --locked \
  rejected_candidate_keeps_old_hub_authoritative_and_reaps_child \
  -- --ignored --exact --nocapture --test-threads=1

cargo test -p sozu --test main_upgrade_transfer_rejection_e2e --locked \
  sigterm_during_prepare_aborts_upgrade_then_stops_the_old_main \
  -- --ignored --exact --nocapture --test-threads=1

cargo test -p sozu --test upgrade_pid_file_failure_e2e --locked \
  -- --ignored --nocapture --test-threads=1

SOZU_MATRIX_LEGACY=/path/to/legacy-sozu \
SOZU_MATRIX_OPTION3=/path/to/replacement-sozu \
cargo test -p sozu --test main_upgrade_compatibility_matrix_e2e --locked \
  -- --ignored --nocapture --test-threads=1
```

The compatibility matrix runs both protocol directions. Without both
variables each case prints a skip line and passes, so a plain `-- --ignored`
run of the whole crate is not stopped by it; CI never sets them. A replacement sender
must reject a legacy candidate before exposing the live Hub, keep its boot
generation and audit unchanged, and reap the candidate. A legacy sender cannot
provide the same transactional accounting, but it must stay authoritative and
continue its held and subsequent commands when the replacement refuses the
legacy handoff. The first deployment therefore requires the controlled restart
described above; the matrix does not turn a legacy sender into a lossless V2
sender.

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
| `lib/src/server.rs` | `Server::notify_activate_listener` — SCM FD activation |
| `command/src/state.rs` | `ConfigState::generate_requests` — state replay including listeners |
