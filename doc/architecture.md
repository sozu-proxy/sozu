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

The configuration messages are transmitted in protobuf binary format, and they are defined in the [command library](https://github.com/sozu-proxy/sozu/tree/main/command). There are three possible message answers: processing (meaning the message has been received but the change is not active yet), failure or ok.

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

Each protocol will work with the `Readiness` structure to indicate if it wants to read or write on each socket. As an example, the [rustls based handshake](https://github.com/sozu-proxy/sozu/blob/main/lib/src/protocol/rustls.rs) is only interested in the frontend socket.

They are all defined in [`lib/src/protocol`](https://github.com/sozu-proxy/sozu/tree/main/lib/src/protocol).

### HTTP/2 and the Mux layer

HTTP/2 support is implemented via a unified multiplexer (`Mux`) in `lib/src/protocol/mux/`.
The Mux layer handles both HTTP/1.1 and HTTP/2 connections through a `Connection` enum with
`H1` and `H2` variants, enabling any combination of frontend and backend protocols.

#### Protocol combinations

```
  Client             Sōzu               Backend
  ──────           ────────            ─────────
   H1  ──────────▶  Mux  ──────────▶   H1        (classic)
   H1  ──────────▶  Mux  ──────────▶   H2        (H2 backend, h2c)
   H2  ──────────▶  Mux  ──────────▶   H1        (H2 frontend, H1 backend)
   H2  ──────────▶  Mux  ──────────▶   H2        (end-to-end H2)
```

The frontend protocol is determined by ALPN negotiation during TLS handshake (or H2 prior
knowledge for cleartext). The backend protocol is controlled by the `cluster.http2` protobuf
config flag.

#### Mux session structure

A `MuxState` session owns one frontend `Connection` and a `Router` containing zero or more
backend `Connection`s. All connections share a `Context` holding a `Vec<Stream>`.

```
MuxState
├── frontend: Connection<FrontRustls>     ◄── one frontend socket
│   ├── H1(ConnectionH1)                      (either H1 or H2)
│   └── H2(ConnectionH2)
│
├── router: Router
│   └── backends: HashMap<Token, Connection<TcpStream>>
│       ├── Token(7)  → H1(ConnectionH1)  ◄── backend to cluster "app-1"
│       ├── Token(12) → H2(ConnectionH2)  ◄── backend to cluster "app-2" (h2c)
│       └── ...
│
└── context: Context
    ├── streams: Vec<Stream>              ◄── shared stream pool
    │   ├── [0] Stream { state: Linked(Token(7)),  front: Kawa, back: Kawa, ... }
    │   ├── [1] Stream { state: Linked(Token(12)), front: Kawa, back: Kawa, ... }
    │   ├── [2] Stream { state: Recycle, ... }
    │   └── ...
    ├── pool: Weak<RefCell<Pool>>         ◄── buffer allocator
    └── listener: Rc<RefCell<L>>
```

#### Stream lifecycle

Each `Stream` transitions through states as it processes a request/response pair:

```
             ┌──────────────────────────────────────────────────┐
             │                                                  │
             ▼                                                  │
          ┌──────┐   HEADERS    ┌──────┐   connect()   ┌────────┐
  new ──▶ │ Idle │ ──────────▶ │ Link │ ────────────▶ │ Linked  │
          └──────┘   received   └──────┘   to backend  │(Token)  │
             ▲                                          └────┬───┘
             │                                               │
             │       ┌─────────┐   response    ┌─────────┐   │ backend
             │       │ Recycle │ ◀──────────── │end_stream│ ◀─┘ done or
             │       └────┬────┘   complete     └─────────┘    error
             │            │                         │
             │ reuse      │                         │ can't retry
             └────────────┘                         ▼
                                              ┌──────────┐
                                              │ Unlinked │ ──▶ error response
                                              └──────────┘
```

- **Idle**: Stream allocated but no request yet. In H2, streams in this state can be
  recycled from a previous request.
- **Link**: Request headers fully parsed, waiting for the `Router` to connect to a backend.
- **Linked(Token)**: Connected to a backend identified by `Token`. Data flows bidirectionally.
- **Unlinked**: Backend disconnected and the request cannot be retried. A default error
  response (502/503/504) is generated.
- **Recycle**: Response fully sent. In H2, the stream is returned to the pool for reuse.
  In H1, the session closes or resets for keep-alive.

#### StreamParts: direction-aware borrows

A `Stream` contains two Kawa buffers (`front` and `back`). The `split(&position)` method
returns a `StreamParts` struct with direction-appropriate aliases:

```
                        Stream
             ┌─────────────────────────┐
             │  front: Kawa (Request)  │
             │  back:  Kawa (Response) │
             │  window: i32            │
             │  context: HttpContext    │
             └────────┬────────────────┘
                      │
          ┌───────────┴───────────┐
          │                       │
   Position::Server          Position::Client
   (frontend conn)           (backend conn)
          │                       │
          ▼                       ▼
   StreamParts {             StreamParts {
     rbuffer: &front,          rbuffer: &back,
     wbuffer: &back,           wbuffer: &front,
     window, context           window, context
   }                         }
```

The frontend (Server position) reads from `front` and writes to `back`.
The backend (Client position) reads from `back` and writes to `front`.
This inversion is the key abstraction that lets H1 and H2 connections share streams.

#### H2 connection state machine

Each `ConnectionH2` runs a frame-level state machine for reading. The struct is decomposed
into logical sub-structs for maintainability:

- `H2FlowControl` — connection-level window, peer window, initial window size
- `H2ByteAccounting` — overhead bytes, zero-window count
- `H2DrainState` — GoAway sent flag, last stream ID, pending RST streams
- `H2FloodConfig` — 6 configurable flood detection thresholds (per-listener)
- `Prioriser` — RFC 9218 urgency + incremental tracking per stream

For detailed internals, see [h2_mux_internals.md](./h2_mux_internals.md).

The H2 state machine:

```
              ┌───────────────┐
              │ ClientPreface │   (server waits for "PRI * HTTP/2.0..." + SETTINGS)
              └───────┬───────┘
                      │ preface valid
                      ▼
              ┌───────────────┐
              │ServerSettings │   (server sends own SETTINGS + ACK)
              └───────┬───────┘
                      │ settings exchanged
                      ▼
     ┌──────────────────────────────────┐
     │            Header                │ ◄──────────────────────────┐
     │  (read 9-byte frame header)      │                            │
     └───────────────┬──────────────────┘                            │
                     │ parsed FrameHeader                            │
                     ▼                                               │
     ┌──────────────────────────────────┐                            │
     │         Frame(header)            │   expect_header()          │
     │  (read payload_len bytes)        │ ──────────────────────────▶│
     └───────────────┬──────────────────┘                            │
                     │                                               │
          ┌──────────┼───────────┐                                   │
          │          │           │                                    │
          ▼          ▼           ▼                                    │
     HEADERS      DATA      SETTINGS/PING/...                        │
     (if end_headers=0)         │                                    │
          │                     └────────────────────────────────────▶│
          ▼                                                          │
     ┌──────────────────────┐                                        │
     │ ContinuationHeader   │  (read 9-byte continuation header)     │
     └──────────┬───────────┘                                        │
                │                                                    │
                ▼                                                    │
     ┌──────────────────────┐                                        │
     │ ContinuationFrame    │  (accumulate header block)             │
     └──────────┬───────────┘                                        │
                │ end_headers=1                                      │
                └────────────────────────────────────────────────────▶│
                                                                     │
     ┌──────────────────────┐                                        │
     │       GoAway         │  (drain: send GOAWAY, close)           │
     └──────────────────────┘                                        │
     ┌──────────────────────┐                                        │
     │       Error          │  (protocol error detected)             │
     └──────────────────────┘
```

For writing, the `writable()` method delegates to `write_streams()` which iterates over
all streams sorted by RFC 9218 urgency (lower urgency = higher priority), then by stream
ID for FIFO within the same urgency level. Each stream's Kawa blocks are converted to H2
frames via `H2BlockConverter`. Connection-level and stream-level flow control windows
limit how many DATA bytes can be sent per iteration.

#### H2 flow control

Flow control operates at two levels per RFC 9113 §6.9:

```
    ConnectionH2                          Stream
  ┌──────────────┐                    ┌──────────────┐
  │ window: i32  │  ◄── connection    │ window: i32  │  ◄── stream
  │              │      level         │              │      level
  └──────┬───────┘                    └──────┬───────┘
         │                                   │
         │  effective window = min(connection.window, stream.window)
         │
         ▼
  Sending DATA: decrement both windows by bytes sent.
  Receiving DATA: track received_bytes_since_update.

  When received_bytes_since_update > initial_window_size / 2:
    → Queue connection-level WINDOW_UPDATE
    → Queue stream-level WINDOW_UPDATE
    → Flush during writable() before sending DATA
```

WINDOW_UPDATE frames are coalesced per stream ID in `pending_window_updates` and flushed
inline at the start of `writable()`, avoiding extra event loop iterations.

#### H2 frame processing pipeline

```
  Socket ──read──▶ zero.storage ──parse──▶ FrameHeader ──▶ Frame
                   (connection                              │
                    buffer)                                  │
                                                ┌───────────┼───────────┐
                                                ▼           ▼           ▼
                                             HEADERS      DATA     SETTINGS
                                                │           │        PING
                                                │           │       GOAWAY
                                                ▼           ▼      WINDOW_UPDATE
                                          ┌──────────┐ ┌────────┐
                                          │  pkawa   │ │ stream │
                                          │  HPACK   │ │ .front │
                                          │  decode  │ │ .push  │
                                          │  + kawa  │ │ _block │
                                          │  blocks  │ └────────┘
                                          └──────────┘

  stream.back ──kawa::prepare()──▶ H2BlockConverter ──▶ HPACK encode
                                                           │
                                   gen_frame_header() ◄────┘
                                          │
                                          ▼
                                   kawa.out ──write──▶ Socket
```

- **Inbound**: `parser.rs` (nom) decodes binary frames. `pkawa.rs` decodes HPACK headers
  into Kawa blocks. DATA payloads are zero-copy slices into the stream's storage buffer.
- **Outbound**: `converter.rs` (`H2BlockConverter`) encodes Kawa blocks into H2 frames.
  HPACK encoding uses `loona-hpack::Encoder`. Large header blocks are automatically split
  into HEADERS + CONTINUATION frames respecting `max_frame_size`.

#### Module layout

```
lib/src/protocol/mux/
├── mod.rs          Mux session, Stream, Router, ready() loop, stream lifecycle (2605 lines)
├── h1.rs           HTTP/1.1 connection (ConnectionH1) (675 lines)
├── h2.rs           HTTP/2 connection (ConnectionH2), state machine, flow control,
│                   flood detection, RFC 9218 priorities, shutdown handling (3994 lines)
├── parser.rs       H2 binary frame parser (nom), wire format constants (1598 lines)
├── serializer.rs   H2 frame serializer (cookie-factory), SETTINGS/GOAWAY/RST_STREAM (493 lines)
├── converter.rs    Kawa → H2 frame converter (H2BlockConverter), HPACK encoding (823 lines)
└── pkawa.rs        H2 → Kawa converter, HPACK decoding, pseudo-header validation,
                    RFC 9218 priority header parsing (990 lines)
```

Total: 11178 lines of Rust across 7 modules.

#### Key design decisions

- **Bidirectional end-of-stream tracking**: Separate `front_received_end_of_stream` and
  `back_received_end_of_stream` fields on each `Stream` prevent the frontend and backend
  connections from interfering with each other's stream state.
- **Shared-nothing stream pool**: Streams are stored in `Vec<Stream>` indexed by
  `GlobalStreamId`. Each `ConnectionH2` maps its H2 stream IDs to global IDs via
  `HashMap<StreamId, GlobalStreamId>`. This allows H2 frontend and H2 backend to reference
  the same stream without shared ownership.
- **HPACK safety**: Decode callbacks (`pkawa.rs`) use fallible `write_all()` calls and
  validate headers per RFC 9113 §8.2 (no uppercase, no connection-specific headers,
  pseudo-header ordering). Invalid headers trigger a stream reset, not a connection error.
- **GoAway drain**: When a GoAway frame is received, the connection enters `draining` mode.
  New streams are rejected, but existing streams complete normally. Streams above
  `peer_last_stream_id` are eligible for retry on a new connection.
- **Actively driven graceful shutdown**: During worker drain, the mux layer forces
  H2 readable and writable passes outside the normal epoll loop so it can observe
  final peer EOF / END_STREAM events, flush GOAWAY and TLS records, and prune
  stale stream mappings created by incomplete HEADERS blocks.
- **H2 backend (h2c)**: Controlled by `cluster.http2` in protobuf config. Sōzu speaks
  cleartext H2 to backends. The `:scheme` pseudo-header is derived from the frontend
  listener protocol (HTTP or HTTPS), not the backend connection.
- **RFC 9218 priorities**: The `Prioriser` struct tracks urgency (0-7) and incremental
  flags per stream from the `priority` header. Stream scheduling in `write_streams()`
  sorts by urgency first (lower = higher priority), then by stream ID. Default urgency
  is 3 per RFC 9218.
- **Configurable flood detection**: Six thresholds (RST_STREAM, PING, SETTINGS, empty
  DATA, CONTINUATION, glitch count) are configurable per-listener via protobuf, with
  compile-time defaults. Exceeding any threshold triggers GOAWAY(ENHANCE_YOUR_CALM).
- **Proportional overhead distribution**: Connection-level overhead bytes (SETTINGS, PING,
  WINDOW_UPDATE) are distributed to streams proportional to their bytes transferred, not
  equally. This ensures accurate per-stream accounting for billing and metrics.
- **OpenTelemetry trace propagation**: Behind the `opentelemetry` feature flag,
  `traceparent`/`tracestate` headers are extracted during H2 stream creation and
  propagated into access log entries for distributed tracing correlation.

## Logging

The [logger](https://github.com/sozu-proxy/sozu/blob/3111e2db420d2773b1f0404d6556f40b2f2ea85b/lib/src/logging.rs) is designed to reduce allocations and string interpolations, using Rust's formatting system. It can send logs on various backends: stdout, file, TCP, UDP, Unix sockets.

The logger can be invoked through a thread local storage variable accessible from anywhere with logging macros.

## Metrics

[Metrics](https://github.com/sozu-proxy/sozu/tree/3111e2db420d2773b1f0404d6556f40b2f2ea85b/lib/src/network/metrics) work like the logger, accessible from anywhere with macros and TLS. We support two "drains": one that sends the metrics on the networks with a statsd compatible protocol, and one aggregating metrics locally, to be queried through the configuration socket.

## Load balancing

For a given cluster, Sōzu keeps a list of backends to which the connection is redirected.
Sōzu detects broken servers and redirects traffic only to healthy ones, with several available loadbalancing algorithms:
round robin (default), random, least_loaded, and power of two.

## TLS

Sōzu is an TLS endpoint, powered by rustls.
It decrypts the traffic using the TLS key and certificate, and forwards it, unencrypted, to the backends.

## Deep dive

### Buffers

Sōzu is optimised for very limited memory use.
All traffic is (briefly) stored in a pool of fix-sized (usually 16 kB), reusable buffers.

### channels

They are an abstraction layer over a unix socket, making chatting with a sōzu easier.
