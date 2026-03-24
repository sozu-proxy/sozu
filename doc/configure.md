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
| `log_level`                | possible values are                                                                 | `debug`, `trace`, `error`, `warn`, `info`|
| `log_target`               | possible values are                                                                 | `stdout, tcp or udp address`             |
| `access_logs_target`        | possible values are (if activated, sends access logs to a separate target)          | `stdout`, `tcp` or `udp address`         |
| `command_socket`           | path to the unix socket command                  |                                          |
| `command_buffer_size`      | size, in bytes, of the buffer used by the main process to handle commands.          |                                          |
| `max_command_buffer_size`  | maximum size of the buffer used by the main process to handle commands.             |                                          |
| `worker_count`             | number of workers                                                                   |                                          |
| `worker_automatic_restart` | if activated, workers that panicked or crashed are restarted (activated by default) |                                          |
| `handle_process_affinity`  | bind workers to cpu cores.                                                          |                                          |
| `max_connections`          | maximum number of simultaneous / opened connections                                 |                                          |
| `max_buffers`              | maximum number of buffers use to proxying                                           |                                          |
| `min_buffers`              | minimum number of buffers preallocated for proxying                                 |                                          |
| `buffer_size`              | size, in bytes, of requests buffer used by the workers. Must be at least 16393 for HTTP/2 (16384 max frame size + 9 byte frame header) |                                          |
| `ctl_command_timeout`      | maximum time the command line will wait for a command to complete                            |                                          |
| `pid_file_path`            | stores the pid in a specific file location                                          |                                          |
| `front_timeout`            | maximum time of inactivity for a front socket                                       |                                          |
| `connect_timeout`          | maximum time of inactivity for a request to connect                                 |                                          |
| `request_timeout`          | maximum time of inactivity for a request                                            |                                          |
| `zombie_check_interval`    | duration between checks for zombie sessions                                         |                                          |
| `activate_listeners`       | automatically start listeners                                                       |                                          |

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
buffer_size = 16384
activate_listeners = true
```

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

```toml
# supported TLS versions. Possible values are "SSL_V2", "SSL_V3",
# "TLS_V12", "TLS_V13". Defaults to "TLS_V12" and "TLS_V13"
tls_versions = ["TLS_V12", "TLS_V13"]
```

#### Options specific to Rustls based HTTPS listeners

```toml
# option specific to rustls based HTTPS listeners
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

HTTP/2 uses a default maximum frame size of 16384 bytes (16 KiB). Sōzu needs at least
9 additional bytes for the frame header. Set `buffer_size` in the global section to at
least **16393**:

```toml
buffer_size = 16393
```

A smaller buffer size will cause HTTP/2 frames to be rejected by peers that expect the
default maximum frame size.

#### H2 flood detection thresholds

Sozu includes built-in flood detection for HTTP/2 connections. When a client sends
an excessive number of certain frame types within a rolling window, Sozu terminates
the connection with a `GOAWAY(ENHANCE_YOUR_CALM)` frame. This protects against
several known HTTP/2 denial-of-service vectors.

Six thresholds are configurable per-listener. When omitted, compile-time defaults
are used:

| Parameter | Default | Protects against | CVE |
|-----------|---------|-----------------|-----|
| `h2_max_rst_stream_per_window` | 100 | Rapid Reset attack: client opens and immediately resets streams in a tight loop | CVE-2023-44487 |
| `h2_max_ping_per_window` | 100 | Ping flood: client sends PING frames faster than the server can respond | CVE-2019-9512 |
| `h2_max_settings_per_window` | 50 | Settings flood: client sends SETTINGS frames requiring ACKs, exhausting server resources | CVE-2019-9515 |
| `h2_max_empty_data_per_window` | 100 | Empty DATA flood: client sends zero-length DATA frames to consume processing time | CVE-2019-9518 |
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
h2_max_continuation_frames = 20       # CONTINUATION flood (CVE-2024-27316)
h2_max_glitch_count = 100             # Cumulative protocol violations
```

> **Note:** When any threshold is exceeded, the connection is terminated with a
> `GOAWAY` frame using the `ENHANCE_YOUR_CALM` error code (HTTP/2 error code 0xb).
> The event is logged at `warn` level with the specific flood type that triggered
> disconnection.

#### H2 connection tuning

Three additional H2 parameters control connection-level behavior. All are optional
per-listener with safe compile-time defaults:

| Parameter | Default | Description |
|-----------|---------|-------------|
| `h2_initial_connection_window` | 1048576 (1MB) | Connection-level receive window size in bytes (RFC 9113 §6.9.2). Clamped to [65535, 2^31-1]. |
| `h2_max_concurrent_streams` | 100 | Maximum concurrent H2 streams the proxy accepts (`SETTINGS_MAX_CONCURRENT_STREAMS`). Minimum: 1. |
| `h2_stream_shrink_ratio` | 2 | Shrink threshold ratio for recycled stream slots. The internal stream Vec is shrunk when `total_slots > active_streams * ratio`. Minimum: 2. |

_Configuration example:_

```toml
[[listeners]]
address = "0.0.0.0:443"
protocol = "https"

# H2 connection tuning (optional, defaults shown)
h2_initial_connection_window = 1048576  # 1MB, min 65535, max 2147483647
h2_max_concurrent_streams = 100         # min 1
h2_stream_shrink_ratio = 2              # min 2
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
```

Metrics are sent at most once per second per key (if updated). Cluster/backend metrics
that have not been updated for 10 minutes are automatically dropped from the network drain.

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
| `client.connections_percentage` | gauge | proxy | Percentage of `max_connections` in use |
| `client.max_connections` | gauge | proxy | Configured maximum connections |
| `slab.entries` | gauge | proxy | Session slab allocator slots used |
| `buffer.number` | gauge | proxy | Buffers currently allocated from the buffer pool |
| `zombies` | counter | proxy | Zombie sessions detected and removed |

#### Accept queue

| Metric | Type | Scope | Description |
|--------|------|-------|-------------|
| `accept_queue.connections` | gauge | proxy | Sockets waiting in the accept queue |
| `accept_queue.backpressure` | gauge | proxy | `1` when max connections reached, `0` when accepting again |
| `accept_queue.wait_time` | time | proxy | How long a socket waited in the accept queue (ms) |
| `accept_queue.timeout` | counter | proxy | Sockets that timed out in the accept queue and were closed |

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
| `http.errors` | counter | proxy | General HTTP processing errors |
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

Incremented per backend response (scope: cluster + backend for `1xx`–`5xx`):

| Metric | Type | Scope | Description |
|--------|------|-------|-------------|
| `http.status.1xx` | counter | cluster, backend | 1xx informational responses |
| `http.status.2xx` | counter | cluster, backend | 2xx success responses |
| `http.status.3xx` | counter | cluster, backend | 3xx redirection responses |
| `http.status.4xx` | counter | cluster, backend | 4xx client error responses |
| `http.status.5xx` | counter | cluster, backend | 5xx server error responses |
| `http.status.other` | counter | proxy | Non-standard status codes |
| `http.status.none` | counter | proxy | Responses without a status code |

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

#### HTTP/2 specific

| Metric | Type | Scope | Description |
|--------|------|-------|-------------|
| `h2.active_streams` | gauge | proxy | Open HTTP/2 streams on a connection |
| `h2.connection_window` | gauge | proxy | Connection-level flow control window size (bytes) |
| `h2.pending_window_updates` | gauge | proxy | Pending WINDOW_UPDATE frames to send |
| `h2.flow_control_stall` | counter | proxy | Converter stalled due to flow control |
| `h2.close_with_active_streams` | counter | proxy | H2 connections closed while streams were still active |
| `h2.window_update_dropped` | counter | proxy | WINDOW_UPDATE frames dropped (no matching stream) |
| `h2.headers_no_stream.error` | counter | proxy | HEADERS frame received with no matching stream (protocol error) |

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

## OpenTelemetry

Sōzu supports [W3C Trace Context](https://www.w3.org/TR/trace-context/) propagation
behind the `opentelemetry` compile-time feature flag.

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
for HTTP/1.1 requests:

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

### Limitations

- OpenTelemetry propagation currently works on **HTTP/1.1 requests only** (the kawa H1 editor
  path). HTTP/2 requests have the trace context extracted from access log records but do not
  yet rewrite `traceparent` on forwarded H2 frames.
- Sōzu does **not** export traces via OTLP. It acts as a propagator only — trace context
  flows through access logs and forwarded headers. Use your access log pipeline to ingest
  traces into Jaeger, Tempo, or similar backends.
- There is no runtime configuration for OpenTelemetry — it is a compile-time feature flag only.

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
