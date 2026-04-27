# Sōzu Supervisor — Command-Server Lifecycle

Reference document for maintainers of the master / supervisor surface under
`bin/src/command/`. Companion to `lib/src/protocol/mux/LIFECYCLE.md` (the H2
mux that workers run inside their own event loop) and to
`bin/src/upgrade.rs` (the hot-upgrade re-exec orchestrator).

Every claim is anchored to a concrete `file.rs:LINE`; line numbers were last
refreshed against the `docs/feat-h2-mux-audit` branch tip on 2026-04-26.

---

## 1. Supervisor Lifecycle

### 1.1 Boot — `begin_main_process`

Entry point: `begin_main_process` (`bin/src/command/mod.rs:66`), called from
`bin/src/main.rs:85` for the `start` sub-command. It:

1. Bumps process limits (`update_process_limits`,
   `bin/src/command/mod.rs:133` on Linux / `:214` on other targets) — RLIMIT_NOFILE
   in particular must be high enough for the configured listener count plus
   per-worker per-session sockets.
2. Initialises logging (`logging::setup_logging`).
3. Constructs the `Server` (`bin/src/command/server.rs:619`) and binds the
   unix command socket (`UnixListener` per the configured `command_socket`
   path with mode `0o600`).
4. Forks the configured number of workers via `fork_main_into_worker`
   (`bin/src/worker.rs:208`). Each worker inherits a `Channel` pair plus an
   `ScmSocket` for FD passing.
5. Enters the master event loop via `Server::run`
   (`bin/src/command/server.rs:398`).

### 1.2 Main loop — `Server::run`

The supervisor is a single-threaded mio event loop. Each tick:

- polls registered tokens (the command socket, every connected client, and
  every worker channel);
- accepts new clients on the command socket and registers them in
  `CommandHub.clients` keyed by mio `Token`;
- drains worker responses and routes them back to the originating client
  via `GatheringTask` callbacks (`server.rs:84-99` declares the trait,
  `requests.rs` defines the concrete tasks: `QueryClustersTask`,
  `LoadStaticConfigTask`, `WorkerTask`, `QueryMetricsTask`,
  `LoadStateTask`, `StatusTask`, `StopTask`, …);
- ticks per-task timeouts (`Timeout`, `server.rs:124`) so a wedged worker
  cannot block a client forever.

`CommandHub` (`server.rs:194`) owns the per-client and per-worker session
maps; it derefs to `Server` (`server.rs:207`/`:214`) so verb handlers can
reach both ends without juggling two mutable borrows.

### 1.3 Shutdown and re-exec

Soft / hard stops are dispatched from `requests.rs::stop`
(`bin/src/command/requests.rs:2117`) via `StopTask` (`requests.rs:2156`).
Hot upgrades are dispatched from `upgrade::upgrade_main`
(`bin/src/upgrade.rs::fork_main_into_new_main`, `upgrade.rs:92`) and
re-enter the new master via the same `begin_main_process` path with
serialized state passed in over the upgrade channel.

---

## 2. Accept Path on the Command Socket

The accept handler runs once per CLI invocation. It captures a per-client
identity envelope used by the audit log and the `command_allowed_uids`
admission gate.

### 2.1 SO_PEERCRED snapshot — `peer_cred_from_stream`

`peer_cred_from_stream` (`bin/src/command/server.rs:1140` on Linux / `:1156`
on other targets) reads `(uid, gid, pid)` via the `SO_PEERCRED`
`getsockopt`. The result is a `PeerCred` (`server.rs:1127`); on platforms
without `SO_PEERCRED` (or when the syscall fails) every field is `None` and
the audit line will render the affected slots as `"unknown"`. `getsockopt`
failure is logged at `warn!` once per accept — it must not panic the main
process.

### 2.2 Per-session identity captured at accept time

Every client gets a `ClientSession` (`bin/src/command/sessions.rs:29`) at
accept time. The relevant fields:

| Field           | Source                                                 | Used for                                                          |
|-----------------|--------------------------------------------------------|-------------------------------------------------------------------|
| `session_ulid`  | `Ulid::generate()` at accept (`sessions.rs:32-35`)     | Stable correlation key across every audit line for this session   |
| `actor_uid`     | `SO_PEERCRED.uid` (`sessions.rs:37-40`)                | Audit attribution + `command_allowed_uids` gate                   |
| `actor_gid`     | `SO_PEERCRED.gid` (`sessions.rs:41-43`)                | Audit attribution                                                 |
| `actor_pid`     | `SO_PEERCRED.pid` (`sessions.rs:44-48`)                | `journalctl _PID=…` correlation                                   |
| `actor_comm`    | `peer_comm(pid)` at accept (`sessions.rs:49-52`)       | Distinguish the `sozu` CLI from ad-hoc shells with the same UID   |
| `actor_user`    | `peer_user(uid)` at accept (`sessions.rs:53-57`)       | Human-readable account name in the audit line                     |
| `socket_path`   | `Arc<str>` of the listener path (`sessions.rs:58-62`)  | Disambiguate audit lines from a multi-instance deployment         |
| `connect_ts`    | `SystemTime::now()` at accept (`sessions.rs:63-68`)    | Wall-clock anchor for SOC windowing                                |

### 2.3 PID-reuse-guarded `peer_comm`

`peer_comm` (`bin/src/command/server.rs:1177` on Linux / `:1198` elsewhere)
reads `/proc/<pid>/comm` to capture the command-line basename used by the
peer. Between the `SO_PEERCRED` snapshot and the `/proc` read the kernel
could (a) recycle the PID into a different process, or (b) the original
process could `execve()` and become a different binary. To prevent (a)
from leaking the recycled owner's name into the audit line, the function
opens `/proc/<pid>/stat` first (`server.rs:1182`); if the stat read fails
the PID is gone and `peer_comm` returns `None`. Case (b) cannot be
detected by `starttime` alone — `execve` does not change `starttime` —
but `exec` is not adversarial in our deployment (the `sozu` CLI never
exec's), and the SOC analyst seeing two different binaries on the same
PID across audit lines for the same session is itself a useful signal.

### 2.4 Cached `peer_user`

`peer_user` (`bin/src/command/server.rs:1213` on Linux / `:1250` elsewhere)
resolves `uid → POSIX account name` via `getpwuid_r` / NSS. NSS lookups
are synchronous and on a misconfigured host (SSSD wedge, LDAP timeout,
broken nscd socket) can block the supervisor's event loop for tens of
seconds. The function caches up to `MAX_PEER_USER_CACHE = 16`
(`server.rs:1221`) UID → name pairs in a process-local `Mutex<Vec<…>>`
(`server.rs:1223`) so a steady-state operator UID is paid at most once
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

`handle_client_request` (`bin/src/command/requests.rs:182`) is invoked when
a client sends a complete `Request` over the channel. Steps:

1. Reject empty `request_type` with an `error!` log (`requests.rs:185-188`).
2. Apply `command_allowed_uids` admission (`requests.rs:195-215`).
   When `Config::command_allowed_uids` is `None` (the default), every
   same-UID local process is permitted; when set,
   `actor_uid` must be in the allowlist or the request is rejected with
   `client.finish_failure("unauthorized: ...")` and recorded in the audit
   trail. The historical "any same-UID" behaviour is preserved for sites
   that do not set the field.
3. Match on `RequestType` (`requests.rs:216-271`) and dispatch:
   - master-only verbs (`SaveState`, `LoadState`, `ListWorkers`,
     `Status`, `Logging`, `SubscribeEvents`, `ReloadConfiguration`,
     `UpgradeMain`, `CountRequests`, …) handle inline;
   - mutating verbs that also need worker fan-out
     (`AddCluster`, `Add*Frontend`, `Add*Listener`, `AddCertificate`,
     `RemoveBackend`, `Update*Listener`, `ReplaceCertificate`,
     `ConfigureMetrics`, …) go through `worker_request`
     (`requests.rs:1505`) which records a `WorkerTask` and sends to
     every running worker;
   - query verbs (`QueryClusters*`, `QueryCertificatesFromWorkers`,
     `QueryMetrics`, …) go through `query_clusters` / `query_metrics` /
     `query_certificates_from_main` and gather worker responses through
     a `GatheringTask`.

### 3.2 Master state vs worker state

`save_state` (`requests.rs:375`) and `load_state` (`requests.rs:1848`)
serialise the `ConfigState` held by the master. The on-disk record format
uses a `\n\0` separator (see `command/src/state.rs:1613, 1630` cited in
`bin/README.md`); this is distinct from the `usize`-prefixed framing the
command channel itself uses (see `command/src/channel.rs:611`).

### 3.3 Worker channels and FD passing

Workers communicate with the master over `mio::net::UnixStream` channels
plus an `ScmSocket` (`command/src/scm_socket.rs`) that carries listener
FDs at boot and during hot upgrades. The supervisor multiplexes both
endpoints in the same event loop; per-worker queues and read buffers
live in `WorkerSession` (`bin/src/command/sessions.rs:323`).

---

## 4. Hot-Upgrade Interaction

`bin/src/upgrade.rs` provides the re-exec orchestration. The supervisor:

1. On `UpgradeMain`, calls `upgrade_main` (`bin/src/upgrade.rs::upgrade_main`)
   which serialises the master state via
   `SerializedWorkerSession::try_from(&worker_session)`
   (`bin/src/command/server.rs:1102`) into an `UpgradeData` blob, forks a
   replacement master via `fork_main_into_new_main`
   (`bin/src/upgrade.rs:92`), hands the blob over a pipe, and exits once
   the new master takes over.
2. The new master `exec`s into the freshly built `sozu` binary
   (`get_executable_path` in `bin/src/util.rs`) and re-enters
   `begin_main_process` with a flag indicating "resume from upgrade".
3. Workers stay alive across the swap; they continue talking to the
   surviving channel endpoints which are forwarded through the FD-handoff
   protocol.

`UpgradeWorker` follows the analogous pattern through `upgrade_worker`
(`bin/src/upgrade.rs`) and re-exec of an individual worker.

---

## 5. Recent Additions (post `feat/h2-mux` baseline)

This subsection groups the supervisor-side hardening that landed during
the `feat/h2-mux` review pass. Each bullet is a single commit that can be
read in isolation; the commit subject is the canonical search key.

### 5.1 `command_allowed_uids` admission — commit `f6c1bc81`

Optional `Config::command_allowed_uids: Option<Vec<u32>>`
(`command/src/config.rs:1424`, propagated into the runtime config at
`command/src/config.rs:1611`). Enforced in
`Server::handle_client_request` (`bin/src/command/requests.rs:195-215`).
`None` preserves the historical "any same-UID local process" behaviour;
`Some(allowlist)` rejects every actor UID outside the list. Rejected
verbs still emit an audit line (`client.finish_failure(...)` is captured
by the audit envelope at the verb call sites). Documented in
`doc/configure.md:42` and `command/README.md:78`.

### 5.2 Audit-log envelope — commit `ad487958`

The supervisor renders one structured audit line per mutating verb. The
machinery:

- `audit_log_context!` macro (`bin/src/command/requests.rs:84`) — produces
  the `[session req cluster|- backend|-]\tAUDIT\tCommand(verb=…, …)`
  envelope, mirroring the `MUX-*` / `RUSTLS` / `PIPE` bracket layout so
  operators can grep `AUDIT` alongside data-plane lines.
- `AuditEntry` / `AuditExtras` (`requests.rs:868`) — the typed payload
  built before the macro is invoked.
- `audit_emit` (`requests.rs:1287`) — text sink (the `info!`-driven log
  drain).
- `audit_record_to_json` (`requests.rs:1345`) — JSON sink. Every free-form
  field is passed through `sanitize_for_audit` at render time (see
  `requests.rs:1395-1399`) so `\n`/`\t`/ANSI sequences cannot forge a
  second JSON record. The fix at `ad487958` extended the sanitization
  to the JSON sink free-form fields after the initial pass shipped with
  text-only sanitization.
- `sanitize_for_audit` (`bin/src/command/sessions.rs:185`) — single-pass
  scrubber that replaces ASCII control bytes (`\x00..=\x1f`, `\x7f`) with
  `?`. Cheap (no allocation when the input is already clean).

The two helpers `display_or_unknown` (`sessions.rs:205`) and
`display_sanitized_or_unknown` (`sessions.rs:215`) collapse the per-field
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
  framing in `command/src/state.rs:1613, 1630`).
- `doc/observability.md` — operator-facing description of the audit
  output, including the field reference.
- `doc/configure_admin_ops.md` — operational worked examples that drive
  this surface from the CLI side.
- `lib/src/protocol/mux/LIFECYCLE.md` — counterpart inside the workers'
  data plane; the supervisor delivers listener / cluster / certificate
  state to the workers that drive the mux.
