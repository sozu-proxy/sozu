# Sōzu — Operational Reconfiguration Guide

This document covers the runtime knobs operators reach for during
incidents and maintenance, as worked examples driven from the `sozu` CLI.
It is the operational counterpart to [`configure.md`](configure.md): that
file is the per-field reference; this one walks through the verbs in the
shape an operator runs them, including the per-field "preserved on omit"
semantics introduced with the `Update*Listener` verbs.

For data-plane internals see
[`../lib/src/protocol/mux/LIFECYCLE.md`](../lib/src/protocol/mux/LIFECYCLE.md);
for the supervisor side see
[`../bin/src/command/LIFECYCLE.md`](../bin/src/command/LIFECYCLE.md).

---

## 1. Patchable Listener Fields

Every `sozu listener {http,https,tcp} update` invocation produces an
`Update*Listener` request type
(`UpdateHttpListenerConfig` / `UpdateHttpsListenerConfig` /
`UpdateTcpListenerConfig` — see
`bin/src/command/requests.rs:33` for the imports). The semantic is
**preserve on omit**: every CLI flag you do not pass keeps its current
value on the worker side, so an update is a true patch rather than a full
replacement.

A condensed table — see [`configure.md`](configure.md) for the full per-field
reference, including defaults, mutability class, and metric impact:

| Field                                  | Listener kinds | Preserved on omit | Notes                                                      |
|----------------------------------------|----------------|-------------------|-------------------------------------------------------------|
| `front_timeout` / `back_timeout`       | http, https, tcp | yes              | Seconds; only affects sessions accepted after the patch     |
| `connect_timeout`                      | http, https, tcp | yes              | Seconds                                                     |
| `request_timeout`                      | http, https     | yes              |                                                              |
| `disable_http11`                       | https          | yes               | Affects new handshakes only                                 |
| `alpn_protocols`                       | https          | yes               | Use `--reset-alpn` to restore `["h2", "http/1.1"]` default  |
| `strict_sni_binding`                   | https          | yes               | `:authority` covered by served cert SANs (CWE-346 / CWE-444) |
| `sozu_id_header`                       | http, https    | yes               | Rebranding the per-request correlation header               |
| H2 flood thresholds (`h2_max_*`)       | https          | yes               | Per-connection setup; new connections only                  |
| `h2_stream_idle_timeout_seconds`       | https          | yes               | Slow-multiplex Slowloris defence                            |
| `h2_max_header_table_size`             | https          | yes               | HPACK dynamic-table cap                                     |
| `h2_stream_shrink_ratio`               | https          | yes               | Per-connection scratch-Vec shrink threshold                 |
| `expect_proxy`                         | tcp            | yes               | PROXY-v2 ingress                                            |

CLI-level flag-to-field mapping lives in
`bin/src/cli.rs` and `bin/src/ctl/request_builder.rs`.

---

## 2. Worked Example — Tighten H2 Flood Thresholds Under Attack

When a Rapid Reset (CVE-2023-44487) signature shows up in the metrics
dashboard, halve the per-window cap on RST_STREAM and the lifetime
abusive cap without restarting any worker:

```bash
# Inspect current per-listener thresholds first.
sozu listener list

# Halve the Rapid Reset budget on the HTTPS listener.
sozu listener https update -a 0.0.0.0:8443 \
    --h2-max-rst-stream-per-window 50 \
    --h2-max-rst-stream-abusive-lifetime 25

# Confirm the patch landed.
sozu listener list
```

Existing H2 connections keep the thresholds they were accepted with —
flood detectors are wired at connection setup and never re-read. New
connections opened after the patch acknowledge see the tighter limits.

The CONTINUATION-flood cap (CVE-2024-27316) and the server-emitted
RST_STREAM cap (CVE-2025-8671) follow the same pattern with their own
`--h2-max-continuation-frames` and `--h2-max-rst-stream-emitted-lifetime`
flags. See `configure.md:560-567` for the catalogue.

The relevant counters to watch on the receiving side are
`h2.flood.violation.<kind>` (per CVE) and
`h2.{goaway,rst_stream}.{sent,received}.<code>` (for error attribution
of the GOAWAY / RST_STREAM emitted in response).

---

## 3. Worked Example — Toggle `disable_http11` Per Listener

`disable_http11` is the per-listener kill switch for HTTP/1.1
connections. When `true`, the rustls handshake refuses any client whose
ALPN offer does not include `h2` (or who omits ALPN entirely). The
metrics fired on the refusal path are documented in §5 below.

```bash
# Force every new client on the HTTPS listener to negotiate h2.
sozu listener https update -a 0.0.0.0:8443 --disable-http11

# Revert — allow HTTP/1.1 fallback again.
sozu listener https update -a 0.0.0.0:8443 --enable-http11
```

In-flight HTTPS handshakes complete on the rustls config they started
with; the new policy applies to fresh handshakes only. Existing HTTP/1.1
sessions on this listener continue until they close.

---

## 4. Worked Example — `cluster h2 enable|disable`

The `sozu cluster h2 enable | disable` verb toggles
`Cluster::http2`, the backend-capability hint that drives "should the
proxy attempt H2 to this cluster's backends". On `feat/h2-mux` Sōzu
still speaks H1 to backends, so this knob is forward-looking — it does
NOT gate frontend H2, which is driven entirely by TLS ALPN.

```bash
# Mark a cluster as H2-capable on the backend side.
sozu cluster h2 enable --id my-cluster

# Revert.
sozu cluster h2 disable --id my-cluster
```

Behaviourally this is a **query-then-resubmit** dance, not a partial
patch. See `bin/src/ctl/request_builder.rs:214-238`:

1. The CLI emits a `QueryClusterById(my-cluster)` request and waits
   synchronously for the master's response (`request_builder.rs:220-221`).
2. It locates the matching `ClusterInformation` in the response and
   extracts the current `ClusterConfiguration`
   (`request_builder.rs:222-240`).
3. It rewrites the `http2` field on the extracted configuration and
   re-submits as a full `AddCluster(updated)`
   (`request_builder.rs:242-247`). The supervisor treats `AddCluster`
   as upsert, so this acts as a targeted edit even though no dedicated
   "patch cluster" verb exists.

The implication for operators: an `AddCluster` configuration that did
not originate from this query-then-resubmit dance — for example one
loaded from a state file — replaces the cluster entirely. To apply a
partial cluster change without writing a custom client, follow the same
two-step query-then-resubmit pattern.

---

## 5. Behaviours Added on `feat/h2-mux`

These items were introduced during the `feat/h2-mux` branch and are
documented here as **operational mechanism** — what they do, when they
fire, what to monitor — rather than re-stating the per-field reference
already in `configure.md`.

### 5.1 Per-connection H2 stream-Vec shrink

Commit: `e478cf8b`. Reference: `doc/configure.md:442, 458, 570`.

Each `ConnectionH2` keeps a `Vec<Stream>` of per-stream slots in the
mux `Context`. Recycled slots accumulate over the connection's lifetime;
without bounded shrink-back the Vec stays at peak watermark even after
the workload drops to a handful of in-flight streams.

`h2_stream_shrink_ratio` (default `2`, minimum `2`) controls the
shrink-back threshold: the Vec is shrunk when
`total_slots > active_streams * ratio`. Tighten the ratio under
memory-pressure investigations; loosen it on bursty topologies that
prefer to keep the slots warm.

What to monitor: the Vec's effective size is not yet a public metric —
the symptom is RSS growth on long-lived connections proportional to
peak stream concurrency. If an operator suspects the shrink-back is
mis-sized, capture an RSS sample, force a reduction in client
concurrency, and re-sample after one second.

### 5.2 Channel `message_len` upper bound

Commit: `18c251f1`. Reference: `command/src/channel.rs`.

The supervisor↔worker `Channel` framing carries a `usize`-prefixed
message length. The new upper bound rejects any peer-sent length that
exceeds the configured `command_buffer_size`, so a malformed worker
cannot trick the supervisor into allocating a multi-gigabyte read
buffer. The default `command_buffer_size = 16384` is generous for the
verbs Sōzu speaks today; raise it (in tandem on both ends) if you
introduce a verb whose payload genuinely exceeds the cap.

When the cap is exceeded the supervisor logs and drops the offending
peer's session; the worker observes the drop as an EOF on its channel
and is restarted by the supervisor's standard worker-respawn path
(when `worker_automatic_restart` is enabled).

### 5.3 Drop-on-register-fail for the unix command socket

Commit: `b8c8fc61`. Reference:
[`../bin/src/command/LIFECYCLE.md`](../bin/src/command/LIFECYCLE.md) §2.5.

If `mio::Registry::register` fails for a freshly accepted command-socket
client (slab exhaustion, FD invalid), the supervisor now treats the
failure as terminal: log with the token + error, drop the
`UnixStream`, send RST/EOF to the peer. Previously the failure was
swallowed and the unwired session sat in the client map until manual
cleanup, leaking a slab token per occurrence.

What to monitor: a sudden burst of "register failure" warnings in the
supervisor log usually indicates either slab exhaustion (raise the
slab budget) or an FD leak elsewhere; the dropped clients are now
observable as RST close events on the peer side.

### 5.4 SNI trailing-dot normalisation

Commit: `c5fe3655`.

The TLS handshake path normalises a trailing dot on the
client-supplied SNI before it is matched against the configured
certificate map. Without this, a client sending
`SNI = host.example.com.` (a fully-qualified DNS form) would miss the
`host.example.com` certificate and trigger an ALPN-or-cert mismatch
that closes the handshake. The fix strips a single trailing dot before
the match.

The new counter `https.alpn.rejected.unsupported` (see §5.5 below)
makes the previously-untracked "unknown ALPN protocol" refusal path
observable; it is documented here so the operator's "ALPN refusal"
ratebar matches the sum of the labelled buckets.

### 5.5 `https.alpn.rejected.unsupported` counter

Source: `lib/src/https.rs:359`. Documented in `doc/configure.md:933`.

Fires on the rustls accept path when the negotiated ALPN protocol is
not one of the explicitly handled values (`h2`, `http/1.1`, or absent).
This branch was previously silent — any operator dashboard graphing
`https.alpn.rejected.*` missed unknown-protocol refusals (e.g. an `h3`
mistake bleeding through some misconfiguration). Add the counter to
the same alerting bucket as `https.alpn.rejected.http11_disabled`.

### 5.6 `ensure_frame_size!` macro

Source: `lib/src/protocol/mux/parser.rs`.

Internal hardening note: the H2 frame parser previously open-coded the
"is this fixed-size frame the right length?" check at every fixed-size
frame site. The macro consolidates the check so a future fixed-size
frame addition cannot regress against the length-confusion family of
CVEs that motivated the consolidation. Operators see no behavioural
change; the practical effect is that
[`fuzz_frame_parser`](../fuzz/README.md#21-fuzz_frame_parser) has a
single chokepoint to assert against.

---

## 6. Cross-References

- [`configure.md`](configure.md) — per-field reference for every
  configurable knob (defaults, mutability, validators, metric impact).
- [`../lib/src/protocol/mux/LIFECYCLE.md`](../lib/src/protocol/mux/LIFECYCLE.md)
  — H2 session and stream lifecycle internals.
- [`../bin/src/command/LIFECYCLE.md`](../bin/src/command/LIFECYCLE.md)
  — supervisor / command-socket lifecycle.
- [`observability.md`](observability.md) — log envelopes, audit log
  fields, metric reference.
- [`../fuzz/README.md`](../fuzz/README.md) — H2 framing + HPACK
  fuzzing harnesses.
