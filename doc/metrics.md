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

## Retrieving Metrics

```bash
sozu -c /etc/config.toml metrics get
```
