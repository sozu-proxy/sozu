# H2 Mux Internals

A developer reference for the HTTP/2 multiplexer implementation.
For architecture overview and diagrams, see [architecture.md](./architecture.md).
For user-facing configuration, see [configure.md](./configure.md).

Source files covered by this document:

| File | Role |
|------|------|
| `lib/src/protocol/mux/h2.rs` | `ConnectionH2` struct, state machine, flow control, flood detection |
| `lib/src/protocol/mux/pkawa.rs` | HPACK decoding, pseudo-header validation, RFC 9218 priority parsing |
| `lib/src/protocol/mux/mod.rs` | Mux session, Stream, Router, ready() loop, stream lifecycle |
| `lib/src/protocol/mux/converter.rs` | Kawa-to-H2 frame encoding (`H2BlockConverter`) |
| `lib/src/protocol/mux/parser.rs` | H2 binary frame parser (nom) |
| `lib/src/protocol/mux/serializer.rs` | H2 frame serializer (SETTINGS, GOAWAY, RST_STREAM) |

---

## ConnectionH2 Sub-structures

`ConnectionH2` is the central H2 connection type, generic over `Front: SocketHandler`.
Its fields are decomposed into focused sub-structures to separate concerns:

```
ConnectionH2<Front>
 |
 |-- socket: Front                          // TLS or TCP socket
 |-- state: H2State                         // Frame-level state machine
 |-- position: Position                     // Server or Client(cluster, scheme)
 |-- readiness: Readiness                   // Edge-triggered interest tracking
 |
 |-- flow_control: H2FlowControl            // Connection-level flow control
 |   |-- window: i32                        // Send window (can go negative per RFC 9113 s6.9.2)
 |   |-- received_bytes_since_update: u32   // Inbound bytes since last WINDOW_UPDATE
 |   |-- pending_window_updates: Vec<(u32, u32)>  // Queued (stream_id, increment) pairs
 |
 |-- bytes: H2ByteAccounting                // Overhead attribution bookkeeping
 |   |-- zero_bytes_read: usize             // Bytes read on stream 0 not yet attributed
 |   |-- overhead_bin: usize                // Overhead bytes received (connection frames)
 |   |-- overhead_bout: usize               // Overhead bytes sent (connection frames)
 |
 |-- drain: H2DrainState                    // Graceful shutdown state
 |   |-- draining: bool                     // True after first GOAWAY sent
 |   |-- peer_last_stream_id: Option<StreamId>  // From peer's GOAWAY (for retry)
 |
 |-- flood_detector: H2FloodDetector        // CVE-mitigation rate limiters
 |   |-- config: H2FloodConfig              // 6 configurable thresholds
 |   |-- rst_stream_count, ping_count, ...  // Per-window counters
 |   |-- glitch_count: u32                  // Cumulative anomaly counter
 |   |-- window_start: Instant              // Sliding window epoch
 |
 |-- prioriser: Prioriser                   // RFC 9218 stream priorities
 |   |-- priorities: HashMap<StreamId, (u8, bool)>  // urgency + incremental
 |
 |-- decoder: loona_hpack::Decoder          // HPACK decoder (inbound)
 |-- encoder: loona_hpack::Encoder          // HPACK encoder (outbound)
 |-- local_settings: H2Settings             // Settings we advertise
 |-- peer_settings: H2Settings              // Settings the peer advertised
 |-- streams: HashMap<StreamId, GlobalStreamId>  // H2 stream ID -> shared pool index
 |-- highest_peer_stream_id: StreamId       // For GOAWAY last_stream_id
 |-- converter_buf: Vec<u8>                 // Reusable HPACK encode buffer
 |-- lowercase_buf: Vec<u8>                 // Reusable header key lowercase buffer
 |-- pending_rst_streams: Vec<(StreamId, H2Error)>  // Queued RST_STREAM frames
 |-- rst_sent: HashSet<StreamId>            // Dedup: RST_STREAM already sent
 |-- settings_sent_at: Option<Instant>      // SETTINGS ACK timeout tracking
 |-- zero: GenericHttpStream                // Connection-level (stream 0) buffer
 |-- timeout_container: TimeoutContainer    // Session timeout management
```

Access patterns use the sub-structure names directly:

```rust
self.flow_control.window -= consumed;
self.bytes.overhead_bin += size;
self.drain.draining = true;
self.flood_detector.check_flood();
self.prioriser.get(&stream_id);
```

---

## RFC 9218 Extensible Priorities

### Prioriser struct

`Prioriser` manages per-stream scheduling priorities per RFC 9218. It wraps a
`HashMap<StreamId, (u8, bool)>` where the tuple is `(urgency, incremental)`.

**Methods:**

| Method | Signature | Behavior |
|--------|-----------|----------|
| `push_priority` | `(&mut self, StreamId, PriorityPart) -> bool` | Inserts/updates priority. Returns `true` on self-dependency (protocol error). Clamps urgency to 0-7. Ignores deprecated RFC 7540 tree priorities. |
| `get` | `(&self, &StreamId) -> (u8, bool)` | Returns `(urgency, incremental)`. Defaults to `(3, false)` if absent. |
| `remove` | `(&mut self, &StreamId)` | Removes entry at stream cleanup. |

### parse_rfc9218_priority()

Located in `pkawa.rs`, this function parses the `priority` HTTP header value:

```rust
fn parse_rfc9218_priority(value: &[u8]) -> (u8, bool)
```

The header uses RFC 8941 Structured Fields dictionary format. Examples:

| Input | Parsed |
|-------|--------|
| `u=0, i` | urgency=0, incremental=true |
| `u=3` | urgency=3, incremental=false |
| `i` | urgency=3 (default), incremental=true |
| `u=9` | urgency=7 (clamped), incremental=false |
| (absent) | urgency=3, incremental=false |
| `i=?0` | urgency=3, incremental=false |

The parser splits on `,`, trims OWS (SP/HTAB per RFC 9110 s5.6.3), and processes
tokens `u=N` and `i`/`i=?1`/`i=?0`. Malformed tokens are silently ignored.

### How priorities affect stream scheduling

In `write_streams()`, all active stream IDs are collected and sorted:

```rust
let mut priorities = self.streams.keys().collect::<Vec<_>>();
priorities.sort_by(|a, b| {
    let (ua, _) = self.prioriser.get(a);
    let (ub, _) = self.prioriser.get(b);
    ua.cmp(&ub).then_with(|| a.cmp(b))
});
```

Lower urgency values are served first (urgency 0 = highest priority).
Among streams with equal urgency, lower stream IDs go first for stability.
The `incremental` flag is stored but not yet used for round-robin scheduling
within an urgency level.

### Priority cleanup

Priority entries are removed at 4 lifecycle sites to prevent HashMap growth:

1. **dead_streams loop** (end of `write_streams`): `self.prioriser.remove(&stream_id)`
2. **RST_STREAM received** (in `handle_frame`): cleaned via stream removal
3. **GoAway processing** (`close_all_streams`): streams map is cleared
4. **`end_stream`** (backend-initiated close): `self.prioriser.remove(&id)`

---

## Flood Detection

### H2FloodConfig

Configurable thresholds with safe compile-time defaults:

| Field | Default | CVE | Attack |
|-------|---------|-----|--------|
| `max_rst_stream_per_window` | 100 | CVE-2023-44487, CVE-2019-9514 | Rapid Reset / Reset Flood |
| `max_ping_per_window` | 100 | CVE-2019-9512 | Ping Flood |
| `max_settings_per_window` | 50 | CVE-2019-9515 | Settings Flood |
| `max_empty_data_per_window` | 100 | CVE-2019-9518 | Empty Frames Attack |
| `max_continuation_frames` | 20 | CVE-2024-27316 | CONTINUATION Flood |
| `max_glitch_count` | 100 | (cumulative) | General protocol abuse |

The sliding window duration is 1 second (`FLOOD_WINDOW_DURATION`).

### H2FloodDetector

Created via `H2FloodDetector::new(config)`. Tracks per-window counters for each
frame type plus a cumulative `glitch_count` for miscellaneous protocol violations.

**Sliding window decay** (`maybe_reset_window`): When the window expires,
counters are halved (not zeroed). This half-decay catches burst-then-wait attack
patterns where an attacker sends a burst, waits for the window to reset, then
bursts again.

**Check flow** (`check_flood`):

```
check_flood()
  |-- maybe_reset_window()   // half-decay if window expired
  |-- check rst_stream_count > threshold?  --> Some(EnhanceYourCalm)
  |-- check ping_count > threshold?        --> Some(EnhanceYourCalm)
  |-- check settings_count > threshold?    --> Some(EnhanceYourCalm)
  |-- check empty_data_count > threshold?  --> Some(EnhanceYourCalm)
  |-- check continuation_count > threshold? --> Some(EnhanceYourCalm)
  |-- check accumulated_header_size > 64KB? --> Some(EnhanceYourCalm)
  |-- check glitch_count > threshold?      --> Some(EnhanceYourCalm)
  '-- None (all OK)
```

**CONTINUATION-specific counters** are reset when a header block completes
(`reset_continuation()`), since they track per-block counts, not per-window.

### The glitch_count mechanism

`glitch_count` is incremented for protocol anomalies that don't fit a specific
flood pattern but indicate abuse in aggregate:

- Frames on closed streams (RST_STREAM, WINDOW_UPDATE, DATA on already-closed streams)
- Other minor protocol violations that don't warrant an immediate GOAWAY

Unlike the rate-based counters, `glitch_count` uses the same half-decay window,
providing cumulative abuse detection.

### What happens when a threshold is exceeded

When `check_flood()` returns `Some(EnhanceYourCalm)`:

1. A warning is logged identifying the specific threshold exceeded
2. `goaway(H2Error::EnhanceYourCalm)` is called
3. The connection enters `H2State::Error`, `drain.draining = true`
4. A GOAWAY frame with error code ENHANCE_YOUR_CALM (0xb) is serialized
5. The connection transitions to `H2State::GoAway` for final write + disconnect

### Per-listener configurability

Thresholds are configurable via protobuf listener config. Both `HttpListenerConfig`
and `HttpsListenerConfig` expose optional fields:

```protobuf
// In HttpListenerConfig and HttpsListenerConfig:
// Flood detection thresholds:
optional uint32 h2_max_rst_stream_per_window = 13;
optional uint32 h2_max_ping_per_window = 14;
optional uint32 h2_max_settings_per_window = 15;
optional uint32 h2_max_empty_data_per_window = 16;
optional uint32 h2_max_continuation_frames = 17;
optional uint32 h2_max_glitch_count = 18;
// Connection tuning:
optional uint32 h2_initial_connection_window = 19;
optional uint32 h2_max_concurrent_streams = 20;
optional uint32 h2_stream_shrink_ratio = 21;
```

When absent (`None`), the built-in defaults apply:
- Flood thresholds from `H2FloodConfig::default()`
- Connection tuning from `H2ConnectionConfig::default()`

`H2ConnectionConfig` controls connection-level parameters:

| Field | Default | Description |
|-------|---------|-------------|
| `initial_connection_window` | 1048576 (1MB) | Connection receive window (RFC 9113 §6.9.2), clamped to [65535, 2^31-1] |
| `max_concurrent_streams` | 100 | `SETTINGS_MAX_CONCURRENT_STREAMS`, also sizes the pending WINDOW_UPDATE cap |
| `stream_shrink_ratio` | 2 | Stream Vec shrink threshold: `total > active * ratio`, minimum 2 |

This allows operators to tune both security and performance per listener.

---

## Overhead Distribution

### Problem

In HTTP/2, connection-level frames (SETTINGS, PING, WINDOW_UPDATE, GOAWAY,
SETTINGS ACK) consume bandwidth but don't belong to any specific stream.
For accurate per-stream byte accounting in access logs and metrics, this overhead
must be attributed proportionally.

### distribute_overhead()

A **free function** (not a method) to avoid borrow conflicts:

```rust
fn distribute_overhead(
    metrics: &mut SessionMetrics,
    overhead_bin: &mut usize,
    overhead_bout: &mut usize,
    stream_bytes: (usize, usize),
    total_bytes: (usize, usize),
    active_streams: usize,
)
```

It is extracted as a free function because `write_streams()` borrows `self.encoder`
through the converter while simultaneously needing to update per-stream metrics
and connection overhead counters. A `&mut self` method would conflict.

**Distribution formula:**

```
share_in  = overhead_bin  * (stream_bytes_in  / total_bytes_in)
share_out = overhead_bout * (stream_bytes_out / total_bytes_out)
```

When `total_bytes` is zero (no stream has transferred data yet), falls back to
even distribution: `overhead / max(active_streams, 1)`.

After attribution, the distributed amounts are subtracted from the overhead
accumulators, so remaining overhead carries over to subsequent streams.

### compute_stream_byte_totals()

```rust
fn compute_stream_byte_totals(&self, context: &Context) -> (usize, usize)
```

Iterates all active streams summing `(bin + backend_bin, bout + backend_bout)`.
Must be called **before** taking mutable borrows on individual streams to avoid
borrow conflicts with the context.

### Where overhead bytes come from

Bytes are classified as overhead in two places:

- **`attribute_bytes_to_overhead()`**: Called after processing connection-level
  frames (SETTINGS, PING, WINDOW_UPDATE). Moves `zero_bytes_read` into
  `bytes.overhead_bin`.
- **`flush_zero_to_socket()`**: Every byte written from the zero buffer
  (connection-level frames) increments `bytes.overhead_bout`.

### How it feeds into SessionMetrics

At stream completion (`complete_server_stream` or `reset_stream`), the overhead
is distributed to the stream's `SessionMetrics` before the access log is emitted:

```rust
self.distribute_overhead(&mut stream.metrics, byte_totals);
stream.generate_access_log(false, Some("H2::Complete"), listener);
```

This ensures `metrics.bin` and `metrics.bout` in the access log include the
stream's proportional share of connection overhead.

---

## Method Decomposition

The `ConnectionH2` implementation is decomposed into focused methods to manage
the complexity of the H2 state machine:

### readable() entry point

```rust
pub fn readable(&mut self, context, endpoint) -> MuxResult
```

Dispatches based on `H2State`:

| State | Delegates to |
|-------|-------------|
| `Header` | `handle_header_state(context)` |
| `ContinuationHeader(headers)` | `handle_continuation_header_state(&headers)` |
| `Frame(header)` | inline `handle_frame()` logic |
| `ContinuationFrame(headers)` | inline continuation assembly |
| `ClientPreface` / `ServerSettings` | handshake validation |
| `Discard` | skips payload bytes |

### handle_header_state()

Parses a 9-byte frame header from `self.zero`, validates the stream (new, existing,
closed, or idle), creates new streams for HEADERS on odd-numbered IDs, and
transitions to `H2State::Frame(header)` for payload reading.

Key decisions in this method:
- MAX_CONCURRENT_STREAMS enforcement: queues RST_STREAM(REFUSED_STREAM) and
  transitions to `Discard` state to skip the HEADERS payload
- Buffer pool exhaustion: same treatment as MAX_CONCURRENT_STREAMS
- Closed vs idle stream detection: frames on closed streams get RST_STREAM or
  GOAWAY depending on frame type; frames on idle streams get GOAWAY(PROTOCOL_ERROR)

### handle_continuation_header_state()

Parses CONTINUATION frame headers, validates stream ID continuity, tracks
CONTINUATION flood counters (CVE-2024-27316), and accumulates the header block
fragment length.

### writable() entry point

```rust
pub fn writable(&mut self, context, endpoint) -> MuxResult
```

1. Calls `flush_pending_control_frames()` as preamble
2. Dispatches based on `(H2State, Position)`:
   - Handshake states: serializes client preface, SETTINGS, connection WINDOW_UPDATE
   - Proxying states: delegates to `write_streams(context, endpoint)`

### flush_pending_control_frames()

Flushes control data before application frames, in order:

1. **SETTINGS ACK timeout check**: If peer hasn't ACK'd within 5 seconds,
   sends GOAWAY(SETTINGS_TIMEOUT)
2. **Zero buffer resume**: If a previous control frame write was partial
   (WouldBlock), resume flushing via `flush_zero_to_socket()`
3. **WINDOW_UPDATE frames**: Serializes queued `pending_window_updates` into
   the zero buffer, coalescing entries by stream ID, then flushes
4. **Pending RST_STREAM frames**: Drains `pending_rst_streams` into the zero
   buffer, with flood detection (`MAX_PENDING_RST_STREAMS` cap)

Returns `Some(MuxResult)` if the caller should return early, `None` to proceed.

### write_streams()

The main data-plane write path:

1. Resumes any partially-written stream (`expect_write`)
2. Pre-computes `byte_totals` for overhead distribution
3. Sets up `H2BlockConverter` borrowing `self.encoder`
4. Sorts streams by priority (urgency, then stream_id)
5. For each stream: converts kawa blocks to H2 frames, writes to socket
6. Recycles completed streams, distributes overhead, emits access logs
7. Cleans up `dead_streams` (removes from streams map, rst_sent, prioriser)
8. Shrinks converter buffers if they grew beyond 16KB

**Why write_streams() can't be further decomposed**: The `H2BlockConverter`
borrows `self.encoder` for the duration of the priority loop. This prevents
calling any `&mut self` method within the loop body. The free function
`distribute_overhead()` works around this for metrics, but the converter setup
and priority iteration must remain in a single method scope.

### flush_zero_to_socket()

```rust
fn flush_zero_to_socket(&mut self) -> bool
```

Writes the zero buffer to the socket in a loop. Returns `true` if the socket
stalled (WouldBlock), `false` when fully drained. Counts written bytes as
`overhead_bout`. Clears the buffer after draining to reset positions.

### Shutdown and close path

The branch's final hardening work is concentrated in the shutdown path, where
H2 stream state, GOAWAY sequencing, and rustls buffering interact:

- `Mux::delay_close_for_frontend_flush()` turns an immediate session close into
  a final writable phase when the frontend still has TLS data or GOAWAY bytes
  buffered. On TLS frontends it first asks rustls to generate `close_notify`.
- `Mux::drive_frontend_shutdown_io()` actively runs both `readable()` and
  `writable()` during worker drain. This matters because graceful H2 shutdown
  may require one more read to observe peer EOF / END_STREAM and one more write
  to emit the final GOAWAY or flush buffered TLS records, even when epoll does
  not deliver a fresh readiness edge.
- `ConnectionH2::prune_inactive_streams_while_closing()` removes H2 stream-ID
  mappings for streams that never became active before a connection-level close
  (for example, partial or oversized HEADERS blocks that were abandoned during
  GOAWAY). Without this pruning, shutdown can wait forever on idle entries that
  no longer correspond to useful work.
- `peer_gone_after_final_goaway()` is the terminal shutdown condition for the
  frontend H2 connection. Once the final GOAWAY has been queued, all stream
  mappings are gone, and the peer has already hung up, the remaining rustls
  backlog is no longer deliverable and the session may close immediately.
- `FrontRustls::peer_disconnected` suppresses new TLS writes after EOF/HUP so
  the close path does not keep retrying application writes to a dead peer.
- HTTPS uses `shutdown(Write)` rather than `shutdown(Both)`. On Linux,
  `shutdown(Both)` discards unread receive-buffer data and can convert an
  otherwise clean post-drain close into a TCP RST, truncating bytes that the
  drain loop already flushed.

### complete_server_stream()

Static helper that finalizes a server-side stream: increments `http.e2e.h2`
counter, stops backend metrics, generates access log, resets metrics, and
transitions the stream to `StreamState::Recycle`.

---

## OpenTelemetry Integration

### Feature flag

All OpenTelemetry code is gated behind:

```rust
#[cfg(feature = "opentelemetry")]
```

When disabled, the `otel` field in access log records is `None`.

### How it works in the Mux layer

OpenTelemetry context propagation is handled by `HttpContext` (defined in
`lib/src/protocol/kawa_h1/editor.rs`), which contains:

```rust
#[cfg(feature = "opentelemetry")]
pub otel: Option<sozu_command::logging::OpenTelemetry>,
```

During stream creation in the H1 editor path (`editor.rs`), the `traceparent`
and `tracestate` headers are extracted from inbound requests:

1. `parse_traceparent()` extracts `(trace_id: [u8; 32], parent_id: [u8; 16])`
   from the W3C Trace Context format `00-{trace_id}-{parent_id}-{flags}`
2. A new `span_id` is generated for the proxy hop
3. The `traceparent` header is rewritten with the new span ID
4. If no `traceparent` was present, one is injected; orphaned `tracestate` is elided

### SpanContext propagation into access logs

At access log emission time (`Stream::generate_access_log` in `mod.rs`):

```rust
#[cfg(feature = "opentelemetry")]
otel: context.otel.as_ref(),
#[cfg(not(feature = "opentelemetry"))]
otel: None,
```

The `OpenTelemetry` struct (trace_id, span_id, parent_span_id) is passed to the
log formatter, enabling correlation of proxy access logs with distributed traces.

### H2-specific considerations

The H2 path shares `HttpContext` with the H1 path through the unified `Stream`
abstraction. The `traceparent`/`tracestate` header extraction happens at the
kawa block level, which works identically for both H1 and H2 decoded headers.
No H2-specific OpenTelemetry code exists; the integration is protocol-agnostic
by design.

---

## HPACK Safety

### Fallible write_all() pattern

All HPACK decode callbacks in `pkawa.rs` use fallible writes to kawa storage:

```rust
if kawa.storage.write_all(value).is_err() {
    invalid_headers = true;
    return;
}
```

This prevents buffer overflows when the decoded header block exceeds available
storage. The `invalid_headers` flag is checked after decoding completes, and
triggers a stream reset (`H2Error::ProtocolError`) rather than a connection error.

### invalid_headers flag

Set `true` by the decode callback for any of:

- Uppercase ASCII in header name (RFC 9113 s8.2)
- Connection-specific headers: `connection`, `proxy-connection`,
  `transfer-encoding`, `upgrade`, `keep-alive` (RFC 9113 s8.2.2)
- `TE` header with value other than `trailers` (RFC 9113 s8.2.2)
- Duplicate pseudo-headers (`:method`, `:scheme`, `:path`, `:authority`)
- Pseudo-headers after regular headers
- Unknown pseudo-headers (starting with `:` but not recognized)
- Invalid `content-length` (non-numeric)
- Storage write failures (buffer full)

When `invalid_headers` is `true` after decoding:
- For requests: returns `Err((H2Error::ProtocolError, false))` -- stream error
- For responses: same treatment

The `false` in the error tuple indicates this is a stream-level error, not
connection-level. The caller sends RST_STREAM rather than GOAWAY.

### Table size sync on SETTINGS ACK

Per RFC 7541 s4.2, the HPACK dynamic table size must be synchronized when
SETTINGS are acknowledged:

```rust
// On receiving SETTINGS ACK from peer:
self.decoder.set_max_allowed_table_size(
    self.local_settings.settings_header_table_size as usize,
);

// On receiving peer's SETTINGS:
self.peer_settings.settings_header_table_size = v;
self.encoder.set_max_table_size(v as usize);
```

The decoder's allowed table size matches what we advertised; the encoder's
table size matches what the peer advertised. This prevents desynchronization
that would cause `CompressionError` (GOAWAY).

### Buffer shrinking after large headers

The `converter_buf` and `lowercase_buf` are reusable buffers moved into and
out of `H2BlockConverter` each `write_streams()` cycle:

```rust
// After reclaiming buffers from the converter:
if self.converter_buf.capacity() > 16_384 {
    self.converter_buf.shrink_to(4096);
}
if self.lowercase_buf.capacity() > 16_384 {
    self.lowercase_buf.shrink_to(4096);
}
```

This prevents a single request with abnormally large headers from permanently
inflating memory for the lifetime of the connection.

---

## Testing

### Test inventory

185 e2e tests across 7 files:

| File | Count | Focus |
|------|-------|-------|
| `e2e/src/tests/tests.rs` | 40 | General HTTP proxying, keep-alive, routing, worker lifecycle |
| `e2e/src/tests/h2_security_tests.rs` | 41 | Security edge cases: flood detection thresholds, rapid reset, CONTINUATION bombs, settings flood, empty DATA flood, glitch counting, malformed frame handling |
| `e2e/src/tests/h2_tests.rs` | 64 | Protocol correctness: HEADERS, DATA, flow control, GOAWAY, stream lifecycle, priority, HPACK, concurrent streams, window updates, graceful shutdown, H2 backend behavior |
| `e2e/src/tests/mux_tests.rs` | 21 | Cross-protocol scenarios: H1-to-H2 backend, H2-to-H1 backend, end-to-end H2, mixed protocol combinations |
| `e2e/src/tests/h1_security_tests.rs` | 8 | H1-specific security (request smuggling, header injection) |
| `e2e/src/tests/tls_tests.rs` | 6 | TLS handshake, ALPN negotiation, certificate handling, close semantics |
| `e2e/src/tests/tcp_tests.rs` | 5 | Raw TCP proxying |

### Test infrastructure

Tests use the `e2e` crate which provides:

- `mock::h2_backend` -- h2 crate-based mock backend for H2 backend tests
- `mock::https_client` -- TLS client with ALPN support
- `mock::sync_backend` / `mock::async_backend` -- HTTP/1.1 mock backends
- `sozu::worker` -- Embedded sozu worker for integration testing

### h2spec conformance

The implementation targets 145/145 h2spec test cases for RFC 9113 conformance.
h2spec is an external conformance testing tool (https://github.com/summerwind/h2spec)
that validates frame-level protocol correctness.

### Shutdown-focused regression coverage

Recent branch tests explicitly cover the shutdown hardening described above:

- `test_h2_double_goaway_graceful_shutdown`
- `test_h2_graceful_shutdown_completes_large_transfer`
- `test_h2_graceful_shutdown_waits_for_inflight_request`

Together they exercise the double-GOAWAY sequence, completion of in-flight
large responses during worker drain, and the requirement that soft-stop waits
for active requests instead of tearing sessions down early.

### Safety properties

The implementation maintains a zero-panic-path policy for the H2 read/write
paths. All fallible operations (frame parsing, HPACK decoding, buffer writes)
return errors that are converted to GOAWAY or RST_STREAM rather than panicking.
The compile-time assertion at the top of `h2.rs` guards against silent pointer
truncation on sub-32-bit platforms:

```rust
const _: () = assert!(
    std::mem::size_of::<usize>() >= 4,
    "sozu requires at least 32-bit pointers"
);
```
