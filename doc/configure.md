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
| `buffer_size`              | size, in bytes, of requests buffer use by the workers                               |                                          |
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

```toml
# path to custom 404 and 503 answers
# a 404 response is sent when sozu does not know about the requested domain or path
# a 503 response is sent if there are no backend servers available
answer_404 = "../lib/assets/404.html"
answer_503 = "../lib/assets/503.html"

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

### Clusters

You can declare the list of your _clusters_ under the `[clusters]` section.
They follow the format:

_Mandatories parameters:_

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

## Metrics

Sōzu reports its own state to another network component through a `UDP` socket.
The main process and the workers are responsible to send their states.
We implement the [statsd](https://github.com/b/statsd_spec) protocol to send statistics.
Any service that understands the `statsd` protocol can then gather metrics from Sōzu.

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

Currently, we can't change the frequency of sending messages.

### Example of externals services

- [statsd](https://github.com/etsy/statsd)
- [grad](https://github.com/geal/grad)

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
