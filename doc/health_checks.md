# Health Checks

Sōzu supports active health checks for backend servers. When configured on a
cluster, Sōzu periodically probes each backend and tracks whether it passes.
Backends that fail consecutive checks are marked as unhealthy and excluded from
load balancing. Once they pass again, they are marked healthy and traffic
resumes.

Two probe modes exist (`mode`):

- **`HTTP`** (the default): an HTTP `GET <uri>`; the backend passes when the
  response status is accepted (`accepted_statuses`, or `expected_status` when
  that list is empty).
- **`TCP`**: a TCP connect; the backend passes when the TCP connection is
  established within `timeout`. Nothing is sent and the connection is closed as
  soon as it is established. `uri` is ignored; a non-zero `expected_status` or
  a non-empty `accepted_statuses` is refused, since a TCP probe judges no
  status. Use it when the only question is whether the
  backend accepts connections — for example to take blackholed backends out of
  rotation on a platform where an application answering 3xx, 401, 404 or even
  500 on a fixed path is still serving.

Both modes dial each backend's address, unless `port` is set: the probe then
connects to the backend's IP address on that port, for a backend whose health
endpoint or companion listener lives beside its data port. `port` is refused in a
cluster with a backend carrying `unix_path`, in either order and on every ingress (file,
CLI, command socket, worker): a UNIX socket has no IP address, so the probe endpoint
would be ambiguous. Without `port`, a `unix_path` backend is probed through its socket.

> **Before enabling `TCP` mode or `accepted_statuses`, upgrade the main process
> and every worker, and use the new `sozu` CLI.** Both are new protocol fields
> (`HealthCheckConfig` fields 7 and 8) that an older Sōzu ignores, so it
> silently runs the default probe instead — `GET <uri>` (`/` when the TOML or
> CLI left `uri` unset) accepting only 2xx — and marks down every backend that
> answers 3xx, 404 or 500. That happens with an older worker still running
> between `sozu upgrade main` and `sozu upgrade worker`; with an older `sozu`
> CLI that patches a cluster by read-modify-write (`QueryClusterById` →
> `AddCluster`, as `sozu cluster h2 enable|disable` does), which re-sends the
> health check without the two fields and so switches it back to that default
> on every worker; and with an older binary loading a saved state written by a
> newer one. Downgrading while a cluster uses either field is not supported.

> **Before setting `port`, upgrade the main process and every worker to 2.3.2,
> and use the new `sozu` CLI.** `port` is `HealthCheckConfig` field 9, an
> additive field on the existing `AddCluster` and `SetHealthCheck` messages, so
> it is **not** fail-closed: an older worker decodes the message, ignores the
> unknown field and probes the backend's data port instead — and in `TCP` mode
> passes every backend that accepts connections there, whatever the probe port
> would have answered. Only the new `unix_path`, header-routing and
> `silent_close` request verbs are refused by an older worker.

> **Custom request headers** (`headers`, 2.3.2) carry credentials and follow
> their own rules: values come from files or standard input, never from the
> configuration, and they stay in memory. Read
> [Custom request headers](#custom-request-headers) before using them.

## How it works

Health checks run inside the main mio event loop using non-blocking TCP
connections. There is no separate thread or process — checks are interleaved
with normal request processing.

For each cluster with a health check configured, Sōzu performs the following on
every check cycle:

1. Opens a non-blocking TCP connection to each backend in the cluster: to its
   address, or to its IP address on `port` when that is set. In `TCP`
   mode the check ends here: it passes once the connection is established
   (the socket becomes writable with no pending error) and fails on a
   connection error or when `timeout` elapses first
2. Sends a probe whose wire format follows the cluster's `http2` flag: HTTP/1.1
   (`GET <uri> HTTP/1.1` with `Connection: close`) when `cluster.http2 = false`
   (the default), or HTTP/2 prior-knowledge (connection preface + empty
   SETTINGS + HEADERS frame on stream 1 carrying `GET <uri>`) when
   `cluster.http2 = true`. Its `Host` (h2c `:authority`) is the `ip:port`
   dialed, so it names the probe port when `port` is set. The custom
   [request headers](#custom-request-headers) of the health check, if any, follow
   in their declared order
3. Reads the response: HTTP/1.1 status line for the default path, or HTTP/2
   frames until a HEADERS frame on stream 1 yields `:status` for h2c
4. Compares the HTTP status code against the accepted statuses
5. Updates the backend's health state based on success or failure

A probe is bound to the backend it was launched for, identified by its backend
id and address: two backends of a cluster may share an address under distinct
ids, and each probe updates only its own. When `RemoveBackend` removes a
backend while one of its probes is in flight, the probe's result is discarded
on completion, even if a backend with the same id and address was added in the
meantime: the re-added backend is a new one, starts healthy, and is probed on
the next check cycle without waiting for the old probe to end.

### Health state machine

Each backend maintains a `HealthState` with counters for consecutive successes
and failures:

- **Healthy → Unhealthy**: After `unhealthy_threshold` consecutive failed
  checks, the backend is marked DOWN. Sōzu logs a warning, increments the
  `health_check.down` metric, and emits a `HealthCheckUnhealthy` event.
- **Unhealthy → Healthy**: After `healthy_threshold` consecutive successful
  checks, the backend is marked UP. Sōzu logs an info message, increments the
  `health_check.up` metric, and emits a `HealthCheckHealthy` event.

In `HTTP` mode, a check is considered successful when the response status code
is accepted:

- when `accepted_statuses` is set, the status must fall in one of its entries;
- otherwise, if `expected_status` is `0` (the default), any 2xx status code
  (200–299) is accepted, and any other value accepts exactly that status.

The status judged is the final one: interim `1xx` responses (`100 Continue`,
`103 Early Hints`, RFC 9110 §15.2) are skipped, on HTTP/1.1 and h2c alike, so
accepting `1xx` or `any` never lets an interim response stand in for a final
500. `101 Switching Protocols` is final. The probe buffers at most 4096 bytes
of response before reaching its verdict, interim responses included: a backend
that sends, say, a `103 Early Hints` with more than about 4 KB of headers
before its final status fails the probe.

Setting both a non-zero `expected_status` and `accepted_statuses` is refused
when the configuration is loaded or sent, so one never silently overrides the
other.

A check fails when:

- The TCP connection cannot be established
- The check times out (`TCP` mode: the connection is not established within
  `timeout` seconds; `HTTP` mode: no response within `timeout` seconds)
- `HTTP` mode only: the response status code is not accepted

### Effect on load balancing

Unhealthy backends are skipped during backend selection. They remain registered
in the cluster — Sōzu continues to health-check them, and they are automatically
reintroduced into the pool once they recover.

## Configuration

### Configuration file (TOML)

Add a `health_check` section to any cluster definition:

```toml
[clusters.my-cluster]
protocol = "http"
frontends = [
    { address = "0.0.0.0:8080", hostname = "example.com" }
]
backends = [
    { address = "127.0.0.1:3000" },
    { address = "127.0.0.1:3001" },
]

[clusters.my-cluster.health_check]
uri = "/health"
interval = 10
timeout = 5
healthy_threshold = 3
unhealthy_threshold = 3
expected_status = 0
```

A TCP connect probe needs no `uri`:

```toml
[clusters.my-cluster.health_check]
mode = "TCP"
interval = 10
timeout = 5
```

An HTTP probe on a dedicated endpoint that counts every non-5xx answer as
healthy:

```toml
[clusters.my-cluster.health_check]
uri = "/livez"
accepted_statuses = ["200-499"]
```

A probe on a port other than the backends' data port, here an HTTP health
endpoint on port 9090 of each backend:

```toml
[clusters.my-cluster.health_check]
uri = "/health"
port = 9090
```

A probe that sends an `Authorization` header whose value is read from a file
(see [Custom request headers](#custom-request-headers)):

```toml
[clusters.my-cluster.health_check]
uri = "/health"

[[clusters.my-cluster.health_check.headers]]
name = "Authorization"
value_file = "/run/secrets/sozu-probe-authorization"
```

### Configuration parameters

| Parameter             | Type     | Default  | Description                                                                                                     |
| --------------------- | -------- | -------- | --------------------------------------------------------------------------------------------------------------- |
| `mode`                | string   | `"HTTP"` | `"HTTP"` probes `uri` and judges the status; `"TCP"` only checks that the TCP connection is established.        |
| `uri`                 | string   | `"/"`    | `HTTP` mode: the path to request (e.g. `/livez`, `/healthz`, `/readyz`). Must start with `/`. Ignored in `TCP`. |
| `interval`            | u32      | `10`     | Seconds between check cycles for this cluster.                                                                  |
| `timeout`             | u32      | `5`      | Seconds to wait for the connection (`TCP`) or the response (`HTTP`) before marking the check as failed.         |
| `healthy_threshold`   | u32      | `3`      | Consecutive successes required to transition from unhealthy to healthy.                                         |
| `unhealthy_threshold` | u32      | `3`      | Consecutive failures required to transition from healthy to unhealthy.                                          |
| `expected_status`     | u32      | `0`      | `HTTP` mode, when `accepted_statuses` is empty: `0` accepts any 2xx, any other value exactly that status. Must stay `0` in `TCP` mode. |
| `accepted_statuses`   | [string] | `[]`     | `HTTP` mode: the accepted statuses (see below). Exclusive with a non-zero `expected_status`; refused in `TCP` mode. |
| `port`                | u32      | —        | Port the probe dials on the backend's IP address, in `1-65535`, in both modes. Unset = the backend's data port. Refused with a `unix_path` backend in the cluster. |
| `headers`             | [table]  | `[]`     | `HTTP` mode: extra request headers, each `{ name, value_file }`, sent in order (see [Custom request headers](#custom-request-headers)). Refused in `TCP` mode. |

Each `accepted_statuses` entry is one of:

| Entry       | Accepts                                                       |
| ----------- | ------------------------------------------------------------- |
| `"404"`     | exactly that status                                           |
| `"200-399"` | the inclusive range                                           |
| `"2xx"`     | the class, `1xx` to `5xx`                                     |
| `"any"`     | every valid status, `100-599`: any well-formed HTTP response  |

Every bound must lie in `100-599` (RFC 9110 §15). In the protocol
(`HealthCheckConfig`), `mode` is field 7 (`HealthCheckMode`) and
`accepted_statuses` field 8, a list of inclusive `HttpStatusRange { start, end }`;
`port` is the optional field 9, absent when unset;
`uri` stays a required field on the wire and may be empty in `TCP` mode. A
message without field 7 decodes as `HTTP`, so configurations written before
the modes existed keep their meaning. `headers` is field 10, a list of
`HealthCheckHeader { name, fingerprint }`: names and fingerprints only, never a
value.

In TOML, `mode` is written in upper case, `"HTTP"` or `"TCP"`, and the
spelling is case-sensitive like every other protocol enum key (`shard_mode`,
`udp.health.mode`); the CLI takes `--mode http|tcp`. Do not confuse it with the
UDP cluster health check, whose `[clusters.<id>.udp.health] mode` takes
`"TCP_PROBE"`, `"UDP_PROBE"` or `"HEALTH_OFF"`.

In `HTTP` mode the probe wire format follows the cluster's `http2` flag. Setting
`[clusters.<id>] http2 = true` switches both the data-plane backend connection
and the health-check probe to HTTP/2 prior-knowledge in lockstep, so an h2c-only
backend is never probed with HTTP/1.1 (and vice versa).

### Command line

Health checks can be managed at runtime using `sozu cluster health-check`:

#### Set or update a health check

```bash
sozu cluster health-check set \
    --id my-cluster \
    --uri /health \
    --interval 10 \
    --probe-timeout 5 \
    --healthy-threshold 3 \
    --unhealthy-threshold 3 \
    --expected-status 0

# TCP connect probe
sozu cluster health-check set --id my-cluster --mode tcp

# HTTP probe accepting every non-5xx answer
sozu cluster health-check set --id my-cluster --uri /livez --accepted-statuses 200-499

# TCP connect probe on port 9090 of each backend instead of its data port
sozu cluster health-check set --id my-cluster --mode tcp --port 9090

# HTTP probe sending an Authorization header whose value is read from a file
sozu cluster health-check set --id my-cluster --uri /health \
    --header-file Authorization=/run/secrets/sozu-probe-authorization
```

Creates or replaces the health check configuration for the given cluster. Only
`--id` is required — all other flags have sensible defaults (shown above).

Each probe snapshots its policy when it is launched. `SetHealthCheck` validates
and stores the replacement policy, then acknowledges the command without
cancelling or rewriting probes already in flight. Those probes keep their old
mode and request, timeout, accepted-status rule, and healthy/unhealthy thresholds
through completion; their result can still update the backend's health after the
new policy has been acknowledged. Only probes launched afterwards use the new
policy. The acknowledgement is therefore a draining policy boundary, not an
atomic cutover of work already in flight.

Updating a policy does not itself launch a probe or reset the last launch time.
The new interval controls when the next probe becomes eligible relative to that
existing launch time. Removing a health check has the stronger cancellation and
reset semantics described in [Remove a health check](#remove-a-health-check).
Removing a backend discards the result of its probes still in flight, as
described in [How it works](#how-it-works).

The probe timeout is `--probe-timeout`, in seconds. `--timeout` (`-t`) is the
global `sozu` command timeout, in milliseconds, on this subcommand as on every
other: `sozu cluster health-check set --timeout 5 …` waits 5 ms for the answer
and leaves the probe timeout at its default. Before `--probe-timeout` existed
the two flags collided and every `health-check set` invocation panicked.

| Flag                    | Required | Default | Description                                                                                |
| ----------------------- | -------- | ------- | ------------------------------------------------------------------------------------------ |
| `--id`, `-i`            | yes      | —       | Cluster ID to configure.                                                                   |
| `--mode`                | no       | `http`  | `http` or `tcp` (see [Configuration parameters](#configuration-parameters)).               |
| `--uri`, `-u`           | no       | `/`     | HTTP path to request (e.g. `/health`). Ignored in `tcp` mode.                              |
| `--interval`            | no       | `10`    | Seconds between check cycles.                                                              |
| `--probe-timeout`       | no       | `5`     | Seconds before a check is considered failed.                                               |
| `--healthy-threshold`   | no       | `3`     | Consecutive successes to mark a backend UP.                                                |
| `--unhealthy-threshold` | no       | `3`     | Consecutive failures to mark a backend DOWN.                                               |
| `--expected-status`     | no       | `0`     | Expected HTTP status code (`0` = any 2xx). Exclusive with `--accepted-statuses`.           |
| `--accepted-statuses`   | no       | —       | Comma-separated accepted statuses: codes, ranges, classes or `any` (e.g. `200-399,404`).   |
| `--port`                | no       | —       | Port the probe dials on the backend's IP address, `1-65535`. Unset = the data port. Refused with `unix_path`. |
| `--header-file`         | no       | —       | `NAME=PATH`: send the header `NAME` on every HTTP probe, its value read from the file `PATH` (`-` reads standard input, at most once). Repeatable, up to 32. See [Custom request headers](#custom-request-headers). |

#### List health check configurations

```bash
# List all configured health checks
sozu cluster health-check list

# Filter by cluster ID
sozu cluster health-check list --id my-cluster
```

Example output:

```
┌────────────┬──────┬──────┬─────────┬──────────┬─────────┬───────────────────┬─────────────────────┬───────────────────┬───────────────────────────────────────────────────┐
│ cluster    │ mode │ port │ uri     │ interval │ timeout │ healthy threshold │ unhealthy threshold │ accepted statuses │ headers                                           │
├────────────┼──────┼──────┼─────────┼──────────┼─────────┼───────────────────┼─────────────────────┼───────────────────┼───────────────────────────────────────────────────┤
│ api        │ http │ data │ /ready  │ 5s       │ 3s      │ 2                 │ 5                   │ 200               │ Authorization: sozu-hc-hmac-sha256:v1:3f9a…:c41e… │
│            │      │      │         │          │         │                   │                     │                   │ X-Probe-Token: sozu-hc-hmac-sha256:v1:3f9a…:08d7… │
│ apps       │ tcp  │ 9090 │ -       │ 10s      │ 5s      │ 3                 │ 3                   │ -                 │ -                                                 │
│ my-cluster │ http │ data │ /health │ 10s      │ 5s      │ 3                 │ 3                   │ 2xx               │ -                                                 │
│ web        │ http │ data │ /livez  │ 10s      │ 5s      │ 3                 │ 3                   │ 200-399,404       │ -                                                 │
└────────────┴──────┴──────┴─────────┴──────────┴─────────┴───────────────────┴─────────────────────┴───────────────────┴───────────────────────────────────────────────────┘
```

The `port` column shows the probe `port`, or `data` when the probe dials each
backend's data port. The `accepted statuses` column uses the
`--accepted-statuses` syntax; a legacy `expected_status` shows as `2xx` (for
`0`) or as its code. The `headers` column lists, in sending order, each custom
header name with the fingerprint of its value, or `-` when there is none; a
header named without any fingerprint, for example by the configuration file
before its value was provided, shows as `<name>: (no fingerprint)`. It never
shows a value (see
[Custom request headers](#custom-request-headers)).

When `--id` is provided, only the health check for that cluster is shown. When
omitted, all clusters with a health check are listed. Clusters without a health
check configured do not appear.

The output is also available as JSON when using
`sozu --json cluster health-check list`.

#### Remove a health check

```bash
sozu cluster health-check remove --id my-cluster
```

Stops health checking for the given cluster. All backends in the cluster are
reset to healthy and resume receiving traffic immediately, including those a
probe had marked DOWN; probes still in flight are dropped, so none can mark a
backend DOWN after the reset. Adding the cluster again without a health check
(`sozu cluster add` on the existing id) has the same effect. The protected
header values held for the cluster are dropped with the health check.

## Custom request headers

An `HTTP` health check can send extra request headers on every probe, typically
an `Authorization` header for a health endpoint that requires one. They apply to
the HTTP/1.1 probe and to the h2c probe alike. `TCP` mode sends nothing, so a
header is refused there.

A header value is a credential. It is never written in the configuration, the
saved state, a query result or a log: the configuration names the header and
points at a file (`value_file`) or standard input, and Sōzu keeps the value in
memory only. What you can see of a header is its name and a keyed fingerprint of
its value.

### Declare the headers

In the configuration file, one `[[clusters.<id>.health_check.headers]]` table
per header, in the order they must be sent:

```toml
[clusters.my-cluster.health_check]
uri = "/health"

[[clusters.my-cluster.health_check.headers]]
name = "Authorization"
value_file = "/run/secrets/sozu-probe-authorization"

[[clusters.my-cluster.health_check.headers]]
name = "X-Probe-Token"
value_file = "/run/secrets/sozu-probe-token"
```

Each table takes exactly `name` and `value_file`; there is no inline `value`
key, and an unknown key is refused. The configuration stores the `value_file`
reference as written: parsing it validates the header names and performs no I/O
on the file, and the diagnostics for a bad table are static, so they never echo
a value or the content of a file. Write absolute paths; no relative-path
resolution is defined for the reference. The file is opened only when the
protected activation reads the value, and then with the operating system's
ordinary behaviour (permissions, symbolic links). For `sozu cluster
health-check set --header-file`, that is after the CLI's support and validation
gates described below. For a `value_file` declared in the configuration, the
hardened main process may read it into protected memory before it forks or
launches the workers, and the activation then goes through the same
acknowledged exchange with them; the main process does not run the typed
capability query first.

On the command line, repeat `--header-file NAME=PATH`:

```bash
sozu cluster health-check set --id my-cluster --uri /health \
    --header-file Authorization=/run/secrets/sozu-probe-authorization \
    --header-file X-Probe-Token=/run/secrets/sozu-probe-token

# the value of one header from standard input: `-` as the path, at most once
generate-probe-credential | sozu cluster health-check set --id my-cluster \
    --header-file Authorization=-
```

`NAME=PATH` is split at the first `=`, so a path may contain one. `PATH=-`
reads standard input, which can be the source of one header only. Opening a
file follows the operating system's ordinary behaviour (permissions,
symbolic links): Sōzu adds no restriction on the file type, owner or mode, so
keep each file readable only by the account that runs the command.

### What is accepted

| Rule | Detail |
| ---- | ------ |
| Order | Headers are sent in declaration order: the order of the TOML tables, or of the `--header-file` flags. The order is kept in the state and in queries. |
| Count | At most 32 headers. |
| Names | A non-empty RFC 9110 token. Names are unique, compared case-insensitively. Refused: names that begin with `:` and `Host`, `Connection`, `Keep-Alive`, `Proxy-Connection`, `Transfer-Encoding`, `Content-Length`, `TE`, `Trailer`, `Upgrade`, `Expect`, which the probe owns or which change the framing. |
| Mode | `HTTP` only. Headers in a `TCP` health check are refused. |
| Values | Valid UTF-8, with no control byte (a horizontal tab inside the value is allowed), and no leading or trailing space or tab. |
| Line ending | The reader removes one final `LF` and the `CR` right before it, when present, as an editor or `echo` adds them. It trims nothing else, and a lone final `CR` is not a line ending. A value that then starts or ends with a space or a tab, or holds a control byte, is refused, not rewritten. |

Two size limits apply, and they are not interchangeable:

- **Header size, 8192 bytes.** Every header counts its name, its value and 4
  bytes (the `: ` and the line end of an HTTP/1.1 header line); the sum over all
  headers must not exceed 8192. A single value is read up to that same
  bound and refused beyond it.
- **HTTP/2 header block, 16384 bytes.** The h2c probe sends its request in one
  HEADERS frame, which is never split. The encoded block, made of the four
  pseudo-headers (counted with the longest possible `:authority`), the URI and
  the custom headers, must fit in 16384 bytes. It is checked whatever the
  cluster's `http2` flag, so a configuration that is valid for HTTP/1.1 cannot
  become invalid when `http2` is enabled later.

A request that breaks a rule is refused as a whole, with a static message that
never names a value. The CLI checks the names, their number and their order
before it connects, and checks the values once it has read them; the main
process and each worker check the whole request again.

### Capability check before any value is read

The typed capability query before any file or standard-input I/O is a guarantee
of the CLI, not of the main process. `sozu cluster health-check set` with at
least one `--header-file` is a two-step operation implemented by the CLI:

1. It asks the main process whether protected health-check headers are
   supported by the main process and by every worker. This query carries no
   header and no cluster. The CLI waits for the positive answer for a fixed 5
   seconds, even when the global `--timeout` is `0`; progress messages and
   unrelated replies do not extend that deadline.
2. Only after a positive answer does it open the files or read standard input,
   validate the values, and send them in one request.

A missing, malformed, negative or late answer means the CLI reads no file, reads
no standard input and sends no value. This is what protects a rolling upgrade: a
main process or worker older than 2.3.2 cannot answer, so no credential is ever
sent to it. Without `--header-file`, the command is the ordinary `set` described
above, with no capability query.

The main process then applies the change on every worker as an acknowledged
Prepare, Commit, Abort or Release exchange; a worker's acknowledgement gates
each step. When a worker refuses, fails or goes silent before every Commit is
acknowledged, the protected change is aborted, every worker is rolled back, and
the previous health check, with the configuration and state it came from, is
retained or restored. Once every Commit is acknowledged, Release only makes the
workers forget the state the commit replaced: a Release that is refused or
times out is a retention or cleanup failure, reported as such, and it cannot roll
back a before-image that the workers already destroyed. A worker that is
replaced, or respawned automatically, receives the protected values and
acknowledges them before it is marked Running and before it sends any probe.
While a protected change is in flight, other changes to the same cluster's
health check are fenced off and refused: retry once it has finished. This
transaction covers the protected health-check change only; it is not a global
transaction and it does not make Sōzu commands in general transactional.

Applications that talk to the command socket through `sozu-command-lib` instead
of the CLI must run the same capability query before they read or send a value:
the library types do not add that orchestration automatically (see
[command/README.md](../command/README.md#protected-health-check-headers)).

A plain `set` never carries a value. To change or renew protected headers, run
`set` again with the complete `--header-file` list.

### What is stored, shown and logged

| Where | What it holds |
| ----- | ------------- |
| Configuration file | The ordered header name and `value_file` reference: the source of a value, never a literal value. |
| Orders sent to the workers | The header names and fingerprints. The `value_file` references never reach a worker; the values reach it only in the protected change described above. |
| Saved state, `sozu cluster health-check list`, other queries | The ordered header names and, for each, the fingerprint `sozu-hc-hmac-sha256:v1:<epoch>:<tag>`: the full HMAC-SHA256 of the header name and value under a key that only the main process holds, and the epoch (key identifier) it was made under. Never a value, never a source path. |
| Logs, `Debug` output, error messages | Static text. A diagnostic can name a header, never a value or what a file contained. |
| Memory of the main process, of the workers and of the CLI while it reads | The value, in carriers and buffers that refuse to be serialized or printed and that Sōzu zeroes when it releases them. |
| The wire | The value, in clear, on the probe request itself. |

The fingerprint lets you check that two health checks hold the same value, or
that a value changed, without revealing it. Zeroing covers the buffers Sōzu owns;
it cannot reach copies the allocator, the kernel, a socket buffer or swap may
have made. In transit, a value crosses the command socket and the main-to-worker
channels as part of a command message: restrict them with `command_socket_mode`
and `command_allowed_uids`.

Every `sozu` process (main, workers and CLI) sets `RLIMIT_CORE` to 0, soft and
hard, and on Linux marks itself non-dumpable, before it reads any header value,
so a crash leaves no core dump and a same-user `ptrace` or `/proc/<pid>/mem`
access is refused without `CAP_SYS_PTRACE`. These are per-process hardenings,
not host-wide isolation: root, swap and the hypervisor stay outside what Sōzu
can promise.

**The probe is not encrypted.** Protecting the control plane and the state does
not protect the request that carries the header: the health check speaks plain
HTTP/1.1 or h2c to the backend, and HTTPS probes are not implemented. Use a
dedicated, least-privilege credential that is valid for the health endpoint
only, and keep the probe on a private or otherwise protected network path to the
backend.

### After a cold restart

The values are not saved, and a cold `SaveState` never restores clear values. A
cold start of the main process generates a fresh key and a fresh epoch kept in
memory only, so no fingerprint made before the restart can be verified by the
new process; the saved state holds names and fingerprints, never a value.

A saved state is authoritative for startup, for a reload and for `LoadState`,
including `RemoveCluster`. For every cluster it adds, removes or rewrites the
health check of, the same-name `value_file` intent of the configuration is
dropped: a foreign or stale epoch cannot activate a value by carrying the same
cluster name, and a configuration entry for the same name cannot resurrect a
health check that the saved state removed.

When the new TOML supplies `value_file` headers for an existing protected
cluster, the generic `AddCluster` of the reload temporarily carries the existing
complete health check A; only the acknowledged protected transaction publishes
the new health check B. An authoritative reload whose health check has no
headers or `value_file` is not that transition: it removes the old protected
headers and aligns the in-memory journal with the resulting main and worker
state. An explicit `RemoveHealthCheck` or `RemoveCluster` keeps its removal
semantics. This is not a global transaction.

When a saved state is restored and is authoritative for a health check, its
header names and order are kept. If it carries fingerprints of a foreign epoch,
a name alone never activates a value. While the exact ordered metadata lacks its
values, the health check sends no probe, and the complete ordered header list
must match the restored metadata, name for name and fingerprint for fingerprint,
before it does. Each skipped probe is counted by `health_check.missing_secret`;
it is neither a success nor a failure, and the backend routing is not changed by
it. In `health-check list`, the epoch (the third colon-separated field of a
fingerprint) of such a header differs from the one of headers provided since the
start.

There are two ways to provide the values again, and they differ. An operator or
controller can explicitly re-inject the exact ordered headers with
`sozu cluster health-check set ... --header-file ...`; a value is applied
together with the complete health check it belongs to, never because a name
happens to match, and that creates the key and epoch the fingerprints are made
under. The `value_file` intent of the configuration, on the other hand, only
applies to a cluster the saved state leaves alone, as described above. Neither
way writes a value to the saved state.

### Upgrades and rollback

**Hot main upgrade (`sozu upgrade`).** A 2.3.2 main hands its protected state
over to its replacement in upgrade protocol V3: the key with its epoch and the
in-memory values travel with the rest of the handoff, so the fingerprints stay
valid and probes continue with their headers. Nothing is read again from a
`value_file` or from standard input to compensate. A 2.3.2 main writes V3 only;
it never falls back to the V2 handoff of 2.3.1. The V3 handoff is a memory-only
transport: the whole possibly-sensitive snapshot crosses a dedicated Unix socket
as one bounded frame, with no tempfile and no memfd.

A 2.3.2 main still reads the V2 handoff of an incoming 2.3.1 main, and the
legacy handoff of 2.2.1 (the one without a protocol marker). Release 2.3.1 has
no protected headers, so those handoffs hold none: once such an upgrade is
complete, use `health-check set --header-file` to enable them. See
[how_to_use.md](./how_to_use.md#hot-upgrade-protocols-and-compatibility) for the
whole matrix.

An in-flight protected health-check change, or a change fence or version still
unresolved, makes the main refuse `sozu upgrade`: let it finish first, and check
the result of the upgrade before you change protected headers again.

**Mixed fleet.** Upgrade the main process and every worker to 2.3.2 before you
use `--header-file`. The capability check refuses the change while any worker
cannot take part, and no value leaves the CLI. A health check that only names
headers (loaded from a saved state, or set without values) probes nothing until
the values are provided.

**Rollback to 2.3.1.** A 2.3.1 main cannot read a V3 handoff, so `sozu upgrade`
is not a rollback path once 2.3.2 runs, and rolling back is not a hot handoff.
Two different things go wrong, and both need cleanup before a controlled
restart:

- An older peer that decodes the protobuf state or a message ignores the unknown
  `headers` field (10): it probes without the headers and drops them when it
  writes the state back, so a backend that requires the credential may mark its
  probes DOWN.
- The 2.3.1 TOML parser does not understand the new
  `[[clusters.<id>.health_check.headers]]` tables: its health-check table rejects
  unknown keys, so a configuration that still holds them fails to load.

Before going back, set the affected health checks again without `--header-file`
(or remove them), delete the `[[clusters.<id>.health_check.headers]]` tables from
the configuration file, then stop the service and start it on 2.3.1 from that
configuration and its saved state. Provide the protected headers again when you
return to 2.3.2.

## Metrics

Sōzu exposes the following health check metrics:

| Metric                          | Type    | Description                                                                                                                                                                                                                                                                                                                                         |
| ------------------------------- | ------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `health_check.success`          | counter | Total number of successful health check responses.                                                                                                                                                                                                                                                                                                  |
| `health_check.failure`          | counter | Total number of failed health check attempts.                                                                                                                                                                                                                                                                                                       |
| `health_check.missing_secret`   | counter | Probes not sent because the health check names [custom headers](#custom-request-headers) whose values this worker does not hold. Neither a success nor a failure.                                                                                                                                                                                   |
| `health_check.up`               | counter | Number of healthy transitions (unhealthy → healthy).                                                                                                                                                                                                                                                                                                |
| `health_check.down`             | counter | Number of unhealthy transitions (healthy → unhealthy).                                                                                                                                                                                                                                                                                              |
| `health_check.healthy_backends` | gauge   | Healthy backends per cluster — labelled with `cluster_id`. Updated after every probe-result update for clusters with at least one configured backend (including `0` when all backends are unhealthy, so dashboards can detect universal-outage / fail-open). The label was missing in earlier releases and per-cluster values overwrote each other. |

The cross-cluster availability story (`cluster.available_backends`,
`cluster.total_backends`, `cluster.no_available_backends`,
`cluster.available_recovered`, `backend.available`) lives in
[`configure.md` § Cluster availability](configure.md#cluster-availability)
because those signals are not driven exclusively by health checks — they also
fold in retry-policy state observed on the data path. As a consequence the
`cluster.*` surface and the per-backend `backend.available` gauge work
**without** a configured health check: each TCP connect failure on the data path
arms `Backend::retry_policy` — on the HTTP/HTTPS mux and on the raw TCP proxy
alike, including a connect refused after `EINPROGRESS` — and once every backend reaches
`retry_policy.is_down()` the next routing call flips the cluster to `AllDown`
and emits the matching event + log line. Adding an active health check on top is
useful when a cluster is idle (no requests means no passive observations) but is
not required for the surface to function.

## Events

Health state transitions emit events that can be consumed by subscribers (via
`sozu events subscribe`):

- `HealthCheckHealthy` — a backend transitioned to healthy
- `HealthCheckUnhealthy` — a backend transitioned to unhealthy
- `NoAvailableBackends` — a cluster transitioned to `Available → AllDown` (every
  backend fails the availability predicate; not exclusive to health-check
  failure — retry-policy backoff also counts)
- `ClusterRecovered` — a cluster transitioned back to `AllDown → Available`
  (proto tag 29). Pairs with `NoAvailableBackends` so subscribers track all-down
  and recovery without polling

The first two events include the `cluster_id`, `backend_id`, and backend
`address`. The cluster-availability events carry only `cluster_id` (backend
identity is moot — the event is about the cluster as a whole).

## Design considerations

- **Non-blocking**: Health checks share the mio event loop with normal proxy
  operations. There are no additional threads, and checks never block request
  processing.
- **Per-cluster configuration**: Each cluster can have its own probe mode, port,
  URI, interval, thresholds, and accepted statuses. Clusters without a `health_check`
  section are not checked.
- **Fail-open routing when ALL backends are unhealthy**: when every backend in a
  cluster has been marked DOWN by the threshold state machine, Sōzu falls back
  to routing across the `Normal` backends whose retry policy allows a try
  instead of returning 503 — see `BackendList::select_tiers` in
  `lib/src/backends.rs`. The Amazon health-check
  paper's reasoning (returning 503 is rarely the right answer when health-check
  signal itself may be wrong) drives this. A `warn!("fail-open: ...")` is logged
  when fail-open kicks in, and the `health_check.healthy_backends` gauge drops
  to 0. Use conservative thresholds and the `health_check.down` counter to alert
  on sustained outages.
- **Health checks and request failover are complementary**: without a health
  check, a backend is taken out of rotation only by the requests that fail to
  connect to it — refused, timed out on `connect_timeout`, or reporting a
  socket error. Each such request is retried on another backend within its
  `max_connection_attempts` (5 by default), and the failure puts the backend
  in its retry policy's back-off, but every return from back-off costs one
  request a connect attempt, and a blackholed backend a full `connect_timeout`.
  A health check finds it without spending requests. See "Backend connection
  failover" in [configure.md](./configure.md)
  ([#1800](https://github.com/sozu-proxy/sozu/issues/1800)).
- **`TCP` mode sends nothing**: the connect-only probe never writes or reads,
  so it works for any TCP backend, HTTP or not, and cannot be confused by the
  application's answer. It proves only that the backend accepts connections:
  an application that accepts and then answers errors stays healthy.
- **Wire format follows `cluster.http2`** (`HTTP` mode): when `cluster.http2 = false` (the
  default) the probe sends a plain-text HTTP/1.1 request. When
  `cluster.http2 = true` the probe sends the HTTP/2 connection preface, an empty
  client SETTINGS frame, and a single HEADERS frame on stream 1 carrying
  `GET <uri>` with `END_STREAM | END_HEADERS`. The response parser walks the H2
  frames looking for a HEADERS frame on stream 1 and decodes the `:status`
  pseudo-header to determine probe outcome; a GOAWAY frame on the connection is
  treated as a probe failure. The probe and the data-plane backend connection
  share the same `cluster.http2` switch so they cannot diverge. HTTPS probes (h2
  over TLS) are still not implemented; backends that only accept TLS connections
  need either a co-located HTTP/1.1 health endpoint or no `health_check`
  configuration today.
- **No persistent state**: Health check state is held in memory. On worker
  restart, all backends start as healthy and must fail enough consecutive checks
  to be marked down. Protected header values are never saved either; see
  [After a cold restart](#after-a-cold-restart).
