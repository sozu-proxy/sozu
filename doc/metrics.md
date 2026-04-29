# Key Metrics to Monitor

Sōzu exposes the following metrics for monitoring:

| Metric | Type | Description |
|--------|------|-------------|
| `sozu.http.requests` | counter | Total HTTP request count |
| `sozu.http.errors` | counter | Failed requests (parse errors + 4xx/5xx) |
| `sozu.client.connections` | gauge | Active frontend connections |
| `sozu.backend.connections` | gauge | Active backend connections |
| `sozu.buffer.number` | gauge | Active buffers from the buffer pool |
| `sozu.slab.entries` | gauge | Slab allocator usage (session slots) |
| `sozu.zombies` | gauge | Zombie sessions detected (indicates bugs — should be 0) |
| `health_check.success` | counter | Successful health-check probe responses (per cluster, when emitted with labels). |
| `health_check.failure` | counter | Failed health-check probes (connect error, timeout, status mismatch). |
| `health_check.up` | counter | Backend transitions to healthy after `healthy_threshold` consecutive successes. |
| `health_check.down` | counter | Backend transitions to unhealthy after `unhealthy_threshold` consecutive failures. |
| `health_check.healthy_backends` | gauge | Healthy-backend count for clusters with at least one healthy backend; emitted when ≤50% of backends are healthy. Drops to 0 when fail-open kicks in. |

## Retrieving Metrics

```bash
sozu -c /etc/config.toml metrics get
```
