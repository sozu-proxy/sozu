# Health Checks

Sōzu supports active HTTP health checks for backend servers. When configured on a cluster, Sōzu periodically sends HTTP requests to each backend and tracks whether they respond successfully. Backends that fail consecutive checks are marked as unhealthy and excluded from load balancing. Once they start responding again, they are marked healthy and traffic resumes.

## How it works

Health checks run inside the main mio event loop using non-blocking TCP connections. There is no separate thread or process — checks are interleaved with normal request processing.

For each cluster with a health check configured, Sōzu performs the following on every check cycle:

1. Opens a non-blocking TCP connection to each backend in the cluster
2. Sends a probe whose wire format follows the cluster's `http2` flag: HTTP/1.1 (`GET <uri> HTTP/1.1` with `Connection: close`) when `cluster.http2 = false` (the default), or HTTP/2 prior-knowledge (connection preface + empty SETTINGS + HEADERS frame on stream 1 carrying `GET <uri>`) when `cluster.http2 = true`
3. Reads the response: HTTP/1.1 status line for the default path, or HTTP/2 frames until a HEADERS frame on stream 1 yields `:status` for h2c
4. Compares the HTTP status code against the expected value
5. Updates the backend's health state based on success or failure

### Health state machine

Each backend maintains a `HealthState` with counters for consecutive successes and failures:

- **Healthy → Unhealthy**: After `unhealthy_threshold` consecutive failed checks, the backend is marked DOWN. Sōzu logs an error, increments the `health_check.down` metric, and emits a `HealthCheckUnhealthy` event.
- **Unhealthy → Healthy**: After `healthy_threshold` consecutive successful checks, the backend is marked UP. Sōzu logs an info message, increments the `health_check.up` metric, and emits a `HealthCheckHealthy` event.

A check is considered successful when the HTTP response status code matches the `expected_status`. If `expected_status` is `0` (the default), any 2xx status code (200–299) is accepted.

A check fails when:
- The TCP connection cannot be established
- The request times out (no response within `timeout` seconds)
- The response status code does not match the expected value

### Effect on load balancing

Unhealthy backends are skipped during backend selection. They remain registered in the cluster — Sōzu continues to health-check them, and they are automatically reintroduced into the pool once they recover.

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

### Configuration parameters

| Parameter             | Type   | Default | Description                                                                 |
|-----------------------|--------|---------|-----------------------------------------------------------------------------|
| `uri`                 | string | —       | **Required.** The HTTP path to request (e.g. `/health`, `/ready`, `/ping`). |
| `interval`            | u32    | `10`    | Seconds between check cycles for this cluster.                              |
| `timeout`             | u32    | `5`     | Seconds to wait for a response before marking the check as failed.          |
| `healthy_threshold`   | u32    | `3`     | Consecutive successes required to transition from unhealthy to healthy.      |
| `unhealthy_threshold` | u32    | `3`     | Consecutive failures required to transition from healthy to unhealthy.       |
| `expected_status`     | u32    | `0`     | Expected HTTP status code. `0` means any 2xx is accepted.                   |

The probe wire format follows the cluster's `http2` flag. Setting `[clusters.<id>] http2 = true` switches both the data-plane backend connection and the health-check probe to HTTP/2 prior-knowledge in lockstep, so an h2c-only backend is never probed with HTTP/1.1 (and vice versa).

### Command line

Health checks can be managed at runtime using `sozu cluster health-check`:

#### Set or update a health check

```bash
sozu cluster health-check set \
    --id my-cluster \
    --uri /health \
    --interval 10 \
    --timeout 5 \
    --healthy-threshold 3 \
    --unhealthy-threshold 3 \
    --expected-status 0
```

Creates or replaces the health check configuration for the given cluster. Only `--id` and `--uri` are required — all other flags have sensible defaults (shown above).

| Flag                    | Required | Default | Description                                      |
|-------------------------|----------|---------|--------------------------------------------------|
| `--id`, `-i`            | yes      | —       | Cluster ID to configure.                         |
| `--uri`, `-u`           | yes      | —       | HTTP path to request (e.g. `/health`).           |
| `--interval`            | no       | `10`    | Seconds between check cycles.                    |
| `--timeout`             | no       | `5`     | Seconds before a check is considered failed.     |
| `--healthy-threshold`   | no       | `3`     | Consecutive successes to mark a backend UP.      |
| `--unhealthy-threshold` | no       | `3`     | Consecutive failures to mark a backend DOWN.     |
| `--expected-status`     | no       | `0`     | Expected HTTP status code (`0` = any 2xx).       |

#### List health check configurations

```bash
# List all configured health checks
sozu cluster health-check list

# Filter by cluster ID
sozu cluster health-check list --id my-cluster
```

Example output:

```
┌────────────┬─────────┬──────────┬─────────┬───────────────────┬─────────────────────┬─────────────────┐
│ cluster    │ uri     │ interval │ timeout │ healthy threshold │ unhealthy threshold │ expected status │
├────────────┼─────────┼──────────┼─────────┼───────────────────┼─────────────────────┼─────────────────┤
│ my-cluster │ /health │ 10s      │ 5s      │ 3                 │ 3                   │ any 2xx         │
│ api        │ /ready  │ 5s       │ 3s      │ 2                 │ 5                   │ 200             │
└────────────┴─────────┴──────────┴─────────┴───────────────────┴─────────────────────┴─────────────────┘
```

When `--id` is provided, only the health check for that cluster is shown. When omitted, all clusters with a health check are listed. Clusters without a health check configured do not appear.

The output is also available as JSON when using `sozu --json cluster health-check list`.

#### Remove a health check

```bash
sozu cluster health-check remove --id my-cluster
```

Stops health checking for the given cluster. All backends in the cluster are reset to healthy and resume receiving traffic immediately.

## Metrics

Sōzu exposes the following health check metrics:

| Metric                  | Type    | Description                                        |
|-------------------------|---------|----------------------------------------------------|
| `health_check.success`  | counter | Total number of successful health check responses.  |
| `health_check.failure`  | counter | Total number of failed health check attempts.       |
| `health_check.up`       | counter | Number of healthy transitions (unhealthy → healthy).|
| `health_check.down`     | counter | Number of unhealthy transitions (healthy → unhealthy).|
| `health_check.healthy_backends` | gauge | Healthy backends per cluster — labelled with `cluster_id`. Updated after every probe-result update for clusters with at least one configured backend (including `0` when all backends are unhealthy, so dashboards can detect universal-outage / fail-open). The label was missing in earlier releases and per-cluster values overwrote each other. |

The cross-cluster availability story (`cluster.available_backends`,
`cluster.total_backends`, `cluster.no_available_backends`,
`cluster.available_recovered`, `backend.available`) lives in
[`configure.md` § Cluster availability](configure.md#cluster-availability)
because those signals are not driven exclusively by health checks — they
also fold in retry-policy state observed on the data path.

## Events

Health state transitions emit events that can be consumed by subscribers (via `sozu events subscribe`):

- `HealthCheckHealthy` — a backend transitioned to healthy
- `HealthCheckUnhealthy` — a backend transitioned to unhealthy
- `NoAvailableBackends` — a cluster transitioned to `Available → AllDown` (every backend fails the availability predicate; not exclusive to health-check failure — retry-policy backoff also counts)
- `ClusterRecovered` — a cluster transitioned back to `AllDown → Available` (proto tag 29). Pairs with `NoAvailableBackends` so subscribers track all-down and recovery without polling

The first two events include the `cluster_id`, `backend_id`, and backend
`address`. The cluster-availability events carry only `cluster_id`
(backend identity is moot — the event is about the cluster as a whole).

## Design considerations

- **Non-blocking**: Health checks share the mio event loop with normal proxy operations. There are no additional threads, and checks never block request processing.
- **Per-cluster configuration**: Each cluster can have its own health check URI, interval, thresholds, and expected status. Clusters without a `health_check` section are not checked.
- **Fail-open routing when ALL backends are unhealthy**: when every backend in a cluster has been marked DOWN by the threshold state machine, Sōzu falls back to routing across all `Normal` backends instead of returning 503 — see `lib/src/load_balancing.rs::BackendList::healthy`. The Amazon health-check paper's reasoning (returning 503 is rarely the right answer when health-check signal itself may be wrong) drives this. A `warn!("fail-open: ...")` is logged when fail-open kicks in, and the `health_check.healthy_backends` gauge drops to 0. Use conservative thresholds and the `health_check.down` counter to alert on sustained outages.
- **Wire format follows `cluster.http2`**: when `cluster.http2 = false` (the default) the probe sends a plain-text HTTP/1.1 request. When `cluster.http2 = true` the probe sends the HTTP/2 connection preface, an empty client SETTINGS frame, and a single HEADERS frame on stream 1 carrying `GET <uri>` with `END_STREAM | END_HEADERS`. The response parser walks the H2 frames looking for a HEADERS frame on stream 1 and decodes the `:status` pseudo-header to determine probe outcome; a GOAWAY frame on the connection is treated as a probe failure. The probe and the data-plane backend connection share the same `cluster.http2` switch so they cannot diverge. HTTPS probes (h2 over TLS) are still not implemented; backends that only accept TLS connections need either a co-located HTTP/1.1 health endpoint or no `health_check` configuration today.
- **No persistent state**: Health check state is held in memory. On worker restart, all backends start as healthy and must fail enough consecutive checks to be marked down.
