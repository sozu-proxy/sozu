# Configure Sōzu

> Before a deep dive in the configuration part of the proxy, you should take a look at the [getting started documentation](./getting_started.md) if you haven't done it yet.

## Configuration file

> The configuration file is using the [.toml](https://github.com/toml-lang/toml) format.

Sōzu configuration process involves 3 major sources of parameters:

* The `global` section, which sets process-wide parameters.
* The definition of the protocols like `https`, `http`, `tcp`.
* The applications sections under: `[applications]`.

### Global parameters

Parameters in the global section allow you to define the global settings shared by the main process and workers (like the log level):

* `command_socket` path to the unix socket command (see sozuctl for more information)
* `saved_state` path from which sozu tries to load its state at startup
* `log_level` possible values are: `debug, trace, error, warn, info`
* `log_target` possible values are: `stdout, tcp or udp address`
* `log_access_target` possible values are: `stdout, tcp or udp address` (if activated, sends access logs to a separate target)
* `command_buffer_size` size, in bytes, of the buffer used by the main process to handle commands.
* `max_command_buffer_size` maximum size of the buffer used by the main process to handle commands.
* `worker_count` number of workers
* `worker_automatic_restart` if activated, workers that panicked or crashed are restarted (activated by default)
* `handle_process_affinity` bind workers to cpu cores.
* `max_connections` maximum number of simultaneous / opened connections
* `max_buffers` maximum number of buffers use to proxying
* `buffer_size` size, in bytes, of requests buffer use by the workers
* `ctl_command_timeout` maximum time sozuctl will wait for a command to complete
* `pid_file_path` stores the pid in a specific file location
* `tls_provider` specifies which TLS implementation to use (openssl or rustls)
* `front_timeout` maximum time of inactivity for a front socket
* `zombie_check_interval` duration between checks for zombie sessions

*Example:*

``` toml
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
```

### Listeners

The _listener_ section describes a set of listening sockets accepting client connections.
You can define as many listeners as you want.
They follow the format:

*General parameters:*

```toml
[[listeners]]
# possible values are http, https or tcp
protocol = "http"
# listening address
address = "127.0.0.1:8080"

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
# an application. Defaults to "SOZUBALANCEID"
sticky_name = "SOZUBALANCEID"
```

#### Options specific to HTTPS listeners

```toml
# supported TLS versions. Possible values are "SSLv2", "SSLv3", "TLSv1",
# "TLSv1.1", "TLSv1.2", "TLSv1.3". Defaults to "TLSv1.2"
tls_versions = ["TLSv1.2"]
```

#### Options specific to Openssl based HTTPS listeners

```toml
#option specific to Openssl based HTTPS listeners
cipher_list = "ECDHE-ECDSA-AES256-GCM-SHA384:ECDHE-RSA-AES256-GCM-SHA384:ECDHE-ECDSA-CHACHA20-POLY1305:ECDHE-RSA-CHACHA20-POLY1305:ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-GCM-SHA256:ECDHE-ECDSA-AES256-SHA384:ECDHE-RSA-AES256-SHA384:ECDHE-ECDSA-AES128-SHA256:ECDHE-RSA-AES128-SHA256"
```

#### Options specific to Rustls based HTTPS listeners

```toml
#option specific to rustls based HTTPS listeners
rustls_cipher_list = ["TLS13_CHACHA20_POLY1305_SHA256"]
```

### Applications

You can declare the list of your _applications_ under the `[applications]` section.
They follow the format:

*Mandatories parameters:*

``` toml
[applications]

[applications.NameOfYourApp]
# possible values are http or tcp
# https proxies will use http here
protocol = "http"

# per application load balancing algorithm. The possible values are
# "roundrobin" and "random". Defaults to "roundrobin"
# load_balancing_policy="roundrobin"

# force application to redirect http traffic to https
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

Sōzu reports its own state to another network component through a `UDP` socket. The main process and the workers are responsible to send their states. We implement the [statsd](https://github.com/b/statsd_spec) protocol to send the statistics.
Any service that understands the `statsd` protocol can then gather metrics from Sōzu.

### Configure

In your `config.toml`, you can define the address and port of your external service by adding:

``` toml
[metrics]
address = "127.0.0.1:8125"
# use InfluxDB's statsd protocol flavor to add tags
# tagged_metrics = false
# metrics key prefix
# prefix = "sozu"
```

> Currently, we can't change the frequency of sending messages.

### Example of externals services

* [statsd](https://github.com/etsy/statsd)
* [grad](https://github.com/geal/grad)

## PROXY Protocol

When a network stream goes through a proxy, the backend server will only see the IP address and port used by the proxy as client address. The real source IP address and port will only be seen by the proxy. Since this information is useful for logging, security, etc, the [PROXY protocol](https://www.haproxy.org/download/1.8/doc/proxy-protocol.txt) was developed to transmit it to backend servers. With this protocol, after connecting to the backend server, the proxy will first send a small header indicating the client IP address and port, and the proxy's receiving IP address and port, and then it will send the stream from the client.

Sōzu support the *version 2* of the `PROXY protocol` in three configurations:

* "send" protocol: Sōzu, in TCP proxy mode, will send the header to the backend server
* "expect" protocol: Sōzu receives the header from a proxy, and interprets it for its own logging and metrics, and uses it in HTTP forwarding headers
* "relay" protocol: Sōzu, in TCP proxy mode, can reveice the header, and transmit it to a backend server

More information here: [proxy-protocol spec](https://www.haproxy.org/download/1.8/doc/proxy-protocol.txt)

### Configuring Sōzu to *expect* a PROXY Protocol header

Configures the client-facing connection to receive a PROXY protocol header before any byte sent by the client is read from the socket.

```txt
                            send PROXY               expect PROXY
                            protocol header          protocol header
    +--------+
    | client |             +---------+                   +------------+      +-----------+
    |        |             | proxy   |                   | Sozu       |      | upstream  |
    +--------+  ---------> | server  |  ---------------> |            |------| server    |
   /        /              |         |                   |            |      |           |
  /________/               +---------+                   +------------+      +-----------+
```

It is supported by HTTP, HTTPS and TCP proxies.

*Configuration:*

```toml
[[listeners]]
address = "0.0.0.0:80"
expect_proxy = true
```

### Configuring Sōzu to *send* a PROXY Protocol header to an upstream backend

Send a PROXY protocol header over any connection established to the backends declared in the application.

```txt
                                send PROXY
    +--------+                  protocol header
    | client |             +---------+                +-----------------+
    |        |             | Sozu    |                | proxy/upstream  |
    +--------+  ---------> |         |  ------------> | server          |
   /        /              |         |                |                 |
  /________/               +---------+                +-----------------+
```

*Configuration:*

```toml
[[listeners]]
address = "0.0.0.0:81"

[applications]
[applications.NameOfYourTcpApp]
send_proxy = true
frontends = [
  { address = "0.0.0.0:81" }
]
```

NOTE: Only for TCP applications (HTTP and HTTPS proxies will use the forwarding headers).

### Configuring Sōzu to *relay* a PROXY Protocol header to an upstream

Sōzu will receive a PROXY protocol header from the client connection, check its validity and then forward it to an upstream backend. This allows chains of reverse-proxies without losing the client connection information.

```txt
                                 send PROXY           expect PROXY    send PROXY
                                 protocol header      protocol header protocol header
    +--------+
    | client |             +---------+                      +------------+             +-------------------+
    |        |             | proxy   |                      | Sozu       |             | proxy/upstream    |
    +--------+  +--------> | server  |  +-----------------> |            | +---------> | server            |
   /        /              |         |                      |            |             |                   |
  /________/               +---------+                      +------------+             +-------------------+
```

*Configuration:*

This only concerns TCP applications (HTTP and HTTPS proxies can work directly in expect mode, and will use the forwarding headers).

```toml
[[listeners]]
address = "0.0.0.0:80"
expect_proxy = true

[applications]

[applications.NameOfYourApp]
send_proxy = true
frontends = [
  { address = "0.0.0.0:80" }
]
```
