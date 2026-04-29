# Configure Sōzu

> Before a deep dive in the configuration part of the proxy, you should take a look at the [getting started documentation](./getting_started.md) if you haven't yet.

## Configuration file

> The configuration file uses the [.toml](https://github.com/toml-lang/toml) format.

Sōzu configuration process involves 3 major sources of parameters:

- The `global` section, which sets process-wide parameters.
- The definition of the protocols like `https`, `http`, `tcp`.
- The clusters sections under: `[clusters]`.

### Global parameters

Parameters in the global section allow you to define the global settings shared by the main process and workers (like the log level):

| parameter                  | description                                                                         | possible values                          |
|----------------------------|:------------------------------------------------------------------------------------|------------------------------------------|
| `saved_state`              | path from which sozu tries to load its state at startup                             |                                          |
| `automatic_state_save`     | if `saved_state` is set, persists state to it whenever a command changes routing. Defaults to `false` | `true`, `false`              |
| `log_level`                | possible values are                                                                 | `debug`, `trace`, `error`, `warn`, `info`|
| `log_target`               | possible values are                                                                 | `stdout, tcp or udp address`             |
| `log_colored`              | emit ANSI colour codes on the main log stream. Only honoured when `log_target` is `stdout` (tcp / udp / file sinks ignore this flag). Defaults to `false` | `true`, `false` |
| `access_logs_target`        | possible values are (if activated, sends access logs to a separate target)          | `stdout`, `tcp` or `udp address`         |
| `access_logs_format`       | wire format of access logs. Defaults to `ascii`                                     | `ascii`, `protobuf`                      |
| `access_logs_colored`      | emit ANSI colour codes on access logs. Only honoured when the access-log target is `stdout`. If unset, inherits `log_colored`. Defaults to `false` | `true`, `false` |
| `command_socket`           | path to the unix socket command                  |                                          |
| `command_buffer_size`      | size, in bytes, of the buffer used by the main process to handle commands.          |                                          |
| `max_command_buffer_size`  | maximum size of the buffer used by the main process to handle commands.             |                                          |
| `worker_count`             | number of workers                                                                   |                                          |
| `worker_automatic_restart` | if activated, workers that panicked or crashed are restarted (activated by default) |                                          |
| `worker_timeout`           | maximum time (in seconds) the main process waits for a worker reply before marking it `NotAnswering`. Defaults to `10`                                   | seconds |
| `disable_cluster_metrics`  | if `true`, per-cluster metrics are not registered. Defaults to `false` (cluster metrics enabled)                                                        | `true`, `false` |
| `handle_process_affinity`  | bind workers to cpu cores.                                                          |                                          |
| `max_connections`          | maximum number of simultaneous / opened connections                                 |                                          |
| `max_buffers`              | maximum number of buffers use to proxying                                           |                                          |
| `min_buffers`              | minimum number of buffers preallocated for proxying                                 |                                          |
| `buffer_size`              | size, in bytes, of requests buffer used by the workers. Must be at least 16393 for HTTP/2 (16384 max frame size + 9 byte frame header) |                                          |
| `slab_entries_per_connection` | how many slab entries each `max_connections` reserves. Defaults to 4 (1 frontend + up to 3 backend H2 connections). Raise for fan-out topologies that exceed 4 backends per session; clamped to [2, 32]. Slab capacity is `10 + slab_entries_per_connection * max_connections`. | integer 2-32 |
| `splice_pipe_capacity_bytes` | requested kernel-pipe capacity, in bytes, per `splice(2)` direction on `Protocol::TCP` listeners (Linux only, requires the `splice` cargo feature). Omitted or `None` keeps the kernel default of 64 KiB. Applied via `fcntl(F_SETPIPE_SZ)` per pipe at session start; the kernel rounds up to a page boundary and clamps at `/proc/sys/fs/pipe-max-size` (default 1 MiB unprivileged; CAP_SYS_RESOURCE goes higher). The realised capacity is read back via `fcntl(F_GETPIPE_SZ)` and used as the per-call `len` for `splice_in`. Larger values amortise syscalls and reduce wakeups for bulk-transfer workloads at the cost of per-session pinned memory; raise `/proc/sys/fs/pipe-max-size` first if you want above 1 MiB without root. | integer (bytes), e.g. `262144` |
| `command_allowed_uids`     | optional allowlist of POSIX UIDs permitted to invoke command-socket requests (PR #1209). Omitted or `None` preserves the historical "any same-UID local process" behaviour. Set to `[<operator_uid>]` to restrict mutating verbs to a specific UID even when other same-UID daemons coexist (CI runners, monitoring scrapers). Rejected requests still appear in the audit trail. | TOML array of integers, e.g. `[1000]` |
| `ctl_command_timeout`      | maximum time the command line will wait for a command to complete                            |                                          |
| `pid_file_path`            | stores the pid in a specific file location                                          |                                          |
| `front_timeout`            | maximum time of inactivity for a front socket                                       |                                          |
| `back_timeout`             | maximum time of inactivity for a backend socket (seconds). Defaults to `30`. Can be overridden per listener.                                            | seconds |
| `connect_timeout`          | maximum time of inactivity for a request to connect                                 |                                          |
| `accept_queue_timeout`     | maximum time (in seconds) a TCP connection stays in sozu's accept queue before being dropped. Defaults to `60`.                                         | seconds |
| `request_timeout`          | maximum time of inactivity for a request                                            |                                          |
| `zombie_check_interval`    | duration between checks for zombie sessions                                         |                                          |
| `evict_on_queue_full`      | evict the least-recently-active sessions when `max_connections` is reached, making room for new accepts. Defaults to `false`: during a DDoS the existing connections are more likely to be legitimate clients than the queued ones, so refusing new accepts is the safer mitigation. Enable when overload is dominated by normal traffic spikes. Triggers a config-load `warn!` when `max_connections < 100` because the 1% eviction batch clamps to 1 (so the per-round share grows). | `true`, `false` (default: `false`) |
| `activate_listeners`       | automatically start listeners                                                       |                                          |
| `max_connections_per_ip`   | global default per-(cluster, source-IP) connection limit. `0` disables the feature (default). The source IP is taken from the parsed PROXY-protocol header when present, else `peer_addr`. HTTP/HTTPS clients hitting the limit receive `429 Too Many Requests`; TCP clients see a graceful FIN. Each cluster may override via its own `max_connections_per_ip` field (`None` inherits, `Some(0)` is explicit unlimited, `Some(n > 0)` overrides). Counters are kept per `(cluster_id, source_ip)`, so two clusters never share a counter. | integer (0 = unlimited) |
| `retry_after`              | global default `Retry-After` header value (seconds) for HTTP 429 responses. `0` omits the header — `Retry-After: 0` invites an immediate retry that defeats the limit. Each cluster may override. TCP listeners ignore this value (no HTTP envelope). | integer (0 = omit) |

_Example:_

```toml
command_socket = "./command_folder/sock"
saved_state = "./state.json"
log_level = "info"
log_target = "stdout"
command_buffer_size = 16384
worker_count = 2
handle_process_affinity = false
max_connections = 500
max_buffers = 500
buffer_size = 16393
activate_listeners = true
```

#### Privacy note on logs and retention

`log_target` at `debug`/`trace` emits per-connection context on every protocol
log line, including:

- peer IP (client source address)
- SNI (TLS hostname the client requested) on TLS listeners
- a per-session ULID generated at connection accept
- mio frontend token and cluster/backend identifiers

Each of these is an identifier under most data-protection regimes (GDPR, CNIL
guidance on proxy logs). Keep production workers at `log_level = "info"` or
tighter unless debugging; access logs (`access_logs_target`) carry the same
fields in a shape meant for long-term retention and should be the durable
store.

Retention of the live log stream depends on the sink:

- `stdout`: inherited from the surrounding process supervisor (journald, the
  init system, the container runtime). On Clever Cloud ADCs the journald cap
  is size-bounded at ~4 GB — time coverage varies with traffic.
- `tcp://` / `udp://`: forwarded to the remote collector; retention becomes
  the collector's responsibility. Confirm DPA coverage before routing logs
  to a third party.

If logs egress beyond Clever Cloud infrastructure, the per-session ULID plus
peer IP plus SNI combination is a durable cross-system correlator — treat it
accordingly in your privacy impact assessment.

### Listeners

The _listener_ section describes a set of listening sockets accepting client connections.
You can define as many listeners as you want.
They follow the format:

_General parameters:_

```toml
[[listeners]]
# possible values are http, https or tcp
protocol = "http"
# listening address
address = "0.0.0.0:8080"
# address = "[::]:8080"

# specify a different IP than the one the socket sees, for logs and forwarded headers
# public_address = "1.2.3.4:80

# Configures the client socket to receive a PROXY protocol header
# expect_proxy = false
```

#### Options specific to HTTP and HTTPS listeners

Since version 1.0.0, Sōzu allows custom HTTP answers defined for HTTP and HTTPS listeners.

These answers are customizable:

  - 301 Moved Permanently
  - 400 Bad Request
  - 401 Unauthorized
  - 404 Not Found
  - 408 Request Timeout
  - 413 Payload Too Large
  - 502 Bad Gateway
  - 503 Service Unavailable
  - 504 Gateway Timeout
  - 507 Insufficient Storage

These answers are to be provided in plain text files of whichever extension (we recommend `.http`
for clarity) and may look like this:

```html
HTTP/1.1 404 Not Found
Cache-Control: no-cache
Connection: close
Sozu-Id: %REQUEST_ID

<style>pre{background:#EEE;padding:10px;border:1px solid #AAA;border-radius: 5px;}</style>
<h1>404 Not Found</h1>

<p>insert your custom text here, in fact, all HTML is changeable, including the CSS.</p>

<pre>
{
    \"route\": \"%ROUTE\",
    \"request_id\": \"%REQUEST_ID\",
}
</pre>
<footer>This is an automatic answer by Sozu.</footer>",
```

There are a number of available template variables, like `REQUEST_ID` or `CLUSTER_ID`, that will be replaced
by the proxying logic when producing the error.

To create your own custom HTTP answers, we highly suggest you first copy the default answers present
in `lib/src/protocol/kawa_h1/answers.rs`, and then change them to your liking. Feel free to remove the
`\r` newlines of the default strings for clarity.
Sōzu will parse your file and replace whatever newline symbol(s) you use.

Then, for each listener, provide the absolute paths of each custom answer.

```toml
# a 404 response is sent when sozu does not know about the requested domain or path
answer_404 = "/path/to/my-404-answer.http"
# a 503 response is sent if there are no backend servers available
answer_503 = "/path/to/my-503-answer.http"
# answer_507 = ...
```

If a frontend has a `sticky_session`, the sticky name is defined at the listener level.

```toml
# defines the sticky session cookie's name, if `sticky_session` is activated format
# a cluster. Defaults to "SOZUBALANCEID"
sticky_name = "SOZUBALANCEID"
```

#### Options specific to HTTPS listeners

> **Crypto provider**: The cryptographic backend used by Rustls is a compile-time choice
> (feature flags `crypto-ring`, `crypto-aws-lc-rs`, `crypto-openssl`, `fips`).
> It cannot be changed at runtime. See [Getting started — Choosing a crypto provider](./getting_started.md#choosing-a-crypto-provider) for build instructions.

```toml
# supported TLS versions. Possible values are "SSL_V2", "SSL_V3",
# "TLS_V12", "TLS_V13". Defaults to "TLS_V12" and "TLS_V13"
tls_versions = ["TLS_V12", "TLS_V13"]
```

#### Options specific to Rustls based HTTPS listeners

##### Cipher suites

```toml
# Sets the list of available cipher suites, in order of preference.
# If omitted, the following default list is used (ANSSI-recommended order):
cipher_list = [
    # TLS 1.3 cipher suites
    "TLS13_AES_256_GCM_SHA384",
    "TLS13_AES_128_GCM_SHA256",
    "TLS13_CHACHA20_POLY1305_SHA256",
    # TLS 1.2 cipher suites
    "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384",
    "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256",
    "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256",
    "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384",
    "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256",
    "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256",
]
```

#### HTTP/2 support and ALPN protocols

HTTP/2 is available on HTTPS listeners through ALPN (Application-Layer Protocol
Negotiation). During the TLS handshake, Sōzu advertises protocols from the `alpn_protocols`
list. The server selects the first protocol from its list that the client also supports.

By default, Sōzu advertises both `h2` and `http/1.1`, preferring HTTP/2:

```toml
# Default: both protocols, H2 preferred
alpn_protocols = ["h2", "http/1.1"]
```

| Value | Protocol | Notes |
|-------|----------|-------|
| `h2` | HTTP/2 | Multiplexed, binary framing (RFC 9113) |
| `http/1.1` | HTTP/1.1 | Traditional text-based protocol (RFC 9112) |

Invalid values are rejected at configuration load time. Order matters: the first entry
is the most preferred protocol.

**Examples:**

```toml
# HTTP/1.1 only — disables HTTP/2 on this listener
alpn_protocols = ["http/1.1"]

# HTTP/2 only — clients without H2 support will fail TLS negotiation
alpn_protocols = ["h2"]

# Prefer HTTP/1.1 over HTTP/2
alpn_protocols = ["http/1.1", "h2"]
```

When `alpn_protocols` is omitted or empty, the default `["h2", "http/1.1"]` is used.
Clients that do not send an ALPN extension default to HTTP/1.1.

> **Note:** HTTP/2 is only supported over TLS (HTTPS listeners). Plain HTTP listeners
> always use HTTP/1.1.

> **Note**: When using the `fips` crypto provider, CHACHA20_POLY1305 cipher suites are not available. Only AES-GCM suites are FIPS-approved. Sōzu's `cipher_suite_by_name` filters its result through the active provider's supported set so a misconfigured `cipher_list` cannot silently downgrade an FIPS build.

##### Key exchange groups

The `groups_list` option controls which key exchange algorithms are offered during the TLS handshake. Groups are listed in order of preference.

```toml
# Default: ["X25519MLKEM768", "x25519", "P-256", "P-384"]
groups_list = ["X25519MLKEM768", "x25519", "P-256", "P-384"]
```

| Group name | Description | Provider support |
|------------|-------------|------------------|
| `x25519` / `X25519` | Curve25519 ECDHE | All providers |
| `secp256r1` / `P-256` | NIST P-256 ECDHE | All providers |
| `secp384r1` / `P-384` | NIST P-384 ECDHE | All providers |
| `X25519MLKEM768` | Post-quantum hybrid (X25519 + ML-KEM 768) | `crypto-aws-lc-rs`, `crypto-openssl` (OpenSSL 3.5+) |

Unknown or unsupported group names are silently skipped with a log warning. This allows using the same configuration across different crypto providers — for example, `X25519MLKEM768` is safely ignored when building with `crypto-ring`.

**Examples:**

```toml
# Post-quantum enabled (default). PQ-capable clients negotiate X25519MLKEM768,
# others fall back to classical X25519.
groups_list = ["X25519MLKEM768", "x25519", "P-256", "P-384"]

# Classical only (explicitly disable post-quantum)
groups_list = ["x25519", "P-256", "P-384"]

# FIPS 140-3 compliant (NIST curves only, no X25519)
groups_list = ["P-256", "P-384"]
```

> **Post-quantum key exchange**: X25519MLKEM768 is a hybrid scheme that combines classical X25519 with the ML-KEM 768 post-quantum algorithm. It protects against future quantum computer attacks while maintaining security against current classical attacks. The handshake is slightly larger (~1 KB overhead) but has negligible latency impact. Clients that do not support it automatically negotiate a classical group.

##### Certificates

TLS certificates can be configured in two ways:

**1. Default certificate on the HTTPS listener** (without SNI):

```toml
[[listeners]]
protocol = "https"
address = "0.0.0.0:8443"
certificate = "/path/to/certificate.pem"
certificate_chain = "/path/to/chain.pem"
key = "/path/to/private-key.pem"
```

**2. Per-frontend certificates** (with SNI, recommended):

```toml
[clusters.MyCluster]
protocol = "http"
frontends = [
    { address = "0.0.0.0:8443", hostname = "example.com",
      certificate = "/path/to/example.com.pem",
      certificate_chain = "/path/to/chain.pem",
      key = "/path/to/example.com.key" },
]
backends = [
    { address = "127.0.0.1:8080" }
]
```

Sōzu supports the following certificate and key types:

| Type | Key format | Notes |
|------|-----------|-------|
| RSA 2048+ | PKCS#1 or PKCS#8 PEM | Most common, widely supported |
| ECDSA P-256 | SEC1 or PKCS#8 PEM | Faster handshakes, smaller certificates |
| ECDSA P-384 | SEC1 or PKCS#8 PEM | Higher security margin |

All certificate files must be PEM-encoded. The `certificate_chain` should contain intermediate CA certificates (not the root CA).

**Generating test certificates:**

```bash
# RSA 2048
openssl req -newkey rsa:2048 -nodes -keyout rsa.key -x509 -days 365 \
    -subj "/CN=example.com" -addext "subjectAltName=DNS:example.com" -out rsa.pem

# ECDSA P-256
openssl ecparam -name prime256v1 -genkey -out ecdsa.key
openssl req -new -key ecdsa.key -x509 -days 365 \
    -subj "/CN=example.com" -addext "subjectAltName=DNS:example.com" -out ecdsa.pem
```

> **Important**: Certificates must include a Subject Alternative Name (SAN) extension matching the frontend hostname. Certificates without SANs may cause TLS handshake failures.

### Clusters

You can declare the list of your _clusters_ under the `[clusters]` section.
They follow the format:

_Mandatory parameters:_

```toml
[clusters]

[clusters.NameOfYourCluster]
# possible values are http or tcp
# https proxies will use http here
protocol = "http"

# per cluster load balancing algorithm. The possible values are
# "roundrobin" and "random". Defaults to "roundrobin"
# load_balancing_policy="roundrobin"

# force cluster to redirect http traffic to https
# https_redirect = true

frontends = [
  { address = "0.0.0.0:8080", hostname = "lolcatho.st" },
  { address = "0.0.0.0:8443", hostname = "lolcatho.st", certificate = "../lib/assets/certificate.pem", key = "../lib/assets/key.pem", certificate_chain = "../lib/assets/certificate_chain.pem" }
]
# additional options for frontends: sticky_session (boolean)

backends  = [
  { address = "127.0.0.1:1026" }
]
```

#### HTTP/2 backend connections (h2c)

By default, Sōzu speaks HTTP/1.1 to backend servers. You can enable cleartext HTTP/2 (h2c)
for backend connections on a per-cluster basis using the `http2` option:

```toml
[clusters.MyH2Cluster]
protocol = "http"
http2 = true

frontends = [
  { address = "0.0.0.0:8443", hostname = "app.example.com", certificate = "cert.pem", key = "key.pem", certificate_chain = "chain.pem" }
]
backends = [
  { address = "127.0.0.1:8080" }
]
```

When `http2 = true`, Sōzu opens cleartext HTTP/2 connections to the backend servers.
This is useful when your backends natively support HTTP/2 (e.g., gRPC servers).

> **Important:** `http2` is a **backend-capability hint** — it tells Sōzu whether the
> backend speaks H2, nothing more. It does **not** gate H2 acceptance at the frontend.
> Frontend H2 is negotiated entirely via TLS ALPN (the `alpn_protocols` listener option)
> and is independent of per-cluster configuration. A cluster with `http2 = false` (or
> omitted) can still receive H2 requests from clients; Sōzu will translate them to H1
> before forwarding to the backend. See `command/src/config.rs:998` for the field definition.

You can also toggle HTTP/2 at runtime on an existing cluster via the CLI:

```bash
sozu cluster h2 enable --id MyH2Cluster
sozu cluster h2 disable --id MyH2Cluster
```

The frontend and backend protocols are independent. All four combinations work:

| Client → Sōzu | Sōzu → Backend | Configuration |
|----------------|-----------------|---------------|
| HTTP/1.1       | HTTP/1.1        | Default (no `http2` flag) |
| HTTP/2         | HTTP/1.1        | Client negotiates H2 via ALPN, default backend |
| HTTP/1.1       | HTTP/2          | `http2 = true` on cluster |
| HTTP/2         | HTTP/2          | Client negotiates H2 via ALPN + `http2 = true` |

> **Note:** The `http2` option controls the backend protocol only. The frontend protocol
> is determined by TLS ALPN negotiation between the client and Sōzu.

#### Buffer size for HTTP/2

HTTP/2 uses a default maximum frame size of 16384 bytes (16 KiB) per RFC 9113 §6.5.2.
Sōzu needs at least 9 additional bytes for the frame header (§4.1). Set `buffer_size`
in the global section to at least **16393**:

```toml
buffer_size = 16393
```

This is also the default — no action is needed unless an operator explicitly lowers
`buffer_size`. **As of [PR #1209](https://github.com/sozu-proxy/sozu/pull/1209) Sōzu
rejects start-up if `buffer_size < 16393` and any HTTPS listener advertises `h2` in
its ALPN list**, with a `BufferSizeTooSmallForH2` error pointing at the conflicting
listeners. The previous behaviour (silently accept the typo, then deadlock H2 mux on
full-size frames) is gone.

To run with a smaller buffer, remove `h2` from the listeners' `alpn_protocols`:

```toml
[[listeners]]
address = "0.0.0.0:443"
protocol = "https"
alpn_protocols = ["http/1.1"]   # h2 removed; buffer_size < 16393 is now valid
```

#### H2 flood detection thresholds

Sozu includes built-in flood detection for HTTP/2 connections. When a client sends
an excessive number of certain frame types within a rolling window, Sozu terminates
the connection with a `GOAWAY(ENHANCE_YOUR_CALM)` frame. This protects against
several known HTTP/2 denial-of-service vectors.

Six per-window thresholds are configurable per-listener. When omitted, compile-time
defaults are used (see also [RST_STREAM lifetime caps](#h2-rst_stream-lifetime-caps)
for connection-lifetime counters):

| Parameter | Default | Protects against | CVE |
|-----------|---------|-----------------|-----|
| `h2_max_rst_stream_per_window` | 100 | Rapid Reset attack: client opens and immediately resets streams in a tight loop | CVE-2023-44487 |
| `h2_max_ping_per_window` | 100 | Ping flood: client sends PING frames faster than the server can respond | CVE-2019-9512 |
| `h2_max_settings_per_window` | 50 | Settings flood: client sends SETTINGS frames requiring ACKs, exhausting server resources | CVE-2019-9515 |
| `h2_max_empty_data_per_window` | 100 | Empty DATA flood: client sends zero-length DATA frames to consume processing time | CVE-2019-9518 |
| `h2_max_window_update_stream0_per_window` | 100 | Connection-level (stream 0) WINDOW_UPDATE flood: client sends a torrent of non-zero stream-0 WINDOW_UPDATE frames to burn server CPU parsing each one (zero-increment frames short-circuit into `GOAWAY(PROTOCOL_ERROR)` per RFC 9113 §6.9). | |
| `h2_max_continuation_frames` | 20 | CONTINUATION flood: client sends many small CONTINUATION frames to exhaust header memory | CVE-2024-27316 |
| `h2_max_glitch_count` | 100 | Cumulative protocol violations: total number of minor protocol errors before disconnection | |

_Configuration example:_

```toml
[[listeners]]
address = "0.0.0.0:443"
protocol = "https"

# H2 flood detection thresholds (optional, defaults shown)
h2_max_rst_stream_per_window = 100    # Rapid Reset (CVE-2023-44487)
h2_max_ping_per_window = 100          # Ping flood (CVE-2019-9512)
h2_max_settings_per_window = 50       # Settings flood (CVE-2019-9515)
h2_max_empty_data_per_window = 100    # Empty DATA flood (CVE-2019-9518)
h2_max_window_update_stream0_per_window = 100  # Connection-level WINDOW_UPDATE flood (stream 0)
h2_max_continuation_frames = 20       # CONTINUATION flood (CVE-2024-27316)
h2_max_glitch_count = 100             # Cumulative protocol violations
```

> **Note:** When any threshold is exceeded, the connection is terminated with a
> `GOAWAY` frame using the `ENHANCE_YOUR_CALM` error code (HTTP/2 error code 0xb).
> The event is logged at `warn` level with the specific flood type that triggered
> disconnection.

##### Tuning `h2_max_glitch_count` in production

`h2_max_glitch_count` is a catch-all counter for *low-severity* protocol drift
that no other flood counter covers. It is incremented on stream-close races
(`RST_STREAM` / `WINDOW_UPDATE` / `DATA` on a closed stream), `WINDOW_UPDATE`
with zero increment on a closed stream, and unknown SETTINGS identifiers. The
counter uses a 1-second sliding window with *half-decay* (it halves at each
window roll rather than resetting), so a threshold of `N` tolerates a one-shot
burst of `N` glitches or a sustained rate of roughly `N/2` glitches per second.

The default of `100` is conservative and protects a lightly-loaded edge well,
but busy proxies that terminate aggressive-cancellation traffic (mobile
clients, gRPC with deadlines, browser prefetch, fuzz harnesses) routinely
trip it on legitimate races. If `h2.flood.violation.glitch_window` fires on
traffic you know is benign, raise the threshold per-listener:

| Traffic profile | Suggested `h2_max_glitch_count` |
|---|---|
| Default / low traffic | 100 |
| Busy public edge, mixed clients | 500 |
| gRPC / mobile / high cancellation | 1000 – 2000 |
| Load-test absorption only | 5000 |

Before raising blindly, drop the relevant module to `debug` level and check
which branch dominates — a single misbehaving backend or client emitting
`WINDOW_UPDATE` on closed streams can be fixed upstream instead of hiding
behind a larger budget. Never set the threshold to `u32::MAX`; the catch-all
is the last line of defence against peers that stay just under every specific
per-frame cap.

#### H2 connection tuning

Additional H2 parameters control connection-level behavior. All are optional
per-listener with safe compile-time defaults:

| Parameter | Default | Description |
|-----------|---------|-------------|
| `h2_initial_connection_window` | 1048576 (1MB) | Connection-level receive window size in bytes (RFC 9113 §6.9.2). Clamped to [65535, 2^31-1]. |
| `h2_max_concurrent_streams` | 100 | Maximum concurrent H2 streams the proxy accepts (`SETTINGS_MAX_CONCURRENT_STREAMS`). Minimum: 1. |
| `h2_stream_shrink_ratio` | 2 | Shrink threshold ratio for recycled stream slots. The internal stream Vec is shrunk when `total_slots > active_streams * ratio`. Minimum: 2. |
| `h2_max_header_list_size` | 65536 | Maximum accumulated HPACK-decoded header list size per request (`SETTINGS_MAX_HEADER_LIST_SIZE`, RFC 9113 §6.5.2). |
| `h2_stream_idle_timeout_seconds` | `max(30, back_timeout)` | Per-stream idle timeout in seconds. An open H2 stream that receives no meaningful application data (non-empty DATA or HEADERS) for this duration is cancelled (`RST_STREAM` / `CANCEL`) to defend against slow-multiplex Slowloris. When unset the listener inherits `back_timeout` (floored at 30 s) so streams are not cancelled before the backend socket budget elapses; set explicitly to cap the per-stream deadline below `back_timeout` when under a slow-multiplex attack. Active uploads that trickle DATA frames reset the timer on each frame. |
| `h2_max_header_table_size` | 65536 | Maximum HPACK dynamic table size (`SETTINGS_HEADER_TABLE_SIZE`) accepted from the peer. Caps the peer-advertised value to prevent unbounded HPACK encoder memory growth. |
| `h2_graceful_shutdown_deadline_seconds` | 5 | Maximum wall-clock seconds to wait for in-flight H2 streams after `GOAWAY(NO_ERROR)` has been sent during soft-stop. Once the deadline elapses the connection is forcibly closed. Set to `0` to disable the forced close entirely — shutdown then waits for every stream to drain naturally (use with caution: a long-running request can delay the whole soft-stop indefinitely). |

_Configuration example:_

```toml
[[listeners]]
address = "0.0.0.0:443"
protocol = "https"

# H2 connection tuning (optional, defaults shown)
h2_initial_connection_window = 1048576            # 1MB, min 65535, max 2147483647
h2_max_concurrent_streams = 100                   # min 1
h2_stream_shrink_ratio = 2                        # min 2
h2_max_header_list_size = 65536                   # HPACK decoded header budget
h2_stream_idle_timeout_seconds = 30               # per-stream idle timeout (default: max(30, back_timeout))
h2_max_header_table_size = 65536                  # HPACK dynamic table size cap
h2_graceful_shutdown_deadline_seconds = 5         # soft-stop forced-close deadline (0 = wait forever)
```

#### H2 RST_STREAM lifetime caps

In addition to the per-window `h2_max_rst_stream_per_window` threshold, three
lifetime counters limit the total number of RST_STREAM frames associated with
a single connection — two on the **received** side (Rapid Reset, CVE-2023-44487)
and one on the **emitted** side (MadeYouReset, CVE-2025-8671). Together they
catch patient-attacker patterns that stay just below the per-window threshold.

| Parameter | Default | Description |
|-----------|---------|-------------|
| `h2_max_rst_stream_lifetime` | 10000 | Absolute lifetime cap on RST_STREAM frames **received** on this connection. |
| `h2_max_rst_stream_abusive_lifetime` | 50 | Lifetime cap on "abusive" **received** RST_STREAM frames — resets sent by the peer before a response starts, the Rapid Reset signature (CVE-2023-44487). |
| `h2_max_rst_stream_emitted_lifetime` | 500 | Absolute lifetime cap on RST_STREAM frames **emitted by the server** (CVE-2025-8671 "MadeYouReset"). Increments on every non-`NoError` reset triggered by an attacker-crafted frame (Content-Length mismatch, header parse error, rejected priority, zero-increment `WINDOW_UPDATE` on an open stream). Graceful `NoError` cancels (stream recycle, propagated client cancel) are exempt. Crossing the threshold emits `GOAWAY(EnhanceYourCalm)`. |

_Configuration example:_

```toml
[[listeners]]
address = "0.0.0.0:443"
protocol = "https"

h2_max_rst_stream_lifetime = 10000
h2_max_rst_stream_abusive_lifetime = 50
h2_max_rst_stream_emitted_lifetime = 500
```

#### Security and protocol settings

| Parameter | Default | Description |
|-----------|---------|-------------|
| `strict_sni_binding` | true | Every HTTP request must have its `:authority` / `Host` exact-match the TLS SNI negotiated at handshake (CWE-346 / CWE-444). Applies to HTTPS listeners only; plaintext listeners never have an SNI to compare against. |
| `disable_http11` | false | Only accept HTTP/2 connections; clients that do not negotiate `h2` via TLS ALPN (including those that omit ALPN entirely) are dropped at handshake instead of silently downgrading to HTTP/1.1. |

_Configuration example:_

```toml
[[listeners]]
address = "0.0.0.0:443"
protocol = "https"

strict_sni_binding = true   # reject host/SNI mismatch (default)
disable_http11 = false      # allow HTTP/1.1 fallback (default)
sozu_id_header = "Sozu-Id"  # rename the per-request correlation header (default "Sozu-Id")
```

The `sozu_id_header` knob renames the correlation header Sozu injects on every request AND response. Each request gets a unique ULID whose value is written to both sides — operators can grep the same identifier across client logs, proxy access logs, and backend logs. Default is `Sozu-Id`; a common rebrand is `X-Request-Trace` or `X-Edge-Id`. The value must be a valid HTTP header name (token chars per RFC 9110 §5.1). Applies to both HTTP and HTTPS listeners.

#### `X-Real-IP` injection and anti-spoof elision

Two listener-scoped flags control how Sōzu handles the `X-Real-IP`
header. They default off (current behaviour: client-supplied value
passes through, no proxy injection) and are independently combinable:

| Parameter | Default | Description |
|-----------|---------|-------------|
| `elide_x_real_ip` | false | Strip any client-supplied `X-Real-IP` header from forwarded requests before they reach the backend. Anti-spoofing — without this, a client can claim any value and downstream apps that key on `X-Real-IP` trust it verbatim. |
| `send_x_real_ip` | false | Append a proxy-generated `X-Real-IP` header carrying `session_address.ip()` — the connection peer IP after PROXY-v2 unwrap, i.e. the original client IP. |

The four valid combinations cover the typical use cases:

| `elide_x_real_ip` | `send_x_real_ip` | Behaviour |
|---|---|---|
| `false` | `false` | Default. Client header passes through, no proxy injection. |
| `false` | `true`  | Send-only. Both client and proxy headers reach the backend. **Operator caveat:** backends that read the *first* `X-Real-IP` header (rather than the last) trust the client value over the proxy value. Use `elide_x_real_ip = true` alongside for full anti-spoofing. |
| `true`  | `false` | Anti-spoof only. Client header stripped, no proxy injection. |
| `true`  | `true`  | Anti-spoof + send. Client header stripped, proxy header carries the connection peer IP. |

The injected value is the IP of the connection peer **after** PROXY-v2
unwrap. When the listener has `expect_proxy = true` and the upstream
sends a PROXY-v2 frame, `session_address` is rewritten to the original
client IP before this header is generated, so the carried address is
the real client even with one (or more) PROXY-v2 hops in front of
Sōzu.

Both flags apply uniformly to HTTP/1 and HTTP/2 because the elision
and injection live on the shared `HttpContext::on_request_headers`
callback. The H2 trailer-block code path additionally honours
`elide_x_real_ip` for trailer HEADERS frames, so an H2 client cannot
spoof `x-real-ip` as a trailer to bypass the anti-spoof.

Both knobs are runtime-patchable via `UpdateHttpListenerConfig` /
`UpdateHttpsListenerConfig`. Patches apply immediately to all H1
sessions and to **new** H2 connections; **already-open H2 connections
continue using the values captured at their handshake** (same
connection-scoped-capture semantic as `strict_sni_binding`). Long-lived
H2 connections (CDN-style, mobile keep-alive) therefore observe a
delayed flag flip — this is the established mux precedent, not a new
defect.

```toml
[[listeners]]
address = "0.0.0.0:443"
protocol = "https"

elide_x_real_ip = true   # strip client-supplied X-Real-IP before forwarding
send_x_real_ip  = true   # inject a proxy-generated X-Real-IP carrying the peer IP
```

#### TLS handshake log severity cheat-sheet

Sōzu tiers the log severity of every rustls handshake error by root cause so
scanner noise does not crowd out real configuration errors (commit `156cc217`).
Operators monitoring logs should expect the following classification:

| Error variant | Level | Typical cause |
|---------------|-------|---------------|
| `NoApplicationProtocol`, `NoKxGroupsInCommon`, `NoCipherSuitesInCommon`, `NoCertificatesPresented` | `warn` | Remote scanner / ssllabs probe with restricted cipher or ALPN set. **Benign** — no action required unless the client is legitimate. |
| `InappropriateMessage`, `InappropriateHandshakeMessage`, `InvalidMessage`, `PeerMisbehaved`, `PeerIncompatible`, `PeerSentOversizedRecord` | `warn` | Peer protocol violation — typically a broken or outdated TLS client, occasionally a port-scan. Review if persistent from a known client. |
| `InvalidCertificate`, `DecryptError` | `warn` | Client certificate rejected (e.g. expired, unknown CA) or bad shared key material. Relevant only when mTLS is configured. |
| `AlertReceived(_)` | `debug` | Client sent a TLS alert (e.g. `user_canceled`, `close_notify_required`) — conversation-level noise. |
| Everything else (server-side faults, unexpected internal errors) | `error` | Real bug or misconfiguration on the proxy. **Actionable** — triage immediately. |

In particular, the recurring `Could not look up a certificate for server name "<name>"` line (issue #774) is emitted at `warn` when the `rustls::server::ClientHello` SNI does not match any configured frontend — the usual culprit is a generic scanner (Shodan, ssllabs, Censys) probing `default.example` or an IP-only hello. No action required unless the name matches a frontend you expect to serve.

## Custom HTTP answer templates

Sōzu lets operators replace any default error response with a templated body.
Templates are specified as a `[listeners.<id>.answers]` map at listener
scope — the **global default** that fires whenever no cluster-level
override matches — or as a `[clusters.<id>.answers]` map at cluster
scope, which overrides the matching listener entry on requests routed
through that cluster. Both layers accept the same key/value shape: the
key is the HTTP status code (e.g. `"503"`); the value is **either** an
inline literal body (the default — the value is taken verbatim) **or**
`file://<path>` to load the body off disk. The inline form is
convenient for short canned responses, secrets-free containers where
mounting a template directory is awkward, and test rigs that want to
avoid disk dependencies.

```toml
# listener-level: global default for every status not overridden by a cluster.
[listeners.https.answers]
"401" = "file:///etc/sozu/templates/401.http"        # load from disk
"404" = "file:///etc/sozu/templates/404.http"
"503" = """HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nContent-Length: 4\r\n\r\nbusy"""  # inline literal (no prefix)

# cluster-level: overrides the listener default for THIS cluster only.
[clusters.MyCluster.answers]
"503" = "file:///etc/sozu/templates/MyCluster.503.http"
```

Each template is a complete HTTP response (status line + headers + body) with
optional placeholders. Sōzu substitutes the placeholders at render time:

| Placeholder | Scope | Meaning |
|-------------|-------|---------|
| `%STATUS_CODE` | header & body | Resolved HTTP status (e.g. `503`) |
| `%REQUEST_ID` | header & body | Per-request ULID (matches `Sozu-Id`) |
| `%CLUSTER_ID` | header & body | Cluster the request was routed to (or empty) |
| `%BACKEND_ID` | header & body | Backend the request was forwarded to (or empty) |
| `%ROUTE` | header & body | Request method + authority + path |
| `%REDIRECT_LOCATION` | header & body | Resolved `Location` URL (301 only) |
| `%WWW_AUTHENTICATE` | header only | Realm string for 401 (header is elided when empty) |
| `%MESSAGE`, `%PHASE`, `%SUCCESSFULLY_PARSED`, `%PARTIALLY_PARSED`, `%INVALID`, `%CAPACITY`, `%DURATION` | varies | Diagnostic detail for parse / size / timeout errors |

When the template carries a `Content-Length: <N>` header, the engine
recomputes the value from the actual rendered body size after
`%`-substitutions — so a literal value that drifted from the body
length cannot land on the wire (RFC 9110 §8.6 / RFC 7230 §3.3.2 anti-
smuggling). Templates that omit `Content-Length` keep the byte-for-byte
shape they were written with; nothing is synthesised. Operators who
want a Content-Length include one; those who rely on `Connection:
close` for body framing get a clean header-only response.

When a template carries `Connection: close`, the response will close the
frontend connection after delivery; a custom template without that header
keeps frontend keep-alive on. The HAProxy parallel is `errorfile NNN /path`.

The legacy `answer_NNN = "/path"` per-status fields under
`[listeners.<name>]` continue to work — they are merged into the new map at
load time so existing state files round-trip cleanly. New configs should
prefer the `[listeners.<name>.answers]` shape.

## Frontend redirect, URL rewrite, and custom headers

Each frontend can carry a routing decision richer than "forward to a cluster".
Three top-level knobs drive different policies:

```toml
[[clusters.MyCluster.frontends]]
address  = "0.0.0.0:80"
hostname = "old.example.com"
# Force a permanent 301 to the canonical name on a different port.
redirect        = "permanent"      # forward (default) | permanent | unauthorized
redirect_scheme = "use-https"      # use-same (default) | use-http | use-https
rewrite_host    = "new.example.com"
rewrite_port    = 8443             # paired with `cluster.https_redirect_port`
```

`redirect = "permanent"` returns a 301 with a resolved `Location` URL built
from `redirect_scheme`, the (optionally rewritten) host, the cluster's
`https_redirect_port` (or the rewritten `rewrite_port`), and the original
request path. `redirect = "unauthorized"` returns 401 unconditionally —
useful for blanket deny-by-default frontends that still want to surface a
login prompt with the cluster's `www_authenticate` realm.

`rewrite_host` and `rewrite_path` accept a small template grammar:
`$HOST[n]` references the n-th host capture (0 is the full hostname; 1+ are
regex / wildcard subgroups), and `$PATH[n]` references the n-th path
capture. When the host is rewritten, Sōzu injects the original host into
`X-Forwarded-Host` so backends can reconstruct the request URL.

Custom headers attach to the same frontend:

```toml
[[clusters.MyCluster.frontends.headers]]
position = "request"               # request | response | both
key      = "X-Forwarded-Proto"
value    = "https"

[[clusters.MyCluster.frontends.headers]]
position = "response"
key      = "X-Cache-Backend"
value    = ""                      # empty value DELETES the header by name
                                   # (HAProxy `del-header` parity)
```

HAProxy parallels: `http-request redirect` (permanent / scheme), `set-uri`
and `set-path` (rewrite), `http-request set-header` and
`http-request del-header` (custom headers).

## HTTP Basic authentication on a frontend

Operators can require a valid `Authorization: Basic` header on a frontend
and validate it against a list of pre-hashed credentials on the cluster.
The mux iterates the entire authorized list in constant time (via
`subtle::ConstantTimeEq`), so neither the matching index nor a successful
lookup leak through the time spent validating.

```toml
[clusters.MyCluster]
# Realm rendered into `WWW-Authenticate: Basic realm="…"` on a 401.
www_authenticate = 'Basic realm="MyCluster"'
# Each entry is `username:hex(sha256(password))`. The runtime hashes
# ONLY the password — the username appears verbatim before the colon
# and is NOT part of the hashed input. Generate the hash for password
# `secret` with:
#   printf 'secret' | sha256sum | awk '{print $1}'
# `printf` (not `echo`) omits the trailing newline so the digest matches
# the bytes carried in `Authorization: Basic <base64>`.
authorized_hashes = [
    "admin:2bb80d537b1da3e38bd30361aa855686bde0eacd7162fef6a25fe97bf527a25b",
]

[[clusters.MyCluster.frontends]]
address       = "0.0.0.0:80"
hostname      = "secured.example.com"
required_auth = true               # gate this frontend on basic-auth
```

Failure modes:

- Missing `Authorization: Basic` header → 401 with the cluster's realm
- Malformed credential (bad base64, missing `:`) → 401
- Wrong username or password → 401
- Empty `authorized_hashes` while the frontend has `required_auth = true`
  → 401 (closed-by-default policy)

The `WWW-Authenticate` header is rendered through the answer template's
`%WWW_AUTHENTICATE` placeholder; when no realm is configured the entire
header line is elided from the 401 response. HAProxy parallel:
`http-request auth realm Foo unless { http_auth(...) }`.

### Tuning the credential decode cap

A hostile peer can send arbitrarily long `Authorization: Basic <token>`
values. Sōzu base64-decodes the token in a transient allocation; an
unbounded decode per failed-auth attempt is a memory pressure vector.
The worker caps the decoded length to **4096 bytes by default** —
well above the realistic `username:password` shape (typical credentials
are <100 bytes). Operators running hardened tenants can lower this in
the main TOML config:

```toml
# in the top-level config (alongside `buffer_size`, `max_connections`, etc.)
basic_auth_max_credential_bytes = 256
```

The override is committed once on each worker at boot and applies to
every cluster. Setting `0` is a no-op (the built-in default stays in
force) so an explicit zero in a config file does not accidentally
disable the cap.

The config validator emits a `warn!` line at boot when the configured
cap is **>= `buffer_size / 3`**: at that point a single failed-auth
attempt can pin ~33% of the per-frontend buffer's worth of bytes,
which combined with in-flight request/response framing pushes the
buffer toward back-pressure under load. The warning is informational
only — operators with a deliberate trade-off can keep the value, but
the surprise stays visible in the boot log.

## Runtime patch — `sozu listener update`

`sozu listener {http,https,tcp} update` patches a live listener **in place** without cycling the listening socket. Only the fields you pass are written; all others are preserved exactly as they are. Existing sessions continue with their configuration snapshot; only new sessions, connections, or TLS handshakes — depending on the field — pick up the new values. Use `sozu listener list` to inspect current values before patching.

> **Bind-only fields are not patchable.** The address, TLS crypto parameters, and the `active` flag can only be changed by removing and re-adding the listener:
> ```bash
> sozu listener https remove -a 0.0.0.0:8443
> sozu listener https add --address 0.0.0.0:8443 [...]
> ```
> Bind-only fields: `address`, `tls_versions`, `cipher_list`, `cipher_suites`,
> `signature_algorithms`, `groups_list`, `certificate`, `certificate_chain`, `key`,
> `send_tls13_tickets`, `active`.

### Updatable fields

Fields are grouped by the earliest point at which a patched value takes effect for connections already in progress. All fields apply to new sessions immediately after the patch is acknowledged.

#### HTTP and HTTPS listeners

| Field | Type | Mutability class | Default | Notes |
|---|---|---|---|---|
| `public_address` | `SocketAddr` | session-at-accept | — | Source address reported to backends / logs |
| `expect_proxy` | `bool` | session-at-accept | `false` | Enable PROXY protocol v1/v2 on new sessions |
| `sticky_name` | `string` | session-at-accept | `"SOZUBALANCEID"` | Sticky-session cookie name |
| `front_timeout` | `u32` (seconds) | session-at-accept | `60` | Max idle time on the client socket |
| `back_timeout` | `u32` (seconds) | session-at-accept | `30` | Max idle time on the backend socket |
| `connect_timeout` | `u32` (seconds) | session-at-accept | `3` | Max time to establish a backend connection |
| `request_timeout` | `u32` (seconds) | session-at-accept | `10` | Max time to send a complete request |
| `http_answers` | file paths | session-at-accept | built-in defaults | Listener-default HTTP error bodies (301/401/404/408/413/421/502/503/504/507). Per-cluster `answer_503` overrides are preserved. |
| `sozu_id_header` | `string` | session-at-accept | `"Sozu-Id"` | Correlation header name (RFC 9110 §5.1 token; reject empty or containing CR/LF/colon/space) |
| `h2_max_rst_stream_per_window` | `u32` (≥ 1) | per-connection setup | `100` | RST_STREAM flood cap — CVE-2023-44487, CVE-2019-9514 |
| `h2_max_ping_per_window` | `u32` (≥ 1) | per-connection setup | `100` | PING flood cap — CVE-2019-9512 |
| `h2_max_settings_per_window` | `u32` (≥ 1) | per-connection setup | `50` | SETTINGS flood cap — CVE-2019-9515 |
| `h2_max_empty_data_per_window` | `u32` (≥ 1) | per-connection setup | `100` | Empty DATA flood cap — CVE-2019-9518 |
| `h2_max_continuation_frames` | `u32` (≥ 1) | per-connection setup | `20` | CONTINUATION flood cap — CVE-2024-27316 |
| `h2_max_glitch_count` | `u32` (≥ 1) | per-connection setup | `100` | Cumulative protocol-anomaly budget |
| `h2_max_window_update_stream0_per_window` | `u32` (≥ 1) | per-connection setup | `100` | Stream-0 WINDOW_UPDATE flood cap |
| `h2_max_rst_stream_lifetime` | `u64` (≥ 1) | per-connection setup | `10000` | Lifetime RST_STREAM received cap — CVE-2023-44487 |
| `h2_max_rst_stream_abusive_lifetime` | `u64` (≥ 1) | per-connection setup | `50` | Lifetime abusive RST_STREAM cap (Rapid Reset signature) |
| `h2_max_rst_stream_emitted_lifetime` | `u64` (≥ 1) | per-connection setup | `500` | Lifetime server-emitted RST_STREAM cap — CVE-2025-8671 |
| `h2_initial_connection_window` | `u32` | per-connection setup | `1048576` | Connection receive window (bytes, RFC 9113 §6.9.2) |
| `h2_max_concurrent_streams` | `u32` (≥ 1) | per-connection setup | `100` | `SETTINGS_MAX_CONCURRENT_STREAMS` |
| `h2_stream_shrink_ratio` | `u32` (≥ 2) | per-connection setup | `2` | Stream-slot Vec shrink threshold |
| `h2_max_header_list_size` | `u32` | per-connection setup | `65536` | HPACK decoded header budget (`SETTINGS_MAX_HEADER_LIST_SIZE`) |
| `h2_max_header_table_size` | `u32` | per-connection setup | `65536` | HPACK dynamic table size cap (`SETTINGS_HEADER_TABLE_SIZE`) |
| `h2_stream_idle_timeout_seconds` | `u32` | per-connection setup | `max(30, back_timeout)` | Per-stream idle timeout (slow-multiplex Slowloris defence). When unset, inherits `back_timeout` floored at 30 s; set explicitly to cap below `back_timeout`. |
| `h2_graceful_shutdown_deadline_seconds` | `u32` | per-connection setup | `5` | Forced-close deadline after `GOAWAY(NO_ERROR)` on soft-stop. `0` = wait forever. |

#### HTTPS-only fields

| Field | Type | Mutability class | Default | Notes |
|---|---|---|---|---|
| `alpn_protocols` | `string[]` | per-handshake | `["h2","http/1.1"]` | Rebuilds the rustls `ServerConfig`. In-flight handshakes finish on the old config. Pass `--reset-alpn` on the CLI to restore the default. |
| `strict_sni_binding` | `bool` | per-handshake | `true` | Enforce `:authority`/`Host` == TLS SNI (CWE-346/CWE-444) |
| `disable_http11` | `bool` | per-handshake | `false` | Drop clients that do not negotiate `h2` via ALPN |

#### TCP listeners

| Field | Type | Mutability class | Default | Notes |
|---|---|---|---|---|
| `public_address` | `SocketAddr` | session-at-accept | — | |
| `expect_proxy` | `bool` | session-at-accept | `false` | |
| `front_timeout` | `u32` (seconds) | session-at-accept | `60` | |
| `back_timeout` | `u32` (seconds) | session-at-accept | `30` | |
| `connect_timeout` | `u32` (seconds) | session-at-accept | `3` | |

### Examples

#### Tighten H2 flood thresholds under attack (CVE-2023-44487)

```bash
# Inspect current values first
sozu listener list

# Halve the Rapid Reset budget on the HTTPS listener
sozu listener https update -a 0.0.0.0:8443 \
    --h2-max-rst-stream-per-window 50 \
    --h2-max-rst-stream-abusive-lifetime 25

# Confirm the new values are live
sozu listener list
```

Existing H2 connections continue with their original thresholds. New connections
opened after the patch acknowledge see the tighter limits.

#### Toggle `disable_http11` to enforce H2-only mode

```bash
# Require all clients to negotiate h2 via TLS ALPN
sozu listener https update -a 0.0.0.0:8443 --disable-http11

# Revert — allow HTTP/1.1 fallback again
sozu listener https update -a 0.0.0.0:8443 --enable-http11
```

The existing HTTP/1.1 sessions in progress are not disrupted; only new TLS
handshakes that omit `h2` from their ALPN offer are refused after the patch.

#### Rebrand the correlation header

```bash
# Rename "Sozu-Id" to "X-Edge-Id" organisation-wide
sozu listener http  update -a 0.0.0.0:80   --sozu-id-header "X-Edge-Id"
sozu listener https update -a 0.0.0.0:8443 --sozu-id-header "X-Edge-Id"
```

The new header name takes effect for sessions accepted after the patch.
Previously accepted sessions continue to inject the old `Sozu-Id` header until
they close.

### Observability

Two worker metrics track update outcomes:

| Metric | Description |
|---|---|
| `listener.updated` | Incremented each time a worker successfully applies a patch |
| `listener.update_failed` | Incremented when the worker-side apply returns an error |

The control-plane command server also emits a `LISTENER_UPDATED` event on the
`SubscribeEvents` bus (carrying the listener address and type) and writes a
structured audit log line at `info!` level in the MUX `Session(...)` layout.
See [`observability.md`](observability.md#control-plane-audit-trail) for the
full format.

#### Backend health checks

You can optionally configure active HTTP health checks for backends. See [health_checks.md](./health_checks.md) for full details, including the **HTTP/1.1-only** probe limitation when a cluster sets `http2 = true` (h2c backends).

```toml
[clusters.NameOfYourCluster.health_check]
uri = "/health"
interval = 10
timeout = 5
healthy_threshold = 3
unhealthy_threshold = 3
expected_status = 0
```

## Metrics

Sōzu reports its own state to another network component through a `UDP` socket.
The main process and the workers are responsible to send their states.
We implement the [statsd](https://github.com/b/statsd_spec) protocol to send statistics.
Any service that understands the `statsd` protocol can then gather metrics from Sōzu.

### Architecture

Metrics are collected via thread-local storage macros (`count!`, `gauge!`, `gauge_add!`,
`time!`, `incr!`, `decr!`) and dispatched to two drains:

- **Local drain**: Accumulates metrics in-memory with HDR histograms for latency percentiles.
  Queried via the CLI (`sozu metrics get`).
- **Network drain**: Sends metrics over UDP using the statsd protocol. Supports both plain
  dotted format and InfluxDB-style tagged format.

Metric types:

| Type | Macro | StatsD suffix | Description |
|------|-------|---------------|-------------|
| Counter | `count!`, `incr!`, `decr!` | `\|c` | Monotonically increasing value, reset to 0 after each network send |
| Gauge | `gauge!`, `gauge_add!` | `\|g` | Snapshot value (absolute or delta) |
| Time | `time!` | `\|ms` | Latency in milliseconds, stored as HDR histogram locally |

Metrics have three scopes:

- **Proxy-level**: Global to the worker (no cluster or backend context)
- **Cluster-level**: Tagged with a `cluster_id`
- **Backend-level**: Tagged with both `cluster_id` and `backend_id`

### Configure metrics

In your `config.toml`, you can define the address and port of your external service by adding:

```toml
[metrics]
address = "127.0.0.1:8125"
# use InfluxDB's statsd protocol flavor to add tags
# tagged_metrics = false
# metrics key prefix
# prefix = "sozu"
# cardinality knob — defaults to "cluster" (preserves historical behaviour)
# detail = "cluster"
```

Metrics are sent at most once per second per key (if updated). Cluster/backend metrics
that have not been updated for 10 minutes are automatically dropped from the network drain.

#### Cardinality knob (`metrics.detail`)

Mirrors HAProxy's `extra-counters` opt-in: operators choose the lowest level that
satisfies their dashboards so the StatsD keyspace stays bounded. The level filters
the `(cluster_id, backend_id)` labels at emission time — both the local CLI drain
and the network drain see the same filtered labels, so dashboards stay consistent.

Each level is a SUPERSET of the previous one:

| Level | Behaviour | Use when |
|---|---|---|
| `process` | Both `cluster_id` and `backend_id` are dropped — proxy-only counters. | The smallest possible keyspace; dashboards aggregate everything at the worker level. |
| `frontend` | Same as `process` today. Reserved for the per-listener (frontend) counters that are tracked as a follow-up — operators can opt in already, the value is just stored on `ServerMetricsConfig.detail` and applied on every emission. | Forward-compatible config that picks up per-listener counters when they ship without a config-file change. |
| `cluster` | Keeps `cluster_id`, drops `backend_id`. **Default** — preserves the historical pre-knob behaviour. | The current shape of every existing dashboard. |
| `backend` | Keeps both labels. Highest cardinality. | Per-backend SLOs / hotspot debugging when the cluster has few backends. |

Filtering happens centrally in `Aggregator::receive_metric` (`lib/src/metrics/mod.rs`)
via the pure helper `filter_labels_for_detail`, unit-tested exhaustively across all
four levels. Workers receive the level over the SCM socket as a proto enum
(`MetricDetail`); old binaries on either side fall back to `cluster` so a
mixed-version rollout keeps emitting the historical metric shape.

#### StatsD wire format

**Untagged** (default, `tagged_metrics = false`):

```
sozu.WRK-00.http.requests:1|c
sozu.WRK-00.cluster.my-cluster.http.errors:0|c
sozu.WRK-00.cluster.my-cluster.backend.backend-1.backend_response_time:125|ms
```

**Tagged** (InfluxDB format, `tagged_metrics = true`):

```
sozu.http.requests,origin=WRK-00,version=1.1.0:1|c
sozu.cluster.http.errors,origin=WRK-00,version=1.1.0,cluster_id=my-cluster:0|c
sozu.backend.backend_response_time,origin=WRK-00,version=1.1.0,cluster_id=my-cluster,backend_id=backend-1:125|ms
```

### Available metrics

Sōzu emits the following metrics via statsd. All metrics are emitted by both HTTP/1.1
and HTTP/2 code paths unless noted otherwise. The prefix (default `sozu`) is omitted
from metric names below.

#### Infrastructure

| Metric | Type | Scope | Description |
|--------|------|-------|-------------|
| `panic` | counter | proxy | Worker thread panicked (logged before crash) |
| `configuration.clusters` | gauge | proxy | Number of configured clusters |
| `configuration.backends` | gauge | proxy | Number of configured backend servers |
| `configuration.frontends` | gauge | proxy | Number of configured frontends |
| `client.connections` | gauge | proxy | Active frontend connections |
| `client.connections_max` | gauge | proxy | Configured `max_connections`. Renamed from `client.max_connections` |
| `client.connections_percent` | gauge | proxy | Percentage of `max_connections` in use. Renamed from `client.connections_percentage` |
| `connections.rejected_per_cluster_ip` | counter | cluster, backend | HTTP/HTTPS request answered with 429 (or TCP session closed pre-backend) because the per-(cluster, source-IP) connection limit was reached. Labels carry `cluster_id` and `backend_id` (always empty for this counter — the rejection happens before backend selection). |
| `slab.entries` | gauge | proxy | Session slab allocator slots used |
| `slab.capacity` | gauge | proxy | Configured slab capacity (`10 + slab_entries_per_connection * max_connections`) |
| `slab.usage_percent` | gauge | proxy | `slab.entries * 100 / slab.capacity`. Pure slab-utilisation gauge |
| `slab.accept_threshold_percent` | gauge | proxy | `slab.entries * 100 / (10 + 2 * max_connections)`. Charts proximity to the `at_capacity()` accept gate, which can flip true while `slab.usage_percent` still shows headroom (configured slab is larger when `slab_entries_per_connection > 2`) |
| `process.uptime_seconds` | gauge | proxy | Seconds since the worker started. Captured once in `Server::new`; never reset on hot upgrade (the new worker starts its own counter) |
| `server.live` | gauge | proxy | `1` while the worker accepts traffic, `0` once a graceful shutdown is requested. Mirrors Envoy `server.live` semantics — L4 health checks (HAProxy / cloud LBs) can poll this gauge to drain a worker before the OS-level termination signal lands |
| `buffer.in_use` | gauge | proxy | Buffers currently checked out of the buffer pool. Renamed from `buffer.number` |
| `buffer.capacity` | gauge | proxy | Configured buffer pool capacity |
| `buffer.usage_percent` | gauge | proxy | `buffer.in_use * 100 / buffer.capacity` |
| `zombies` | counter | proxy | Zombie sessions detected and removed |

#### Accept queue

| Metric | Type | Scope | Description |
|--------|------|-------|-------------|
| `accept_queue.connections` | gauge | proxy | Sockets waiting in the accept queue |
| `accept_queue.backpressure` | gauge | proxy | `1` when max connections reached, `0` when accepting again |
| `accept_queue.wait_time` | time | proxy | How long a socket waited in the accept queue (ms) |
| `accept_queue.timeout` | counter | proxy | Sockets that timed out in the accept queue and were closed |
| `accept_queue.saturated_seconds` | counter | proxy | Incremented at 1 Hz while `SessionManager::can_accept` is `false`. Distinguishes "queue spent N seconds at max" from "queue briefly hit max" — the binary `accept_queue.backpressure` gauge collapses that duration |
| `listener.accepted.total` | counter | proxy | Sockets accepted by the worker, all listeners combined |
| `listener.accepted.tcp` | counter | proxy | Sockets accepted on TCP listeners |
| `listener.accepted.http` | counter | proxy | Sockets accepted on HTTP listeners |
| `listener.accepted.https` | counter | proxy | Sockets accepted on HTTPS listeners |
| `listener.connection_capped` | counter | proxy | Sockets refused by `create_sessions` because `SessionManager::check_limits` returned `false` (max connections reached or slab at capacity) |
| `client.connect.per_source.bucket_000` … `bucket_255` | counter | proxy | Per-accept counter bucketed by masked source subnet. Source IPs are masked to /24 (IPv4) or /48 (IPv6) and hashed (`DefaultHasher`) into 256 fixed buckets. The bucket noise is intentional: `incr!` requires `&'static str` keys, and a per-IP counter would be unbounded under SYN flood (OWASP A05, NIST SP 800-92). Operators wanting per-IP attribution should pair these counters with structured access logs or a downstream rate-limiter |
| `sessions.evicted` | counter | proxy | Sessions force-closed by `evict_on_queue_full` to make room for new accepts. Counts the number of evictions DECIDED in a cap event (one per call to `evict_least_active_sessions`); the helper may skip already-closed tokens, so a small drift versus the slab-removal count is possible. Only emitted when `evict_on_queue_full = true` |

The accept-loop telemetry above does **not** label by listener address.
`incr!` requires `&'static str` keys, and listener addresses can be added or
removed at runtime via the control plane — labelling by address would either
require runtime `Box::leak` of unbounded strings or a fixed bucket cap. We
chose the per-protocol breakdown (3 keys + aggregate) instead. Operators
needing per-listener-address attribution should run distinct workers per
listener or correlate via access logs.

#### Event loop

| Metric | Type | Scope | Description |
|--------|------|-------|-------------|
| `epoll_time` | time | proxy | Time spent in `epoll_wait`/`kqueue` (ms) |
| `event_loop_time` | time | proxy | Total event loop iteration time (ms) |

#### Protocol state

These gauges track how many sessions are in each protocol phase. A session
transitions through phases (e.g., Expect → TLS Handshake → HTTPS → WSS).

| Metric | Type | Scope | Description |
|--------|------|-------|-------------|
| `protocol.proxy.expect` | gauge | proxy | Sessions expecting a PROXY protocol header |
| `protocol.proxy.send` | gauge | proxy | Sessions sending a PROXY protocol header to a backend |
| `protocol.proxy.relay` | gauge | proxy | Sessions relaying a PROXY protocol header |
| `protocol.tls.handshake` | gauge | proxy | Sessions in TLS handshake (HTTPS only) |
| `protocol.http` | gauge | proxy | Active HTTP sessions |
| `protocol.https` | gauge | proxy | Active HTTPS sessions |
| `protocol.tcp` | gauge | proxy | Active TCP proxy sessions |
| `protocol.ws` | gauge | proxy | Active WebSocket sessions (over HTTP) |
| `protocol.wss` | gauge | proxy | Active WebSocket sessions (over HTTPS) |
| `websocket.active_requests` | gauge | proxy | Active WebSocket requests (HTTP + HTTPS) |

#### Request lifecycle

| Metric | Type | Scope | Description |
|--------|------|-------|-------------|
| `http.requests` | counter | proxy, cluster, backend | Total HTTP requests received (incremented when headers are fully parsed) |
| `http.active_requests` | gauge | proxy | Currently in-flight requests |
| `http.e2e.http11` | counter | proxy | Completed HTTP/1.1 request/response cycles |
| `http.e2e.h2` | counter | proxy | Completed HTTP/2 request/response cycles |
| `http.errors` | counter | proxy, cluster, backend | General HTTP processing errors. Labels are filtered centrally per `metrics.detail`: `process` / `frontend` collapse to a proxy-wide counter, `cluster` (default) attributes per cluster, `backend` keeps the per-backend split |
| `tcp.requests` | counter | proxy | TCP proxy connection requests |

#### Byte counters

| Metric | Type | Scope | Description |
|--------|------|-------|-------------|
| `bytes_in` | counter | proxy, cluster, backend | Bytes received from frontend clients |
| `bytes_out` | counter | proxy, cluster, backend | Bytes sent to frontend clients |
| `back_bytes_in` | counter | proxy | Bytes received from backend servers |
| `back_bytes_out` | counter | proxy | Bytes sent to backend servers |

#### Timing / latency

These are recorded as HDR histograms locally (queryable as percentiles: p50, p90, p99,
p99.9, p99.99, p99.999, p100) and sent as `|ms` values over StatsD.

| Metric | Type | Scope | Description |
|--------|------|-------|-------------|
| `request_time` | time | proxy, cluster | Total request time: first byte received to last byte sent (ms) |
| `service_time` | time | proxy, cluster | Internal processing time excluding backend I/O (ms) |
| `backend_response_time` | time | cluster, backend | Time from backend connection to last response byte (ms) |
| `backend_connection_time` | time | cluster, backend | TCP connection establishment time to backend (ms) |
| `frontend_matching_time` | time | cluster | Cluster/frontend route matching time (ms) |
| `regex_matching_time` | time | proxy | Regex evaluation time for path-based routing (ms) |

#### Response status classes

Incremented per backend response (scope: cluster + backend for `1xx`–`5xx`).
Buckets are emitted for every response with a status; per-code counters
below are emitted in addition to the bucket for the eighteen short-listed
codes Sōzu either generates as a default answer or that operators routinely
chart. Status codes outside that list contribute only to their bucket so
the metric keyspace stays bounded.

| Metric | Type | Scope | Description |
|--------|------|-------|-------------|
| `http.status.1xx` | counter | cluster, backend | 1xx informational responses |
| `http.status.2xx` | counter | cluster, backend | 2xx success responses |
| `http.status.3xx` | counter | cluster, backend | 3xx redirection responses |
| `http.status.4xx` | counter | cluster, backend | 4xx client error responses |
| `http.status.5xx` | counter | cluster, backend | 5xx server error responses |
| `http.status.other` | counter | proxy | Non-standard status codes |
| `http.status.none` | counter | proxy | Responses without a status code |
| `http.status.200` / `201` / `204` | counter | cluster, backend | Common 2xx success codes (in addition to `http.status.2xx`) |
| `http.status.301` / `302` / `304` | counter | cluster, backend | Common 3xx redirect/cache codes (in addition to `http.status.3xx`) |
| `http.status.400` / `401` / `403` / `404` / `408` / `413` / `429` | counter | cluster, backend | Common 4xx client-error codes (in addition to `http.status.4xx`) |
| `http.status.500` / `502` / `503` / `504` / `507` | counter | cluster, backend | Common 5xx server-error codes (in addition to `http.status.5xx`) |

#### Default answer / error responses

Incremented when Sōzu generates a default error response instead of proxying:

| Metric | Type | Scope | Description |
|--------|------|-------|-------------|
| `http.301.redirection` | counter | proxy | 301 Moved Permanently (HTTP→HTTPS redirect) |
| `http.400.errors` | counter | proxy | 400 Bad Request (cannot parse hostname) |
| `http.401.errors` | counter | proxy | 401 Unauthorized |
| `http.404.errors` | counter | proxy | 404 Not Found (no matching cluster) |
| `http.408.errors` | counter | proxy | 408 Request Timeout |
| `http.413.errors` | counter | proxy | 413 Payload Too Large |
| `http.502.errors` | counter | proxy | 502 Bad Gateway |
| `http.503.errors` | counter | proxy | 503 Service Unavailable (no backends or circuit breaker triggered) |
| `http.504.errors` | counter | proxy | 504 Gateway Timeout |
| `http.507.errors` | counter | proxy | 507 Insufficient Storage (buffer full) |
| `http.other.errors` | counter | proxy | Non-standard error response code (mux path only) |

#### Parse errors

| Metric | Type | Scope | Description |
|--------|------|-------|-------------|
| `http.frontend_parse_errors` | counter | proxy | Frontend request parsing failures (malformed HTTP/1.1 or HPACK decode errors in HTTP/2) |
| `http.backend_parse_errors` | counter | proxy | Backend response parsing failures |

#### Backend health

| Metric | Type | Scope | Description |
|--------|------|-------|-------------|
| `backend.connections` | gauge | proxy | Active backend connections |
| `connections_per_backend` | gauge | cluster, backend | Per-backend connection count |
| `backend.up` | counter | proxy | Backend marked as healthy (after successful connection) |
| `backend.down` | counter | proxy | Backend marked as unhealthy (retry policy triggered) |
| `backend.connections.error` | counter | proxy | Backend connection failures |
| `backend.connect.retries_exhausted` | counter | cluster, backend | Per-session backend-connect retry budget (`CONN_RETRIES = 3`) was exhausted. Emitted once per event at the TCP, HTTP/1, and HTTP/2-mux gates. Alert on this counter's rate instead of grepping `WARN` / `ERROR` logs — the underlying log line is `warn!` since the condition is peer-driven backpressure, not a Sōzu invariant break. |

#### Backend pool

H2 mux reuses backend connections via `Router::backends: HashMap<Token, Connection>`
(`lib/src/protocol/mux/router.rs`). There is no separate pool abstraction: the map
is the pool. Reuse picks an existing non-draining H2 multiplex slot (below
`SETTINGS_MAX_CONCURRENT_STREAMS`) or an H1 keep-alive socket; misses dial a fresh
backend socket.

| Metric | Type | Scope | Description |
|--------|------|-------|-------------|
| `backend.pool.hit` | counter | proxy | Request attached to an existing backend connection (H2 multiplex slot or H1 keep-alive). |
| `backend.pool.miss` | counter | proxy | No reusable connection found; a fresh dial starts. Incremented before `backend_from_request`, so failed selections still count. Dial may still fail — in that case `backend.pool.size` is not bumped. |
| `backend.pool.size` | gauge | proxy | Live mux router entries. `+1` at `router.rs::connect` new-dial commit; `-1` at `connection.rs::pre_close_client_bookkeeping` and `mod.rs::close_backend`. Mirrors the `backend.connections` site set in mux exactly, so gauge symmetry follows from `backend.connections` correctness. Non-mux H1/TCP paths are NOT counted here. |
| `backend.flow_control.paused` | counter | proxy | Direction-scoped counterpart of `h2.flow_control_stall`: emitted only when the converter stalls while writing toward an upstream backend (`Position::Client`) because the backend's HTTP/2 receive window is empty. |

Intentionally **not** emitted in this slice (no corresponding lifecycle exists):

* `backend.pool.idle_closed` — mux has no connection-level idle eviction. The H2 `stream_idle_timeout` cancels individual streams (slow-multiplex guard), not pool entries.
* `backend.pool.overflow` — `Router::backends` is unbounded. The only new-connection refusal is HTTP/2 buffer-pool exhaustion (`MaxBuffers`), unrelated to pool sizing.
* `backend.flow_control.resumed` — the converter has no "resumed" boundary; the next writable cycle just succeeds when the backend ACKs window updates. Plumbing an explicit marker through `flush_stream_out` was deferred.

#### Backend metrics (per cluster/backend)

These metrics are recorded with `cluster_id` and `backend_id` labels via the
`record_backend_metrics!` macro at the end of each request:

| Metric | Type | Scope | Description |
|--------|------|-------|-------------|
| `requests` | counter | cluster, backend | Requests handled by this backend |
| `bytes_in` | counter | cluster, backend | Bytes received from this backend |
| `bytes_out` | counter | cluster, backend | Bytes sent to this backend |
| `backend_response_time` | time | cluster, backend | Response time for this backend (ms) |
| `backend_connection_time` | time | cluster, backend | Connection setup time for this backend (ms) |

#### ALPN negotiation

| Metric | Type | Scope | Description |
|--------|------|-------|-------------|
| `http.alpn.h2` | counter | proxy | TLS connections where client negotiated HTTP/2 via ALPN |
| `http.alpn.http11` | counter | proxy | TLS connections where client negotiated HTTP/1.1 via ALPN (or no ALPN) |
| `https.alpn.rejected.http11_disabled` | counter | proxy | TLS connection refused on an H2-only listener because the peer offered `http/1.1` or no ALPN value |
| `https.alpn.rejected.unsupported` | counter | proxy | TLS connection refused because the peer negotiated an ALPN protocol Sōzu does not recognise (anything other than `h2`, `http/1.1`, or absent ALPN). Add to the same SOC bucket as `https.alpn.rejected.http11_disabled`. |

#### TLS version and cipher suite

Incremented once per TLS connection after the handshake completes.

| Metric | Type | Scope | Description |
|--------|------|-------|-------------|
| `tls.version.SSLv2` | counter | proxy | Connections using SSLv2 |
| `tls.version.SSLv3` | counter | proxy | Connections using SSLv3 |
| `tls.version.TLSv1_0` | counter | proxy | Connections using TLS 1.0 |
| `tls.version.TLSv1_1` | counter | proxy | Connections using TLS 1.1 |
| `tls.version.TLSv1_2` | counter | proxy | Connections using TLS 1.2 |
| `tls.version.TLSv1_3` | counter | proxy | Connections using TLS 1.3 |
| `tls.version.DTLSv1_0` | counter | proxy | Connections using DTLS 1.0 |
| `tls.version.DTLSv1_2` | counter | proxy | Connections using DTLS 1.2 |
| `tls.version.DTLSv1_3` | counter | proxy | Connections using DTLS 1.3 |
| `tls.version.Unknown` | counter | proxy | Unrecognized TLS version |
| `tls.version.unimplemented` | counter | proxy | TLS version not yet handled in code |
| `tls.default_cert_used` | counter | proxy | Fallback to default certificate (no SNI match) |

Negotiated cipher suite (rustls):

| Metric | Type | Scope |
|--------|------|-------|
| `tls.cipher.TLS13_AES_128_GCM_SHA256` | counter | proxy |
| `tls.cipher.TLS13_AES_256_GCM_SHA384` | counter | proxy |
| `tls.cipher.TLS13_CHACHA20_POLY1305_SHA256` | counter | proxy |
| `tls.cipher.TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256` | counter | proxy |
| `tls.cipher.TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384` | counter | proxy |
| `tls.cipher.TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256` | counter | proxy |
| `tls.cipher.TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256` | counter | proxy |
| `tls.cipher.TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384` | counter | proxy |
| `tls.cipher.TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256` | counter | proxy |
| `tls.cipher.Unsupported` | counter | proxy |

#### TLS handshake telemetry

Emitted by the rustls handshake driver at `lib/src/protocol/rustls.rs`.
Failure counters sit next to the tiered log emission in `log_handshake_error`
so every `warn!`/`error!`/`debug!` about a broken handshake also bumps the
matching counter. The histogram is recorded at the moment the handshake
transitions out of `is_handshaking()` (both through the `readable` and
`writable` exit paths).

| Metric | Type | Scope | Description |
|--------|------|-------|-------------|
| `tls.handshake.failed.alert_received` | counter | proxy | Remote peer sent a fatal TLS alert (`RustlsError::AlertReceived`). Typical causes: cert-pinning client, stale CA bundle, scanner. Logged at `debug!`. |
| `tls.handshake.failed.peer_incompatible` | counter | proxy | Peer advertised a version/feature mix we cannot negotiate (`PeerIncompatible`). Logged at `warn!`. |
| `tls.handshake.failed.peer_misbehaved` | counter | proxy | Peer deviated from the TLS state machine (`PeerMisbehaved`). Logged at `warn!`. |
| `tls.handshake.failed.invalid_message` | counter | proxy | Wire-level record parse failure (`InvalidMessage`). Logged at `warn!`. |
| `tls.handshake.failed.inappropriate_message` | counter | proxy | Peer sent a record type that was valid on the wire but not allowed in the current phase (`InappropriateMessage`). Logged at `warn!`. |
| `tls.handshake.failed.inappropriate_handshake_message` | counter | proxy | Peer sent a handshake sub-type the state machine did not expect (`InappropriateHandshakeMessage`). Logged at `warn!`. |
| `tls.handshake.failed.oversized_record` | counter | proxy | Peer sent a record larger than the RFC 8446 §5.1 cap (`PeerSentOversizedRecord`). Logged at `warn!`. |
| `tls.handshake.failed.no_alpn` | counter | proxy | ALPN negotiation failed (`NoApplicationProtocol`) — e.g. peer offered only protocols the listener does not serve. Logged at `warn!`. |
| `tls.handshake.failed.invalid_certificate` | counter | proxy | Peer-supplied certificate failed verification (`InvalidCertificate`). Logged at `warn!`. Only relevant when mTLS is enabled. |
| `tls.handshake.failed.decrypt_error` | counter | proxy | Record failed to decrypt (`DecryptError`) — almost always an attack or a broken middlebox. Logged at `warn!`. |
| `tls.handshake.failed.no_certificates_present` | counter | proxy | mTLS: peer sent an empty Certificate message (`NoCertificatesPresented`). Logged at `warn!`. |
| `tls.handshake.failed.other` | counter | proxy | Catch-all for local/config/provider failures (`General`, `Other`, `EncryptError`, `FailedToGetRandomBytes`, CRL errors, future rustls variants). Logged at `error!` — these indicate a server-side problem, not a bad client. |
| `tls.handshake_ms` | time | proxy | Wall-clock duration in milliseconds from the first TLS byte observed on the socket to the handshake leaving `is_handshaking()`. Histogram — not a counter — so alert rules should use quantiles. |

#### TLS certificate expiration

| Metric | Type | Scope | Description |
|--------|------|-------|-------------|
| `tls.cert.min_expires_at_seconds` | gauge | proxy | Unix-seconds timestamp of the soonest-expiring certificate currently loaded in the `CertificateResolver`. Recomputed on every add/remove/replace at `lib/src/tls.rs`. Aggregate only — per-SNI granularity is intentionally omitted because statsd has no label support and the resolver can hold tens of thousands of names; operators query per-cert detail through the command API. Already-expired certificates clamp to `0`, which dashboards should interpret as the "rotate now" alert condition. |

#### HTTP/2 specific

| Metric | Type | Scope | Description |
|--------|------|-------|-------------|
| `h2.connection.active_streams` | gauge | proxy | **Aggregate** count of open HTTP/2 streams across every live H2 connection on the worker. Emitted as a [`gauge_add!`] lifecycle delta from `ConnectionH2::gauge_connection_state`; the per-connection contribution is subtracted on connection drop, so the value sums correctly under multi-connection load. |
| `h2.connection.window_bytes` | gauge | proxy | **Aggregate** sum of available connection-level flow-control window bytes across every live H2 connection. Negative per-connection windows clamp to 0 — the aggregate measures available capacity, not deficit. Lifecycle-delta semantics as above. |
| `h2.connection.pending_window_updates` | gauge | proxy | **Aggregate** number of queued (un-flushed) per-stream WINDOW_UPDATE entries across every live H2 connection. Lifecycle-delta semantics as above. |

> **Migration note (renamed metrics).** The three keys above replace the
> earlier `h2.connection_window`, `h2.active_streams`, and
> `h2.pending_window_updates` gauges. The old keys had **per-connection
> snapshot** semantics implemented via absolute `gauge!`: under concurrent
> load every H2 connection clobbered the previous one's value, so the
> dashboard saw the last writer rather than the aggregate. The new keys
> emit lifecycle deltas via `gauge_add!`, so the value is the **sum across
> all live H2 connections** on the worker. Update dashboards and alerts
> accordingly — the new values typically rise into the hundreds or
> thousands under load instead of cycling 0…N.

| `h2.flow_control_stall` | counter | proxy | Converter stalled due to flow control |
| `h2.close_with_active_streams` | counter | proxy | H2 connections closed while streams were still active |
| `h2.window_update_dropped` | counter | proxy | WINDOW_UPDATE frame dropped because the per-connection pending-update queue was already at capacity |
| `h2.headers_no_stream.error` | counter | proxy | HEADERS frame received with no matching stream (protocol error) |
| `h2.frames.tx.headers` | counter | proxy | HEADERS frames emitted by the H2 block converter (one per response, plus the first frame of any header block split into HEADERS+CONTINUATION when the encoded headers exceed the negotiated `max_frame_size`) |
| `h2.frames.tx.continuation` | counter | proxy | CONTINUATION frames emitted by the converter when a single response's encoded headers cross `max_frame_size` (default 16 KB). Stays at zero for typical responses |
| `h2.frames.tx.data` | counter | proxy | DATA frames emitted by the converter — both the normal data path and the empty `END_STREAM`-only marker emitted when the response has zero bytes after headers |

#### TLS / SNI binding

| Metric | Type | Scope | Description |
|--------|------|-------|-------------|
| `http.sni_authority_mismatch` | counter | proxy | Request rejected because its `:authority` (HTTP/2) or `Host` header (HTTP/1.1) crossed the TLS SNI boundary negotiated for the connection. Defence against cross-tenant frontend confusion (CWE-346). Increment site: `lib/src/protocol/mux/router.rs`. |

#### HTTP/2 frame counters

Per-frame-type counters split by direction. Receive side counts every H2
frame the parser hands to `handle_frame` — single chokepoint, so adding a
new H2 frame type fails the build inside the metric helper. Send side
covers the control frames Sozu emits; HEADERS and DATA tx flow through
the H2 block converter and are not yet broken out per type (tracked as
follow-up — pair with `back_bytes_out` for the byte view today).

| Metric | Type | Scope | Description |
|---|---|---|---|
| `h2.frames.rx.data` | counter | proxy | DATA frames received |
| `h2.frames.rx.headers` | counter | proxy | HEADERS frames received |
| `h2.frames.rx.push_promise` | counter | proxy | PUSH_PROMISE frames received (always rejected; see `h2.goaway.sent.protocol_error`) |
| `h2.frames.rx.priority` | counter | proxy | RFC 7540 PRIORITY frames received |
| `h2.frames.rx.rst_stream` | counter | proxy | RST_STREAM frames received (paired with `h2.rst_stream.received.<code>` for per-error breakdown) |
| `h2.frames.rx.settings` | counter | proxy | SETTINGS frames received (both peer-settings and ACKs) |
| `h2.frames.rx.ping` | counter | proxy | PING frames received (both probes and ACKs) |
| `h2.frames.rx.goaway` | counter | proxy | GOAWAY frames received (paired with `h2.goaway.received.<code>`) |
| `h2.frames.rx.window_update` | counter | proxy | WINDOW_UPDATE frames received |
| `h2.frames.rx.continuation` | counter | proxy | Reachable only via the defensive fallback path (RFC 9113 §6.10 standalone CONTINUATION) — the inline header parser absorbs CONTINUATION during HEADERS decoding, so under normal conditions this stays at zero |
| `h2.frames.rx.unknown` | counter | proxy | Unknown frame type ignored per RFC 9113 §5.5. A non-zero rate is the early-warning signal for a peer trying H2 extensions |
| `h2.frames.tx.settings` | counter | proxy | SETTINGS frames emitted (initial + later updates) |
| `h2.frames.tx.window_update` | counter | proxy | WINDOW_UPDATE frames emitted (per-frame in the queued-update flush loop) |
| `h2.frames.tx.rst_stream` | counter | proxy | RST_STREAM frames emitted (across both the queued flush loop and the end-stream cancel path) |
| `h2.frames.tx.goaway` | counter | proxy | GOAWAY frames emitted (one per phase of `graceful_goaway`, plus error-path `goaway`) |
| `h2.frames.tx.ping_ack` | counter | proxy | PING ACK frames emitted in response to peer probes |

#### HTTP/2 edge-triggered wake-up instrumentation

LIFECYCLE `§9` invariant 15 compliance counters. Each firing records a
site where sozu paired `Ready::WRITABLE` with `signal_pending_write` so
the edge-triggered epoll scheduler re-runs `writable()` on the next
tick — required whenever bytes land in sozu-owned buffers (the kernel
never signals WRITABLE for buffers it does not own). Useful as a
regression baseline: the first two correlate with 504/500/400 rate and
RFC 9218 PRIORITY_UPDATE rate respectively, the last two correlate
with backend-H2 traffic volume. A 10× spike on any counter without a
matching spike on the upstream driver is the early warning for a hot
loop regression.

| Metric | Type | Scope | Description |
|---|---|---|---|
| `h2.signal.writable.rearmed.default_answer` | counter | proxy | Fired in `mux/answers.rs::set_default_answer` (504 backend timeout, 500/400 parse errors). Rate should track the default-answer render rate closely. |
| `h2.signal.writable.rearmed.forcefully_terminate_answer` | counter | proxy | Fired in `mux/answers.rs::forcefully_terminate_answer` when the proxy injects a default answer mid-response (e.g. backend disconnects after partial body). Companion to `default_answer`; the forcefully-terminate path is exercised on backend hard-failure and proxy-initiated stream resets. |
| `h2.signal.writable.rearmed.priority_update` | counter | proxy | Fired in `mux/h2.rs::handle_priority_update_frame` whenever a PRIORITY_UPDATE mutates `Prioriser` state. Pairs with `h2.frames.rx.priority_update`. Low-to-zero on Firefox-only fleets (Firefox does not emit 0x10). |
| `h2.signal.writable.rearmed.peer_data` | counter | proxy | Fired in `mux/h2.rs::handle_data_frame` when an H2 DATA frame wakes the linked peer. Non-zero only on clusters that use H2 to the origin. |
| `h2.signal.writable.rearmed.peer_headers` | counter | proxy | Fired in `mux/h2.rs::handle_headers_frame` when an H2 HEADERS frame wakes the linked peer. Non-zero only on clusters that use H2 to the origin. |
| `h2.signal.writable.rearmed.control_queue` | counter | proxy | Fired in `mux/h2.rs::flush_pending_control_frames` when a queued WINDOW_UPDATE or RST_STREAM forces an extra writable pass. Pairs with `h2.frames.tx.window_update` / `h2.frames.tx.rst_stream`. Persistent non-zero rate without matching tx growth points to a control-frame queue that fills faster than it drains. |
| `h2.streams.ready_incremental.by_urgency` | gauge | proxy | Post-scheduling-pass snapshot of the sum of ready incremental streams across all urgency buckets (RFC 9218 §4). Debug hint for scheduler fairness — a consistently non-zero value under load means the round-robin is active. |
| `h2.trailers_dropped_content_length` | counter | proxy | Fired in `mux/pkawa.rs::handle_trailer` when an H2 trailer block is rejected for carrying a `Content-Length` header (RFC 9113 §8.1 disallows pseudo-headers and Content-Length in trailers). Spikes correlate with malformed gRPC clients or smuggling attempts; aggregate cardinality is bounded. |
| `h1.backend_eof_before_message_complete` | counter | proxy | Fired in `mux/h1.rs::back_readable` when the H1 backend closes the socket before the response body is fully delivered (chunked-EOF or Content-Length-EOF mid-stream). The H2 converter surfaces this as `RST_STREAM(InternalError)` to the H2 client; non-zero rate maps to backend application crashes or proxy-side reads. |

#### HTTP/2 GOAWAY and RST_STREAM by error code

Counters split by direction (sent vs received) and RFC 9113 §7 error code.
Every variant in the H2Error enum gets its own counter so SOC dashboards can
distinguish a sustained `protocol_error` (parser issue / fuzzer) from
`enhance_your_calm` (flood-detector trip) from `no_error` (graceful drain).
Codes the wire delivers but RFC 9113 does not define are bucketed under
`unknown_error` to keep cardinality bounded.

| Metric pattern | Type | Scope | Description |
|---|---|---|---|
| `h2.goaway.sent.<code>` | counter | proxy | GOAWAY emitted by Sozu. `<code>` ∈ `no_error`, `protocol_error`, `internal_error`, `flow_control_error`, `settings_timeout`, `stream_closed`, `frame_size_error`, `refused_stream`, `cancel`, `compression_error`, `connect_error`, `enhance_your_calm`, `inadequate_security`, `http_1_1_required`. The graceful drain (`graceful_goaway`) emits two `no_error` increments per connection — one per phase. |
| `h2.goaway.received.<code>` | counter | proxy | GOAWAY received from peer. Same code suffixes as sent, plus `unknown_error` for codes outside RFC 9113 §7. Useful on backend H2 connections to detect upstreams under pressure (`enhance_your_calm`) or with bugs (`internal_error`). |
| `h2.rst_stream.sent.<code>` | counter | proxy | RST_STREAM emitted by Sozu. Same code suffixes. The `no_error` and `cancel` increments are graceful (stream recycle, propagated client cancel); the rest are server-side defences against attacker-crafted frames. |
| `h2.rst_stream.received.<code>` | counter | proxy | RST_STREAM received from peer. Same code suffixes plus `unknown_error`. |
| `h2.rst_stream.received.pre_response_start` | counter | proxy | Subset of `h2.rst_stream.received.*` where the RST arrived before the backend started answering. The canonical Rapid Reset signature (CVE-2023-44487). Emitted alongside the per-code counter, not instead of, so a Rapid Reset attack surfaces both as a `cancel` rate spike and as the pre-response signal. |

#### HTTP/2 header validation (HPACK rejections)

Counters emitted whenever the HPACK decoder in the H2→H1 converter
rejects a header. `h2.headers.rejected.total` is bumped on every reject;
a per-reason counter is bumped alongside so `total == sum(per_reason)`.
Rejection is silent on the wire (the mux either RSTs the stream or
treats the request as malformed) — this counter family is the only
externally visible signal for request-smuggling probes, HPACK fuzzing,
and H2-specific protocol abuse.

| Metric | Type | Scope | Description |
|---|---|---|---|
| `h2.headers.rejected.total` | counter | proxy | Aggregate count of all HPACK rejections |
| `h2.headers.rejected.invalid_name_byte` | counter | proxy | Header name with uppercase / CTL / separator / space byte (CWE-93) |
| `h2.headers.rejected.connection_specific_header` | counter | proxy | `connection`, `proxy-connection`, `transfer-encoding`, `upgrade`, `keep-alive` (RFC 9113 §8.2.2) |
| `h2.headers.rejected.te_not_trailers` | counter | proxy | `te` with value other than `trailers` (RFC 9113 §8.2.2) |
| `h2.headers.rejected.crlf_in_value` | counter | proxy | CR or LF in a header value — request-smuggling vector (CWE-444) |
| `h2.headers.rejected.nul_in_value` | counter | proxy | NUL byte in a header value |
| `h2.headers.rejected.oversized_pseudo_value` | counter | proxy | Pseudo-header value exceeded storage cap |
| `h2.headers.rejected.cl_te_conflict` | counter | proxy | Content-Length declared alongside a Transfer-Encoding — classic smuggling vector |
| `h2.headers.rejected.duplicate_cl` | counter | proxy | Multiple disagreeing Content-Length values (RFC 9110 §8.6) |
| `h2.headers.rejected.duplicate_pseudo` | counter | proxy | Same pseudo-header appeared twice |
| `h2.headers.rejected.pseudo_after_regular` | counter | proxy | Pseudo-header appeared after a regular header (RFC 9113 §8.3) |
| `h2.headers.rejected.unknown_pseudo` | counter | proxy | Unknown `:` -prefixed pseudo-header |
| `h2.headers.rejected.empty_pseudo` | counter | proxy | Pseudo-header value was empty (RFC 9113 §8.3.1) |
| `h2.headers.rejected.invalid_method` | counter | proxy | `:method` value was not a valid RFC 9110 §9 token |
| `h2.headers.rejected.invalid_scheme` | counter | proxy | `:scheme` was not `http` or `https` |
| `h2.headers.rejected.invalid_path` | counter | proxy | `:path` contained a `#` fragment (not allowed on the wire) |
| `h2.headers.rejected.invalid_status` | counter | proxy | Response `:status` was not three ASCII digits |

#### Request-ID propagation

Sōzu preserves or generates an `x-request-id` header on every H1 request
and every H2 stream (both paths share the same H1 editor callback via
`pkawa.rs`). The value also lands on the access log's
`x_request_id` field — same value Sōzu forwarded to the backend, end to
end.

| Metric | Type | Scope | Description |
|---|---|---|---|
| `http.x_request_id.propagated` | counter | proxy | Request already carried an `x-request-id` header; Sōzu preserved it verbatim |
| `http.x_request_id.generated` | counter | proxy | Request had no `x-request-id`; Sōzu generated one from the request ULID and injected it before forwarding |

The `x_request_id` access-log field (wire tag `ProtobufAccessLog.x_request_id` #24)
carries whichever value was sent to the backend.

#### TLS handshake metadata on access logs

Five additional access-log fields surface the negotiated TLS metadata and
the upstream-attested forwarded chain. They are wire-compatible appends to
`ProtobufAccessLog` (tags 25–29) and are populated end-to-end on H1 and H2
mux paths, plus the WSS post-upgrade pipe. Pure plaintext paths (HTTP, WS,
TCP) emit `None` for all five.

| Field | Wire tag | Source | Notes |
|---|---|---|---|
| `tls_version` | `ProtobufAccessLog.tls_version` #25 | `rustls_version_label(handshake.session.protocol_version())` | Short form (e.g. `TLSv1.3`). Captured once at handshake completion in `lib/src/https.rs::upgrade_handshake`. `None` when rustls reports an unknown variant. |
| `tls_cipher` | `ProtobufAccessLog.tls_cipher` #26 | `rustls_ciphersuite_label(handshake.session.negotiated_cipher_suite())` | Short form (e.g. `TLS_AES_128_GCM_SHA256`). `None` when rustls reports an unsupported cipher. |
| `tls_sni` | `ProtobufAccessLog.tls_sni` #27 | `handshake.session.server_name()` | Pre-lowercased, no port. Same value the routing layer uses to enforce the SNI ↔ `:authority` binding. `None` when the client omitted SNI. |
| `tls_alpn` | `ProtobufAccessLog.tls_alpn` #28 | ALPN negotiation in `upgrade_handshake` | `h2`, `http/1.1`, or `None` when no ALPN was negotiated. |
| `xff_chain` | `ProtobufAccessLog.xff_chain` #29 | Verbatim `X-Forwarded-For` header value | Snapshotted in `editor.rs::on_request_headers` *before* Sōzu appends its own peer hop, so the log records the upstream-attested chain (e.g. `203.0.113.5, 198.51.100.10`). `None` when the request has no `X-Forwarded-For` header. |

TLS fields are connection-scoped: they are stamped once on the mux
`Context` at handshake time and propagated to every per-stream
`HttpContext` via `Context::create_stream`, so an H2 connection
multiplexing N streams pays the cost once. The labels are `&'static str`
borrows into rustls's static label tables — no per-request allocation.

#### HTTP/2 flood mitigations

Incremented once per connection at the moment the H2 flood detector trips its
threshold and the proxy escalates to `GOAWAY(ENHANCE_YOUR_CALM)`. Every CVE
mitigation in the H2 family (Rapid Reset, MadeYouReset, the CONTINUATION /
PING / SETTINGS / empty-DATA flood family, oversized header lists, and the
generic `glitch` budget) routes through `ConnectionH2::handle_flood_violation`,
which emits both the contextual log line and the per-kind counter below.

| Metric | Type | Scope | Description |
|--------|------|-------|-------------|
| `h2.flood.violation.rst_stream_window` | counter | proxy | Per-window RST_STREAM rate ceiling exceeded. Generic stream-cancel storm signal — usually a misbehaving client, sometimes Rapid Reset. |
| `h2.flood.violation.rst_stream_lifetime` | counter | proxy | Lifetime received-RST ceiling exceeded. Catches a patient Rapid Reset attacker that stays under the windowed cap (CVE-2023-44487). |
| `h2.flood.violation.rst_stream_pre_response_lifetime` | counter | proxy | Lifetime received-RST ceiling exceeded for streams that the backend had not yet started answering. The canonical Rapid Reset signature (CVE-2023-44487). |
| `h2.flood.violation.rst_stream_emitted_lifetime` | counter | proxy | Lifetime server-emitted RST ceiling exceeded. MadeYouReset mitigation (CVE-2025-8671) — peer kept feeding the server crafted frames that forced it to reset streams. |
| `h2.flood.violation.ping_window` | counter | proxy | Per-window PING flood (CVE-2019-9512). |
| `h2.flood.violation.ping_lifetime` | counter | proxy | Lifetime PING ceiling exceeded — catches sustained low-rate PING abuse that stays under the windowed cap. |
| `h2.flood.violation.settings_window` | counter | proxy | Per-window SETTINGS flood (CVE-2019-9515). |
| `h2.flood.violation.settings_lifetime` | counter | proxy | Lifetime SETTINGS ceiling exceeded. |
| `h2.flood.violation.empty_data_window` | counter | proxy | Per-window flood of empty DATA frames (CVE-2019-9518). |
| `h2.flood.violation.continuation_per_block` | counter | proxy | Single header block split across more CONTINUATION frames than the configured cap (CVE-2024-27316). |
| `h2.flood.violation.header_size_per_block` | counter | proxy | Single header block accumulated more bytes than the configured cap (CVE-2024-27316 sibling — header overflow). |
| `h2.flood.violation.glitch_window` | counter | proxy | Generic anomaly budget exceeded (unknown SETTINGS, WINDOW_UPDATE on closed stream, other low-severity protocol drift). |

#### Protocol upgrade failures

Incremented when a session fails to transition between protocol phases:

| Metric | Type | Scope | Description |
|--------|------|-------|-------------|
| `http.upgrade.expect.failed` | counter | proxy | HTTP: PROXY protocol expect → mux transition failed |
| `http.upgrade.mux.failed` | counter | proxy | HTTP: mux protocol upgrade failed |
| `http.upgrade.ws.failed` | counter | proxy | HTTP: WebSocket upgrade failed |
| `https.upgrade.expect.failed` | counter | proxy | HTTPS: PROXY protocol expect → handshake transition failed |
| `https.upgrade.handshake.failed` | counter | proxy | HTTPS: TLS handshake → mux transition failed |
| `https.upgrade.mux.failed` | counter | proxy | HTTPS: mux protocol upgrade failed |
| `https.upgrade.wss.failed` | counter | proxy | HTTPS: WebSocket over TLS upgrade failed |
| `tcp.upgrade.pipe.failed` | counter | proxy | TCP: pipe protocol upgrade failed |
| `tcp.upgrade.send.failed` | counter | proxy | TCP: PROXY protocol send transition failed |
| `tcp.upgrade.relay.failed` | counter | proxy | TCP: PROXY protocol relay transition failed |
| `tcp.upgrade.expect.failed` | counter | proxy | TCP: PROXY protocol expect transition failed |

#### Socket and I/O errors

| Metric | Type | Scope | Description |
|--------|------|-------|-------------|
| `socket.read.infinite_loop.error` | counter | proxy | TCP socket read loop safety breaker triggered |
| `socket.write.infinite_loop.error` | counter | proxy | TCP socket write loop safety breaker triggered |
| `tcp.read.error` | counter | proxy | TCP socket read error |
| `tcp.write.error` | counter | proxy | TCP socket write error |
| `tcp.infinite_loop.error` | counter | proxy | TCP session event loop safety breaker triggered |
| `rustls.read.error` | counter | proxy | TLS read error |
| `rustls.write.error` | counter | proxy | TLS write error |
| `rustls.read.infinite_loop.error` | counter | proxy | TLS read loop safety breaker triggered |
| `rustls.write.infinite_loop.error` | counter | proxy | TLS write loop safety breaker triggered |

#### Other

| Metric | Type | Scope | Description |
|--------|------|-------|-------------|
| `http.infinite_loop.error` | counter | proxy | HTTP event loop safety breaker triggered |
| `http.failed_backend_matching` | counter | proxy | Frontend matched but no backend could be selected |
| `http.early_response_close` | counter | proxy | Client closed before response was fully sent |
| `http.trusting.x_proto` | counter | proxy | Request had an existing `X-Forwarded-Proto` header (trusted) |
| `http.trusting.x_proto.diff` | counter | proxy | Trusted `X-Forwarded-Proto` differed from actual protocol |
| `http.trusting.x_port` | counter | proxy | Request had an existing `X-Forwarded-Port` header (trusted) |
| `http.trusting.x_port.diff` | counter | proxy | Trusted `X-Forwarded-Port` differed from actual port |
| `pipe.errors` | counter | proxy | Pipe/WebSocket protocol errors |
| `proxy_protocol.errors` | counter | proxy | PROXY protocol v1/v2 parsing errors |
| `unsent-access-logs` | counter | proxy | Access log entries that could not be sent |
| `access_logs.count` | counter | cluster, backend | Access log entries emitted per cluster/backend |

### Example of external services

- [statsd](https://github.com/etsy/statsd)
- [grad](https://github.com/geal/grad)

## OpenTelemetry (traceparent passthrough)

The `opentelemetry` compile-time feature flag enables **W3C Trace Context passthrough**:
Sōzu parses, generates, and forwards `traceparent` headers across the proxy hop, and
records the trace identifiers in its access logs. The feature name is historic — the
implementation is intentionally a propagator only. There is no OpenTelemetry SDK
dependency, no span lifecycle, and no OTLP exporter. See "Out of scope" below.

### Enabling

Build Sōzu with the `opentelemetry` feature:

```bash
cargo build --release --features opentelemetry
```

Or in `Cargo.toml`:

```toml
[dependencies]
sozu-lib = { path = "lib", features = ["opentelemetry"] }
```

### How it works

When the `opentelemetry` feature is enabled, Sōzu acts as a **trace context propagator**
for both HTTP/1.1 and HTTP/2 frontends. The kawa H1 editor (`lib/src/protocol/kawa_h1/editor.rs`)
runs on every request, including those decoded from HPACK on the H2 mux path
(`lib/src/protocol/mux/pkawa.rs` calls back into the same editor). On the wire the H2
backend leg re-encodes the rewritten `traceparent` via `H2BlockConverter`.

1. **Incoming request with `traceparent` header**: Sōzu parses the W3C traceparent
   (format: `00-<trace_id>-<parent_id>-<flags>`), preserves the trace ID, generates
   a new span ID for the Sōzu hop, and rewrites the header before forwarding to the backend.

2. **Incoming request without `traceparent` header**: Sōzu generates a new random
   trace ID and span ID, and injects a `traceparent` header into the request before
   forwarding.

3. **`tracestate` header**: Preserved if a valid `traceparent` is present. Elided if
   no `traceparent` accompanies it (per W3C spec).

4. **Access logs**: The trace context (trace ID, span ID, parent span ID) is included in
   access log entries and in the protobuf `AccessLog` message, enabling correlation
   between Sōzu access logs and distributed traces in your observability platform.

### Access log fields

When OpenTelemetry is enabled, access logs include:

| Field | Format | Description |
|-------|--------|-------------|
| `trace_id` | 32 hex characters | W3C trace ID (propagated or generated) |
| `span_id` | 16 hex characters | Sōzu-generated span ID for this hop |
| `parent_span_id` | 16 hex characters or `-` | Parent span ID from incoming `traceparent`, if present |

### Out of scope

The `opentelemetry` feature is intentionally narrow. It does NOT:

- Pull `opentelemetry`, `opentelemetry-sdk`, `opentelemetry-otlp`, or `tracing-opentelemetry`
  as dependencies. The feature gate (`lib/Cargo.toml`) wires no extra crates.
- Emit spans. There is no `Span::start`/`Span::end`, no in-process exporter, no OTLP/gRPC
  client. Use your access log pipeline to feed Jaeger, Tempo, Honeycomb, or Datadog with
  the per-request trace identifiers.
- Honour the sampled-flag bit of incoming `traceparent`. Today the rewritten header is
  always emitted with `-01` (sampled). Downstream backends that respect the sampled flag
  will see every request as sampled.
- Provide runtime configuration. The feature is selected at compile time only.

A real span model and an OTLP exporter are tracked separately and would land as their own
feature flag rather than expanding the scope of `opentelemetry`.

## PROXY Protocol

When a network stream goes through a proxy, the backend server will only see the IP address and port used by the proxy as client address.
The real source IP address and port will only be seen by the proxy.
Since this information is useful for logging, security, etc,
the [PROXY protocol](https://www.haproxy.org/download/1.8/doc/proxy-protocol.txt) was developed to transmit it to backend servers.
With this protocol, after connecting to the backend server, the proxy will first send a small header indicating the client IP address and port,
and the proxy's receiving IP address and port, and will then send the stream from the client.

Sōzu support the _version 2_ of the `PROXY protocol` in three configurations:

- "send" protocol: Sōzu, in TCP proxy mode, will send the header to the backend server
- "expect" protocol: Sōzu receives the header from a proxy, interprets it for its own logging and metrics, and uses it in HTTP forwarding headers
- "relay" protocol: Sōzu, in TCP proxy mode, can receive the header, and transmit it to a backend server

More information here: [proxy-protocol spec](https://www.haproxy.org/download/1.8/doc/proxy-protocol.txt)

### Configuring Sōzu to _expect_ a PROXY Protocol header

Configures the client-facing connection to receive a PROXY protocol header before any byte sent by the client is read from the socket.

```txt
                           send PROXY                    expect PROXY
                           protocol header               protocol header
    +--------+
    | client |             +---------+                   +------------+      +-----------+
    |        |             | proxy   |                   | Sozu       |      | upstream  |
    +--------+  ---------> | server  |  ---------------> |            |------| server    |
   /        /              |         |                   |            |      |           |
  /________/               +---------+                   +------------+      +-----------+
```

It is supported by HTTP, HTTPS and TCP proxies.

_Configuration:_

```toml
[[listeners]]
address = "0.0.0.0:80"
expect_proxy = true
```

### Configuring Sōzu to _send_ a PROXY Protocol header to an upstream backend

Send a PROXY protocol header over any connection established to the backends declared in the cluster.

```txt
                           send PROXY
    +--------+             protocol header
    | client |             +---------+                +-----------------+
    |        |             | Sozu    |                | proxy/upstream  |
    +--------+  ---------> |         |  ------------> | server          |
   /        /              |         |                |                 |
  /________/               +---------+                +-----------------+
```

_Configuration:_

```toml
[[listeners]]
address = "0.0.0.0:81"

[clusters]
[clusters.NameOfYourTcpCluster]
send_proxy = true
frontends = [
  { address = "0.0.0.0:81" }
]
```

NOTE: Only for TCP clusters (HTTP and HTTPS proxies will use the forwarding headers).

### Configuring Sōzu to _relay_ a PROXY Protocol header to an upstream

Sōzu will receive a PROXY protocol header from the client connection, check its validity and then forward it to an upstream backend. This allows for chains of reverse-proxies without losing the client connection information.

```txt
                           send PROXY                       expect PROXY               send PROXY
                           protocol header                  protocol header            protocol header
    +--------+
    | client |             +---------+                      +------------+             +-------------------+
    |        |             | proxy   |                      | Sozu       |             | proxy/upstream    |
    +--------+  +--------> | server  |  +-----------------> |            | +---------> | server            |
   /        /              |         |                      |            |             |                   |
  /________/               +---------+                      +------------+             +-------------------+
```

_Configuration:_

This only concerns TCP clusters (HTTP and HTTPS proxies can work directly in expect mode, and will use the forwarding headers).

```toml
[[listeners]]
address = "0.0.0.0:80"
expect_proxy = true

[clusters]

[clusters.NameOfYourCluster]
send_proxy = true
frontends = [
  { address = "0.0.0.0:80" }
]
```
