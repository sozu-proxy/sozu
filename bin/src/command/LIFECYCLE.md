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

Entry point: `begin_main_process` (`bin/src/command/mod.rs`), called from
`bin/src/main.rs:91` for the `start` sub-command. It:

1. Bumps process limits (`update_process_limits`,
   `bin/src/command/mod.rs`, one `#[cfg]` arm per target) — RLIMIT_NOFILE
   in particular must be high enough for the configured listener count plus
   per-worker per-session sockets.
2. Initialises logging (`logging::setup_logging`).
3. Constructs the `Server` (`bin/src/command/server.rs`) and binds the
   unix command socket (`UnixListener` per the configured `command_socket`
   path with mode `0o600`).
4. Forks the configured number of workers via `fork_main_into_worker`
   (`bin/src/worker.rs`). Each worker inherits a `Channel` pair plus an
   `ScmSocket` for FD passing.
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
  cannot block a client forever.

`CommandHub` (`server.rs`) owns the per-client and per-worker session
maps; it derefs to `Server` (`Deref` / `DerefMut for CommandHub`,
`server.rs`) so verb handlers can
reach both ends without juggling two mutable borrows.

### 1.3 Shutdown and re-exec

Soft / hard stops are dispatched from `requests.rs::stop`
(`bin/src/command/requests.rs`) via `StopTask` (`requests.rs`).
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
opens `/proc/<pid>/stat` first (`bin/src/command/server.rs:1617`); if the stat read fails
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
adds (inverted to `RemoveListener` on the same address and proxy type) and all
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

Both also arm a bounded deadline (`bulk_replay_timeout`: one
`worker_timeout` plus 10 ms per scattered entry, capped at ten
`worker_timeout`s) instead of the former `Timeout::None`, and report a
deadline as a failure. A request that can never be delivered on a worker channel
(`Server::scatter_on`) and every request still in flight on a worker that
closes (`CommandHub::fail_in_flight_requests_of_worker`) are accounted as
synthetic `Failure`s, so `ok + errors` always reaches
`expected_responses`; a terminal answer retires its `in_flight` entry
immediately, so a worker that answers and then dies is not re-counted as a
rejection. A bulk sender that fills a worker's back buffer past
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

`bin/src/upgrade.rs` provides the re-exec orchestration. The supervisor:

1. On `UpgradeMain`, calls `upgrade_main` (`bin/src/command/upgrade.rs`)
   which serialises the master state via
   `SerializedWorkerSession::try_from(&worker_session)`
   (`bin/src/command/server.rs:1537`) into an `UpgradeData` blob, forks a
   replacement master via `fork_main_into_new_main`
   (`bin/src/upgrade.rs`), hands the blob over a pipe, and exits once
   the new master takes over.
2. The new master `exec`s into the freshly built `sozu` binary
   (`get_executable_path` in `bin/src/util.rs`) and re-enters
   `begin_main_process` with a flag indicating "resume from upgrade".
3. Workers stay alive across the swap; they continue talking to the
   surviving channel endpoints which are forwarded through the FD-handoff
   protocol.

`UpgradeWorker` follows the analogous pattern through `upgrade_worker`
(`bin/src/command/upgrade.rs`) and re-exec of an individual worker.

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
