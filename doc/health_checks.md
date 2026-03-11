# Health Checks

Sōzu supports active HTTP health checks for backend servers. When configured on a cluster, Sōzu periodically sends HTTP requests to each backend and tracks whether they respond successfully. Backends that fail consecutive checks are marked as unhealthy and excluded from load balancing. Once they start responding again, they are marked healthy and traffic resumes.

## How it works

Health checks run inside the main mio event loop using non-blocking TCP connections. There is no separate thread or process — checks are interleaved with normal request processing.

For each cluster with a health check configured, Sōzu performs the following on every check cycle:

1. Opens a non-blocking TCP connection to each backend in the cluster
2. Sends a minimal `GET <uri> HTTP/1.1` request with `Connection: close`
3. Reads the response status line
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

## Events

Health state transitions emit events that can be consumed by subscribers (via `sozu events subscribe`):

- `HealthCheckHealthy` — a backend transitioned to healthy
- `HealthCheckUnhealthy` — a backend transitioned to unhealthy

Both events include the `cluster_id`, `backend_id`, and backend `address`.

## Design considerations

- **Non-blocking**: Health checks share the mio event loop with normal proxy operations. There are no additional threads, and checks never block request processing.
- **Per-cluster configuration**: Each cluster can have its own health check URI, interval, thresholds, and expected status. Clusters without a `health_check` section are not checked.
- **No fail-open (yet)**: If all backends in a cluster become unhealthy, Sōzu will **not** route traffic to them — the cluster effectively becomes unavailable. A fail-open mechanism (continuing to route when all backends are down) is planned for a future release. Until then, use conservative thresholds to avoid false positives.
- **HTTP only**: Health checks use plain-text HTTP over TCP. Backends that only accept TLS connections will fail health checks. HTTPS health check support is not yet implemented.
- **No persistent state**: Health check state is held in memory. On worker restart, all backends start as healthy and must fail enough consecutive checks to be marked down.
