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
| `health_check.success` | counter | Passed health-check probes: an accepted HTTP status, or an established connection in `TCP` mode. Not labelled by cluster or backend. |
| `health_check.failure` | counter | Failed health-check probes (connect error, timeout, HTTP status not accepted). |
| `health_check.up` | counter | Backend transitions to healthy after `healthy_threshold` consecutive successes. |
| `health_check.down` | counter | Backend transitions to unhealthy after `unhealthy_threshold` consecutive failures. |
| `health_check.healthy_backends` | gauge | Healthy-backend count per cluster, emitted on every health-check result update for clusters with at least one configured backend — including `0` when all backends are unhealthy. Pair with `backends.fail_open` to detect universal-outage / fail-open routing on dashboards. |
| `backends.fail_open` | counter | Incremented per routing decision that landed on the fail-open path, i.e., when no backend passed the regular `can_open()` gate and the load balancer fell back to backends in `Normal` status with retry policy `OKAY`. The companion `warn!` log fires only on regime entry/exit, so this counter is the operator-visible per-request signal. |
| `backend.shard.spillover` | counter | Shuffle sharding `FALLBACK`, per cluster: a selection whose client shard had no backend able to take a connection went to a backend outside the shard (a fail-open pick that lands back inside the shard is not counted). A sustained rate means some shards are down. |
| `backend.shard.exhausted` | counter | Shuffle sharding `STRICT`, per cluster: a selection whose client shard had no backend able to take a connection selected none (HTTP 503, TCP close, UDP flow not admitted). |

## Retrieving Metrics

```bash
sozu -c /etc/config.toml metrics get
```
