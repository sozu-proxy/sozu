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
`UpdateTcpListenerConfig` — the payloads of
`RequestType::UpdateHttpListener`, `RequestType::UpdateHttpsListener` and
`RequestType::UpdateTcpListener`, dispatched in
`bin/src/command/requests.rs`). The semantic is
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
patch. See `CommandManager::cluster_h2_command`
(`bin/src/ctl/request_builder.rs`):

1. The CLI emits a `QueryClusterById(my-cluster)` request and waits
   synchronously for the master's response (`request_builder.rs:388-389`).
2. It locates the matching `ClusterInformation` in the response and
   extracts the current `ClusterConfiguration`
   (`request_builder.rs:391-397`).
3. It rewrites the `http2` field on the extracted configuration and
   re-submits as a full `AddCluster(updated)`
   (`request_builder.rs:399-404`). The supervisor treats `AddCluster`
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
message length. The upper bound rejects any peer-sent length that
exceeds the configured `max_command_buffer_size` — **not**
`command_buffer_size` — so a malformed worker cannot trick the
supervisor into allocating a multi-gigabyte read buffer.
`Channel::try_read_delimited_message` compares the declared
`message_len` against the channel's private `max_buffer_size` field,
and `Channel::write_delimited_message` (reached from `write_message`)
bounds an outgoing frame against that same field. Every *production*
construction site populates it from `max_command_buffer_size`:
`bin/src/command/server.rs`, `bin/src/worker.rs`, `bin/src/ctl/mod.rs`
and `bin/src/upgrade.rs`. No other construction site says anything
about the shipped bound: the unit tests in `command/src/channel.rs`,
`lib/src/http.rs`, `lib/src/server.rs` and `lib/src/tcp.rs`, and all
three example programs under `lib/examples/` — `http.rs`, `https.rs`
and `tcp.rs` — pass literal sizes, while the end-to-end harness
(`Worker::create_server` and `Worker::spawn_worker` in
`e2e/src/sozu/worker.rs`) does build its channels from
`command_buffer_size` and `max_command_buffer_size`, but from a
configuration the test run assembles rather than a deployed one.

The two keys are distinct, default independently, and are not
interchangeable:

| key | role | default |
| --- | --- | --- |
| `command_buffer_size` | initial capacity a channel buffer is allocated at, and the capacity it shrinks back to once drained — on the supervisor↔worker channels (`bin/src/worker.rs`, `bin/src/upgrade.rs`, `CommandHub::from_upgrade_data`) and on the CLI's own channel (`bin/src/ctl/mod.rs`). It does **not** size the command-socket client channel: `CommandHub::register_client` allocates that one at a hardcoded `4096` and only its ceiling comes from configuration | `DEFAULT_COMMAND_BUFFER_SIZE`, `1_000_000` bytes |
| `max_command_buffer_size` | ceiling the buffers may grow to by doubling, **and** the bound a peer-declared `message_len` is rejected against | `DEFAULT_MAX_COMMAND_BUFFER_SIZE`, `2_000_000` bytes |

`command_buffer_size` must never exceed `max_command_buffer_size`: the
first is the capacity a channel buffer is configured to start at and to
shrink back to once drained, the second is the ceiling that buffer may
never grow past, so a pair in the other order asks for a starting
buffer larger than the bound that must contain it. Check the two values
against each other whenever you change either.

So raise `max_command_buffer_size` (in tandem on both ends) if you
introduce a verb whose payload genuinely exceeds the cap. Lowering
`command_buffer_size` tightens nothing — it only makes the channel
start smaller and grow more often. Note also that all four
configuration files shipped in this repository — `bin/config.toml`,
`os-build/config.toml`, `command/assets/config.toml` and
`.github/workflows/bench.toml` — set both keys explicitly and below
these defaults, so read the values actually in force rather than
assuming the built-in ones.

What the supervisor does with a frame the cap rejects is deliberately
not stated here: the handling of `MessageTooLarge` on the command
channel is the subject of the open issue sozu-proxy/sozu#1428, and this
section will name the behaviour once that is settled.

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

Source: `HttpsSession::upgrade_handshake` (`lib/src/https.rs:483`).
Documented in `doc/configure.md:933`.

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
