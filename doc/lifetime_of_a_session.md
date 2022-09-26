# Lifetime of a session

In this document, we will explore what happens to a client session, from the
creation of the socket, to its shutdown, with all the steps describing how the
HTTP request and response can happen

Before we dive into the creation, lifetime and death of sessions, we need to
understand two concepts that are entirely segregated in Sozu:

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

## Accepting a connection

Sozu uses TCP listener sockets to get new connections. It will typically listen
on ports 80 (HTTP) and 443 (HTTPS), but could have other ports for TCP proxying,
or even HTTP/HTTPS proxys on other ports.

For each frontend, Sozu:

1. registers a `TcpListener` with mio
2. adds a matching `ListenSession` in the `SessionManager`

### An event loop on TCP sockets

Sozu is hot reconfigurable. We can add listeners and frontends at runtime. For each added listener,
the SessionManager will store a `ListenSession` in its Slab.

The [event loop]() uses mio to check for any activity, on all sockets.
Whenever mio detects activity on a socket, it returns an event that is passed to the `SessionManager`.

Typically we will have a `readable` event which mio will report on with the Token.
The Sozu server looks up which of its listeners (there can be multiple TCP/HTTP/HTTPS listeners), and calls
the accept method of the corresponding listener.
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

### Creating the Session

The proxy will then create a session that holds the data associated
[with this session](https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/https_openssl.rs#L65-L83):
tokens, current timeout state, protocol state, client address...

A session is the core unit of Sozu's business logic: forwarding trafic from a frontend to a backend and vice-versa.

The created session is wrapped in a
[`Rc<RefCell<...>>`](https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/https_openssl.rs#L1544)

The `TcpStream` is registered in mio with a new Token (called frontend_token).
The Session is added in the SessionManager with the same Token.

### Check for zombie sessions

Since there can be bugs in how sessions are created and removed, and some of them
could be "forgotten", there's a regular task called
["zombie checker"](https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/server.rs#L446-L496)
that checks on the sessions in the list and kills those that are stuck or too old.

## Reading data from the front socket

When data comes from the network on the TcpStream, it is stored in a kernel
buffer, and the kernel notifies the event loop that the socket is readable.

<details>
<summary>event loop</summary>

As with listen sockets, the token associated with the TCP socket will get a
"readable" event, and we will use the token to lookup which session it is
associated with. We then call its `process_events` method (https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/server.rs#L1431) to notify it of the new
socket state(https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/https_openssl.rs#L810-L820).

Then we call its `ready()` method (https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/https_openssl.rs#L822-L827) to let it read data, parse, etc.
That method will run in a loop (https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/https_openssl.rs#L548-L692),
looking at which sockets are readable or writable, which ones we want to read
or write, and call the `readable()`, `writable()` (for the front socket) and
`back_readable()`, `back_writable()` (for the back socket) methods, until there
is no more work to do in this session.
</details>

The [`readable()` method](https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/https_openssl.rs#L309-L339)
of the session is called. It will then call that same method of the underlying
[state machine](https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/https_openssl.rs#L58-L63).

The state machine is where the protocols are implemented. A session might need
to recognize different protocols over its lifetime, depending on its configuration,
and [upgrade](https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/https_openssl.rs#L165) between them. They are all in the [protocol directory](https://github.com/sozu-proxy/sozu/tree/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/protocol).
Example:
- a HTTPS session could start in Expect proxy protocol (a TCP proxy adds data at the beginning of the TCP stream to indicate the client's IP address)
- once the expect protocol has run its course, we upgrade to the TLS handshake
- once the handshake is done, we have a TLS stream, and we upgrade to a HTTP session
- if required by the client, we can then switch to a WebSocket connection

Now, let's assume we are currently using the HTTP 1 protocol. The session called
the [`readable()` method](https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/protocol/http/mod.rs#L609).
We will first try to [read data from the socket](https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/protocol/http/mod.rs#L646-L667) in the front buffer.
If there were no errors (closed socket, etc), we will then handle that data
in [`readable_parse()`](https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/protocol/http/mod.rs#L728).

The HTTP implementation holds [two smaller state machines](https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/protocol/http/mod.rs#L86-L87),
`RequestState` and `ResponseState`, that indicate where we are in parsing the
request or response, and store data about them.

When we receive the first byte from the client, both are at the `Initial` state.
We [parse data from the front buffer](https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/protocol/http/mod.rs#L734-L737)
until we reach a request state where the headers are entirely parsed. If there
was not enough data, we'll wait for more data to come on the socket and restart
parsing.

Once we're done parsing the headers, and we have a hostname, we will [return SessionResult::ConnectBackend](https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/protocol/http/mod.rs#L751-L759) to the session to notify it that it should find a backend server to send the data.

## Connecting to backend servers

<details>
<summary>event loop</summary>
the `ConnectBackend` result is returned all the way up to the event loop, to ask it for a token for the new socket, then it calls the Listener's `connect_to_backend` method.
</details>

[The Listener handles the backend connection](https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/https_openssl.rs#L1577).
It will first check if the session has triggered its circuit breaker (it will
then refuse to connect and send back an error to the client). Then it will
[extract the hostname and URL from the request](https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/https_openssl.rs#L1472-L1524)
and [find a cluster](https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/https_openssl.rs#L1297-L1332)
that can handle that request. If we do not find a cluster, we will send a
HTTP 404 Not Found response to the client.

Once we know the cluster id, if this is the first request (not a follow up
request in keep-alive), we must find a backend server for this cluster.

<details>
<summary>keep-alive</summary>
In HTTP keep-alive, a TCP connection can be kept after receiving the response,
to send more requests. Since sozu supports routing on the URL along with the
hostname, the next request might go to a different cluster.

So when we get the cluster id from the request, we check if it is the same
as the previous one, and if it is the same, we test if the back socket is still
valid. If it is, we can reuse it. Otherwise, we will replace the back socket
with a new one.
</details>

We [look up the backends list](https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/https_openssl.rs#L1428-L1470)
for the cluster, depending on a sticky session if needed.

<details>
<summary>sticky session</summary>
This is a routing mechanism where we look at a cookie in the request. All requests
coming with the same id in that cookie will be sent to the same backend server.
</details>

That look up will return a result depending on which backend server are
considered valid (if they're answering properly) and on the load balancing
policies configured for the cluster.
If a backend was found, we open a TCP connection to the backend server,
otherwise we return a HTTP 503 response.

<details>
<summary>event loop</summary>
We register the back socket to the event loop and allocate an entry in the slab,
in which we store a copy of the refcounted session.
</details>

Then we wait for an event from the backend connection. If there was an error,
we will retry a connection to a backend server.

## Sending data to the back socket

TODO

