# Sōzu rate-limit design

**Status:** design draft. Addresses issues [#890](https://github.com/sozu-proxy/sozu/issues/890) (generic rate-limiting) and [#1057](https://github.com/sozu-proxy/sozu/issues/1057) (per-source-IP DoS protection). Committed to the 2026-04-21 H2 triage plan as bucket **B3-x** with scope decision *"both vectors"*.

> **Not yet implemented.** This document defines the shape of the feature so a follow-up PR can land code that matches the operator expectations captured here. The existing H2 flood detectors (`H2FloodDetector` in `lib/src/protocol/mux/h2.rs`) cover a **different layer** — per-connection protocol abuse — and are intentionally not part of this design.

---

## 1. Problem statement

Two related but distinct abuse vectors are not mitigated on current `feat/h2-mux`:

1. **Per-tenant rate limiting (#890).** A legitimate client burst pattern can saturate a single cluster at the frontend before the backend's own queue saturates. Operators want a soft-quota signal ("back off but stay connected") rather than a TCP-level reject, and want per-tenant granularity so one noisy tenant does not starve others on the same listener.
2. **Per-source-IP DoS protection (#1057).** Mass connection attempts from a single source saturate the accept queue long before `max_connections` fires. The accept-queue-timeout fallback evicts old SYNs but does not distinguish attacker from legitimate traffic; a slow-SYN flood from 10 k IPs each holding 100 half-open connections defeats it entirely.

The user-selected scope (2026-04-22) is **both vectors**, not either/or:

- Per-tenant soft quotas → 429 response, RFC 6585.
- Per-source-IP hard caps → `SocketResult::Closed` at accept time + metric, pre-TLS, pre-accept-queue.

## 2. Non-goals

- Not a replacement for an upstream WAF (Cloudflare, ModSecurity, `sozu-wasm-plugins`). Sōzu's rate-limiter is a defence-in-depth floor, not a policy engine.
- No cross-worker / cross-process state. Each worker enforces its own quota independently. Sync via shared state is explicitly deferred (see §6).
- Not a rate limit on individual HTTP methods or paths. Path/method-scoped limits belong in `router` middleware, not the listener.
- Does not touch the H2 flood detector (`H2FloodDetector`). Protocol-layer abuse stays separate.

## 3. Per-tenant soft quota (#890) — **Layer 7**

### 3.1 Data model

```rust
// command/src/command.proto (listener-level or cluster-level)
message RateLimitRule {
  required string  name        = 1;          // operator label
  required uint32  requests    = 2;          // burst count (token bucket capacity)
  required uint32  per_seconds = 3;          // refill period
  repeated string  match_tenants = 4;        // cluster IDs or `*` wildcard
  optional uint32  wait_hint_seconds = 5;    // Retry-After hint, default 1
}
```

Per listener, a `Vec<RateLimitRule>`. At request time, `router.rs::frontend_from_request` resolves the cluster, then `rate_limit_check(cluster_id, tenant_bucket)` runs **before** backend selection.

### 3.2 Algorithm

Token bucket, per `(rule_name, tenant_key)` pair. Tenant key derivation in order of preference:
1. If the request carries a `X-Forwarded-For` header that Sōzu trusts (listener config `trust_forwarded_for = true`), use the left-most IP.
2. Otherwise, use the connection peer IP.
3. For internal clusters with a stable identity header (`X-Tenant-Id`, configurable), use that.

The per-bucket state is `(tokens: f32, last_refill: Instant)`. Refill rate `requests / per_seconds` tokens/sec, capped at `requests` tokens. Each accepted request consumes 1 token. Tokens below 1 → reject.

### 3.3 Reject behaviour

- Emit a **`429 Too Many Requests`** response via the existing `answers::HttpAnswers` machinery, with:
  - `Retry-After: <wait_hint_seconds>` header.
  - `X-Sozu-RateLimit-Rule: <rule_name>` header (observability).
  - Response body: configurable template (HTML + plaintext fallback) per existing answers pattern.
- `incr!("rate_limit.rejected.l7.{rule_name}")` + `incr!("rate_limit.rejected.l7.total")`.
- `warn!` with `log_context!` carrying session + rule + tenant key (truncated to avoid PII leak).
- Connection stays open — soft quota. Keep-alive pipelining continues normally.

### 3.4 Storage

Per-worker `HashMap<(Arc<str>, RateLimitTenantKey), TokenBucket>` where `RateLimitTenantKey` is an enum over IP / string. LRU eviction at 10 k entries per worker (configurable); entry expiry 10× `per_seconds` to reclaim memory from departed tenants.

### 3.5 Config surface

```toml
[[listeners.rate_limits]]
name = "default-ingress"
requests = 100
per_seconds = 1
match_tenants = ["*"]
wait_hint_seconds = 1

[[listeners.rate_limits]]
name = "api-strict"
requests = 20
per_seconds = 1
match_tenants = ["api-v1", "api-v2"]
wait_hint_seconds = 5
```

## 4. Per-source-IP hard cap (#1057) — **Layer 4**

### 4.1 Scope

Enforced at `Server::accept`, before TLS handshake and before the accept queue. A source IP exceeding `max_connections_per_source_ip` is rejected with a TCP RST (same as `check_limits` in `lib/src/server.rs`). No 429 — this is an infrastructure-layer guard, not a user-visible policy.

### 4.2 Data model

```rust
// command/src/command.proto
message Config {
  // ...
  optional uint32 max_connections_per_source_ip = N;       // default: 0 (disabled)
  optional uint32 connections_per_source_ip_window_seconds = N + 1;  // default: 60
}
```

Global config, not per-listener. A single source IP can open at most `max_connections_per_source_ip` concurrent connections across ALL listeners. Time window: `connections_per_source_ip_window_seconds` for the count to decay (sliding window).

### 4.3 Algorithm

Counter at `Sessions::nb_connections_per_source_ip: HashMap<IpAddr, (u32, Instant)>`. On accept:

```rust
fn accept_pre_check(&mut self, peer: SocketAddr) -> Result<(), AcceptError> {
    let ip = peer.ip();
    let now = Instant::now();
    let entry = self.nb_connections_per_source_ip.entry(ip).or_insert((0, now));
    // Decay: zero out counts older than the window.
    if now.duration_since(entry.1) > self.connections_per_source_ip_window {
        entry.0 = 0;
        entry.1 = now;
    }
    if entry.0 >= self.max_connections_per_source_ip {
        incr!("rate_limit.rejected.l4.per_source_ip");
        return Err(AcceptError::PerSourceIpCap);
    }
    entry.0 = entry.0.saturating_add(1);
    Ok(())
}
```

Counter decrements on session close (`Sessions::decr`) — need to extend `decr` to also know the source IP.

### 4.4 Storage discipline

HashMap capped at 64 k entries per worker (configurable). When the cap is hit, evict LRU. Eviction bumps `rate_limit.per_source_ip.evictions` metric — if it fires frequently, it's a sign of a botnet-scale attack and operators should consider moving protection upstream (CDN, cloud DoS shield).

Memory: 64 k entries × ~48 bytes (IpAddr + counter + timestamp + HashMap overhead) ≈ 3 MB worst-case per worker — acceptable.

### 4.5 Interaction with `max_connections`

Per-source-IP cap is checked BEFORE `check_limits`. A source IP below its cap still contributes to the global `nb_connections`, so `check_limits` fires normally when the listener saturates. The two limits compose; operators should set `max_connections_per_source_ip << max_connections` (typical ratio: 1:100).

## 5. Metrics

| Key | Type | Meaning |
|---|---|---|
| `rate_limit.rejected.l7.total` | counter | All L7 (429) rejects, all rules |
| `rate_limit.rejected.l7.{rule_name}` | counter | Per-rule rejects |
| `rate_limit.rejected.l4.per_source_ip` | counter | L4 IP-cap rejects |
| `rate_limit.l7.active_buckets` | gauge | Live token-bucket entries in the L7 HashMap |
| `rate_limit.l4.active_ips` | gauge | Live source-IP entries in the L4 HashMap |
| `rate_limit.l4.evictions` | counter | LRU evictions from the L4 map |
| `rate_limit.l7.evictions` | counter | LRU evictions from the L7 map |

## 6. Deferred / future work

- **Cross-worker sync.** The current design is per-worker. A 4-worker deployment with `max_connections_per_source_ip = 100` effectively allows 400 per source IP. A shared-state variant (command socket broadcast, or external Redis/memcached backend) is a separate feature request.
- **Per-route rate limits.** Path- or method-scoped limits belong in a router-layer middleware. Not in this design.
- **Adaptive quotas.** Auto-scaling `requests` based on backend health is out of scope; operators should tune statically.
- **Ban list integration.** Integration with an external ban-list API (abuseipdb, Clever Cloud internal allow/deny) is a follow-up; the L4 hook makes it straightforward (check ban-list AFTER the cap check).

## 7. Rollout plan

1. Land the proto + config + reject-machinery skeleton first (no enforcement). Verify config parse + display.
2. Land L4 (per-source-IP) enforcement. Small, testable, high-value for botnet scenarios.
3. Land L7 (per-tenant 429). Larger surface; needs the `answers` template work + e2e.
4. Document in `doc/configure.md` with TOML examples.
5. Enable by default with generous defaults (e.g. `max_connections_per_source_ip = 1000` at window 60s) OR keep disabled by default with clear docs. Decision point before landing step 2.

## 8. Risks & open questions

| Risk | Mitigation |
|---|---|
| HashMap unbounded growth | 64 k cap + LRU eviction; metrics surface the condition |
| Legitimate CGI spike rejected | `wait_hint_seconds` tuning + operator guidance in docs |
| Per-worker staleness (4 workers × 25% saturation = global saturation but no reject) | Documented trade-off; cross-worker sync deferred |
| TCP RST burst visible to legitimate users behind a shared NAT | L4 cap tuning + docs warning about NAT environments |
| Token bucket timekeeping `Instant::now()` overhead | Already used elsewhere in sozu (`session.created_at` etc.); benchmark at implementation time |

**Open for user:**

1. Should L4 be enabled by default (with a safe default like 1000 conns/IP) or opt-in?
2. Should the L7 rule key default to connection peer IP, or require `trust_forwarded_for = true` explicitly when behind another LB?
3. Is cross-worker sync worth a follow-up ticket now, or deferred indefinitely until an operator hits the limitation?

---

*Written by `team-lead` on feat/h2-mux post-merge sweep 2026-04-22. Filed per user directive `feedback_do_not_create_issues_unless_asked.md` — design lives in-tree, no GitHub issue opened.*
