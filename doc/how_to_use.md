# How to use Sōzu

> Before a deep dive in the configuration part of the proxy, you should take a look at the [getting started documentation](./getting_started.md) if you haven't done it yet.

## Configuration file

> The configuration file is using the [.toml](https://github.com/toml-lang/toml) format.

Sōzu configuration process involves 3 major sources of parameters:

* The `global` section, which sets process-wide parameters.
* The definition of the protocols like `https`, `http`, `tcp`.
* The applications sections under: `[applications]`.

### Global parameters

Parameters in the global section allow you to define the global settings shared by the master and workers (like the log level):

* `command_socket` path to the unix socket command (see below for more information)
* `saved_state` path from which sozu tries to load its state at startup
* `log_level` possible values are: `debug, trace, error, warn, info`
* `log_target` possible values are: `stdout, tcp or udp address`
* `command_buffer_size` size of the buffer used by the master to process commands.
* `worker_count` number of workers
* `handle_process_affinity` bind workers to cpu cores.
* `max_connections` maximum number of simultaneous / opened connections
* `max_buffers` maximum number of buffers use to proxying
* `buffer_size` size of requests buffer use by the workers

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

### Protocols

The _protocols_ section describes a set of listening sockets accepting client connections.
Protocols configuration can be declared in a set of sections : for example `[https]`.

They follow the format:

*Mandatories parameters:*
``` toml
[name]
address = "127.0.0.1"
port = 8080
```

*Optional parameters:*
``` toml
answer_404 = "path/to/404.html"
answer_503 = "path/to/503.html"
default_app_id = "Name of you app"
tls_versions = ["TLSv1.2"]
cipher_list = "ECDHE-ECDSA-....."
default_certificate = "cert.pem"
default_certificate_chain = "cert_chain.pem"
default_key = "key.pem"
max_listeners = 1000
```

Sozu will detect which type of proxy you have set due to your parameters.
It's why you don't have to specify: `https`, `http`...
Currently we support: `tcp, http, https`.

### Applications

You can declare the list of your _applications_ under the `[applications]` section.
They follow the format:

*Mandatories parameters:*
``` toml
[applications]

[applications.NameOfYourApp]
hostname  = "mydomain.foo"
frontends = ["http", "https"]
backends  = ["127.0.0.1:1026"]
```
> Take care, the _frontends_ declaration is case sensitive.
> The _backends_ field describes a set of servers to which the proxy will connect to forward incoming requests.

*Optionals parameters:*
``` toml
path_begin = "/api"
certificate = "certificate.pem"
key = "key.pem"
certificate_chain = "certificate_chain.pem"
sticky_session = true
```

## Sozuctl

Sozuctl is a command line interface for the proxy. You can send configuration orders (e.g. Add a new worker) or reclaim some metrics at the proxy with this executable. Sozuctl talks to the proxy through a unix socket.

You can specify its path by adding to your `config.toml`:

``` toml
command_socket = "path/to/your/command_folder/sock"
```

## Metrics

Sōzu reports its own state to another network component through a `UDP` socket. The master and the workers are responsible to send their states. We implement the [statsd](https://github.com/b/statsd_spec) protocol to send the statistics.
Any service that understands the `statsd` protocol can then gather metrics from Sōzu.

### Configure

In your `config.toml`, you can define the address and port of your external service by adding:

``` toml
[metrics]
address = "127.0.0.1"
port = 8125
```

> Currently, we can't change the frequency of sending messages.

### Example of externals services

* [cernan](https://github.com/postmates/cernan)

* [statsd](https://github.com/etsy/statsd)

## Systemd integration

The repository provides a unit file [here][un]. You can copy it to `/etc/systemd/system/` and invoke `systemctl daemon-reload`.  This will make systemd take notice of it, and now you can start the service with `systemctl start sozu.service`.
Furthermore, we can enable it, so that it is activated by default on future boots with 
`systemctl enable sozu.service`.

> You have to set your own `__BINDIR__` and `__SYSCONFDIR__`.
> You can use a `bash` script and call `sed` to automte this part. e.g.: [generate.sh][gen]

[un]: https://github.com/sozu-proxy/sozu/blob/master/os-build/systemd/sozu.service.in
[gen]: https://github.com/sozu-proxy/sozu/blob/master/os-build/exherbo/generate.sh

## PROXY Protocol

Sōzu use its own layer 3 and 4 information to get connected on remote servers. Because of this, we lose the initial L3/4 client connection information (like source and destination IP and port) which can be use by upstream proxy/server to configure a website, basic IP-level security, logging/metrics purposes...

Sōzu support the *version 2* of the `PROXY protocol` to solve the problem of L3/4 connection parameters being lost when relaying L3/4 connections through proxies. Thanks to that, Sōzu will informs the other end about the L3/4 addresses of the incoming connection before any byte is exchange from the socket. Furthermore, Sōzu can forward to the upstream service this client connection information sent by downstream proxy servers and load balancers.

The information L3/4 passed via the `PROXY protocol` is:
 - the client IP address
 - the proxy server IP address
 - both port numbers.

More infos here: [proxy-protocol spec](https://www.haproxy.org/download/1.8/doc/proxy-protocol.txt)


### Configuring Sōzu to *expect* a PROXY Protocol

Configures the client-facing connection to receive a PROXY protocol header before any byte is read from the socket.

```
                            send PROXY               expect PROXY
                            protocol header          protocol header
    +--------+
    | client |             +---------+                   +------------+      +-----------+
    |        |             | proxy   |                   | Sozu       |      | upstream  |
    +--------+  ---------> | server  |  ---------------> |            |------| server    |
   /        /              |         |                   |            |      |           |
  /________/               +---------+                   +------------+      +-----------+
```

*Configuration:*
```toml
[applications.NameOfYourApp]
expect_proxy = true
```

### Configuring Sōzu to *send* a PROXY Protocol to an upstream backend

Send a PROXY protocol header over any connection established to the backends declared in the application.
The PROXY protocol informs the upstream backend about the L3/4 addresses of the incoming connection.

```
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
[applications.NameOfYourApp]
send_proxy = true
```

NOTE: Currently supported only for `tcp` applications.

### Configuring Sōzu to *relay* a PROXY Protocol header to an upstream

Expect the client-facing connection to receive a PROXY protocol header, check its validity and then forward it to an upstream backend. This enable to chain proxies / reverse-proxies without losing the client connection information.

```
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
```toml
[applications.NameOfYourApp]
send_proxy = true
expect_proxy = true
```