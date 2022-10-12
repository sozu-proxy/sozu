# Lifetime of a session

A session is the core unit of Sōzu's business logic: forwarding traffic from a
frontend to a backend and vice-versa.

In this document, we will explore what happens to a client session, from the
creation of the socket, to its shutdown, with all the steps describing how the
HTTP request and response can happen

Before we dive into the creation, lifetime and death of sessions, we need to
understand two concepts that are entirely segregated in Sōzu:

- the socket handling with the [mio](https://docs.rs/mio/0.7.13/mio/) crate
- the `SessionManager` that keeps track of all sessions

### What does mio do?

Mio allows us to listen on socket events. For instance we may use TCP sockets to
wait for connection and mio informs us when that socket is readable or when a socket has
data to read, or was closed by the peer, or a timer triggered...

Mio provides an abstraction over the linux syscall [epoll](https://man7.org/linux/man-pages/man7/epoll.7.html).
This epoll syscall allows us to register file descriptors, so that the kernel notifies
us whenever something happens to those file descriptors

At the end of the day, sockets are just raw file descriptors. We use the mio
`TcpListener`, `TcpStream` wrappers around these file descriptors. A `TcpListener`
listens for connections on a specific port. For each new connection it creates a
`TcpStream` on which subsequent trafic will be redirected (both from and to the client).

This is all what we use mio for. "Subscribing" to file descriptors events.

### Keep track of sessions with the SessionManager

Each subscription in mio is associated with a Token (a u64 identifier). The SessionManager's
job is to link a Token to a `ProxySession` that will make use of the subscription.
This is done with a [Slab](), a key-value data structure which optimizes memory usage:

- as a key: the Token provided by mio
- as a value: a reference to a `ProxySession`

That being said let's dive into the lifetime of a session.

### What are Proxys?

A Sōzu worker internally has 3 proxys, one for each supported protocol:

- TCP
- HTTP
- HTTPS

A proxy manages listeners, frontends, backends and the business logic associated
with each protocol (buffering, parsing, error handling...).

## Accepting a connection

Sōzu uses TCP listener sockets to get new connections. It will typically listen
on ports 80 (HTTP) and 443 (HTTPS), but could have other ports for TCP proxying,
or even HTTP/HTTPS proxys on other ports.

For each frontend, Sōzu:

1. generate a new Token *token*
2. uses mio to register a `TcpListener` as *token*
3. adds a matching `ListenSession` in the `SessionManager` with key *token*
4. stores the `TcpListener` in the appropriate Proxy (TCP/HTTP/HTTPS) with key *token*

### An event loop on TCP sockets

Sōzu is hot reconfigurable. We can add listeners and frontends at runtime. For each added listener,
the SessionManager will store a `ListenSession` in its Slab.

The [event loop]() uses mio to check for any activity, on all sockets.
Whenever mio detects activity on a socket, it returns an event that is passed to the `SessionManager`.

Whenever a client connects to a frontend:
1. it reaches a listener
2. Sōzu is notified by mio that a `readable` event was received on a specific Token
3. using the SessionManager, Sōzu gets the corresponding `ListenSession`
4. Sōzu determines the protocol that was used
5. the Token is passed down to the appropriate proxy (TCP/HTTP/HTTPS)
6. using the Token, the proxy determines which `TcpListener` triggered the event
7. the proxy starts accepting new connections from it in a loop (as their might be more than one)

Accepting a connection means to store it as a `TcpStream` in the accept queue, until either:

- there are no more connections to accept
- or the accept queue is full: https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/server.rs#L1204-L1258

### Consuming the accept queue

We create sessions from the accept queue, starting from
the most recent session, and dropping sockets that are too old.

When we accept a new connection (`TcpStream`), it might have waited a bit already in the
listener queue. The kernel might even already have some data available,
like an HTTP request. If we are too slow in handling that request, the client might
have dropped the connection (timeout) by the time we forward the request to the
backend and the backend responds.

If the maximum number of client connections (provided by `max_connections` in the configuration)
is reached, new ones stay in the queue.
And if the queue is full, we drop newly accepted connections.
By specifying a maximum number of concurrent connections, we make sure that the
server does not get overloaded and keep latencies manageable for existing
connections.

### Creating sessions

A session's goal is to forward traffic from a frontend to a backend and vice-versa.
The `Session` struct holds the data associated
[with a session](https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/https_openssl.rs#L65-L83):
tokens, current timeout state, protocol state, client address...
The created session is wrapped in a
[`Rc<RefCell<...>>`](https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/https_openssl.rs#L1544)

The proxies create one session for each item of the accept queue,
using the `TcpStream` provided in the item.
The `TcpStream` is registered in mio with a new Token (called frontend_token).
The Session is added in the SessionManager with the same Token.

### Check for zombie sessions

Because bugs can occur when sessions are created and removed, and some of them
could be "forgotten", there's a regular task called
["zombie checker"](https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/server.rs#L446-L496)
that checks on the sessions in the list and kills those that are stuck or too old.

## How a Session reads data from the front socket

When data arrives from the network on the `TcpStream`, it is stored in a kernel
buffer, and the kernel notifies the event loop that the socket is readable.

As with listen sockets, the token associated with the TCP socket will get a
"readable" event, and we will use the token to lookup which session it is
associated with. We then call
[`Session::process_events`](https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/server.rs#L1431)
to notify it of the new
[socket state](https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/https_openssl.rs#L810-L820).

Then we call
[`Session::ready`](https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/https_openssl.rs#L822-L827)
to let it read data, parse, etc.
That method will run in a loop (https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/https_openssl.rs#L548-L692),

### Bring in the state machine

The [`Session::readable` method](https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/https_openssl.rs#L309-L339)
of the session is called. It will then call that same method of the underlying
[state machine](https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/https_openssl.rs#L58-L63).

The state machine is where the protocols are implemented. A session might need
to recognize different protocols over its lifetime, depending on its configuration,
and [upgrade](https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/https_openssl.rs#L165) between them. They are all in the [protocol directory](https://github.com/sozu-proxy/sozu/tree/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/protocol).

Example:
- an HTTPS session could start in a state called `ExpectProxyProtocol`
- once the expect protocol has run its course, the session upgrades to the TLS handshake state: `HandShake`
- once the handshake is done, we have a TLS stream, and the session upgrades to the `HTTP` state
- if required by the client, the session can then switch to a WebSocket connection: `WebSocket`

Now, let's assume we are currently using the HTTP 1 protocol. The session called
the [`readable()` method](https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/protocol/http/mod.rs#L609).

### Find the backend

We need to parse the HTTP request to find out its:

1. hostname
2. path
3. HTTP verb

We will first try to
[read data from the socket](https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/protocol/http/mod.rs#L646-L667)
in the front buffer. If there were no errors (closed socket, etc), we will then handle that data in
[`readable_parse()`](https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/protocol/http/mod.rs#L728).

The HTTP implementation holds
[two smaller state machines](https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/protocol/http/mod.rs#L86-L87),
`RequestState` and `ResponseState`, that indicate where we are in parsing the
request or response, and store data about them.

When we receive the first byte from the client, both are at the `Initial` state.
We [parse data from the front buffer](https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/protocol/http/mod.rs#L734-L737)
until we reach a request state where the headers are entirely parsed. If there
was not enough data, we'll wait for more data to come on the socket and restart
parsing.

Once we're done parsing the headers, and find what we were looking for, we will
[return SessionResult::ConnectBackend](https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/protocol/http/mod.rs#L751-L759)
to notify the session that it should find a backend server to send the data.

## Connect to the backend servers

The Session:
1. finds which cluster to connect to
2. asks the SessionManager for a new valid Token named `back_token`
3. asks for a connection to the cluster
   - the appropriate Proxy finds a backend (add details)
4. registers the new `TcpStream` in mio with `back_token`
5. inserts itself in the SessionManager with `back_token`

The same session is now stored twice in the SessionManager:

1. once with the front token as key
2. secondly with the back token as key

If Sōzu can't find a cluster it responds with a default HTTP 404 Not Found response to the client.
A session can try to connect to a backend 3 times. If all attempts fail, Sōzu responds with a default
HTTP 503 Service Unavailable response. This happens if Sōzu found a cluster, but all corresponding
backends are down (or not responding).

### Keep a session alive

In case an HTTP request comes with the Connection header set at Keep-Alive,
the underlying TCP connection can be kept after receiving the response,
to send more requests. Since Sōzu supports routing on the URL along with the
hostname, the next request might go to a different cluster.
So when we get the cluster id from the request, we check if it is the same
as the previous one, and if it is the same, we test if the back socket is still
valid. If it is, we can reuse it. Otherwise, we will replace the back socket
with a new one.

### Sticky session: stick a front to one backend only

This is a routing mechanism where we look at a cookie in the request. All requests
coming with the same id in that cookie will be sent to the same backend server.

That look up will return a result depending on which backend server are
considered valid (if they're answering properly) and on the load balancing
policies configured for the cluster.
If a backend was found, we open a TCP connection to the backend server,
otherwise we return a HTTP 503 response.

## Sending data to the back socket

Then we wait for a writable event from the backend connection, then we can start
to forward the pending request to it.
In case of an error, we retry to connect to another backend server in the same Cluster.

## The big U-turn: forwarding from backend to frontend

As explained above, we have a small state machine called `ResponseState` in order to
parse the traffic from the backend. The whole logic is basically the same.

We monitor backend readability and frontend writability, and transfer traffic from
one to the other.

## The end of a session

Once the `ResponseState` reaches a "completed" state and every byte has been sent
back to the client, the full life cycle of a request ends. The session
reaches the `CloseSession` state and is removed from the `SessionManager`'s slab
and its sockets are deregistered from mio.

If the request had a Keep-Alive header, however, the session will be reused
and await a new request. This is the "reset" of a session.