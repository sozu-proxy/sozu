# How to debug sozu

Sozu provides logs and metrics allowing detection of most production issues.

## Gathering information

### Dumping the state through sozu command line

It is useful to gather information on the configuration state in a production system.
Here are some commands you can use to take a snapshot of the current state:

```bash
sozu -c /etc/config.toml status > "sozu-status-$(date -Iseconds).txt"
sozu -c /etc/config.toml query metrics > "sozu-metrics-$(date -Iseconds).txt"
sozu -c /etc/config.toml query clusters > "sozu-applications-$(date -Iseconds).txt"
sozu -c /etc/config.toml state save -f "sozu-state-$(date -Iseconds).txt"
```

### Logging

There are three configuration options related to logging:

* `log_level`: sets logging verbosity
* `log_target`: where logs are sent. It can have the following formats:
  * `stdout`
  * `udp://127.0.0.1:9876`
  * `tcp://127.0.0.1:9876`
  * `unix:///var/sozu/logs`
  * `file:///var/logs/sozu.log`
* `access_logs_target`: if activated, sends the access logs to a separate destination

`log_level` follows [env_logger's level directives](https://docs.rs/env_logger/0.5.13/env_logger/).
Moreover, the `RUST_LOG` environment variable can be used to override the log level.

If sozu is built in release mode, the `DEBUG` and `TRACE` log levels are not compiled in,
unless you set the compilation features `logs-debug` and `logs-trace`.

### Metrics

Various metrics are generated while sozu is running. They can be accessed in two ways:

* through `sozu metrics get`, which will display metrics for the main process and workers. Counters are refreshed between each call
* by UDP, following the statsd protocol (optionally with support for InfluxDB's tags)

Here is how you can set up metrics with statsd in the configuration file:

```toml
[metrics]
address = "127.0.0.1:8125"
# use InfluxDB's statsd protocol flavor to add tags (default: false)
tagged_metrics = true
# metrics key prefix (default: sozu)
# prefix = "sozu"
```

## Graphs and metrics to follow

(assuming we set `sozu` as metric prefix)

### Tracking valid traffic

#### Access logs

Access logs have the following format:

```txt
2018-09-21T14:01:51Z 821136800672570 71013 WRK-00 INFO  450b071a-53b8-4fd7-b2f2-1213f03ef032 MyCluster      127.0.0.1:52323 -> 127.0.0.1:1027       241ms 855μs 560 33084   200 OK lolcatho.st:8080 GET /
```

From left to right:

* date in ISO8601 format, UTC timezone
* monotonic clock (in case some messages appear in the wrong order)
* PID
* worker name ("MAIN" for the main process)
* log level
* request id (UUID, generated randomly for each request, changes on the same connection if doing multiple requests in keep-alive)
* cluster id
* client's source IP and port
* backend server's destination IP and port
* response time (from first byte received from the client, to last byte sent to the client)
* service time (time spent by sozu handling the session)
* uploaded bytes
* downloaded bytes

#### HTTP status metrics

The following metrics track requests that are correctly sent to the backend servers:

* `sozu.http.status.1xx`: counts requests with 100 to 199 status
* `sozu.http.status.2xx`: counts requests with 200 to 299 status
* `sozu.http.status.3xx`: counts requests with 300 to 399 status
* `sozu.http.status.4xx`: counts requests with 400 to 499 status
* `sozu.http.status.5xx`: counts requests with 500 to 599 status
* `sozu.http.requests`: incremented at each request (sum of above counters)

#### data transmitted

There are global `sozu.bytes_in` and `sozu.bytes_out` metrics counting the front traffic
for sozu.
These metrics can also have a backend ID and cluster ID. They would then indicate
bytes in and out from the point of view of the backend server.

#### Response time

?

#### Protocols

Client sessions can be at various state of their network protocols. As an example, a connection
could go from "Expect proxy protocol" (assuming there's a TCP proxy in front) to TLS handshake,
to HTTPS, to WSS (websockets over TLS).

You can track the following gauges indicating the current protocol usage:

* `sozu.protocol.proxy.expect`
* `sozu.protocol.proxy.send`
* `sozu.protocol.proxy.relay`
* `sozu.protocol.tcp`
* `sozu.protocol.tls.handshake`
* `sozu.protocol.http`
* `sozu.protocol.https`
* `sozu.protocol.ws`
* `sozu.protocol.wss`

### Tracking failed requests

Sozu has a way of answering to invalid traffic with minimal resource usage, sending predefined answers.
It does that for invalid traffic (not standard compliant) and routing issues (unknown host and/or path,
unresponsive backend server).

The `sozu.http.errors` counter is the sum of failed requests. It contains the following:

* `sozu.http.frontend_parse_errors`: sozu received some invalid traffic
* `sozu.http.400.errors`: cannot parse hostname
* `sozu.http.404.errors`: unknown hostname and/or path
* `sozu.http.413.errors`: request too large
* `sozu.http.503.errors`: could not connect to backend server, or no backend server available for the corresponding cluster

Going further, backend connections issues are tracked by the following metrics:

* `sozu.backend.connections.error`: could not connect to a backend server
* `sozu.backend.down`: the retry policy triggered and marked the backend server as down

The `sozu.http.503.errors` metric is incremented after a request sent back a 503 error, and a 503 error is sent
after the circuit breaker triggered (we wait for 3 failed connections to the backend server).

A backend connection error would result in the following log message:

```txt
2018-09-21T14:36:08Z 823194694977734 71501 WRK-00 ERROR 839f592b-a194-4c3b-848b-8ef024129969    MyCluster    error connecting to backend, trying again
```

The circuit breaker triggering will write this to the logs:

```txt
2018-09-21T14:36:57Z 823243245414405 71524 WRK-00 ERROR 7029d66e-57a8-406e-ae61-e4bf9ff7b6b8    MyCluster    max connection attempt reached
```

The retry policy marking a backend server as down will write the following log message:

```txt
2018-09-21T14:37:31Z 823277868708804 71524 WRK-00 ERROR no more available backends for cluster MyCluster
```

### Scalability

Sozu handles its resource usage finely, and puts hard limits on the number of requests
or memory usage.

To track connections, follow these gauges:

* `sozu.client.connections` for frontend connections
* `sozu.backend.connections` for backend connections
* `sozu.http.active_requests` for currently active connections (a keep alive connection that's waiting
for the next request is marked as not active)

Client connections should always be higher than backend connections, and backend connections should be higher than
active requests (an inactive session can keep a backend connection around).

These metrics are closely linked to resource usage, which is tracked by the following:

* `sozu.slab.entries`: number of slots used in the slab allocator. Typically, there's one slot per listener socket,
one for the connection to the main process, one for the metrics socket, then one per frontend connection and
one per backend connection. So the number of connections should always be close to (but lower than) the slab count.
* `sozu.buffer.number`: number of buffers used in the buffer pool. Inactive sessions and requests for which we send
a default answer (400, 404, 413, 503 HTTP errors) do not use buffers. Active HTTP sessions use one buffer (except
in pipelining mode), WebSocket sessions use two buffers. So the number of buffers should always be lower than the
slab count, and lower than the number of connections.
* `sozu.zombies`: sozu integrates a zombie session checker. If some session did not do anything for a while, there's
probably a bug in the event loop or the protocol implementations, so its internal state is logged. This counter
is incremented for each zombie session that gets deleted.

New connections are put into a queue, and wait until the session is created (if we have available resources),
or until a configurable timeout has elapsed. The following metrics observe the accept queue usage:

* `sozu.accept_queue.connections`: number of sockets in the accept queue
* `sozu.accept_queue.timeout`: incremented every time a socket stayed too long in the queue and is closed
* `sozu.accept_queue.wait_time`: every time a session is created, this metric records how long the socket had to wait in the accept queue

### TLS specific information

TLS version counter:

* `sozu.tls.version.SSLv2`
* `sozu.tls.version.SSLv3`
* `sozu.tls.version.TLSv1_0`
* `sozu.tls.version.TLSv1_1`
* `sozu.tls.version.TLSv1_2`
* `sozu.tls.version.TLSv1_3`
* `sozu.tls.version.Unknown`

Rustls specific, negotiated ciphersuite:

* `sozu.tls.cipher.TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256`
* `sozu.tls.cipher.TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256`
* `sozu.tls.cipher.TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256`
* `sozu.tls.cipher.TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384`
* `sozu.tls.cipher.TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256`
* `sozu.tls.cipher.TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384`
* `sozu.tls.cipher.TLS13_CHACHA20_POLY1305_SHA256`
* `sozu.tls.cipher.TLS13_AES_256_GCM_SHA384`
* `sozu.tls.cipher.TLS13_AES_128_GCM_SHA256`
* `sozu.tls.cipher.Unsupported`

## Classic error scenarios

### Routing issues

Normal traffic (`sozu.http.requests`) drops while 404 (`sozu.http.404.errors`) and
503 (`sozu.http.503.errors`), that means sozu's configuration is probably invalid.
Check the configuration state with;

```bash
sozu -c /etc/config.toml query clusters
```

And, for the complete configuration for a specific cluster id:

```bash
sozu -c /etc/config.toml query clusters -i cluster_id
```

### Backend server unavailable

`sozu.http.503.errors` increases, lots of `sozu.backend.connections.error` and a
`sozu.backend.down` record: a backend server is down.
Check the logs for `error connecting to backend, trying again` and `no more available backends for cluster <cluster_id>`
to find out which cluster is affected

### Zombies

if the `sozu.zombies` metric triggers, this means there's an event loop or protocol implementation
bug. The logs should contain the internal state of the sessions that were killed. Please copy those
logs and open an issue to sozu.

It usually comes with the `sozu.slab.entries` increasing while the number of connections or active requests stays
the same. The slab count will then drop when the zombie checker activates.

### Invalid session close

if the slab count and active requests stay the same but `sozu.client.connections` and/or `sozu.backend.connections`
are increasing, it means sessions are not properly closed by sozu, please open an issue for this.
(if the slab count stays constant, sockets should still be closed properly, though)

### accept queue filling up

if `sozu.accept_queue.connections` is increasing, that means the accept queue is filling up because sozu is under
heavy load (in a healthy load, this queue is almost always empty). `sozu.accept_queue.wait_time` should increase
as well. If `sozu.accept_queue.timeout` is higher than zero, sozu cannot accept sessions fast enough and
is rejecting traffic.

## During development

In the config.toml file:

- if the bug only affects one of the protocols (HTTP, HTTPS or TCP), deactivate the other ones
- set `worker_count` to 1 to avoid duplicates in the log
- to debug panics:
  - start sozu with `RUST_BACKTRACE=1`
  - set `worker_automatic_restart` to false (so sozu can stop immediately)

### Tracking metrics

The [grad metrics tool](https://github.com/geal/grad) was developed to easily aggregate statsd
metric and display them.
