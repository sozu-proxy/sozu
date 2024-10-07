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

- `parser/`: only the HTTP 1.1 parser for now
- `lib/src/buffer_queue.rs`: data buffering implementation
- `lib/src/protocol/`: the HTTP, HTTP2, TLS handshake and piping proxies
- `lib/src/{http|https|tcp}.rs`: proxies for HTTP, HTTPS and TCP
- `lib/src/server.rs`: the main event loop shared by all proxies
- `lib/src/socket.rs`: abstraction over normal sockets
