# Architecture

This part is mostly for people who want to understand how sōzu works.

## Main/worker model

Sōzu works with one main process and multiple worker processes. This allows it to keep running if a worker encounters an issue and crashes, and upgrading workers one by one when necessary.

### Single thread, shared nothing architecture

Each worker runs a single thread with an epoll based event loop. To avoid synchronization issues, every worker has a copy of the entire routing configuration. Every modification of the routing comes through configuration messages. Logging and metrics are sent by each worker individually, leaving to an external service the work of aggregating and serializing the events.
All of the listening TCP sockets are opened with the [SO_REUSEPORT](https://lwn.net/Articles/542629/) option, allowing multiple process to listen on the same address.

### Configuration

External tools interact with the main process through a unix socket, and configuration change messages will be dispatched to the workers by the main.
The configuration messages are "diffs", like "add a backend server", or "remove a HTTP frontend", instead of changing the whole configuration at once. This allows sōzu to be smarter about handling the configuration changes while under traffic.

The configuration messages are transmitted in JSON format, and they are defined in the [command library](https://github.com/sozu-proxy/sozu/tree/main/command). There are three possible message answers: processing (meaning the message has been received but the change is not active yet), error or ok.

The main exposes a unix socket for configuration instead of a HTTP server on localhost because unix socket access can be secured through file system permissions.

## Proxying

### Event loop with mio

Every worker runs an event looped based on epoll (on Linux) or kqueue (on OSX and BSD), using the [mio library](https://github.com/tokio-rs/mio).

Mio provides a cross platform abstraction allowing callers to receive events, like a socket becoming readable (meaning it received some data).

Sōzu asks mio to send all the events of a socket in [edge triggered mode](http://man7.org/linux/man-pages/man7/epoll.7.html).
That way, it only receives an event once, and stores it in a
[`Readiness` struct](https://github.com/sozu-proxy/sozu/blob/01a78be7d95ac295d30b342d3ec0be403c98e776/lib/src/lib.rs#L527).
It will then use that information and the "interest" (indicating if the current protocol state machine wants to read or write on the socket).

Each socket event is returned with a `Token` indicating its index in a `Slab` data structure. A client session can have multiple sockets (typically, a front socket and a back socket).

### Protocols

Each proxy implementation (HTTP, HTTPS and TCP) will use in each client session a state machine describing the protocol currently in use. It is designed to allow upgrades from one protocol to the next. As an example, you could have the following progression:

- start in TLS handshake protocol
- once the handshake is done, upgrade to the HTTP protocol over the recently negotiated TLS stream
- upgrade to websockets

Each protocol will work with the `Readiness` structure to indicate if it wants to read or write on each socket. As an example, the [OpenSSL based handshake](https://github.com/sozu-proxy/sozu/blob/3111e2db420d2773b1f0404d6556f40b2f2ea85b/lib/src/network/protocol/openssl.rs) is only interested in the frontend socket.

They are all defined in [`lib/src/network/protocol`](https://github.com/sozu-proxy/sozu/tree/3111e2db420d2773b1f0404d6556f40b2f2ea85b/lib/src/network/protocol).

## Logging

The [logger](https://github.com/sozu-proxy/sozu/blob/3111e2db420d2773b1f0404d6556f40b2f2ea85b/lib/src/logging.rs) is designed to reduce allocations and string interpolations, using Rust's formatting system. It can send logs on various backends: stdout, file, TCP, UDP, Unix sockets.

The logger can be invoked through a thread local storage variable accessible from anywhere with logging macros.

## Metrics

[Metrics](https://github.com/sozu-proxy/sozu/tree/3111e2db420d2773b1f0404d6556f40b2f2ea85b/lib/src/network/metrics) work like the logger, accessible from anywhere with macros and TLS. We support two "drains": one that sends the metrics on the networks with a statsd compatible protocol, and one aggregating metrics locally, to be queried through the configuration socket.


## Load balancing

TODO

## SSL

TODO

## Deep dive

### Buffers

TODO

### Channel

TODO
