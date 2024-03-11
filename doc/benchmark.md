# How to benchmark / observe Sōzu

For debugging and optimization purposes.

## Run Sōzu on your machine, with dummy backends

### Create a config with an HTTP and an HTTPS listener

Most defaults of `bin/config.toml` are sensible, copy them into `bin/my-config.toml`.
An important value to tweak is `worker_count`. Testing the performance of one
worker only seems ideal:

```toml
worker_count = 1
```

The HTTP and HTTPS listeners should be identical.
Chose a TLS version and a cipher list for the HTTPS listener, and stick to it.

```toml
[[listeners]]
protocol = "http"
address = "0.0.0.0:8080"

[[listeners]]
protocol = "https"
address = "0.0.0.0:8443"
tls_versions = ["TLS_V13"]

cipher_list = [
  # "TLS13_AES_128_GCM_SHA256",
  "TLS13_CHACHA20_POLY1305_SHA256",
  # ...
]
```

Create a cluster (equivalent to an app) that has two frontends:
one for HTTP requests, one for HTTPS requests.

Make sure you created your own certificate and key for the `localhost` domain.

```toml
frontends = [
    { address = "0.0.0.0:8080", hostname = "localhost", path = "/api" },
    { address = "0.0.0.0:8443", hostname = "localhost", certificate = "/path/to/your/certificate.pem", key = "/path/to/your/key.pem", certificate_chain = "/path/to/your/certificate.pem" },
]
```

or use the certificate and key and certificate chain of `config.toml`,
present in `/lib/assets/`.
But then you have to use `lolcatho.st` as a local domain,
and be sure to have this lines in your `/etc/hosts`:

```
127.0.0.1       lolcatho.st
::1             lolcatho.st
```

### Spawn 4 backends

Create four simple HTTP servers that reply each 200 OK when requested
with HTTP GET on the `/api` path (arbitrarily).
Use your favorite programming langage, it should be simple.
It's [8 lines of javascript](https://www.digitalocean.com/community/tutorials/how-to-create-a-web-server-in-node-js-with-the-http-module).
Make sure that each backend listens on localhost and on a different port,
for exapmle: 1051, 1052, 1053 1054.

Add those backends to `my-config.toml`, under the frontends:

```toml
backends = [
    { address = "127.0.0.1:1051", backend_id = "backend_1" },
    { address = "127.0.0.1:1052", backend_id = "backend_2" },
    { address = "127.0.0.1:1053", backend_id = "backend_3" },
    { address = "127.0.0.1:1054", backend_id = "backend_4" },
]
```

### Build and run Sōzu

Build Sōzu in release mode.

```bash
cargo build --release
```

Tip: rename the release binary and put it in the cargo path:

```bash
mv target/release/sozu $HOME/.cargo/bin/sozu-0.15.14
```

Now you can run Sōzu in release mode:

```bash
cd bin
sozu-0.15.14 --config my-config.toml start
```

Check that Sōzu works by curling the backends through Sōzu:

```bash
curl https://localhost:8443/api -v
curl http://localhost:8080/api  -v
```


## Bombardier

[Bombardier](https://github.com/codesenberg/bombardier)
is an HTTP and HTTPS benchmarking tool written in go.
It is available as a package in most linux distributions,
and easy to use in the command line.

### Test a lot of requests on one connection

```bash
bombardier --connections=1 --requests=10000 https://localhost:8443/api --latencies
bombardier --connections=1 --requests=10000 http://localhost:8080/api --latencies
```

### Test concurrent connections

```bash
bombardier --connections=100 --requests=10000 https://localhost:8443/api --latencies
bombardier --connections=100 --requests=10000 http://localhost:8080/api --latencies
```

Increase the number of connections until Sõzu can't take it anymore.

## TLS-perf

[`tls-perf`](https://github.com/tempesta-tech/tls-perf) is a TLS stress-test tool written in C.
It has no dependencies and is easy to build.

```bash
git clone git@github.com:tempesta-tech/tls-perf.git
cd tls-perf
make
mv tls-perf $HOME/.local/bin
```

Assuming `$HOME/.local/bin` is in your path.

This command does:
- ten thousand TLS handshakes
- using only one connection
- using TLS version 1.3

```bash
tls-perf \
  -n 10000 \
  -l 1 \
  --sni localhost \
  --tls 1.3 \
  127.0.0.1 8443
```

## Find syscalls with `strace`

[`strace`](https://github.com/strace/strace) is a diagnostic tool that can monitor
interactions between a process (Sōzu) and the linux kernel. It is available as a package
in most linux distributions.

Once Sōzu runs, find the pid of the worker:

```bash
ps -aux | grep sozu
user   13368  0.7  0.1 188132 40344 pts/2    Sl+  11:35   0:00 /path/to/sozu --config my-config.toml start
user   14157  0.0  0.0  79908 29628 pts/2    S+   11:35   0:00 /path/to/sozu worker --id 0 --fd 5 --scm 7 --configuration-state-fd 3 --command-buffer-size 16384 --max-command-buffer-size 16384
user   14205  0.0  0.0   6556  2412 pts/3    S+   11:35   0:00 grep --color=auto sozu
```

That second line is the worker, its pid is `14157`.

Use strace to track this pid. You may need root privileges.

```bash
strace --attach=14157
```

You should see regular `epoll_wait` syscalls (system calls), this is Sōzu's main loop.

```
epoll_wait(3, [], 1024, 1000)           = 0
epoll_wait(3, [], 1024, 1000)           = 0
epoll_wait(3, [], 1024, 1000)           = 0
epoll_wait(3, [], 1024, 318)            = 0
```

If you perform one of those curls, you will see all syscalls that Sōzu does during
an HTTP or HTTPS request.
Here are syscalls occuring during an HTTP GET on a simple backend.
This is only the accepting of traffic on a listener socket.

```julia
# main loop waiting for events
epoll_wait(3, [], 1024, 1000)           = 0

# One readable event (EPOLLIN). A client connected to a listener socket.
epoll_wait(3, [{events=EPOLLIN, data={u32=3, u64=3}}], 1024, 1000) = 1

# The proxy accepts the connection, by telling a listener to accept
# incoming connections on the socket
accept4(9, {sa_family=AF_INET, sin_port=htons(46144), sin_addr=inet_addr("127.0.0.1")}, [128 => 16], SOCK_CLOEXEC|SOCK_NONBLOCK) = 11

# Done! No more connections to accept
accept4(9, 0x7ffe89e39b98, [128], SOCK_CLOEXEC|SOCK_NONBLOCK) = -1 EAGAIN (Resource temporarily unavailable)

# The HTTP proxy sets NODELAY on the socket
setsockopt(11, SOL_TCP, TCP_NODELAY, [1], 4) = 0

# Sōzu registers the socket in the accept queue
# The accept queue is popped in another loop, to create sessions

# When creating a session, the socket is registered in the event loop
epoll_ctl(6, EPOLL_CTL_ADD, 11, {events=EPOLLIN|EPOLLOUT|EPOLLRDHUP|EPOLLET, data={u32=267, u64=267}}) = 0

# Get the address of the socket (here, the host is the same, but the port is different)
getpeername(11, {sa_family=AF_INET, sin_port=htons(46144), sin_addr=inet_addr("127.0.0.1")}, [128 => 16]) = 0

# Return to the main event loop
epoll_wait(3, [{events=EPOLLIN|EPOLLOUT, data={u32=267, u64=267}}], 1024, 1000) = 1

# We are ready for traffic to come through this socket

# Here it comes!
recvfrom(11, "GET /api HTTP/1.1\r\nHost: localho"..., 16400, 0, NULL, NULL) = 80
```

### Find which parts of the code cause syscalls

For this kind of digging, Sōzu must run in debug mode, that is:
*not compiled with the `--release` flag*. Do:

```bash
cargo run -- --config my-config.toml start
```

Find the pid as described above, and do:

```bash
strace --attach=14157 --stack-traces
# or
strace -p 14157 -k
```

Curl Sōzu once, for instance

```bash
curl http://localhost:8080/api  -v
```

Now you see what piece of code is responsible for each syscall.
Here is, for instance, the code responsible for a `connect` syscall on a socket with file descriptor 12.
We can see it is an HTTP session connecting to its backend:

```
connect(12, {sa_family=AF_INET, sin_port=htons(1052), sin_addr=inet_addr("127.0.0.1")}, 16) = -1 EINPROGRESS (Operation now in progress)
 > /usr/lib/libc.so.6(__connect+0x14) [0x112454]
 > /path/to/sozu/target/debug/sozu(mio::sys::unix::tcp::connect+0x84) [0x17b2ec4]
 > /path/to/sozu/target/debug/sozu(mio::net::tcp::stream::TcpStream::connect+0x127) [0x17ad9e7]
 > /path/to/sozu/target/debug/sozu(sozu_lib::backends::Backend::try_connect+0x88) [0xb03898]
 > /path/to/sozu/target/debug/sozu(sozu_lib::backends::BackendMap::backend_from_cluster_id+0x465) [0xb04145]
 > /path/to/sozu/target/debug/sozu(sozu_lib::protocol::kawa_h1::Http<Front,L>::get_backend_for_sticky_session+0x5ab) [0xca8beb]
 > /path/to/sozu/target/debug/sozu(sozu_lib::protocol::kawa_h1::Http<Front,L>::backend_from_request+0x159) [0xca69b9]
 > /path/to/sozu/target/debug/sozu(sozu_lib::protocol::kawa_h1::Http<Front,L>::connect_to_backend+0xb7a) [0xcaafba]
 > /path/to/sozu/target/debug/sozu(sozu_lib::protocol::kawa_h1::Http<Front,L>::ready_inner+0x423) [0xcaf3a3]
 > /path/to/sozu/target/debug/sozu(<sozu_lib::protocol::kawa_h1::Http<Front,L> as sozu_lib::protocol::SessionState>::ready+0x2d) [0xcb040d]
 > /path/to/sozu/target/debug/sozu(<sozu_lib::http::HttpStateMachine as sozu_lib::protocol::SessionState>::ready+0xf2) [0xdbb9b2]
 > /path/to/sozu/target/debug/sozu(<sozu_lib::http::HttpSession as sozu_lib::ProxySession>::ready+0x114) [0xdb4b54]
 > /path/to/sozu/target/debug/sozu(sozu_lib::server::Server::ready+0x746) [0xc95846]
 > /path/to/sozu/target/debug/sozu(sozu_lib::server::Server::run+0x66d) [0xc8869d]
 > /path/to/sozu/target/debug/sozu(sozu::worker::begin_worker_process+0x3060) [0x32e480]
 > /path/to/sozu/target/debug/sozu(sozu::main+0x42e) [0x556afe]
 > /path/to/sozu/target/debug/sozu(core::ops::function::FnOnce::call_once+0xb) [0x1e453b]
 > /path/to/sozu/target/debug/sozu(std::sys_common::backtrace::__rust_begin_short_backtrace+0xe) [0x287e6e]
 > /path/to/sozu/target/debug/sozu(std::rt::lang_start::{{closure}}+0x11) [0x2ff2e1]
 > /path/to/sozu/target/debug/sozu(std::rt::lang_start_internal+0x42e) [0x18217fe]
 > /path/to/sozu/target/debug/sozu(std::rt::lang_start+0x3a) [0x2ff2ba]
 > /path/to/sozu/target/debug/sozu(main+0x1e) [0x556f6e]
 > /usr/lib/libc.so.6(__libc_init_first+0x90) [0x27cd0]
 > /usr/lib/libc.so.6(__libc_start_main+0x8a) [0x27d8a]
 > /path/to/sozu/target/debug/sozu(_start+0x25) [0x17bca5]
```