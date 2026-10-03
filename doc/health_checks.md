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

## How it works

Health checks run inside the main mio event loop using non-blocking TCP
connections. There is no separate thread or process — checks are interleaved
with normal request processing.

For each cluster with a health check configured, Sōzu performs the following on
every check cycle:

1. Opens a non-blocking TCP connection to each backend in the cluster. In `TCP`
   mode the check ends here: it passes once the connection is established
   (the socket becomes writable with no pending error) and fails on a
   connection error or when `timeout` elapses first
2. Sends a probe whose wire format follows the cluster's `http2` flag: HTTP/1.1
   (`GET <uri> HTTP/1.1` with `Connection: close`) when `cluster.http2 = false`
   (the default), or HTTP/2 prior-knowledge (connection preface + empty
   SETTINGS + HEADERS frame on stream 1 carrying `GET <uri>`) when
   `cluster.http2 = true`
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
`uri` stays a required field on the wire and may be empty in `TCP` mode. A
message without field 7 decodes as `HTTP`, so configurations written before
the modes existed keep their meaning.

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

#### List health check configurations

```bash
# List all configured health checks
sozu cluster health-check list

# Filter by cluster ID
sozu cluster health-check list --id my-cluster
```

Example output:

```
┌────────────┬──────┬─────────┬──────────┬─────────┬───────────────────┬─────────────────────┬───────────────────┐
│ cluster    │ mode │ uri     │ interval │ timeout │ healthy threshold │ unhealthy threshold │ accepted statuses │
├────────────┼──────┼─────────┼──────────┼─────────┼───────────────────┼─────────────────────┼───────────────────┤
│ api        │ http │ /ready  │ 5s       │ 3s      │ 2                 │ 5                   │ 200               │
│ apps       │ tcp  │ -       │ 10s      │ 5s      │ 3                 │ 3                   │ -                 │
│ my-cluster │ http │ /health │ 10s      │ 5s      │ 3                 │ 3                   │ 2xx               │
│ web        │ http │ /livez  │ 10s      │ 5s      │ 3                 │ 3                   │ 200-399,404       │
└────────────┴──────┴─────────┴──────────┴─────────┴───────────────────┴─────────────────────┴───────────────────┘
```

The `accepted statuses` column uses the `--accepted-statuses` syntax; a legacy
`expected_status` shows as `2xx` (for `0`) or as its code.

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
(`sozu cluster add` on the existing id) has the same effect.

## Metrics

Sōzu exposes the following health check metrics:

| Metric                          | Type    | Description                                                                                                                                                                                                                                                                                                                                         |
| ------------------------------- | ------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `health_check.success`          | counter | Total number of successful health check responses.                                                                                                                                                                                                                                                                                                  |
| `health_check.failure`          | counter | Total number of failed health check attempts.                                                                                                                                                                                                                                                                                                       |
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
- **Per-cluster configuration**: Each cluster can have its own probe mode, URI,
  interval, thresholds, and accepted statuses. Clusters without a `health_check`
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
  to be marked down.
