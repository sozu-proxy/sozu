# sozu_lib, a proxy development library

`sozu_lib` provides tools to write a proxy that can be reconfigured
without any downtime. See `examples/http.rs` for a small example
of starting a HTTP proxy with one cluster.

A proxy starts as an event loop with which you communicate through
a `Channel`. You can add or remove clusters by sending messages
through that channel. Each message has an identifier that the event
loop will use in its answer.

The proxy implementations handle differently the frontend and backend
configurations. A single cluster could have multiple backend
servers, but it can also answer to different hostnames and various
TLS certificates. All those settings can be changed independently
from the currently active connections. As an example, a backend
server could be removed from the configuration while a client
is still proxied through to that server. It should be possible later
to force that connection to close if too many of those are lingering.

## Exploring the source

- `lib/src/protocol/mux/`: HTTP/2 multiplexer (frame parser, HPACK, stream pool, flood detector, RFC 9218 priorities, GOAWAY drain). The on-branch reference is `lib/src/protocol/mux/LIFECYCLE.md`.
- `lib/src/protocol/kawa_h1/`: HTTP/1.1 path built on the [`kawa`](https://github.com/CleverCloud/kawa) zero-copy parser.
- `lib/src/protocol/proxy_protocol/`: HAProxy PROXY protocol v1 + v2 ingest.
- `lib/src/protocol/{pipe,rustls,tcp}.rs`: byte-stream pipe (post-upgrade), TLS handshake / ALPN driver, raw TCP proxy.
- `lib/src/{http,https,tcp}.rs`: front-door dispatch for the three transport stacks.
- `lib/src/server.rs`: the single-threaded mio event loop shared by every proxy in a worker.
- `lib/src/socket.rs`: edge-triggered socket abstractions (`FrontRustls`, `FrontPlain`, vectored writes).
- `lib/src/{metrics,pool,tls}.rs`: metrics emission, buffer pool, certificate resolver.

See `examples/http.rs` for a small example of starting an HTTP proxy with one cluster without the supervisor.
