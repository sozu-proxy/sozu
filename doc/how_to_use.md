# How to use Sōzu

> Before to deep dive in the configuration part of the proxy. You should take a look at the [getting start](./getting_started.md) if you haven't done it yet.

## Configuration file

> The configuration file is a [.toml](https://github.com/toml-lang/toml)

Sōzu configuration process involves 3 major sources of parameters:

* The `global` section, which sets process-wide parameters.
* The definition of the frontends like `https`, `http`, `tcp`.
* The proxies sections under: `[applications]`.

### Global parameters

Parameters in the global section can be use to define the global settings shared by the master and workers (like the log level):

* `command_socket` path to the unix socket command (see below for more infos)
* `saved_state` path to file to dump the proxy state
* `log_level` possible values are: `debug, trace, error, info`
* `log_target` possible values are: `stdout, tcp or udp address`
* `command_buffer_size` size of the buffer use by the master to process commands.
* `worker_count` set the number of workers
* `handle_process_affinity` bind workers to cpu cores.
* `max_connections` maximum number of connections
* `max_buffers` 
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

### Frontends

The _frontend_ sections describes a set of listening sockets accepting client connections.
Frontends configuration can be declared in a set of sections : for example `[https]`.

They follow the format:

*Mandatories informations:*
``` toml
[name]
address = "127.0.0.1"
port = 8080
```

*Optionals informations:*
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

Sozu will detect alone which type of configuration you have set.
Actually we support tcp, http, https.

### Proxies

You can declare the list of your _applications_ under the `[applications]` section.
They follow the format:

*Mandatories informations:*
``` toml
[applications]

[applications.NameOfYourApp]
hostname  = "mydomain.foo"
frontends = ["http", "https"]
backends  = ["127.0.0.1:1026"]
```
> Take care, the _frontends_ declaration is case sensitive.
> The _backends_ field describes a set of servers to which the proxy will connect to forward incoming requests.

*Optionals informations:*
``` toml
path_begin = "/api"
certificate = "certificate.pem"
key = "key.pem"
certificate_chain = "certificate_chain.pem"
sticky_session = true
```

## Sozuctl

Sozuctl is a command line interface for the proxy. You can send configurations orders (e.g. Add a new worker) or reclaim some metrics at the proxy with this executable. Sozuctl talk to the proxy through a unix socket create by the master during the start process.

You can specify his path by adding to your `config.toml`:

``` toml
command_socket = "path/to/your/command_folder/sock"
```

## Metrics

Sōzu report its own state to another network component through a `UDP` socket. The master and the worker are responsible to send their states. We implement the [statsd](https://github.com/b/statsd_spec) protocol to send the stats.
So you can plugable to Sōzu any external service which make their telemetry aggregation with this protocol.

### Configure

In your `config.toml`, you can define the address and port of your external service by adding:

``` toml
[metrics]
address = "127.0.0.1"
port = 8125
```

> Actually, we can't modify the frequency of sending messages.

### Example of externals services

* [cernan](https://github.com/postmates/cernan)

* [statsd](https://github.com/etsy/statsd)