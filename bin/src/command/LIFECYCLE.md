# Sōzu Supervisor — Command-Server Lifecycle

Reference document for maintainers of the master / supervisor surface under
`bin/src/command/`. Companion to `lib/src/protocol/mux/LIFECYCLE.md` (the H2
mux that workers run inside their own event loop) and to
`bin/src/upgrade.rs` (the hot-upgrade re-exec orchestrator).

Every claim is anchored to code. Where the prose names an item — a function, a
method, a struct, an enum — the anchor is that item plus its file, with no line
number, because a line number does not survive an edit above it. A `file.rs:LINE`
or `file.rs:LINE-LINE` anchor is kept only where the claim is about a specific
statement or branch inside an item; those were refreshed against `main` at
`0cb1e2a7` on 2026-09-20.

---

## 1. Supervisor Lifecycle

### 1.1 Boot — `begin_main_process`

Entry point: `begin_main_process` (`bin/src/command/mod.rs`), called from `main`
(`bin/src/main.rs`) for the `start` sub-command. It:

1. Bumps process limits (`update_process_limits`,
   `bin/src/command/mod.rs`, one `#[cfg]` arm per target) — RLIMIT_NOFILE
   in particular must be high enough for the configured listener count plus
   per-worker per-session sockets.
2. Initialises logging (`logging::setup_logging`).
3. Constructs the `Server` (`bin/src/command/server.rs`) and binds the
   unix command socket (`UnixListener` per the configured `command_socket`
   path, then `chmod` to `command_socket_mode`, `0o600` by default).
4. Forks the configured number of workers via `fork_main_into_worker`
   (`bin/src/worker.rs`). Each worker inherits a `Channel` pair plus an
   `ScmSocket` for FD passing. The forked child is a copy of the whole
   supervisor, so it never returns into the caller: it `exec`s the worker
   binary or, when that fails, logs the error and ends with `_exit(1)`. The
   supervisor sees that worker's channel close, answers its in-flight requests
   with synthetic failures (§1.2), and, with `worker_automatic_restart` (the
   default), forks a replacement (`Server::workers_to_spawn`).

   A launch can also fail in the supervisor, before or after the fork: the
   state file, the socket pairs, `fork` itself, the configuration sent to the
   new worker, or the registration of its channel. It leaves no descriptor
   open in the supervisor. `ScmSocket` and `Listeners` carry raw descriptors
   and have no `Drop`, so `fork_main_into_worker` owns both through guards
   that close them on every error return (the listeners on success too, once
   the worker has its own copies), and `Server::register_worker` closes the
   SCM socket when the registration fails before a session exists. After a
   failed automatic launch the round ends and the next one waits
   `SPAWN_RETRY_INTERVAL` (one second, `Server::paced_workers_to_spawn`):
   the failure is resource exhaustion, so retrying at once only repeats it
   with a state file written and synced per attempt. During that delay the
   count of workers to spawn is zero, so `poll` is not given a zero timeout,
   and it blocks for at most the time left. A launch that succeeds ends the
   delay.
5. Enters the master event loop via `CommandHub::run`
   (`bin/src/command/server.rs`).

### 1.2 Main loop — `CommandHub::run`

The supervisor is a single-threaded mio event loop. Each tick:

- polls registered tokens (the command socket, every connected client, and
  every worker channel);
- accepts new clients on the command socket and registers them in
  `CommandHub.clients` keyed by mio `Token`;
- drains worker responses and routes them back to the originating client
  via `GatheringTask` callbacks (`server.rs` declares the trait,
  `requests.rs` defines the concrete tasks: `QueryClustersTask`,
  `LoadStaticConfigTask`, `WorkerTask`, `QueryMetricsTask`,
  `LoadStateTask`, `StatusTask`, `StopTask`, …);
- ticks per-task timeouts (`Timeout`, `server.rs`) so a wedged worker
  cannot block a client forever. `poll` blocks at most until the earliest
  outstanding task deadline (`CommandHub::next_poll_timeout`), so a task
  with a deadline whose worker stays silent is reaped at its own deadline,
  not at the latest deadline of any other pending task (sozu#1826). A task
  scattered with `Timeout::None` has no deadline: it ends only when its
  workers answer or their channels close. The close of any worker, whatever
  its run state (a `Stopping` one included, and a `Stopped` one: a fence
  (`Server::fence_worker`) kills the process and marks the session `Stopped`
  without failing the routes it still owes, so the close that follows is the
  only thing that can), answers its in-flight requests with synthetic
  failures (`CommandHub::on_worker_channel_closed`). A repeated close finds
  no route left, because a synthetic failure is a terminal answer and a
  `Stopped` worker is never sent another request. Each task turns them into the
  client answer it would give a real rejection: an old worker that closes
  before answering the `SoftStop` of an `upgrade --worker` fails that
  upgrade (§4).
- serves the audit-event subscribers (`sozu events subscribe`). Audit events
  are queued on `Server::pending_audit_events` and reach the subscribers only
  through `CommandHub::flush_pending_audit_events`, which the loop calls right
  after the task sweep (`CommandHub::finish_ready_tasks`) at the top of every
  iteration and after each handled client request. The sweep call is what
  delivers the completion line of a task (`WorkerTask`, `LoadStateTask`, and
  `STATE_LOADED`, which a successful `load_state` raises at no other time):
  `GatheringTask::on_finish` runs only from that sweep, outside any request
  handler, so on a control plane where no client sends anything the
  subscribers get the event in the iteration that finished the task, not at
  the next request. A worker's own `Event` answer is not queued there: it is
  forwarded to the subscribers as soon as `CommandHub::handle_worker_response`
  routes it;
- forgets the sessions of dead workers
  (`CommandHub::collect_unreferenced_restored_stopped_workers`, at the top of
  every iteration, after the task sweep). A `Stopped` session is removed, its
  command channel deregistered and both of its descriptors closed (the SCM
  socket is closed explicitly, since `ScmSocket` has no `Drop`), once no task
  retains its token and no live route names its worker, so a worker that died
  with answers still owed keeps its session until its close has failed them.
  A session this main forked is removed only after its own `SIGKILL` (or an
  `ESRCH`) proved the process unable to act (`WorkerSession::process_stop_proven`):
  one whose kill failed keeps its session, its descriptors and its bootstrap
  slot, since the process may still act. A task that may still stop a worker
  by id (`GatheringTask::retained_worker_ids`: the readiness task of a new
  worker, its protected-value bootstrap, the readiness phase of an
  `upgrade --worker`, and a protected health check change for every worker it
  names) keeps that worker's session while it lives, even with no route left,
  because `Server::fence_worker` proves a worker stopped from its session and
  would report one it killed itself as unprovable once the session is gone.
  The process of a removed session is waited for with a non-blocking
  `waitpid` (`Server::unreaped_workers`, `Server::reap_removed_workers`) on
  every sweep, and `poll` blocks at most 100 ms (`REAP_POLL_INTERVAL`) while
  one is still to be waited for, because the `SIGKILL` lands after the close
  and no event announces the zombie; without that wait every worker death
  left one zombie child of the main until it exited. A restored session
  (§4) is removed on the same terms except that its historical PID is never
  waited for, being no child of this main. `ListWorkers` therefore no longer
  reports a dead worker once its session is gone, and a crash loop under
  `worker_automatic_restart` no longer adds two descriptors, two command
  buffers and a zombie per restart.

`CommandHub` (`server.rs`) owns the per-client and per-worker session
maps; it derefs to `Server` (`Deref` / `DerefMut for CommandHub`,
`server.rs`) so verb handlers can
reach both ends without juggling two mutable borrows.

### 1.3 Shutdown and re-exec

Soft / hard stops are dispatched from `requests.rs::stop`
(`bin/src/command/requests.rs`) through `requests.rs::begin_stop` via
`StopTask` (`requests.rs`).

`SIGTERM` reaches the same path without a client. `CommandHub::handle_sigterm`
(`server.rs`), called from `begin_main_process` and `begin_new_main_process`,
installs a handler that writes one byte to a socket pair whose read end the
event loop polls under `SIGTERM_TOKEN`. `CommandHub::on_sigterm` drains it and
calls `begin_stop` once per signal: a soft stop while `Running`, a hard stop
while `WorkersStopping`, nothing once `Stopping`. Workers ignore `SIGTERM`
(`begin_worker_process`, `bin/src/worker.rs`), so a `SIGTERM` sent to the whole
control group, as systemd does for a unit without `ExecStop=`, leaves the
workers to the main process's soft stop. A worker whose main process dies
still leaves: its channel closes and its event loop returns.

Once `StopTask` has moved the main to `Stopping` the loop closes every client
whose response buffer is empty and keeps only those holding unflushed bytes,
returning when none is left. That wait has a deadline: one `worker_timeout`
(at least one second), dated when the loop first sees `Stopping`. A client
that stopped reading with a full socket buffer raises no event and asks for no
retry, and the workers are already gone, so without the deadline nothing but
the service manager's `SIGKILL` ended the stop. When the deadline passes the
loop logs a warning, closes the remaining clients and returns; the poll
timeout is capped at the time left, since no readiness announces the deadline.

Hot upgrades are dispatched from `upgrade::upgrade_main`
(`bin/src/upgrade.rs::fork_main_into_new_main`) and
re-enter the new master via the same `begin_main_process` path with
serialized state passed in over the upgrade channel.

---

## 2. Accept Path on the Command Socket

The accept handler runs once per CLI invocation. It captures a per-client
identity envelope used by the audit log and the `command_allowed_uids`
admission gate.

### 2.1 SO_PEERCRED snapshot — `peer_cred_from_stream`

`peer_cred_from_stream` (`bin/src/command/server.rs`, one `#[cfg]` arm per
target) reads `(uid, gid, pid)` via the `SO_PEERCRED`
`getsockopt`. The result is a `PeerCred` (`server.rs`); on platforms
without `SO_PEERCRED` (or when the syscall fails) every field is `None` and
the audit line will render the affected slots as `"unknown"`. `getsockopt`
failure is logged at `warn!` once per accept — it must not panic the main
process.

### 2.2 Per-session identity captured at accept time

Every client gets a `ClientSession` (`bin/src/command/sessions.rs`) at
accept time. The relevant fields:

| Field           | Source                                                 | Used for                                                          |
|-----------------|--------------------------------------------------------|-------------------------------------------------------------------|
| `session_ulid`  | `Ulid::generate()` at accept (`sessions.rs`)           | Stable correlation key across every audit line for this session   |
| `actor_uid`     | `SO_PEERCRED.uid` (`sessions.rs`)                      | Audit attribution + `command_allowed_uids` gate                   |
| `actor_gid`     | `SO_PEERCRED.gid` (`sessions.rs`)                      | Audit attribution                                                 |
| `actor_pid`     | `SO_PEERCRED.pid` (`sessions.rs`)                      | `journalctl _PID=…` correlation                                   |
| `actor_comm`    | `peer_comm(pid)` at accept (`sessions.rs`)             | Distinguish the `sozu` CLI from ad-hoc shells with the same UID   |
| `actor_user`    | `peer_user(uid)` at accept (`sessions.rs`)             | Human-readable account name in the audit line                     |
| `socket_path`   | `Arc<str>` of the listener path (`sessions.rs`)        | Disambiguate audit lines from a multi-instance deployment         |
| `connect_ts`    | `SystemTime::now()` at accept (`sessions.rs`)          | Wall-clock anchor for SOC windowing                                |

### 2.3 PID-reuse-guarded `peer_comm`

`peer_comm` (`bin/src/command/server.rs`, one `#[cfg]` arm per target)
reads `/proc/<pid>/comm` to capture the command-line basename used by the
peer. Between the `SO_PEERCRED` snapshot and the `/proc` read the kernel
could (a) recycle the PID into a different process, or (b) the original
process could `execve()` and become a different binary. To prevent (a)
from leaking the recycled owner's name into the audit line, the function
opens `/proc/<pid>/stat` first (the PID-reuse guard at the top of `peer_comm`); if the stat read fails
the PID is gone and `peer_comm` returns `None`. Case (b) cannot be
detected by `starttime` alone — `execve` does not change `starttime` —
but `exec` is not adversarial in our deployment (the `sozu` CLI never
exec's), and the SOC analyst seeing two different binaries on the same
PID across audit lines for the same session is itself a useful signal.

### 2.4 Cached `peer_user`

`peer_user` (`bin/src/command/server.rs`, one `#[cfg]` arm per target)
resolves `uid → POSIX account name` via `getpwuid_r` / NSS. NSS lookups
are synchronous and on a misconfigured host (SSSD wedge, LDAP timeout,
broken nscd socket) can block the supervisor's event loop for tens of
seconds. The function caches up to `MAX_PEER_USER_CACHE = 16`
(`server.rs`) UID → name pairs in a process-local `Mutex<Vec<…>>`
(the `CACHE` static in `server.rs`) so a steady-state operator UID is paid at most once
per main lifetime. The cache evicts on insert when the cap is reached.

### 2.5 Drop-on-register-fail

When `mio::Registry::register` fails for a freshly accepted client (e.g.
the slab is exhausted or the FD is invalid), the supervisor previously
inserted the `ClientSession` anyway and the unwired stream sat in
`CommandHub.clients` until manual cleanup. As of commit `b8c8fc61`
(`fix(command): drop unix-socket client when register() fails`),
registration failure is treated as terminal: the error is logged with
`{token, error}` and the function returns early; the `UnixStream` is
dropped at scope end, which sends RST/EOF to the peer.

---

## 3. Verb Dispatch and Worker Fan-Out

### 3.1 Entry point — `Server::handle_client_request`

`Server::handle_client_request` (`bin/src/command/requests.rs`) is invoked when
a client sends a complete `Request` over the channel. Steps:

1. Reject empty `request_type` with an `error!` log — the `None` arm of the
   opening `match request.request_type` in `Server::handle_client_request`.
2. Apply `command_allowed_uids` admission (`Config::command_allowed_uids`,
   `command/src/config.rs`; enforced in `Server::handle_client_request`).
   When `Config::command_allowed_uids` is `None` (the default), every
   same-UID local process is permitted; when set,
   `actor_uid` must be in the allowlist or the request is rejected with
   `client.finish_failure("unauthorized: ...")` and recorded in the audit
   trail. The historical "any same-UID" behaviour is preserved for sites
   that do not set the field.
3. Match on `RequestType` (the `match request_type` block in
   `Server::handle_client_request`) and dispatch:
   - master-only verbs (`SaveState`, `LoadState`, `ListWorkers`,
     `Status`, `Logging`, `SubscribeEvents`, `ReloadConfiguration`,
     `UpgradeMain`, `CountRequests`, …) handle inline;
   - mutating verbs that also need worker fan-out
     (`AddCluster`, `Add*Frontend`, `Add*Listener`, `AddCertificate`,
     `RemoveBackend`, `Update*Listener`, `ReplaceCertificate`,
     `ConfigureMetrics`, …) go through `worker_request`
     (`requests.rs`) which records a `WorkerTask` and sends to
     every running worker;
   - query verbs (`QueryClusters*`, `QueryCertificatesFromWorkers`,
     `QueryMetrics`, …) go through `query_clusters` / `query_metrics` /
     `query_certificates_from_main` and gather worker responses through
     a `GatheringTask`.

Completion contract of `QueryClustersTask::on_finish` (`requests.rs`): it
answers Success only when the deadline did not fire, every scattered worker
response is present and `Ok` with usable content (a set `content_type`), no
worker identity answered twice, no worker answered `Failure`, and the terminal
accounting is complete (`errors == 0` and `ok == expected_responses`).
`Processing` replies are progress, not answers. A timeout, a failed or
undecodable status, missing or malformed content, incomplete accounting or a
duplicate worker identity yields a `Failure` carrying the first concrete
reason, never a partial Success. The main payload is added to the worker map
under `main` only where `query_main` returns `Some`;
`QueryCertificatesFromWorkers` legitimately has no main payload, and a main
entry colliding with an existing map key is also a `Failure`. The task checks
response completeness only: it does not validate that worker and main contents
are equal, and it changes no protocol. A certificate fingerprint query that a worker not holding the certificate intentionally refuses therefore fails the whole query with a `Failure`, not an empty `Success`.

Maintainers note: `worker_request` and `set_metric_detail_request`
(`bin/src/command/requests.rs`) take a `ConfigState::hash_state()` snapshot
solely to feed their `debug_assert_eq!` invariant checks (a rejected validation
or dispatch must not mutate persisted state, and `SetMetricDetail` is a
`ConfigState` no-op). The snapshot and its asserts are gated by
`#[cfg(debug_assertions)]` and are compiled out together in non-debug release
builds. Debug invariant coverage is unchanged. This does not remove hashing
elsewhere: query hash generation still computes the hashes it actually needs.

### 3.2 Master state vs worker state

`save_state` (`requests.rs`) and `load_state` (`requests.rs`)
serialise the `ConfigState` held by the master. The on-disk record format
uses a `\n\0` separator (`ConfigState::write_requests_to_file`,
`command/src/state.rs`, also cited in `bin/README.md`); this is distinct from
the `usize`-prefixed channel framing (`Channel::write_delimited_message`,
`command/src/channel.rs`).

`load_state` and `load_static_config` are the two BULK apply paths: they
scatter hundreds of independent entries onto ONE `GatheringTask`. Both
gather through `PerEntryGatherer` (`requests.rs`), which keeps the
fleet-wide `DefaultGatherer` tally AND a per-entry breakdown keyed by the
scatter `request_id` embedded in every per-worker request id
(`{worker_id}-{task_id}-{request_id}`, parsed by
`server.rs::parse_scatter_request_id`). On completion each entry is
judged on its own by `should_rollback_fanout`, the same predicate the live
single-request path uses: an entry NO worker acknowledged is reverted from
the master's `ConfigState` with the inverse `compute_rollback` captured at
scatter time, so `SaveState` cannot re-persist it and the next replay
cannot re-inject it (sozu#1313). `compute_rollback` covers the four listener
adds (inverted to `RemoveListener` on the same address, interface and proxy type) and all
four frontend adds — HTTP, HTTPS, TCP and UDP — each inverted to its
`Remove*Frontend` counterpart carrying the very request message the add
carried. Each of those removals matches on the very key its add admitted, so
the inverse evicts exactly the entry the add inserted.

UDP was uncovered until its removal key was narrowed. `add_udp_frontend` stores
a full `UdpFrontend { cluster_id, address, tags }`, and it used to admit two
frontends at one (cluster, address) differing only in tags, while
`remove_udp_frontend` retained on the address alone and therefore dropped every
sibling at that address — reverting one unacknowledged add would have evicted
acknowledged siblings from the master's `ConfigState`, the main/worker drift
sozu#1313 exists to prevent. `remove_udp_frontend` now retains on that same
(cluster, address, tags) identity, with the `INV:` comment and the "drops
exactly one entry" assertion `remove_tcp_frontend` carries for its own
(address, sni, alpn) key.
`add_udp_frontend` has since made the address exclusive — one frontend per
address across every cluster, because a datagram carries no SNI, host or path
to discriminate on — so the entry an inverse removes is the only one on its
address and the eviction is unambiguous.
`remove_udp_frontend_drops_exactly_the_frontend_its_tags_name`
(`command/src/state.rs`) pins the mirror; `sozu frontend udp remove --tags`
carries the tags the identity needs.

Upsert verbs (`AddCluster`, `AddBackend`) and non-add verbs stay deliberately
uncovered: they keep the best-effort behaviour rather than risk a wrong revert.

An inverse is the exact undo of its order only while that order is the newest
change to its object: it is captured against the state as it stood when the
order arrived, which already holds the effect of every order still in flight
before it. Two orders on one object that both end unacknowledged — an add and
the removal that follows it, or the reverse — used to be reverted in the order
their deadlines expired, oldest first, and undid each other's work: the add's
removal found nothing to remove (`StateError::NotFound`, only logged), and the
removal's inverse then put back a frontend that neither client was told exists
(or, in the other direction, dropped the frontend that existed before both
orders). `RollbackLedger` (`bin/src/command/rollback.rs`) now holds one stack
per object, oldest order first, keyed by `rollback_key`
(`command/src/state.rs`): an HTTP or HTTPS frontend by its route key, a TCP
frontend by its canonical `(address, sni, alpn)`, a UDP frontend by its address,
a listener and its activation flag by type and `address%interface` — each
spelled the way `ConfigState::dispatch` identifies the object, an order and
the inverse that undoes it sharing one key. Every order registers with
`RollbackLedger::owe` when the state accepts it (`worker_request`,
`apply_saved_entry`, `load_static_config`) and settles with
`RollbackLedger::settle` when its fan-out ends, reverted or kept
(`WorkerTask::on_finish`, `PerEntryGatherer::revert_unacknowledged`). `settle`
walks the stack from the newest order: a revert waits while a newer order on
its object is still in flight, then runs newest first behind it; an order that
a worker acknowledged supersedes every older revert on its object, which is
dropped; orders on different objects never wait for each other. A replay that
`load_state` cancels gives its claims back with `Server::cancel_task`. The
ledger is not part of an upgrade snapshot: a task restored by a main upgrade
holds no claim and reverts on its own when it ends, exactly as before, so an
unacknowledged order that straddles a main upgrade can still be reverted out of
order, and a revert already parked in the ledger behind a newer order is lost
with the old process, its task having ended. `cargo test -p sozu --lib stacked_unacknowledged_orders` and the
tests of `rollback.rs` lock the ordering.

A listener patch (`Update*Listener`) has no inverse either, so it relies on two
properties instead (sozu#1703). First, it is all-or-nothing on both sides:
`ConfigState::update_http_listener` and `ConfigState::update_https_listener`
(`command/src/state.rs`) run every check before their first write, and
`HttpListener::update_config` (`lib/src/http.rs`) and
`HttpsListener::update_config` (`lib/src/https.rs`) apply the patch to a copy of
the listener configuration, build the fallible artifacts (answer registry,
rustls context) from it, and commit only once all of them succeeded — a refused
patch leaves the listener exactly as it was, where it used to keep every field
written before the failing one. Second, the main process refuses the patches the
workers refuse: the answer templates, which `ConfigState` cannot parse, are
checked before dispatch by `validate_listener_request` (`requests.rs`) with the
same `HttpAnswers::validate_patch_templates`
(`lib/src/protocol/kawa_h1/answers.rs`) the worker runs, and an `hsts` block
without `enabled` is refused by `ConfigState::update_https_listener` as by the
worker. The one refusal the main process cannot predict is a worker failing to
build its rustls context for a new ALPN list; the main state then records a
patch that worker refused.

Both also arm a bounded deadline (`bulk_replay_timeout`: one
`worker_timeout` plus 10 ms per scattered entry, capped at ten
`worker_timeout`s) instead of the former `Timeout::None`, and report a
deadline as a failure. A request that can never be delivered on a worker channel
(`Server::scatter_on`) and every request still in flight on a worker that
closes (`CommandHub::fail_in_flight_requests_of_worker`) are accounted as
synthetic `Failure`s, so `ok + errors` always reaches
`expected_responses`; a terminal answer retires its `in_flight` entry
immediately, so a worker that answers and then dies is not re-counted as a
rejection. A replay that `load_state` abandons on a state-file parse error is
cancelled with `Server::cancel_task`, which retires the task's `in_flight`
routes together with the task and keeps every other task's (sozu#1827): a
late answer or a worker closing then finds no route for it. A bulk sender that fills a worker's back buffer past
`max_buffer_size` parks the overflow in the per-worker
`WorkerSession::pending` queue and drains it from the WRITABLE path
(`WorkerSession::flush_pending`), in scatter order: nothing is dropped and
the single-threaded supervisor never blocks on a socket, so clients,
workers and task deadlines keep being served while a large replay is on
the wire.

### 3.3 Worker channels and FD passing

Workers communicate with the master over `mio::net::UnixStream` channels
plus an `ScmSocket` (`command/src/scm_socket.rs`) that carries listener
FDs at boot and during hot upgrades. The supervisor multiplexes both
endpoints in the same event loop; per-worker queues and read buffers
live in `WorkerSession` (`bin/src/command/sessions.rs`).

---

## 4. Hot-Upgrade Interaction

`bin/src/upgrade.rs` provides the re-exec orchestration. A main upgrade is a
two-step ownership transfer of the whole [`CommandHub`](server.rs), not a
replay of its commands:

1. Before changing descriptor flags or freezing the event loop, `upgrade_main`
   (`bin/src/command/upgrade.rs`) runs the replacement executable's internal
   protocol-v2 capability probe. A candidate without that protocol is rejected
   while the old Hub remains authoritative.
2. The old main snapshots every command client and worker session, including
   channel buffers, worker pending queues and stopped workers that may still be
   referenced by an upgrade task. It also snapshots active and queued tasks,
   worker-response routes, subscribers, pending audit events, configuration
   state, monotonic deadlines, task-local main-process metric contributions and
   the counters that allocate future IDs.
   Task restoration rebuilds the saved state directly; it never re-dispatches
   an operator request or re-scatters work already sent to a worker.
3. The replacement restores and registers those descriptors in a paused Hub.
   Registration validates the graph and descriptor state but cannot read or
   write command or worker data. It then runs every fallible preparation that
   touches nothing shared with the old main: it restores `CLOEXEC` on the
   inherited descriptors, installs its `SIGTERM` handler and opens its PID
   file without changing its content (`begin_new_main_process`,
   `bin/src/upgrade.rs`). A failure there still rolls back. It replies
   `PREPARED` with counts covering sessions, tasks, routes and buffered work.
4. The old main compares those counts and checks once more for a pending
   `SIGTERM`. Before `COMMIT`, any rejection closes and reaps the candidate,
   restores `CLOEXEC`, and resumes the unchanged old Hub. The first attempt to
   send `COMMIT` is the irreversible fence: an ambiguous or failed
   post-commit handoff never resumes the old event loop. The old main exits,
   and its workers return from their event loops once their command channels
   close, so a replacement that fails after `COMMIT` stops the whole proxy.
5. After receiving the complete `COMMIT`, the replacement activates the Hub,
   ticks every restored session once so userspace-only buffered frames make
   progress, publishes its PID and systemd `MAINPID`/`READY`, queues the
   initiating client's terminal response, then sends `ACTIVATED`. Activation
   is the only step left that can fail, because restoring a worker's SCM socket
   changes the blocking mode of a file description the old main shares; a
   failure to write the PID file, queue the terminal response or send
   `ACTIVATED` is logged and the replacement keeps running. The old main stays
   fenced until that acknowledgement or its failure, and then exits. A full client back buffer delays the terminal response without
   losing or duplicating it. A restored stopped worker is retained while an
   upgrade task or live response route still uses it, then its command and SCM
   descriptors are closed without signalling its historical PID again.

A binary that predates this protocol cannot export the missing client and task
state. The replacement accepts the flat 2.2.1 snapshot only on the legacy
receiver path selected by an absent internal `--upgrade-protocol` argument.
It restores configuration, state, counters and paused worker sessions, then
sends the legacy boolean acknowledgement. A Linux pidfd observing the actual
parent's exit is the ownership barrier: acknowledgement and confirmation-channel
EOF are not barriers because the old event loop can still process its current
batch. No shared channel I/O or SCM mode change occurs before parent exit.
A delayed parent produces a diagnostic after `worker_timeout`; the receiver
keeps waiting, without killing the parent or releasing the inherited channels.
This path requires externally quiescent administration and fresh client
connections afterward; it cannot preserve tasks or buffers absent from the old
snapshot. It refuses before acknowledgement under notify supervision
(`NOTIFY_SOCKET`), which requires a separately verified supervisor handoff or
a controlled restart. See [the 2.2.1 upgrade procedure](../../../doc/how_to_use.md#first-hot-upgrade-from-221).
Explicit v2 imports remain strict, and the outgoing capability probe still
rejects legacy candidates. A `SIGTERM` already in
the old main's self-pipe aborts before `COMMIT` and is left for its normal event
loop. A signal sent specifically to the old PID after that final check is not
forwarded; service managers should signal the unit rather than a superseded PID.

`UpgradeWorker` follows the analogous pattern through `upgrade_worker`
(`bin/src/command/upgrade.rs`) and re-exec of an individual worker. Its first
phase takes the old worker's listeners and launches the new worker with them;
its second phase (`StopOldActivateNew`) soft-stops the old worker and sends the
activation requests to the new one.

Between the two phases the old worker stays `Running` but holds no listening
socket: `ReturnListenSockets` makes it hand every descriptor over and keep
none, and the supervisor closes its own copies once the replacement has them,
or once the launch has failed (`fork_main_into_worker` closes them on every
error return): a copy left open would keep the supervisor listening on sockets
it never accepts on, so with one worker new connections would complete the
handshake and hang, and with several `SO_REUSEPORT` would still hash a share of
them there until the supervisor exited.
The second phase starts only when the replacement answers its initial `Status`
with `Ok` (`AwaitingReadiness`). Every other exit, a replacement that times
out, is rejected, or is refused its bootstrap, a failed or malformed receive
of the listeners, and a failed launch, ends the upgrade through
`UpgradeWorkerTask::abort_keeping_old_worker`: a replacement that reached
`AwaitingReadiness` is fenced first, and the old worker is sent an
`ActivateListener` for every listener the state
holds as active (`RestoringOldWorker`), so it binds a fresh socket with
`SO_REUSEPORT`. A repeated activation of a listener that is already active
answers `Ok`. The client gets `Processing` while it waits, then one `Failure`
that says the old worker "listens again and keeps serving" only once every
activation was answered `Ok`; a rejected or unanswered activation is named
instead, and a worker that is already stopping or stopped is not asked. With no
active listener the answer is immediate. While the restoration is in flight
the task cannot be handed over to a new main process
(`TaskSnapshotError::OldWorkerRestoreInFlight`), as for the readiness wait. An
old worker that never answers the first `ReturnListenSockets` is not covered:
that answer is indeterminate and the upgrade fails as before.

Only one `upgrade --worker` runs per worker at a time. A worker keeps a single
pending shutdown id, and the old worker stays `Running` until the replacement
is ready, so `get_active_worker_by_id` alone would admit a second upgrade of
it: the worker has no listening socket left to give back, the second upgrade
launches a second replacement for the same slot, and its `SoftStop` overwrites
the id of the first, whose client then gets only the synthetic "closed before
answering" failure. `upgrade_worker` therefore refuses a worker that a live task
is upgrading (`Server::worker_upgrade_in_flight`) with "Worker N is already
being upgraded", before it asks the worker for anything. Every stage of the
upgrade names its old worker (`GatheringTask::upgrading_worker_id`), the
readiness stage included while the protected bootstrap of the replacement
(`WorkerBootstrapTask`) holds it, which forwards the name as it forwards the
retained workers; a refused bootstrap marks no worker as bootstrapping, so this
is the only guard there. The answer is read from two places because a task
changes hands: one created in the current event-loop iteration is still in
`Server::queued_tasks`, and one that went through a sweep is only in
`CommandHub::tasks`, out of the server's reach.
The sweep (`CommandHub::finish_ready_tasks`) therefore rewrites
`Server::upgrading_workers` from the tasks that stay alive, the ones just
chained included. Nothing ever clears it by hand: an upgrade that ended however
it ended (an abort, a failure, the final answer, a worker that died) frees its
worker at that sweep, and a main process that took a phase-one task over in a
handoff has the set after its first sweep, so the set is in no snapshot. The
second upgrade of a worker that is already `Stopping` keeps the answer it had,
"does not exist, or is stopping / stopped". The per-worker upgrades of
`sozu upgrade` target distinct workers and do not refuse each other, but one
of them is refused like any other when an `upgrade --worker` of the same
worker is already in flight.

`UpgradeWorkerTask::on_finish` of the second phase answers
`Ok` only when every response of that second phase is `Ok`. A failure of the
old worker's `SoftStop`, including the synthetic "closed before answering" of
§1.2, or of any activation request of the new worker, answers `Failure` naming
the worker and its reasons, and says whether the new worker is serving. The
CLI maps that answer to a non-zero exit of `sozu upgrade --worker N`; the
per-worker upgrades that `sozu upgrade` runs after a main handoff collect all
results and also exit nonzero if any worker fails. The main handoff is already
committed and is not rolled back (`bin/src/ctl/command.rs`).

---

## 5. Recent Additions (post `feat/h2-mux` baseline)

This subsection groups the supervisor-side hardening that landed during
the `feat/h2-mux` review pass. Each bullet is a single commit that can be
read in isolation; the commit subject is the canonical search key.

### 5.1 `command_allowed_uids` admission — commit `f6c1bc81`

Optional `Config::command_allowed_uids: Option<Vec<u32>>`
(`FileConfig::command_allowed_uids`, `command/src/config.rs`; propagated into
the runtime `Config` by `ConfigBuilder::new`). Enforced in
`Server::handle_client_request` (`bin/src/command/requests.rs`).
`None` preserves the historical "any same-UID local process" behaviour;
`Some(allowlist)` rejects every actor UID outside the list. Rejected
verbs still emit an audit line (`client.finish_failure(...)` is captured
by the audit envelope at the verb call sites). Documented in the
`command_allowed_uids` sections of `doc/configure.md` and
`command/README.md`.

### 5.2 Audit-log envelope — commit `ad487958`

The supervisor renders one structured audit line per mutating verb. The
machinery:

- `audit_log_context!` macro (`bin/src/command/requests.rs`) — produces
  the `[session req cluster|- backend|-]\tAUDIT\tCommand(verb=…, …)`
  envelope, mirroring the `MUX-*` / `RUSTLS` / `PIPE` bracket layout so
  operators can grep `AUDIT` alongside data-plane lines.
- `AuditEntry` / `AuditExtras` (`requests.rs`) — the typed payload
  built before the macro is invoked.
- `audit_emit` (`requests.rs`) — text sink (the `info!`-driven log
  drain).
- `audit_record_to_json` (`requests.rs`) — JSON sink. Every free-form
  field is passed through `sanitize_for_audit` at render time (the
  `*_sanitized` bindings in `audit_record_to_json`) so `\n`/`\t`/ANSI
  sequences cannot forge a
  second JSON record. The fix at `ad487958` extended the sanitization
  to the JSON sink free-form fields after the initial pass shipped with
  text-only sanitization.
- `sanitize_for_audit` (`bin/src/command/sessions.rs`) — single-pass
  scrubber that replaces ASCII control bytes (`\x00..=\x1f`, `\x7f`) with
  `?`. Cheap (no allocation when the input is already clean).

The two helpers `display_or_unknown` (`sessions.rs`) and
`display_sanitized_or_unknown` (`sessions.rs`) collapse the per-field
"render `Option<T>` as `"unknown"` when missing" pattern so the five
near-identical `actor_*_display` accessors on `ClientSession` cannot
regress against the sanitization rule.

### 5.3 PID-reuse-guarded `peer_comm` — commit `a5a41b1d`

See §2.3 for the runtime contract. The fix opens `/proc/<pid>/stat`
before reading `/proc/<pid>/comm` so a recycled PID's new owner cannot
leak into the audit line.

### 5.4 Drop-on-register-fail — commit `b8c8fc61`

See §2.5 for the runtime contract. Registration failure is now terminal
for the affected client — the `UnixStream` is dropped at scope end and
the OS sends RST/EOF to the peer.

---

## 6. Cross-References

- `command/src/scm_socket.rs` — FD-passing primitive used at boot and
  during hot upgrades.
- `bin/src/upgrade.rs` — re-exec orchestration; consumes
  `UpgradeData` blobs produced by the supervisor.
- `command/src/channel.rs` — `usize`-prefixed wire framing used between
  the supervisor and workers (distinct from the `\n\0` save-state
  framing in `ConfigState::write_requests_to_file`, `command/src/state.rs`).
- `doc/observability.md` — canonical observability contract, including the
  audit field reference and the bounded sensitive-value rules for `Debug`,
  direct logs, retained tasks/state, and TLS runtime objects.
- `doc/configure_admin_ops.md` — operational worked examples that drive
  this surface from the CLI side.
- `lib/src/protocol/mux/LIFECYCLE.md` — counterpart inside the workers'
  data plane; the supervisor delivers listener / cluster / certificate
  state to the workers that drive the mux.
