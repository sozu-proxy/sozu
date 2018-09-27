# sozu_lib, a proxy development library

`sozu_lib` provides tools to write a proxy that can be reconfigured
without any downtime. See `examples/minimal.rs` for a small example
of starting a HTTP proxy with one application.

A proxy starts as an event loop with which you communicate through
a `Channel`. You can add or remove applications by sending messages
through that channel. Each message has an identifier that the event
loop will use in its answer.

The proxy implementations handle differently the frontend and backend
configurations. A single application could have multiple backend
servers, but it can also answer to different hostnames and various
TLS certificates. All those settings can be changed independently
from the currently active connections. As an example, a backend
server could be removed from the configuration while a client
is still proxied through to that server. It should be possible later
to force that connection to close if too many of those are lingering.

## Exploring the source

- `parser/`: only the HTTP 1.1 parser for now
- `network/buffer_queue.rs`: data buffering implementation
- `network/protocol/`: the HTTP, TLS handshake and piping proxies
- `network/{{http|https_openssl|tcp}.rs|https_rustls}`: proxies for HTTP, HTTPS and TCP
- `network/server.rs`: the main event loop shared by all proxies
- `network/socket.rs`: abstraction over normal sockets and `SslStream`
