# Lifetime of a session

In this document, we will explore what happens to a client session, from the
creation of the socket, to its shutdown, with all the steps describing how the
HTTP request and response can happen

## Accepting a connection

Sozu uses TCP listener sockets to get new connections. It will typically listen
on ports 80 (HTTP) and 443 (HTTPS), but could have other ports for TCP proxying,
or even HTTP/HTTPS proxys on other ports.

The session starts when the event loop gets an event taht indicates the listen
socket has new sockets to accept.

<details>
<summary>event loop</summary>
The [event loop](https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/server.rs#L310-L342)
manages sockets: we register a socket with [mio](https://docs.rs/mio/0.7.13/mio/)
a crate that provides an abstraction over [epoll](https://man7.org/linux/man-pages/man7/epoll.7.html)
on Linux.

That syscall allows us to register file descriptors to the kernel, and we will
be notified if something happens to those file descriptors. Example: a socket
has data to read, or was closed by the peer, or a timer triggered...

Here, the listen socket was registered for events after its creation. We just
got the "readable" event for this socket, which indicates there are new
connections to accept. From the event, we get a `Token`, an index in the
`Slab` structure that holds sessions. One session can be linked to multiple
sockets and thus have multiple tokens.
</details>

We look up the `Listener` associated with that socket and start accepting new
sockets in a loop and store them in the accept queue, until either there are
no more connections to accept, or the accept queue is full:
https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/server.rs#L1204-L1258

<details>
<summary>accept queue</summary>

When we accept a new connection, it might have waited a bit already in the
listener queue. The kernel might even have some data already available,
like a HTTP request. If we are too slow in handling that request, by the time
we send the request to the backend and the backend responds, the client might
have dropped the connection (timeout).

So when the listen socket can accept new connections, we accept them in a loop
and store them in a queue, then create sessions from the queue, starting from
the most recent session, and dropping sockets that are too old.

If we're already at the maximum number of client connections, new ones stay in
the queue. And if the queue is full, we drop newly accepted connections.
By specifying a maximum number of concurrent connections, we make sure that the
server does not get overloaded and keep latencies manageable for existing
connections.
</details>

The listener will then create a session, that will hold the data associated with
the [session](https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/https_openssl.rs#L65-L83):
tokens, current timeout state, protocol state, client address...

<details>
<summary>event loop</summary>
The created session is [wrapped in a `Rc<RefCell<...>>`](https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/https_openssl.rs#L1544)
that is then stored in the event loop's slab. That way, the frontend and backend
token, that correspond to the front and back sockets, are used to look up the
same instance of `Session`.

Since there can be bugs in how sessions are created and removed, and some of them
could be "forgotten", there's a regular task called ["zombie checker"](https://github.com/sozu-proxy/sozu/blob/e4e7488232ad6523791b94ad201239bcf7eb9b30/lib/src/server.rs#L446-L496)
that verifies the sessions in the list and kills those that are stuck or too old.

The frontend socket will be registered to `Poll` and we will wait for an event
telling us that it is readable.
</details>

## Reading data from the front socket

TODO
