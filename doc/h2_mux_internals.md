# H2 Mux Internals

A developer reference for the HTTP/2 multiplexer implementation.
For architecture overview and diagrams, see [architecture.md](./architecture.md).
For user-facing configuration, see [configure.md](./configure.md).

Source files covered by this document:

| File | Role |
|------|------|
| `lib/src/protocol/mux/h2.rs` | `ConnectionH2` struct, state machine, flow-control orchestration |
| `lib/src/protocol/mux/h2_flow_control.rs` | `H2FlowControl` — connection-level send/receive window + pending WINDOW_UPDATE queue (RFC 9113 §6.9), closed API |
| `lib/src/protocol/mux/h2_flood_detector.rs` | `H2FloodConfig`, `H2FloodViolation`, `H2FloodDetector` — CVE-2023-44487 / CVE-2024-27316 / CVE-2025-8671 flood/abuse detection, closed API |
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
 |-- flow_control: h2_flow_control::H2FlowControl  // Connection-level flow control
 |   |                                       // (defined in h2_flow_control.rs; private
 |   |                                       // fields, reached only through its
 |   |                                       // accessor/mutator methods — see below)
 |   |-- window: i32                        // Send window (can go negative per RFC 9113 s6.9.2)
 |   |-- received_bytes_since_update: u32   // Inbound bytes since last WINDOW_UPDATE
 |   |-- pending_window_updates: BTreeMap<u32, u32>  // Queued stream_id -> increment,
 |   |                                       // ascending order for deterministic drain
 |
 |-- bytes: H2ByteAccounting                // Overhead attribution bookkeeping
 |   |-- zero_bytes_read: usize             // Bytes read on stream 0 not yet attributed
 |   |-- overhead_bin: usize                // Overhead bytes received (connection frames)
 |   |-- overhead_bout: usize               // Overhead bytes sent (connection frames)
 |
 |-- drain: H2DrainState                    // Closed API (h2_drain.rs, private fields):
 |                                         // draining, peer_last_stream_id, started_at,
 |                                         // graceful_shutdown_deadline, initial_goaway_pending
 |
 |-- flood_detector: H2FloodDetector        // Closed API (h2_flood_detector.rs, private fields):
 |                                         // config: H2FloodConfig (13 configurable thresholds),
 |                                         // per-window rate counters + never-decaying lifetime
 |                                         // ceilings (Rapid Reset / MadeYouReset / CONTINUATION /
 |                                         // Ping / Settings floods), glitch_count, window_start
 |
 |-- scheduler: H2Scheduler                 // Closed API (h2_scheduler.rs, private
 |                                         // fields): prioriser (priorities:
 |                                         // HashMap<StreamId, (u8, bool)> urgency +
 |                                         // incremental, incremental_cursor) and the
 |                                         // reusable write-pass order buffer
 |
 |-- hpack: HpackState                      // Closed API (hpack_state.rs, private fields):
 |                                         // decoder, encoder, and the reusable
 |                                         // converter_buf / lowercase_buf / cookie_buf
 |                                         // scratch buffers
 |-- local_settings: H2Settings             // Settings we advertise
 |-- peer_settings: H2Settings              // Settings the peer advertised
 |-- stream_table: H2StreamTable             // Closed API (h2_stream_table.rs, private fields):
 |                                         // streams: HashMap<StreamId, GlobalStreamId>,
 |                                         // highest_peer_stream_id, expect_read, expect_write,
 |                                         // rst_sent, and the per-stream activity/fc-stall maps
 |-- pending_table_size_update: Option<u32> // RFC 7541 s6.3 directive owed to the peer
 |-- control_tx: H2ControlTx                // Closed API (h2_control_tx.rs, private fields):
 |                                         // pending_rst_streams: Vec<(StreamId, H2Error)>,
 |                                         // total_rst_streams_queued (never-decaying, behind the
 |                                         // CVE-2025-8671 cap) and max_pending
 |-- settings_sent_at: Option<Instant>      // SETTINGS ACK timeout tracking
 |-- zero: GenericHttpStream                // Control-frame write scratch + per-frame
 |                                         // read landing zone for stream 0. No longer a
 |                                         // HEADERS+CONTINUATION reassembly buffer — see
 |                                         // header_reassembly below and h2_header_reassembly.rs
 |-- header_reassembly: h2_header_reassembly::HeaderBlockAccumulator  // Closed API
 |                                         // (h2_header_reassembly.rs, private fields): the
 |                                         // owned HEADERS+CONTINUATION field-block accumulator,
 |                                         // decoupled from `zero` so a control-frame flush
 |                                         // cannot reach it — no write-side site names this
 |                                         // field. A READ-side bug (an early return in
 |                                         // handle_headers_frame skipping retirement) can still
 |                                         // leak it; see this file's flush_pending_control_frames
 |                                         // section and LIFECYCLE.md invariant 24
 |-- timeout_duration: Duration             // Configured idle timeout
 |-- timeout_deadline: Option<Instant>      // Next callback instant; the Mux
 |                                         // adapter owns the TimeoutContainer
 |-- connection_config: H2ConnectionConfig  // Per-listener window / max-streams / shrink
 |-- stream_idle_timeout: Duration          // Per-stream idle cap
 |-- now: Instant                           // Clock snapshot for the executing pass
```

The listing is the shape, not the census: `ConnectionH2` also carries the
back-pressure, gauge-rebalancing and discard-state fields that no section below
discusses. Read the struct for the full list.

Access patterns use the sub-structure names directly, except `flow_control`,
`stream_table`, `hpack`, `flood_detector`, `drain`, `scheduler` and
`control_tx`, which are closed APIs (`h2_flow_control.rs`,
`h2_stream_table.rs`, `hpack_state.rs`, `h2_flood_detector.rs`, `h2_drain.rs`,
`h2_scheduler.rs` and `h2_control_tx.rs` respectively, all seven with private
fields) reached only through their accessor/mutator methods:

```rust
self.flow_control.consume_send_window(consumed);
self.bytes.overhead_bin += self.bytes.zero_bytes_read;
self.drain.enter_final_goaway();
self.flood_detector.check_flood(self.now);
self.scheduler.priority(&stream_id);
self.hpack.encoder_mut();
self.control_tx.has_pending();
```

`control_tx` is the newest of the seven, extracted into `h2_control_tx.rs`
with the queued-RST state; `drain` came before it, extracted into `h2_drain.rs` with the
GOAWAY/drain state machine. `h2.rs` reaches it through `draining()` at eleven
call sites plus `begin_graceful_drain`, `deadline_elapsed`,
`observe_peer_goaway` and `enter_final_goaway`, and reads none of its five
fields directly. The `__test_*` backdoors are the sole exception and are
reached only from test code.

The three closed modules are the reason a snippet in this document can stop
compiling without any citation going stale: `self.encoder` and
`self.converter_buf` were plain `ConnectionH2` fields until they moved into
`hpack_state.rs`, and prose quoting them kept resolving long after it stopped
being valid Rust. Fenced blocks here now name the lines they quote, in the
fence's info string, so `check_doc_citations.py` compares the two — see
[README.md](./README.md#pinning-a-quoted-code-block).

---

## RFC 9218 Extensible Priorities

### H2Scheduler and Prioriser

Both live in `lib/src/protocol/mux/h2_scheduler.rs`, behind the closed-API shape
`hpack_state.rs`, `h2_flow_control.rs`, `h2_stream_table.rs` and
`h2_flood_detector.rs` use. `ConnectionH2` holds one private `scheduler` field
and reaches everything below through the methods listed here.

`Prioriser` manages per-stream scheduling priorities per RFC 9218. It wraps a
`HashMap<StreamId, (u8, bool)>` where the tuple is `(urgency, incremental)`,
plus the round-robin `incremental_cursor`.

**`Prioriser` methods:**

| Method | Signature | Behavior |
|--------|-----------|----------|
| `push_priority` | `(&mut self, StreamId, PriorityPart) -> bool` | Inserts/updates priority. Returns `true` on self-dependency (protocol error). Clamps urgency to 0-7. Ignores deprecated RFC 7540 tree priorities. |
| `push_priority_guarded` | `(&mut self, StreamId, PriorityPart, StreamId, &HashMap<StreamId, GlobalStreamId>) -> bool` | Same, behind the open-stream / idle-look-ahead filter that bounds a PRIORITY flood. |
| `get` | `(&self, &StreamId) -> (u8, bool)` | Returns `(urgency, incremental)`. Defaults to `(3, false)` if absent. |
| `remove` | `(&mut self, &StreamId)` | Removes entry at stream cleanup. |
| `apply_incremental_rotation` | `(&self, &mut [StreamId]) -> usize` | Inside each urgency bucket, moves incremental streams to the tail and rotates that tail past `incremental_cursor`. Returns the incremental count. |
| `advance_incremental_cursor` | `(&mut self, Option<StreamId>)` | Commits the pass's leader as the next pass's cursor. `None` is a no-op. |

**`H2Scheduler` methods** — `priority`, `push_priority`,
`push_priority_guarded`, `remove_stream` and `prioriser_mut` delegate to the
`Prioriser` above; the pass API is:

| Method | Signature | Behavior |
|--------|-----------|----------|
| `begin_pass` | `(&mut self, impl IntoIterator<Item = StreamId>, impl FnMut(StreamId) -> bool) -> (Vec<StreamId>, ReadyIncrementalCensus)` | Orders one write pass and takes its same-urgency ready-incremental census. The closure is the caller's readiness projection — the one fact the scheduler does not own — and is called for incremental streams only. |
| `end_pass` | `(&mut self, Vec<StreamId>, ReadyIncrementalCensus)` | Takes the order buffer back and commits the round-robin cursor. |
| `reclaim_idle_buffer` | `(&mut self, usize)` | Quiet-time shrink of the order buffer, beside `HpackState::reclaim_idle_buffers`. |

`ReadyIncrementalCensus` is a fixed `[usize; 8]` (RFC 9218 §4.1 urgency is
`[0, 7]`) plus the pass leader. `write_streams` reads
`incremental_peer_count(urgency)` per stream, calls `note_ineligible` at the
three mid-pass transitions of LIFECYCLE.md invariant 17, `note_fired` when a
stream consumes window, and `ready_total` for the
`h2.streams.ready_incremental.by_urgency` gauge.

### parse_rfc9218_priority()

Located in `pkawa.rs`, this function parses the `priority` HTTP header value:

```rust lib/src/protocol/mux/pkawa.rs:623
pub(super) fn parse_rfc9218_priority(value: &[u8]) -> (u8, bool) {
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

`write_streams()` hands the wire map's keys to the scheduler, which owns the
whole ordering decision:

```rust
let (order, mut census) = self.scheduler.begin_pass(
    self.stream_table.streams().keys().copied(),
    |stream_id| { /* is this stream ready to emit this pass? */ },
);
```

`begin_pass` sorts by `(urgency, stream_id)`, then applies
`Prioriser::apply_incremental_rotation`. Lower urgency values are served first
(urgency 0 = highest priority). Among streams with equal urgency, lower stream
IDs go first for stability, non-incremental streams drain before incremental
ones, and the incremental tail is rotated past `incremental_cursor` so
same-urgency incremental downloads take the lead in turn — one position per
pass, which is LIFECYCLE.md invariant 26's starvation bound.

### Priority cleanup

Priority entries are removed at 4 lifecycle sites to prevent HashMap growth:

1. **dead_streams loop** (end of `write_streams`): `self.scheduler.remove_stream(&stream_id)`
2. **RST_STREAM received** (in `handle_frame`): cleaned via stream removal
3. **GoAway processing** (`handle_goaway_frame`, plus
   `prune_inactive_streams_while_closing` for streams that never opened):
   cleaned via stream removal, one retired stream at a time — neither site
   clears the map wholesale
4. **`end_stream`** (backend-initiated close): `self.scheduler.remove_stream(&id)`

---

## Flood Detection

`H2FloodConfig`, `H2FloodViolation` and `H2FloodDetector` live in
`lib/src/protocol/mux/h2_flood_detector.rs`, behind the same closed-API shape
`hpack_state.rs`, `h2_flow_control.rs` and `h2_stream_table.rs` use:
`H2FloodDetector`'s fields are private to that module, reached only through
its `record_*`/`check_flood`/`config`/accessor methods — `ConnectionH2` (in
`h2.rs`) orchestrates the frame-dispatch logging and GOAWAY around it.

### H2FloodConfig

Configurable thresholds with safe compile-time defaults:

| Field | Default | CVE | Attack |
|-------|---------|-----|--------|
| `max_rst_stream_per_window` | 100 | CVE-2023-44487, CVE-2019-9514 | Rapid Reset / Reset Flood (per-window) |
| `max_rst_stream_lifetime` | 10 000 | CVE-2023-44487 | Rapid Reset, never-decaying lifetime ceiling |
| `max_rst_stream_abusive_lifetime` | 50 | CVE-2023-44487 | Rapid Reset signature (pre-response-start RST) |
| `max_rst_stream_emitted_lifetime` | 500 | CVE-2025-8671 | MadeYouReset (server-emitted RST_STREAM) |
| `max_ping_per_window` | 100 | CVE-2019-9512 | Ping Flood |
| `max_settings_per_window` | 50 | CVE-2019-9515 | Settings Flood |
| `max_empty_data_per_window` | 100 | CVE-2019-9518 | Empty Frames Attack |
| `max_window_update_stream0_per_window` | 100 | (rate cap) | Stream-0 WINDOW_UPDATE CPU-burn |
| `max_continuation_frames` | 20 | CVE-2024-27316 | CONTINUATION Flood (per-block frame count) |
| `max_header_list_size` | 65536 (64 KiB) | CVE-2024-27316 | CONTINUATION Flood (per-block accumulated size) |
| `max_header_table_size` | 65536 (64 KiB) | (HPACK memory) | Peer-advertised dynamic table size cap |
| `max_header_fields` | 128 | (HPACK memory) | Indexed-reference "header bomb" |
| `max_glitch_count` | 100 | (cumulative) | General protocol abuse |

The sliding window duration is 1 second (`FLOOD_WINDOW_DURATION`). The three
`*_lifetime` counters deliberately never decay: a half-decaying window counter
cannot see a patient attacker who stays under the per-second ceiling forever.
The fields are private: `H2FloodConfig::new` and `H2FloodConfig::from_optional` are the
only ways to build one, and both clamp every threshold to at least 1 — a zero
threshold does not disable a check, it makes the first event that counter sees
a violation, since `check_flood` compares `count > threshold`. For
`max_header_list_size` and `max_header_fields` — which are also the HPACK
decode budget — that is every request: a stream reset for a header block that
fits one HEADERS frame, and a connection `GOAWAY(ENHANCE_YOUR_CALM)` for one
that spans CONTINUATION frames.
`get_h2_flood_config` in `lib/src/http.rs` and `lib/src/https.rs` calls
`from_optional`, which resolves each unset listener knob to its default above.

Every door that *states* a listener configuration refuses an out-of-range knob
before the clamp can be reached: the configuration file and
`sozu ctl add listener` via `ConfigError::H2ThresholdBelowMinimum`
(`command/src/config.rs`), `UpdateHttp(s)Listener` via
`validate_h2_flood_knobs_http`/`_https` (`command/src/state.rs`), and a raw
protobuf `Add{Http,Https}Listener` sent straight to the command socket via
`validate_h2_flood_knobs_http(s)_listener`, which
`bin/src/command/requests.rs::validate_h2_knob_floors` runs from the main
process before `ConfigState::dispatch`. All five validators expand the single
`for_each_h2_knob_floor!` list in `command/src/lib.rs`, so the knob set cannot
drift between them.

Replay is deliberately not one of those doors. `LoadState` skips an entry its
pre-dispatch validation rejects, and a skipped listener never binds — so
applying the rejection there would take every frontend behind an already-
serving listener offline to prevent one clamped threshold. `load_state`
instead keeps the listener, logs `keeping a listener whose H2 knob is out of
range …` at `warn!` and counts `config.load_h2_knob_clamped`. The clamp is what
serves that case, and it is worth being exact about its direction: `flag` is
`count > threshold`, so `0` trips on the first counted event and the clamped
`1` trips on the second. The clamp *loosens* every knob it touches, by exactly
one event. It is still the right thing on replay, because `0` is not "no limit"
and not a protection level anyone tuned — the whole difference between the
stated value and the clamped one is that one event, against an unbound
listener.

The rejection lives on the command-plane paths, not in listener construction:
`HttpListener::new` / `HttpsListener::try_new` still accept an out-of-range
knob and clamp it, so an embedder driving the library directly gets the
fail-safe rather than the error. That is deliberate — the rejection is a policy
about what an operator may state, and every entry point sozu itself offers
(the configuration file, `sozu ctl`, the command socket) goes through
`ListenerBuilder` or `validate_h2_knob_floors`.

### H2FloodDetector

Created via `H2FloodDetector::new(config, now)` — `now` is the caller's clock
snapshot (`ConnectionH2::new`'s single accept-time sample); the detector never
reads the clock itself. Tracks per-window counters for each frame type, the
never-decaying lifetime ceilings above, plus a cumulative `glitch_count` for
miscellaneous protocol violations.

**Sliding window decay** (`maybe_reset_window`): When the window expires,
rate-based counters (not the lifetime ceilings) are halved (not zeroed). This
half-decay catches burst-then-wait attack patterns where an attacker sends a
burst, waits for the window to reset, then bursts again.

**Check flow** (`H2FloodDetector::check_flood`, which takes the pass's clock
snapshot and returns `Option<H2FloodViolation>` rather than a bare error code —
the violation carries the counter name, its metric key, the observed count and
the threshold it crossed, so the log line and the statsd counter cannot drift
apart; the checks are evaluated in this fixed order — not an iteration over
anything, so the order cannot vary between runs):

```
check_flood(now)
  |-- maybe_reset_window(now)  // half-decay if window expired
  |-- rst_stream_count      > max_rst_stream_per_window?       --> Some(..)
  |-- ping_count            > max_ping_per_window?             --> Some(..)
  |-- total_ping_received_lifetime     > DEFAULT_MAX_PING_LIFETIME?     --> Some(..)
  |-- settings_count        > max_settings_per_window?         --> Some(..)
  |-- total_settings_received_lifetime > DEFAULT_MAX_SETTINGS_LIFETIME? --> Some(..)
  |-- empty_data_count      > max_empty_data_per_window?       --> Some(..)
  |-- continuation_count    > max_continuation_frames?         --> Some(..)
  |-- window_update_stream0_count > max_window_update_stream0_per_window? --> Some(..)
  |-- accumulated_header_size     > max_header_list_size?      --> Some(..)
  |-- glitch_count          > max_glitch_count?                --> Some(..)
  '-- None (all OK)
```

Every variant carries `H2Error::EnhanceYourCalm`; the checks are strict `>`, and
a `debug_assert!` at the end of the chain holds both properties. `H2FloodDetector`
carries five `*_lifetime` counters — `total_rst_received_lifetime`,
`total_abusive_rst_received_lifetime`, `total_rst_streams_emitted_lifetime`,
`total_ping_received_lifetime` and `total_settings_received_lifetime` — and
none of them decays: `maybe_reset_window` halves exactly the six per-window
counters and leaves the lifetime ones alone. That is what closes the
patient-attacker pattern the half-decaying window counters cannot see. Two of
the five are checked inside `check_flood` above; the three RST ones are
enforced at their own frame-handling sites. `ConnectionH2` reaches this through
the `check_flood_or_return!` macro, which passes `self.now` and routes any
violation to `ConnectionH2::handle_flood_violation`.

The RST_STREAM lifetime ceilings (`max_rst_stream_lifetime`,
`max_rst_stream_abusive_lifetime`) and the MadeYouReset ceiling
(`max_rst_stream_emitted_lifetime`) are checked separately, by
`record_rst_lifetime` and `record_rst_emitted` respectively, at their own
call sites — not inside `check_flood`'s chain.

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

When `check_flood()` returns `Some(violation)`, `handle_flood_violation`:

1. Counts `violation.metric_key` and logs a warning naming `violation.reason`
   with the observed count and the threshold it crossed
2. `goaway(violation.error)` is called — always `H2Error::EnhanceYourCalm`
3. The connection enters `H2State::Error`, `drain.enter_final_goaway()`
4. A GOAWAY frame with error code ENHANCE_YOUR_CALM (0xb) is serialized
5. The connection transitions to `H2State::GoAway` for final write + disconnect

### Per-listener configurability

Thresholds are configurable via protobuf listener config. `HttpListenerConfig`
and `HttpsListenerConfig` expose the same optional field *names*, and so do the
matching `UpdateHttpListenerConfig` / `UpdateHttpsListenerConfig` messages —
but each message numbers them independently, so read the field numbers from
`command/src/command.proto` rather than from here:

```protobuf
// In HttpListenerConfig (HttpsListenerConfig carries the same names
// at its own field numbers):
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

That is an excerpt, not the full set: the same messages also carry the
lifetime RST_STREAM caps, `h2_max_header_list_size`,
`h2_max_header_table_size`, `h2_max_header_fields`,
`h2_stream_idle_timeout_seconds`, `h2_graceful_shutdown_deadline_seconds` and
`h2_max_window_update_stream0_per_window`. `doc/configure.md` is the
user-facing reference for all of them.

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

A **free function**, not a method:

```rust lib/src/protocol/mux/h2.rs:465-473
fn distribute_overhead(
    metrics: &mut SessionMetrics,
    overhead_bin: &mut usize,
    overhead_bout: &mut usize,
    stream_bytes: (usize, usize),
    total_bytes: (usize, usize),
    active_streams: usize,
    is_last_stream: bool,
) {
```

It is a free function so the seven `test_distribute_overhead_*` unit tests can
drive the arithmetic directly, with no `ConnectionH2` fixture.
`ConnectionH2::distribute_overhead` is the `&mut self` wrapper the reset paths
use. `ConnectionH2::try_recycle_server_stream` calls the free function directly
instead, which is a spelling choice rather than a constraint — the wrapper would
credit the same shares at that site.

**Distribution formula**, per direction, in the order the branches are taken:

```
is_last_stream        -> share = the whole remaining pool
total_bytes  > 0      -> share = min(overhead * stream_bytes / total_bytes, overhead)
total_bytes == 0      -> share = overhead / max(active_streams, 1)
```

The `is_last_stream` branch exists because integer division loses a remainder
on every earlier stream; handing the last one whatever is left conserves the
pool exactly. The `min` on the proportional branch exists for the mirror
reason: accumulated rounding can push the shares above the pool, and the
subtraction below is on `usize`, so an overshoot would wrap rather than go
negative. Two `debug_assert!`s hold both properties.

After attribution, the distributed amounts are subtracted from the overhead
accumulators, so remaining overhead carries over to subsequent streams, and the
last stream drains them to zero.

### compute_stream_byte_totals()

`ConnectionH2::compute_stream_byte_totals` (`lib/src/protocol/mux/h2.rs`) — cited
as a symbol, not a line: the block below is its signature, so a line number would
only record where the function currently sits.

```rust
fn compute_stream_byte_totals<L: ListenerHandler + L7ListenerHandler>(
    &self,
    context: &Context<L>,
) -> (usize, usize) {
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

At stream completion the overhead is distributed to the stream's
`SessionMetrics` before the access log is emitted. On the normal completion
path that happens inside `ConnectionH2::try_recycle_server_stream`, which calls
the free function directly rather than through the `&mut self` wrapper — a
spelling choice, not a constraint, since the wrapper would credit the same
shares at this site:

```rust lib/src/protocol/mux/h2.rs:3465-3478
let stream_bytes = (
    stream.metrics.bin + stream.metrics.backend_bin,
    stream.metrics.bout + stream.metrics.backend_bout,
);
let active_streams = self.stream_table.streams().len();
distribute_overhead(
    &mut stream.metrics,
    &mut self.bytes.overhead_bin,
    &mut self.bytes.overhead_bout,
    stream_bytes,
    byte_totals,
    active_streams,
    active_streams == 1,
);
```

It then hands the stream to `ConnectionH2::complete_server_stream`, which emits
the log:

This one keeps a line rather than a symbol: `generate_access_log` has four call
sites in `h2.rs` and the paragraph below is about this call's arguments, not the
method.

```rust lib/src/protocol/mux/h2.rs:3511-3517
stream.generate_access_log(
    false,
    Some("H2::Complete"),
    listener,
    client_rtt,
    server_rtt,
);
```

The other three sites take the `&mut self` wrapper
`ConnectionH2::distribute_overhead` instead, and each emits its own log:

- `cancel_timed_out_streams` (`lib/src/protocol/mux/h2.rs:3803`) passes a
  `reason` variable, one of `H2::WindowStall` or `H2::IdleTimeout`, and counts
  the reap under a different metric for each so a DoS-mitigation reap stays
  distinguishable from an ordinary idle one.
- `handle_rst_stream_frame` (`lib/src/protocol/mux/h2.rs:5334`) uses
  `H2::ResetFrame`.
- `ConnectionH2::reset_stream` (`lib/src/protocol/mux/h2.rs:6053`) uses
  `H2::Reset`.

Only the last two are reset paths; the first is the idle/stall sweep.

`snapshot_rtts` is an ordinary `&self` method: the `H2BlockConverter` is built
for one `kawa.prepare` call rather than held across the per-stream write loop,
so no borrow of `self.hpack` is outstanding at this call site. The call below
sits inside the `let stream = &mut context.streams[global_stream_id];` borrow
taken at the top of that loop (`lib/src/protocol/mux/h2.rs:2396`) and passes
`stream.linked_token()` straight out of it:

```rust lib/src/protocol/mux/h2.rs:2611
let (client_rtt, server_rtt) = self.snapshot_rtts(&endpoint, stream.linked_token());
```

This ensures `metrics.bin` and `metrics.bout` in the access log include the
stream's proportional share of connection overhead, and that the
TCP_INFO-derived `client_rtt` / `server_rtt` cells are populated from
the live frontend/backend sockets at emission time.
`snapshot_rtts` reads the peer side through `Endpoint::peer_rtt(token)`, which
returns an `Option<Duration>` already sampled by the embedder. It deliberately
does NOT return the socket: the predecessor,
`Endpoint::socket(token) -> Option<&TcpStream>`, handed out a concrete
`mio::net::TcpStream`, so any connection could reach any other connection's
socket for any purpose, and no in-memory transport could satisfy the trait.
RTT is intrinsically a live-socket property and stays on the embedder's side
of the boundary; the cores receive a value captured for them.


---

## Method Decomposition

The `ConnectionH2` implementation is decomposed into focused methods to manage
the complexity of the H2 state machine:

### readable() entry point

```rust lib/src/protocol/mux/h2.rs:2076-2080
pub fn readable<E, L>(&mut self, context: &mut Context<L>, mut endpoint: E) -> MuxResult
where
    E: Endpoint,
    L: ListenerHandler + L7ListenerHandler,
{
```

The read path is a **two-call protocol**, the read-side mirror of
`h2_transmit::gather` / `h2_transmit::confirm` on the write side. `readable()`
itself is only the caller that sits between the two halves, and its
`self.socket.socket_read` is the single socket touch on the whole H2 read
path:

1. `poll_read_target(context, endpoint)` runs the pass prelude — the
   `context.now` mirror, `prune_inactive_streams_while_closing`,
   `cancel_timed_out_streams`, the RFC 9113 §6.5 SETTINGS-ACK deadline — and
   then names the buffer that needs bytes. It answers
   `H2ReadTarget::Done(result)` when the pass ended with no read owed,
   `H2ReadTarget::Skip(stream_id)` when the frame in flight carries a
   zero-length payload, or `H2ReadTarget::Fill { stream_id, amount }`
   otherwise.
2. `readable()` turns a `Fill` into a buffer with `read_space`, which returns
   exactly `amount` bytes of the named buffer's free space — never the whole
   of it, because reading past the byte debt would fold the next frame's
   header into this frame's payload — and performs the read.
3. `handle_read(context, endpoint, stream_id, outcome)` is told what happened,
   as `H2ReadOutcome::Skipped` or
   `H2ReadOutcome::Filled { amount, size, status }`. It fills the buffer,
   settles the debt in `expect_read`, classifies the stall through
   `update_readiness_after_read`, and dispatches on `H2State`.

`Skip` is its own variant rather than a `Fill { amount: 0 }` on purpose. A
zero-length read answers `(0, SocketResult::Continue)`, and
`update_readiness_after_read` reads that as "nothing arrived, stop" — so every
frame carrying no payload (an empty SETTINGS, an empty DATA, a SETTINGS ACK)
would stop being parsed at all.

This is not `AsyncRead::poll_read`: nothing here is a future, `context` is the
mux's own `Context` and not a `task::Context`, and `lib/` holds no
asynchronous function. The names follow the sibling UDP core's `UdpManager::poll_output`
and `UdpManager::handle_input` (`lib/src/protocol/udp/manager.rs`). That
core's `Transmit` carries an owned `Vec<u8>`, which is the opposite of what
this path wants: H2 reads straight into `kawa.storage`, so the core offers a
borrowed view of live storage and is told the byte count afterwards.

`handle_read` dispatches based on `H2State`:

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

Parses CONTINUATION frame headers, validates stream ID continuity, and tracks
CONTINUATION flood counters (CVE-2024-27316). No longer touches
`Headers.header_block_fragment`'s length: the accumulated field-block bytes
live in `self.header_reassembly` (`h2_header_reassembly.rs`), appended by the
`(H2State::ContinuationFrame(headers), _)` match arm in `handle_read()` once
each CONTINUATION frame's payload has actually been read, not derived from a
`(start, len)` window into `zero.storage`.

### writable() entry point

```rust lib/src/protocol/mux/h2.rs:3220-3224
pub fn writable<E, L>(&mut self, context: &mut Context<L>, endpoint: E) -> MuxResult
where
    E: Endpoint,
    L: ListenerHandler + L7ListenerHandler,
{
```

1. Calls `flush_pending_control_frames()` as preamble
2. Dispatches based on `(H2State, Position)`:
   - Handshake states: serializes client preface, SETTINGS, connection WINDOW_UPDATE
   - Proxying states: delegates to `write_streams(context, endpoint)`

### flush_pending_control_frames()

Flushes control data before application frames, in order:

1. **Frontend-hung-up-while-draining cleanup**: if
   `frontend_hung_up_while_draining()` (`Position::Server`, draining, AND a
   HUP/ERROR readiness event — all three), clears `expect_write`, the
   pending WINDOW_UPDATE queue and
   `pending_rst_streams` unconditionally — the peer is gone, so nothing
   queued here will ever reach it — but clears `zero.storage` itself only
   when `!header_block_reassembly_in_progress()`, the same guard stages 4-6
   below already carry. This step's first version (sozu-proxy/sozu#1423)
   shipped this stage genuinely unguarded, reasoning that `Ready::HUP`
   means "no further bytes can ever arrive" — that reasoning is wrong:
   `Ready::HUP` is `is_read_closed() || is_write_closed()`
   (`command/src/ready.rs`), and mio documents `is_read_closed()` as true
   not only on a full close but also on a TCP half-close, where data the
   peer already sent can still be sitting, unread, in the kernel receive
   queue. `drive_frontend_shutdown_io` (`mod.rs`) force-calls `readable()`
   for H2 on every `shutting_down()` poll, so a CONTINUATION frame split
   across TCP segments landing a HUP event alongside its first segment is
   the ordinary soft-stop path. Review of this step's first version
   (`e1c3c2fb`) proved the unguarded version corrupts exactly that case
   (`StringDecodingError(NotEnoughOctets)` / decoder desync); this stage now
   shares the guard instead
2. **SETTINGS ACK timeout check**: If peer hasn't ACK'd within 5 seconds,
   sends GOAWAY(SETTINGS_TIMEOUT)
3. **Zero buffer resume**: If a previous control frame write was partial
   (WouldBlock), resume flushing via `flush_zero_to_socket()`
4. **Deferred initial GOAWAY**: `H2DrainState::take_deferred_initial_goaway`
   check-and-clears the deferred advisory (`graceful_goaway` deferred it — see
   below), and it is serialized via `ConnectionH2::send_initial_goaway`
5. **WINDOW_UPDATE frames**: `H2FlowControl::drain_window_updates_into`
   (`h2_flow_control.rs`) serializes every queued entry into the zero buffer
   and removes what it wrote — coalescing already happened at queue time
   (`queue_window_update`, keyed by stream ID; `0` is the connection-level
   entry). Drain order is the map's ascending stream-id order, deterministic
   across processes — see that module's doc comment
6. **Pending RST_STREAM frames**: Asks `H2ControlTx::drain_rst_streams_into`
   (`h2_control_tx.rs`) to serialize as many whole queued frames as fit into
   the zero buffer, with flood detection (`MAX_PENDING_RST_STREAMS` cap).
   A frame that would straddle the end of the buffer stays queued for the next
   pass. Proxy-emitted RSTs (DATA-on-closed, `refuse_stream_and_discard`,
   `reset_stream`, `cancel_timed_out_streams`) are queued via the canonical
   `ConnectionH2::enqueue_rst` helper, which delegates to
   `H2ControlTx::enqueue_rst` — dedupes through the wire-map's `rst_sent` set
   (`H2StreamTable`, `h2_stream_table.rs`), bumps the lifetime counter, and
   arms WRITABLE. The same `MAX_PENDING_RST_STREAMS` bounds the queue at the
   insert: once that queue holds 200 entries a further
   `enqueue_rst` queues nothing and emits `h2.rst_stream_dropped` plus an
   `error!` line, because the connection has by then already met the
   `total_rst_streams_queued >= MAX_PENDING_RST_STREAMS` half of the condition
   this stage escalates to `GOAWAY(ENHANCE_YOUR_CALM)` before it drains
   anything. The other half is the state gate
   `!matches!(self.state, H2State::GoAway | H2State::Error)`, which the drain
   below it does not share: after the first GOAWAY the queue can stay full
   without re-escalating, and a reap arriving then is dropped while the drain
   still serialises what is queued — bounded by `writable()`'s `GoAway` arm
   force-disconnecting on the same pass. The
   insert-side bound is what holds when one caller queues many RSTs in a
   single pass — `cancel_timed_out_streams` reaps the whole timed-out set
   without an intervening flush.
   This path is independent of the owning `Stream` still being
   present in the wire map, so it survives `remove_dead_stream` eviction
   (which the per-stream error callers invoke synchronously after
   `reset_stream` returns). `finalize_write` retains `Ready::WRITABLE`
   whenever the queue is non-empty so a partial write that deferred the
   flush (the RST_STREAM drain stage is gated on
   `stream_table.expect_write().is_none()`) re-runs on the next tick rather
   than stranding the queued RST.

Stages 1, 4, 5, and 6 all defer clearing/reusing `zero.storage` — leaving
their respective queue/flag untouched — while
`header_block_reassembly_in_progress()` is true (`self.state` is
`ContinuationHeader`/`ContinuationFrame`). Since the HEADERS+CONTINUATION
accumulator moved out of `zero.storage` into `self.header_reassembly`, what
all four stages protect is narrower than before the accumulator move: a
single CONTINUATION frame's own not-yet-fully-read payload (a partial
`socket_read()`), not the block's accumulated multi-frame history (which is
unreachable from any of these stages regardless of the guard, since none
names `self.header_reassembly`). That narrower window is still real on an
otherwise-healthy connection, where more bytes genuinely are still coming
— and, per stage 1's own history above, "the frontend already hung up"
does not reliably rule it out either. Nothing is lost — queuing a
WINDOW_UPDATE or RST_STREAM already arms `WRITABLE`, and `graceful_goaway`
arms it explicitly when it defers — the flush just waits for the block to
complete. Stage 4 is the one exception among the four where "nothing is
lost" isn't free: unlike the WINDOW_UPDATE/RST_STREAM queues, there is no
separate pending-GOAWAY queue to fall back on, so the
`initial_goaway_pending` flag itself is what guarantees the advisory GOAWAY
is still sent once reassembly completes rather than silently dropped.

Returns `Some(MuxResult)` if the caller should return early, `None` to proceed.

### write_streams()

The main data-plane write path:

1. Resumes any partially-written stream (`stream_table.expect_write()`)
2. Pre-computes `byte_totals` for overhead distribution
3. Opens a `H2ConverterPass` holding the reusable scratch and the pending
   RFC 7541 §6.3 size-update signal
4. Asks `H2Scheduler::begin_pass` for the pass order (urgency, then
   stream_id, then the rotated incremental tail) and its ready-incremental
   census
5. For each stream: converts kawa blocks to H2 frames, then writes them with
   the `h2_transmit` gather/confirm pair (below)
6. Recycles completed streams, distributes overhead, emits access logs
7. Cleans up `dead_streams` via `remove_dead_stream` (evicts the
   `H2StreamTable` wire mapping, `rst_sent`, and the activity/fc-stall
   caches together, plus the scheduler's priority entry via
   `H2Scheduler::remove_stream`)
8. Returns the scratch buffers to `self.hpack` and shrinks the three converter
   buffers if they grew beyond 16KB (`HpackState::shrink_converter_buffers`),
   then ends the pass with `H2Scheduler::end_pass`, which takes the order
   buffer back and commits the RFC 9218 §4 round-robin cursor

**How the converter borrow is scoped**: `H2BlockConverter` borrows the
connection's HPACK encoder out of `HpackState`. It is built for exactly ONE
`kawa.prepare()` call — not once per pass — so that borrow never spans the
priority loop and every `&self` / `&mut self` method stays callable inside it.
`H2ConverterPass` carries what has to cross from one stream's `prepare` to the
next: the three reusable scratch buffers (moved, never copied) and the
RFC 7541 §6.3 size-update signal, which belongs to the FIRST header block of
the pass and to no other. LIFECYCLE.md invariant 25 states both properties and
names the tests that pin them.

What is still deferred to after the loop — RST accounting via
`freshly_emitted_rsts`, stream retirement via `completed_streams` — is deferred
for ordering reasons, not borrow reasons: a MadeYouReset cap trip must not
preempt the remaining streams' writes, and `try_recycle_server_stream` passes
`is_last_stream` as `streams().len() == 1` — the branch that hands the whole
remaining overhead pool to one stream — so retiring inline would let a later
completer of the same pass drain that pool while other streams are still
live.

### finalize_write()

`write_streams` ends by asking `finalize_write` what the pass owes the next
tick. After the RFC 9113 §6.8 graceful-GOAWAY check (draining with every stream
gone), the rest is one decision taken in `h2_close::finalize_action` and
performed here:

| answer | what `finalize_write` does |
|---|---|
| `Flush` | rustls holds records and the pass wrote nothing: `socket_write(&[])`, then re-ask with `TlsFlushPhase::AfterFlush` |
| `SkipFlush` | rustls holds records but `socket_write_vectored` already attempted this pass's flush: go straight to the post-flush query |
| `Parked` | a partial write set `expect_write`: it owns the next tick, no bit moves |
| `RetainPendingBack` | LIFECYCLE §9 invariant 16: progress plus queued response bytes, so `Ready::WRITABLE` survives (the absence of the withdrawal below) |
| `ArmControlQueue` | a deferred RST_STREAM / WINDOW_UPDATE is still queued: `Readiness::arm_writable` |
| `Quiesce` | nothing owed anywhere: withdraw `Ready::WRITABLE` interest |
| `ReArm` | post-flush: records survived, re-arm the edge-triggered WRITABLE event |
| `Settled` | post-flush: the kernel took everything |

Two things about that list are worth knowing before changing it. The pre-flush
answers and the post-flush answers are disjoint, and the caller matches both
sets exhaustively with named-impossible arms rather than a wildcard — a `_` arm
would turn a new variant into a release-mode panic on the write path instead of
a compile error. And `socket_write(&[])`'s `(size, status)` is discarded here,
as it is at the close sites: the post-flush `socket_wants_write()` query is how
this path learns whether the flush landed, so no `SocketResult` reaches the
decision. `flush_zero_buffer()` is the one site on the write path that does
consume a status, through `update_readiness_after_write`.


### The gather/confirm pair (`h2_transmit.rs`)

`flush_stream_out` does not write a stream's bytes in one call. Each round:

1. `h2_transmit::gather(kawa, io_slices)` walks `kawa.out` up to the first
   `Delimiter` and pushes one `IoSlice` per `Store`, borrowed straight out of
   `kawa.storage` — no copy, and no caller-supplied buffer. It returns the
   byte count those descriptors describe.
2. The caller hands the descriptors to `socket_write_vectored`.
3. `h2_transmit::confirm(kawa, io_slices, size)` clears the descriptors and
   then advances the stream by the byte count the socket actually accepted.

The two-call shape is forced by the medium, not chosen for style: the
descriptors carry a `'static` lifetime they do not have (they point into
storage `Kawa::consume` may relocate via `ptr::copy`), so they must be dropped
before the consume — and the consume needs a number only the socket can
supply, because a TLS byte stream accepts partial writes. A `poll_transmit`
filling a caller-supplied buffer would add a copy this path does not make.
This is NOT `quinn-proto`'s fire-and-forget `poll_transmit`, which works only
because a QUIC datagram is all-or-nothing; there is no `poll_transmit` in this
repository at all, and the sibling UDP core's coarser `UdpManager::poll_output`
drains a manager-wide queue rather than one stream's `Kawa`.

A partial write is ordinary. `size` may be the whole offer, less than it, or
zero (`WouldBlock`); `update_readiness_after_write` classifies the last as
`FlushOutcome::Stalled` and ends the pass. Note that a stalled pass is not
fair to the streams it did not reach — see the module header and LIFECYCLE
invariant 26 for why the trailing urgency buckets are the ones that suffer.

### flush_zero_to_socket()

```rust lib/src/protocol/mux/h2.rs:4379
fn flush_zero_to_socket(&mut self) -> bool {
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

```rust lib/src/protocol/kawa_h1/editor.rs:264-265
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

At access log emission time (`Stream::generate_access_log`, in
`lib/src/protocol/mux/stream.rs`):

```rust lib/src/protocol/mux/stream.rs:588-591
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

All HPACK decode callbacks in `pkawa.rs` use fallible writes to kawa storage.
The helper that stores one regular header returns a typed rejection instead of
writing past the buffer:

```rust lib/src/protocol/mux/pkawa.rs:540-542
if kawa.storage.write_all(value).is_err() {
    return Err(RejectReason::OversizedPseudoValue);
}
```

This prevents buffer overflows when the decoded header block exceeds available
storage. `write_regular_header` returns `Err(RejectReason)` for each validity
rule it enforces; the caller passes it to `metric_reject`, which increments
`h2.headers.rejected.<reason>` via `reject_metric_key!`, and then sets the
`invalid_headers` flag. The flag is checked after decoding completes and
triggers a stream reset (`H2Error::ProtocolError`) rather than a connection
error.

### invalid_headers flag

`decode_headers_with_budget` owns the flag and returns its final value, so the
caller can decide between a stream error and a connection error. It is set
`true` for any of:

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

On receiving a SETTINGS ACK from the peer:

```rust lib/src/protocol/mux/h2.rs:5377-5379
self.hpack.set_decoder_max_allowed_table_size(
    self.local_settings.settings_header_table_size as usize,
);
```

On receiving the peer's own SETTINGS, in the `SETTINGS_HEADER_TABLE_SIZE` arm:

```rust lib/src/protocol/mux/h2.rs:5391-5397
parser::SETTINGS_HEADER_TABLE_SIZE => {
// Cap to the configured maximum — a malicious peer can
// advertise up to 4 GB to inflate HPACK encoder memory.
let cap = self.flood_detector.config().max_header_table_size();
let capped = v.min(cap);
self.peer_settings.settings_header_table_size = capped;
self.hpack.set_encoder_max_table_size(capped as usize);
```

The decoder's allowed table size matches what we advertised; the encoder's
table size matches what the peer advertised, capped to
`H2FloodConfig::max_header_table_size` so a peer cannot advertise up to 4 GB
and inflate encoder memory. This prevents desynchronization that would cause
`CompressionError` (GOAWAY).

Both coders live in `HpackState` (`hpack_state.rs`), whose fields are private
to that module, so neither is reachable as `self.decoder` / `self.encoder`
from `h2.rs` — every access goes through an accessor declared there.

Capping alone is not enough: the change has to reach the peer's decoder. The
arm therefore also sets `ConnectionH2::pending_table_size_update`, which
`H2BlockConverter::emit_pending_size_update_if_new_block` consumes to prepend
the RFC 7541 §6.3 `001xxxxx` dynamic-table-size-update directive to the next
header block this connection emits.

### Buffer shrinking after large headers

`converter_buf`, `lowercase_buf` and `cookie_buf` live in `HpackState`. A
write pass takes all three out by value, hands them to `H2ConverterPass`, and
`H2ConverterPass::converter` / `::reclaim` move them into and out of each
per-`prepare` `H2BlockConverter` — a `Vec` move, never a copy of the bytes.
The pass gives them back at the end, and `HpackState::shrink_converter_buffers`
then caps each one:

```rust lib/src/protocol/mux/hpack_state.rs:106-116
pub(super) fn shrink_converter_buffers(&mut self) {
    if self.converter_buf.capacity() > 16_384 {
        self.converter_buf.shrink_to(4096);
    }
    if self.lowercase_buf.capacity() > 16_384 {
        self.lowercase_buf.shrink_to(4096);
    }
    if self.cookie_buf.capacity() > 16_384 {
        self.cookie_buf.shrink_to(4096);
    }
}
```

This prevents a single request with abnormally large headers from permanently
inflating memory for the lifetime of the connection.

The scheduler's pass-order buffer is deliberately not in that list: it holds
one `StreamId` per active stream, not header bytes, so it is reclaimed on the
quiet-time path instead. `ConnectionH2::cancel_timed_out_streams` calls
`HpackState::reclaim_idle_buffers`, which tests the three converter buffers
independently, and `H2Scheduler::reclaim_idle_buffer`, which applies the same
guard to the order buffer — one `capacity() > retain_size * 4` guard each,
shrinking only those that individually exceed it.

---

## Testing

### Test inventory

The e2e suite lives in `e2e/src/tests/`, registered in that directory's
`mod.rs`. A per-file count is not reproduced here: it rots between releases and
nothing checks it. Count the current one with

```bash
grep -rc '^\s*#\[test\]' e2e/src/tests/
```

The files that carry H2 coverage, and what each is for:

| File | Focus |
|------|-------|
| `e2e/src/tests/tests.rs` | General HTTP proxying, keep-alive, routing, worker lifecycle |
| `e2e/src/tests/mod.rs` | Module registry plus the shared harness every suite imports: `setup_sync_test`, `setup_async_test`, `provide_port` (backed by `e2e/src/port_registry.rs`) |
| `e2e/src/tests/h2_tests.rs` | Protocol correctness: HEADERS, DATA, flow control, GOAWAY, stream lifecycle, priority, HPACK, concurrent streams, window updates, graceful shutdown, H2 backend behavior |
| `e2e/src/tests/h2_correctness_tests.rs` | Large-asset and wake-gap regressions — see the section below |
| `e2e/src/tests/h2_security_tests.rs` | Flood detection thresholds, rapid reset, CONTINUATION bombs, settings flood, empty DATA flood, glitch counting, malformed frame handling |
| `e2e/src/tests/h2_security_parser.rs` | Frame-parser and HPACK adversarial input |
| `e2e/src/tests/h2_security_header_injection.rs` | Pseudo-header ordering, CRLF/NUL injection, smuggling vectors |
| `e2e/src/tests/h2_security_session.rs` | Session-level abuse: idle timeouts, concurrency caps, back-pressure |
| `e2e/src/tests/h2_security_sni.rs` | SNI binding and certificate selection under H2 |
| `e2e/src/tests/h2_priority_rearm_tests.rs` | RFC 9218 urgency, the incremental round-robin, and readiness re-arm |
| `e2e/src/tests/h2_clock_tests.rs` | Clock-snapshot discipline (`ConnectionH2::now`) |
| `e2e/src/tests/h2_log_context_tests.rs` | `[session req cluster backend]` prefix on the H2 paths |
| `e2e/src/tests/mux_tests.rs` | Cross-protocol scenarios: H1-to-H2 backend, H2-to-H1 backend, end-to-end H2, mixed protocol combinations |
| `e2e/src/tests/h2_utils.rs` | Shared H2 client helpers (no tests of its own) |
| `e2e/src/tests/h1_security_tests.rs` | H1-specific security (request smuggling, header injection) |
| `e2e/src/tests/tls_tests.rs` | TLS handshake, ALPN negotiation, certificate handling, close semantics |
| `e2e/src/tests/tcp_tests.rs` | Raw TCP proxying |

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

### Large-asset H1→H2 regression coverage

Multi-MB chunked responses flowing from an H1 backend into an H2 frontend were
historically the hottest source of wake-gap / edge-triggered-readiness bugs on
this branch. The large-asset suite in `e2e/src/tests/h2_correctness_tests.rs`
locks those fixes in:

- `test_h2_php_apache_chunked_flush_drains_fully` — 312 KiB chunked body with
  per-chunk flush cadence exercising the peer-readiness re-arm in
  `ConnectionH1::readable` — its three `peer.arm_writable()` sites, cited by
  symbol because no single range covers them (`lib/src/protocol/mux/h1.rs`)
  (C1).
- `test_h2_slow_backend_idle_timeout_cancels` — 64 KiB chunked body streamed
  over 4 s, exercising the outbound refresh of `stream_last_activity_at` (C2).
- `test_h2_chunked_backend_crash_mid_stream_rsts` — verifies the chunked-EOF
  demotion to `ParsingPhase::Error` + `RST_STREAM(InternalError)` (C3).
- `test_h2_large_gzipped_chunked_drains_fully` — customer-shape regression
  guard from the cleverapps.io 2026-04 ticket: 7.76 MB deterministic payload,
  gzipped and chunked, served through the extended `ChunkedFlushH1Backend`
  with `Content-Encoding: gzip`. Client follows a Chromium-146 profile
  (SETTINGS with `INITIAL_WINDOW_SIZE=6_291_456`, one-shot
  `WINDOW_UPDATE(0, 15_663_105)`, per-stream `WINDOW_UPDATE(sid, 32 KiB)`
  cadence, `priority: u=3, i`). Asserts sha256 byte-identity of the gzipped
  wire body within `LARGE_BODY_DRAIN_BUDGET` (30 s; raised from an unvalidated
  8 s after a 2026-09-21 CI contention failure, sozu#1393 cause D). The
  streaming drain helper
  `drain_h2_stream_streaming` keeps memory linear in one frame (not one
  stream) by hashing DATA payloads as they arrive and only retaining an
  at-most-one-frame `carry` tail between reads.
- `test_h2_large_chunked_7mb_drains_fully` — scale-only companion. Same
  7.76 MB total, `b'Z'` fill, no gzip — isolates multi-MB drain + Chromium
  request shape + per-stream `WINDOW_UPDATE` cadence from content-encoding
  interactions.

`H2FloodDetector` caps stream-0 `WINDOW_UPDATE` frames at
`DEFAULT_MAX_WINDOW_UPDATE_STREAM0_PER_WINDOW = 100` per sliding window
(`lib/src/protocol/mux/h2_flood_detector.rs`, enforced by
`H2FloodDetector::check_flood`). The
drain helper refreshes per-stream windows only; the one-shot conn-level bump
during `h2_handshake_chromium_146` is the single stream-0 `WINDOW_UPDATE`
emitted during the test.

### Safety properties

The implementation maintains a zero-panic-path policy for the H2 read/write
paths. All fallible operations (frame parsing, HPACK decoding, buffer writes)
return errors that are converted to GOAWAY or RST_STREAM rather than panicking.
The compile-time assertion at the top of `h2.rs` guards against silent pointer
truncation on sub-32-bit platforms:

```rust lib/src/protocol/mux/h2.rs:23-26
const _: () = assert!(
    std::mem::size_of::<usize>() >= 4,
    "sozu requires at least 32-bit pointers"
);
```
