# H2 Mux Internals

A developer reference for the HTTP/2 multiplexer implementation.
For architecture overview and diagrams, see [architecture.md](./architecture.md).
For user-facing configuration, see [configure.md](./configure.md).

Source files covered by this document:

| File | Role |
|------|------|
| `lib/src/protocol/mux/h2.rs` | `ConnectionH2` struct, state machine, flow-control orchestration |
| `lib/src/protocol/mux/h2_flow_control.rs` | `H2FlowControl` — connection-level send window, receive-side byte accounting + pending WINDOW_UPDATE queue (RFC 9113 §6.9), closed API. There is no receive *window*: the advertised one is not enforced — see below |
| `lib/src/protocol/mux/h2_flood_detector.rs` | `H2FloodConfig`, `H2FloodViolation`, `H2FloodDetector` — CVE-2023-44487 / CVE-2024-27316 / CVE-2025-8671 flood/abuse detection, closed API |
| `lib/src/protocol/mux/pkawa.rs` | HPACK decoding, pseudo-header validation, RFC 9218 priority parsing |
| `lib/src/protocol/mux/hpack/` | The sans-io RFC 7541 codec: `Decoder`, `Encoder`, dynamic table, Huffman state machine computed at compile time |
| `lib/src/protocol/mux/mod.rs` | Mux session, Stream, Router, ready() loop, stream lifecycle |
| `lib/src/protocol/mux/converter.rs` | Kawa-to-H2 frame encoding (`H2BlockConverter`) |
| `lib/src/protocol/mux/parser.rs` | H2 binary frame parser (nom) |
| `lib/src/protocol/mux/serializer.rs` | H2 frame serializer (SETTINGS, GOAWAY, RST_STREAM) |

---

## ConnectionH2 Sub-structures

`ConnectionH2` is the central H2 connection type. It is **not** generic: it
holds no socket and names no `SocketHandler`. The socket lives one layer out,
in `H2Shell`, which is what `Connection::H2` holds:

```
H2Shell<Front: SocketHandler>
 |
 |-- socket: Front                          // TLS or TCP socket
 |-- core: ConnectionH2                     // everything below
```

`Connection<Front: SocketHandler>` keeps its bound and `Router::backends` keeps
its declared type: the parameter did not leave the mux, it moved off the state
machine. `ConnectionH2`'s own fields are decomposed into focused
sub-structures to separate concerns:

```
ConnectionH2
 |
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
 |                                         // graceful_shutdown_deadline
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
 |                                         // incremental, per-bucket incremental_cursor) and the
 |                                         // reusable write-pass order buffer
 |
 |-- hpack: HpackState                      // Closed API (hpack_state.rs, private fields):
 |                                         // decoder, encoder, and the reusable
 |                                         // converter_buf / lowercase_buf / cookie_buf
 |                                         // scratch buffers. All three buffers come from
 |                                         // the connection's BufferSource, so HpackState::new
 |                                         // is fallible (buffer_source.rs)
 |-- local_settings: H2Settings             // Settings we advertise
 |-- peer_settings: H2Settings              // Settings the peer advertised
 |-- stream_table: H2StreamTable             // Closed API (h2_stream_table.rs, private fields):
 |                                         // streams: BTreeMap<StreamId, GlobalStreamId>,
 |                                         // highest_peer_stream_id, expect_read, expect_write,
 |                                         // rst_sent, and the per-stream activity/fc-stall maps
 |-- pending_table_size_update: Option<u32> // RFC 7541 s6.3 directive owed to the peer
 |-- control_tx: H2ControlTx                // Closed API (h2_control_tx.rs, private fields):
 |                                         // pending_rst_streams: Vec<(StreamId, H2Error)>,
 |                                         // total_rst_streams_queued (never-decaying, behind the
 |                                         // CVE-2025-8671 cap) and max_pending
 |-- settings_sent_at: Option<Instant>      // SETTINGS ACK timeout tracking
 |-- zero: GenericHttpStream                // Per-frame read landing zone: frame headers and
 |                                         // stream-0 payloads. Input only since #1604, and
 |                                         // not a HEADERS+CONTINUATION reassembly buffer —
 |                                         // see header_reassembly below
 |-- output: H2Output                       // The ordered output queue (h2_output.rs): every
 |                                         // control frame and the rest of the one stream
 |                                         // frame a partial write cut, each queued whole
 |-- reads_wait_for_output: bool           // READABLE withdrawn at the output read cap
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
plus the per-urgency-bucket round-robin `incremental_cursor`.

**`Prioriser` methods:**

| Method | Signature | Behavior |
|--------|-----------|----------|
| `push_priority` | `(&mut self, StreamId, PriorityPart) -> bool` | Inserts/updates priority. Returns `true` on self-dependency (protocol error). Clamps urgency to 0-7. Ignores deprecated RFC 7540 tree priorities. |
| `push_priority_guarded` | `(&mut self, StreamId, PriorityPart, StreamId, &BTreeMap<StreamId, GlobalStreamId>) -> bool` | Same, behind the open-stream / idle-look-ahead filter that bounds a PRIORITY flood. |
| `get` | `(&self, &StreamId) -> (u8, bool)` | Returns `(urgency, incremental)`. Defaults to `(3, false)` if absent. |
| `remove` | `(&mut self, &StreamId)` | Removes entry at stream cleanup. |
| `apply_incremental_rotation` | `(&self, &mut [StreamId]) -> usize` | Inside each urgency bucket, moves incremental streams to the tail and rotates that tail past **that bucket's own** entry in `incremental_cursor`. Returns the incremental count. |
| `advance_incremental_cursor` | `(&mut self, &[Option<StreamId>; 8])` | Commits each bucket's own pass leader as that bucket's next cursor. A `None` entry leaves its bucket's cursor alone, so an all-`None` census is a whole no-op. |

**`H2Scheduler` methods** — `priority`, `push_priority`,
`push_priority_guarded`, `remove_stream` and `prioriser_mut` delegate to the
`Prioriser` above; the pass API is:

| Method | Signature | Behavior |
|--------|-----------|----------|
| `begin_pass` | `(&mut self, impl IntoIterator<Item = StreamId>, impl FnMut(StreamId) -> bool) -> (Vec<StreamId>, ReadyIncrementalCensus)` | Orders one write pass and takes its same-urgency ready-incremental census. The closure is the caller's readiness projection — the one fact the scheduler does not own — and is called for incremental streams only. |
| `end_pass` | `(&mut self, Vec<StreamId>, ReadyIncrementalCensus)` | Takes the order buffer back and commits every urgency bucket's round-robin cursor. |
| `reclaim_idle_buffer` | `(&mut self, usize)` | Quiet-time shrink of the order buffer, beside `HpackState::reclaim_idle_buffers`. |

`ReadyIncrementalCensus` is a fixed `[usize; 8]` (RFC 9218 §4.1 urgency is
`[0, 7]`) plus a matching `[Option<StreamId>; 8]` of per-bucket pass leaders.
`write_streams` reads `incremental_peer_count(urgency)` per stream, calls
`note_ineligible` at the three mid-pass transitions of LIFECYCLE.md invariant
17, `note_fired(urgency, ...)` when a stream consumes window, and
`ready_total` for the `h2.streams.ready_incremental.by_urgency` gauge.
`note_fired` takes the urgency because that is what attributes the firing
stream to a bucket: without it the census can name one leader for the whole
connection, and only the bucket that supplied it ever rotates
(sozu-proxy/sozu#1456).

### parse_rfc9218_priority()

Located in `pkawa.rs`, this function parses the `priority` HTTP header value:

```rust lib/src/protocol/mux/pkawa.rs:679
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
ones, and each bucket's incremental tail is rotated past that bucket's own
entry in `incremental_cursor` so same-urgency incremental downloads take the
lead in turn — one position per pass, in every populated bucket, which is
LIFECYCLE.md invariant 26's starvation bound.

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
| `initial_connection_window` | 1048576 (1MB) | Connection receive window **advertised** to the peer (RFC 9113 §6.9.2), clamped to [65535, 2^31-1]. Not enforced on inbound DATA — see below |
| `max_concurrent_streams` | 100 | `SETTINGS_MAX_CONCURRENT_STREAMS`, also sizes the pending WINDOW_UPDATE cap |
| `stream_shrink_ratio` | 2 | Stream Vec shrink threshold: `total > active * ratio`, minimum 2 |

This allows operators to tune both security and performance per listener.

### The advertised connection-level receive window is not enforced

`initial_connection_window` is advertised, not enforced, and the distinction is
the whole of [sozu-proxy/sozu#1488](https://github.com/sozu-proxy/sozu/issues/1488).

**What the peer sees.** A peer's connection-level send allowance is RFC 9113
§6.9.2's fixed 65535-octet initial value — no SETTINGS parameter can change the
connection-level window; `SETTINGS_INITIAL_WINDOW_SIZE` sizes the *per-stream*
one — plus every stream-0 `WINDOW_UPDATE` Sōzu sends. Those are the one-shot
enlargement from 65535 to `initial_connection_window` — serialised by
`ConnectionH2::readable`'s `(H2State::ClientSettings, Position::Server)` arm
into the same output flush as the server SETTINGS and the ACK of the client's,
so a frontend connection's whole server preface costs one `writev(2)`, and
queued by `ConnectionH2::handle_settings_frame` on a backend one — followed by
the periodic grants back. So the peer reads a number, and the
number is an invitation to send.

**What it is not.** It is not a ceiling anything checks. On this side the
configured value governs exactly two things: the size of that one-shot
enlargement, and how often credit is returned —
`ConnectionH2::handle_data_frame` passes `initial_connection_window / 2` as the
grant-back threshold to `H2FlowControl::account_received_bytes`.
`H2FlowControl::window` is the **send** window, peer-granted credit for our own
writes; `account_received_bytes` is a counter that only accumulates. No state is
decremented by an inbound DATA frame, so none can go negative, and no
connection-level `FLOW_CONTROL_ERROR` is ever raised. Measured: against **98303
octets advertised** (65535 plus one 32768 grant), **106496 octets of DATA were
accepted**, with no GOAWAY and no `FLOW_CONTROL_ERROR`.

**What the real boundary is.** Memory, bounded by the buffer pool. Every buffer
a stream needs comes from the caller-supplied `BufferSource`
(`lib/src/protocol/mux/buffer_source.rs`), which may refuse; a refusal degrades
one stream with `RST_STREAM(REFUSED_STREAM)` and never the connection. That
module's doc is the contract — read it there rather than a restatement. The pool
bounds the memory flow control exists to protect, and does so without
per-connection credit bookkeeping. This is a defensible boundary; advertising a
number nothing enforces is the part that is not.

**The consequence, unsoftened.** A peer that trusts the advertisement has no way
to discover the real limit — nothing on the wire reports the pool's remaining
capacity — and RFC 9113 §6.9.1 makes enforcement a MUST, so a conformance suite
will flag this as a §6.9.1 violation. #1488 offered two defensible resolutions,
enforce or document, and the decision was to document. Closing the gap means
tracking outstanding inbound credit and emitting `FLOW_CONTROL_ERROR`, which
changes observable behaviour and belongs to its own changeset. Until then, do
not describe this window as enforced, and do not write an assertion that the
connection window is never overcommitted: `doc/testing.md` records why the H2
simulator deliberately carries no such property.
`lib/src/protocol/mux/h2_flow_control.rs`'s module doc carries the same
statement beside the code.

### Prepared DATA dropped unsent gives its send credit back

The send windows are debited when DATA is **prepared**, not when it is
written: `ConnectionH2::poll_write_target`'s `H2WritePhase::Prepare` arm
encodes up to `min(stream window, connection window)` octets of DATA into the
stream's `kawa.out`, then debits both windows by what it encoded
(`H2FlowControl::consume_send_window`). The converter needs the window as a
budget at that moment, so the debit cannot simply move to the write. RFC 9113
§6.9 counts only the DATA that was sent, though, and the peer returns credit
through WINDOW_UPDATE only for what it received. DATA prepared and then
dropped unsent therefore used to shrink the connection window for the life of
the connection, one drop at a time, until every stream waited on credit that
would never come
([#1641](https://github.com/sozu-proxy/sozu/issues/1641)). A backend
connection belongs to one `Mux` session, so the damage stayed within that
session, but a long-lived one could still starve.

The repair gives back what will never be sent. Only the stream parked in
`expect_write` holds unsent frames (the argument of the section on dropped
header blocks), so where a pass parks a stream it records
`ConnectionH2::parked_data`, the DATA payload `kawa.out` still holds whole
(`h2_transmit::queued_data_payload`, the same frame walk as
`h2_transmit::holds_header_frame`, on the same park, with no allocation). The
rest of a frame a partial write cut is not in `kawa.out` any more: the ordered
output queue adopted it and sends it whole, so it is not counted. Then:

- `ConnectionH2::remove_dead_stream`, reached by every removal (peer
  RST_STREAM, `end_stream`, expiry, `prune_inactive_streams_while_closing`),
  gives the parked credit back to the connection window when it removes the
  parked stream, before `H2StreamTable::remove` clears the park, and re-arms
  WRITABLE when that reopens a closed window;
- the next pass's `H2WritePhase::Start` gives back, to the stream's window and
  the connection's, the part of a live park's DATA that is no longer queued
  (`forcefully_terminate_answer` or a default answer cleared it).

Both go through `ConnectionH2::refund_parked_data` and
`H2FlowControl::refund_send_window`, which asserts that the window grows by
exactly the refund. Giving back too little is the safe side: a queue that
ends inside a DATA frame does not count that frame. The refund can still
overflow, and that is the peer's doing: WINDOW_UPDATEs sent while the DATA
sat parked are each accepted on their own, yet the window they really grant
is the current one plus the unsent octets. When that sum passes 2^31-1,
`refund_send_window` answers `ApplyWindowUpdateOutcome::Overflow` without
touching the window, and the connection sends GOAWAY(FLOW_CONTROL_ERROR),
exactly as `ConnectionH2::handle_window_update_frame` does for an
overflowing increment (RFC 9113 §6.9.1). Nothing asserts on it.

This is the shape of hyperium/h2, whose `Prioritize` reserves capacity when it
assigns it, debits the windows when a frame leaves the stream's queue for the
codec buffer, and reclaims reserved but unsent capacity on a reset
(`Prioritize::reclaim_reserved_capacity`). HAProxy debits `h2s->sws` and
`h2c->mws` when it writes a DATA frame into the connection's `mbuf`, which a
stream never drops, so it has nothing to give back.

---

## Overhead Distribution

### Problem

In HTTP/2, connection-level frames (SETTINGS, PING, WINDOW_UPDATE, GOAWAY,
SETTINGS ACK) consume bandwidth but don't belong to any specific stream.
For accurate per-stream byte accounting in access logs and metrics, this overhead
must be attributed proportionally.

### distribute_overhead()

A **free function**, not a method:

```rust lib/src/protocol/mux/h2.rs:473-481
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
- **`ConnectionH2::consume_output()`**: Every byte written from the output
  queue (connection-level frames) increments `bytes.overhead_bout` — except
  the adopted rest of a stream frame, which `handle_write` already counted to
  its stream when it adopted it (the stream may be gone by the time it is
  written).

### How it feeds into SessionMetrics

At stream completion the overhead is distributed to the stream's
`SessionMetrics` before the access log is emitted. On the normal completion
path that happens inside `ConnectionH2::try_recycle_server_stream`, which calls
the free function directly rather than through the `&mut self` wrapper — a
spelling choice, not a constraint, since the wrapper would credit the same
shares at this site:

```rust lib/src/protocol/mux/h2.rs:4714-4727
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
the log and returns the metric events its caller must record — the method is
`#[must_use]`, and `complete_server_stream` is static, so it propagates them
through its own return rather than queueing them:

This one keeps a line rather than a symbol: `generate_access_log` has four call
sites in `h2.rs` and the paragraph below is about this call's arguments, not the
method.

```rust lib/src/protocol/mux/h2.rs:4762-4768
let events = stream.generate_access_log(
    false,
    Some("H2::Complete"),
    listener,
    client_rtt,
    server_rtt,
);
```

The other three sites take the `&mut self` wrapper
`ConnectionH2::distribute_overhead` instead, and each emits its own log:

- `cancel_timed_out_streams` (`lib/src/protocol/mux/h2.rs`) passes a
  `reason` variable, one of `H2::WindowStall` or `H2::IdleTimeout`, and counts
  the reap under a different metric for each so a DoS-mitigation reap stays
  distinguishable from an ordinary idle one.
- `ConnectionH2::handle_rst_stream_frame` (`lib/src/protocol/mux/h2.rs`) uses
  `H2::ResetFrame`.
- `ConnectionH2::reset_stream` (`lib/src/protocol/mux/h2.rs`) uses
  `H2::Reset`.

Only the last two are reset paths; the first is the idle/stall sweep.

`snapshot_rtts` is an ordinary `&self` method: the `H2BlockConverter` is built
for one `kawa.prepare` call rather than held across the per-stream write loop,
so no borrow of `self.hpack` is outstanding at this call site. The call below
sits inside the `let stream = &mut context.streams[global_stream_id];` borrow
taken at the top of `H2WritePhase::Flush`'s post-flush tail
(`ConnectionH2::poll_write_target`, `lib/src/protocol/mux/h2.rs`) and passes `stream.linked_token()` straight
out of it:

```rust lib/src/protocol/mux/h2.rs:3483-3484
                        let (client_rtt, server_rtt) =
                            self.snapshot_rtts(endpoint, stream.linked_token());
```

This ensures `metrics.bin` and `metrics.bout` in the access log include the
stream's proportional share of connection overhead, and that the
TCP_INFO-derived `client_rtt` / `server_rtt` cells are populated.

Neither cell is read from a socket by this file, and both are asked of the
endpoint at the moment the stream is logged:

- `client_rtt` is `Endpoint::local_rtt()`. `H2Shell` hands the core a
  `ShellEndpoint` that answers it from the shell's own socket, sampling at the
  first stream logged on the connection and reusing that sample for every
  later stream, so every stream of a connection reports one identical number
  and a connection that logs nothing costs no syscall (issue #1590, then the
  per-connection sample). The operator-visible half of this is stated in
  `doc/configure.md`'s "When each cell is measured".
- `server_rtt` is asked per call, through `Endpoint::peer_rtt(token)`, which
  returns an `Option<Duration>` already sampled by the embedder — the backend
  connection's one sample, taken by `Connection::rtt` at its first ask. It
  deliberately does NOT return the socket: the predecessor,
  `Endpoint::socket(token) -> Option<&TcpStream>`, handed out a concrete
  `mio::net::TcpStream`, so any connection could reach any other connection's
  socket for any purpose, and no in-memory transport could satisfy the trait.

RTT is intrinsically a live-socket property and stays on the embedder's side
of the boundary in both directions; the cores receive a value captured for
them through a trait method.


---

## Method Decomposition

The `ConnectionH2` implementation is decomposed into focused methods to manage
the complexity of the H2 state machine:

### The three TLS seams

Everything the H2 connection asks of the TLS layer goes through exactly three
private methods, and nothing else in the file spells the underlying
`SocketHandler` call:

| seam | question or action |
|---|---|
| `H2Shell::tls_wants_write` | does the TLS layer still hold encrypted records it must push? |
| `H2Shell::flush_tls_records` | push whatever it holds, offering no new application bytes |
| `H2Shell::begin_tls_close` | start the `close_notify` handshake |

**All three now live on `H2Shell`, and `ConnectionH2` calls none of them.**
That is the whole of what the seams were for: they were the three inputs and
actions a byte-in / byte-out core has to receive from, or hand back to, the
I/O shell that owns the socket, and the shell is the layer their bodies have
moved to. The call-site counts that used to sit in this table were the measure
of an extraction still in progress; the extraction is done, so the measure is
now simply that the core has none.

The name of the underlying trait method says *socket*, and that is what made
this read as an I/O question for as long as it was spelled out at every site.
It is a question about a buffer that happens to live behind the socket:
`FrontRustls` is the only production `SocketHandler` that overrides it, the
trait's default answer is `false`, and `Router::backends` is declared
`Connection<SessionTcpStream>` whatever the frontend is — so every **backend**
H2 connection resolves every one of these queries statically to `false`.

Two properties of the set matter when changing it. `tls_wants_write` is free of
side effects, so a caller that asks twice around a `flush_tls_records` is asking
two genuinely different questions — "does rustls hold records" and "did the
kernel take them" — which is what `h2_close::TlsFlushPhase` names, while a
caller that asks twice with nothing in between should bind the answer once
instead. And `flush_tls_records` is the only *action* of the three, so its call
sites are exactly the places where the extraction had to invert control rather
than pass a value in.

One composite is built on the query and has a name of its own:
`ConnectionH2::ensure_tls_flushed` is "re-arm the edge-triggered WRITABLE event
if the answer is yes". It takes that answer as a `tls_wants_write` parameter and
does not ask — the query belongs to the caller, which is why this one stayed in
the core when the three seams left. Under edge-triggered epoll a connection
whose bytes are stuck in rustls rather than in the kernel has no other wake-up,
so every site that parks output must end with it. It is called, never spelled
out: a site that writes the body again is the same fact written twice.

### The shell, and what crosses it

`H2Shell<Front: SocketHandler> { core: ConnectionH2, socket: Front }` is what
`Connection::H2` holds. Both halves live in `h2.rs`, so the shell reaches the
core's private fields exactly as the methods it inherited used to — **nothing
was widened to `pub` to let the socket keep working**. The `pub` surface that
did grow exists for a driver outside this crate.

What moved to the shell is everything that touches `Front`: the three seams,
`readable`, `writable`, `write_streams`, `flush_output_to_socket`,
`flush_output_buffer`, `initiate_close_notify`, `has_pending_write`,
`has_pending_write_full` and `close`. What stayed is the state machine.

Three core steps had to change shape to stay in the core, because each one
used to reach the socket from somewhere the shell cannot stand:

- **`ConnectionH2::flush_pending_control_frames`** used to call
  `flush_zero_to_socket` at three points. It answers
  `H2ControlFlushTarget::FlushOutput`, the caller moves the bytes of
  `ConnectionH2::output_pending`, and `ConnectionH2::handle_control_flush`
  turns the caller's `stalled` answer into `Stalled` or `Proceed`. Since #1604
  there is one flush, at the end of the walk: every stage appends whole
  frames to the one ordered output queue, so no stage waits for another's
  bytes to leave a shared buffer, and the per-stage continuations (a READABLE
  re-enable, an `expect_write = Zero` re-arm) are gone with the buffer they
  protected.
- **`ConnectionH2::close`** hands `&mut self.socket` whole to
  `shared::drain_tls_close_notify`, which runs its own drain loop on the far
  side. It moved to the shell entire; `ConnectionH1::close` shares that helper
  byte-for-byte and is untouched.
- **`ConnectionH2::force_disconnect`** is the one that could not simply move.
  See below.

### Inverting `force_disconnect`

`force_disconnect` read `tls_wants_write` itself, and it is called from
seventeen sites in the core — seven of them inside `ConnectionH2::handle_read`,
whose `pub` signature is socket-free by construction and does not change. So
neither moving it nor threading the answer through its callers was available:
the first takes the socket to `handle_read`, the second takes a socket-derived
`bool` into every frame handler by way of `ConnectionH2::goaway` and its
thirty-one call sites.

It is inverted instead, in the same poll/handle grammar as the rest of the
write path, with one deliberate difference. `H2FinalizeTarget`,
`H2ControlFlushTarget` and `H2WritableStateTarget` are all *returned* by the
step that raises them. `H2ForceDisconnectTarget` cannot be: `handle_read`
returns `MuxResult`. So it is parked on the connection, taken by
`ConnectionH2::poll_force_disconnect`, and settled by
`ConnectionH2::force_disconnect_after_query` with the answer the shell read.

What the core settles at the raising site is everything that needs no socket:
`H2State::Error`, and on a `Position::Client` connection the backend status and
the `HUP` readiness event. What it defers is the `h2_close::force_disconnect_action`
decision, the `ReArmAndContinue` readiness write, and the three `debug!` lines
that render `wants_write=`. `peer_gone_after_final_goaway()` is computed at the
raising site and **carried** in the target rather than re-read at settlement:
it reads `stream_table` and the output queue, and `ConnectionH2::remove_dead_stream`
runs between raise and settlement on the `ConnectionH2::reset_stream` paths.

The provisional answer is exact for every case but one. `Position::Client`
always continued; a server whose peer is gone always closes, because the
decision function answers `CloseSession` on `peer_gone` whatever the socket
says. Only a server with a live peer turns on the query, and it answers
`Continue` until settled — the conservative direction, since a session held one
pass too long is recoverable and a session closed with records pending is the
truncation that decision function exists to prevent.

`H2Shell::settled` is the single settlement point, and it *takes* the parked
target, so a double settlement is impossible. Every `MuxResult` a core step
hands the shell passes through it, which makes a missed settlement greppable
rather than asserted: a core call in the shell impl whose type is `MuxResult`
and which is not an argument of `settled` is the bug. `H2Shell::settle` is the
same for the steps whose own answer is not a `MuxResult` —
`ConnectionH2::start_stream` answers a `bool` and
`ConnectionH2::cancel_timed_out_streams` answers nothing, and both can reach
`force_disconnect`. Those settle immediately rather than at the next entry
point, because `Mux` reads `Readiness::filter_interest` between entry points to
decide whether a connection gets a pass at all.

**One behaviour change, named.** Two core sites discard the `MuxResult` that
can carry a force-disconnect: `ConnectionH2::cancel_timed_out_streams` does
`let _ = self.enqueue_rst(..)` and `ConnectionH2::start_stream` discards
`graceful_goaway`'s. A `CloseSession` raised there used to be swallowed and the
connection closed on a later pass through the `H2State::Error` arm; it now
propagates on this pass. Both are reachable only through a `gen_goaway`
serialisation failure — `ConnectionH2::goaway` and
`ConnectionH2::send_initial_goaway` reach `force_disconnect` on that arm alone.

A carried `tls_wants_write` field refreshed by the shell after every socket
call was considered and rejected: it is less code, but it puts an ambient
socket fact back inside the core, which is the thing this series exists to
remove, and its correctness rests on a refresh discipline rather than on a
shape.

### What the `Debug` impl renders

`ConnectionH2`'s `Debug` does not name the socket. It renders `peer_address`,
the address the connection snapshots once at construction and that every
`log_context!` line already carries, where it used to hand
`mio::net::TcpStream`'s own `Debug` a `socket` field through
`SocketHandler::socket_ref` — a local address, a peer address and a file
descriptor.

Three consequences, in the order they matter:

- It keeps the address a reader of a `Debug` line wants and drops the
  descriptor, which named nothing outside this process.
- It survives the peer's reset. A live `getpeername(2)` answers `ENOTCONN`
  there, which is exactly when an operator reads the line; the snapshot does
  not.
- It is a value a byte-in / byte-out core can produce at all. `socket_ref`
  returns a concrete OS type no in-memory transport can synthesise, which is
  why the H2 simulator used to carry a connected loopback stream it never read
  or wrote. It no longer implements `SocketHandler` at all, so that placeholder
  and the `mio` dev-dependency it forced are both gone.

`ConnectionH2` requires **nothing** of `SocketHandler`: it does not name the
trait and has no `Front` parameter. `H2Shell` requires `socket_read`,
`socket_write`, `socket_write_vectored`, `socket_wants_write`, `socket_close`,
`socket_write_then_close` and `peer_addr` — and `socket_ref` only through `Connection::socket`, which the
event loop needs for registration. `H2Shell`'s own `Debug` is hand-written
rather than derived for exactly the reason this section gives: a derive would
render `socket` through `Front`'s `Debug` and hand the descriptor straight back
through the wrapper.

`ConnectionH1`'s `Debug` renders the same slot, for the first two of those
reasons. It was the last `Debug` in `protocol/mux/` still handing
`socket_ref`'s `mio::net::TcpStream` to that type's own `Debug`, so one
connection rendered two different peers depending on which struct a trace
carried: the H2 one the snapshot, the H1 one a live `getpeername(2)` taken at
format time — which answers `ENOTCONN` once the peer has reset — alongside a
file descriptor. The third reason does not transfer. `ConnectionH1` is still
generic over `Front: SocketHandler`, and the three `sample_rtt` reads (the
counted wrapper over `stats::socket_rtt` in `protocol/mux/mod.rs`) in
`ConnectionH1::writable` still reach `socket_ref`; those read a round-trip
time rather than an address, and they stay. Pinned by
`debug_renders_the_proxy_advertised_peer_not_the_transport_socket`
(`lib/src/protocol/mux/h1.rs`).

### What the RTT read cost to remove

Q11's local half is the step that removed it. It made `ConnectionH2::client_rtt`
a carried field that `Mux::refresh_client_rtt` wrote once per pass and
`snapshot_rtts` read — a shape issue #1590 has since replaced, see the next
section. Three shapes were on the table:

- **Per entry point** — mirror the value in wherever `ConnectionH2.now` is
  mirrored from `Context::now`. Rejected on measurement: 7.8–9.7× the syscall
  rate, +6.5% and +5.3% CPU.
- **Per readiness sweep** — one sample at the top of each `Mux` pass. Chosen.
- **Access log as an output** — the core emits the event with the RTT slots
  empty and the shell fills them in. Free, and where this ends up, but a
  larger change than one step.

The chosen shape is cheaper than the rejected one and **more expensive than
the per-recycle read it replaced**, which is worth stating plainly because it
is the opposite of what "sample once and reuse" sounds like. A sample per
sweep beats a sample per stream only when more than one stream recycles per
sweep, and at the concurrency the e2e suite drives, it does not.

Counted by marking `stats::socket_info` and `Mux::refresh_client_rtt` and
running each test three times at a 1-minute load average of 3.5 (the
`getsockopt(TCP_INFO)` count before the change is exactly the stream-recycle
count, which is what the old one-per-recycle read means):

| workload                                            | stream recycles | `client_rtt` syscalls before | after     | ratio | all `TCP_INFO` before | after     | ratio |
| --------------------------------------------------- | --------------- | ---------------------------- | --------- | ----- | --------------------- | --------- | ----- |
| `test_h2_concurrent_streams` (10 × 5 streams)       | 50              | 50                           | 140       | 2.80× | 110                   | 200       | 1.82× |
| `test_h2_50_concurrent_streams_no_crosstalk` (5 × 50) | 250           | 250                          | 483       | 1.93× | 505                   | 738       | 1.46× |

The before-counts are identical across all three runs because they track
stream completions; the after-counts vary by a few percent because they track
readiness events, which is the second-order consequence — **the syscall rate is
now a function of event-loop wake-ups rather than of work completed.** The
ratio improves as multiplexing rises (0.36 recycles per sweep in the first
workload, 0.52 in the second, where 1.0 is break-even), so a busy connection
amortises the sample the way the shape intends and a quiet one does not. The
maintainer reviewed these numbers and kept the shape as measured.

`Endpoint` cannot supply the local value lazily, because it is the other side
of the connection; and once the core may not touch a socket, the shell has to
sample eagerly without knowing whether any stream will finish in the pass.
Paying that is the price of the core no longer holding an OS handle. The
access-log-as-output shape is what removes the cost entirely, and it can
supersede this step without rework.

### Making the read lazy (issue #1590)

The paragraph above has since been answered without the access-log-as-output
shape. `Endpoint` gained `local_rtt`, and `H2Shell` hands its core a
`ShellEndpoint` — built per core call from disjoint borrows of the shell's own
`core`, `socket` and `local_rtt` fields — that answers it from the shell's
socket. The core still holds no OS handle; it asks, the way it already asked
for `server_rtt`, and it asks only in `snapshot_rtts`, that is when it logs a
stream. The first ask of a pass samples and stores into `H2Shell::local_rtt`,
later asks in the pass reuse the value, and `Mux::expire_client_rtt` forgets it
at the top of the next pass without a syscall. `Mux::close` samples only when
its teardown sweep finds a stream to log. HAProxy works the same way: it reads
`TCP_INFO` in `tcp_get_info` (`src/proto_tcp.c`) only when a `fc_rtt` /
`bc_rtt` sample fetch (`src/tcp_sample.c`) is evaluated, never per event.

Nothing periodic needed the eager sample: the access log is the only consumer
of `TCP_INFO` in the tree, so the read now happens once per log line at most.

Measured on a release build (`--no-default-features --features
crypto-ring,opentelemetry,splice,simd`), one worker, python `http.server`
backend on loopback, `curl`, 20 requests, `LD_PRELOAD` interposer counting
`getsockopt(SOL_TCP, TCP_INFO)` with a backtrace per call, 1-minute load
average 2–8. `intentrace -p` on the same scenarios agrees on the direction
(H1 2.00 → 1.00, H2 6.00 → 1.00, multiplexed 3.15 → 1.00 in its aligned
window).

| scenario                                   | `TCP_INFO` per request before (`e49d78ab`) | after |
| ------------------------------------------ | ------------------------------------------ | ----- |
| H1, 20 sequential connections              | 2.80                                       | 1.85  |
| H2, 20 sequential connections              | 7.85                                       | 1.85  |
| H2, 20 streams multiplexed on 1 connection | 5.60                                       | 1.80  |

Every remaining read is an access-log cell: one `client_rtt` per request and one
`server_rtt` per request whose backend connection is still in the router when
the line is written. The python backend closes its connection after each
response, so a few `server_rtt` cells are `-` on both sides of the change and
that read never happens; the per-request figure is therefore just under 2. The
reads removed are the per-pass samples (5.95 per H2 request, one of them the
first `ready()` after the TLS handshake re-enters the new `Mux` from
`HttpsSession::ready`) and the unconditional one in `Mux::close` (one per
connection, H1 and H2); in their place each H2 request pays the one lazy
`client_rtt` read its log line needs.

### One sample per connection

The lazy read above still paid one `client_rtt` and one `server_rtt` read per
request: `Mux::expire_client_rtt` forgot the H2 frontend sample at every pass,
and the H1 sites and `Endpoint::peer_rtt` read their socket at every access
log. On an H1 keep-alive connection that was two `getsockopt(TCP_INFO)` per
request, about 17 % of the worker's syscalls. Each connection now reads its
socket once: `memoized_rtt` (`lib/src/protocol/mux/mod.rs`) stores the first
sample in the connection (`ConnectionH1::rtt`, `H2Shell::local_rtt`) and
`Connection::rtt` answers every later ask from it, frontend and backend alike,
including a backend connection reused from the keep-alive pool.
`Mux::expire_client_rtt` and `H2Shell::expire_local_rtt` are gone.

The sample stays lazy, at the connection's first access log, rather than at
`accept`: on Linux `tcpi_rtt` is already the handshake RTT straight out of
`accept` (27–34 µs on loopback) and of `connect` on the dialling side, so an
earlier read would be valid, but the lazy one costs nothing on a connection
that never logs, and a backend dial is still in flight when its non-blocking
`connect(2)` returns.

Measured with `intentrace -p` on the worker, release build
(`--no-default-features --features jemallocator,crypto-aws-lc-rs`), one
worker, python `http.server` backend on loopback, `curl`, 20 requests on one
client connection, base `58550dd4`, two interleaved runs each:

| scenario                               | syscalls per request before | after | `getsockopt` per request before | after |
| -------------------------------------- | --------------------------- | ----- | ------------------------------- | ----- |
| H1 keep-alive, plaintext               | 11.55                       | 9.65  | 2.00                            | 0.10  |
| H1 keep-alive, TLS                     | 11.85                       | 9.95  | 2.00                            | 0.10  |
| H2, 20 streams multiplexed on 1 client | 12.20–12.35                 | 11.30–11.35 | 2.00                      | 1.05  |

The two H1 remainders are the frontend and backend connection's single reads.
In the H2 case sozu dials one backend connection per concurrent stream, so the
20 backend reads remain — one per connection — and only the 19 repeated
frontend reads go away.

### readable() entry point

```rust lib/src/protocol/mux/h2.rs:8676-8680
pub fn readable<E, L>(&mut self, context: &mut Context<L>, endpoint: E) -> MuxResult
where
    E: Endpoint,
    L: ListenerHandler + L7ListenerHandler,
{
```

The read path is a **two-call protocol**, the read-side mirror of
`h2_transmit::gather` / `h2_transmit::confirm` on the write side. `readable()`
itself is only the caller that sits between the two halves, and its
`self.socket.socket_read` is the single socket touch on the whole H2 read
path. It lives on `H2Shell`, the type that owns the socket, while
`poll_read_target` and
`handle_read` stay among the core impls. Those two are `pub` and re-exported
from `protocol::mux` together with `H2ReadTarget` and `H2ReadOutcome`:

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

On a TLS frontend the read lands in `FrontRustls::socket_read`
(`lib/src/socket.rs`), whose body is the private `rustls_socket_read`. One
TLS record usually carries several frames, so after the first header most
of these one-frame reads can be served from plaintext rustls has already
decrypted. The pass therefore drains that plaintext **before** it calls
`read_tls`, and only reaches the socket when the buffer is still short: a
record of N frames costs one `recv(2)` plus the single EAGAIN that ends the
readiness turn, instead of one EAGAIN per frame after the first
([#1588](https://github.com/sozu-proxy/sozu/issues/1588)). The readiness
contract is unchanged: an EAGAIN always ends the call with
`SocketResult::WouldBlock`, which `update_readiness_after_read` turns into
"drop READABLE until the next event", and a full buffer answers `Continue` so
READABLE stays. A `recv` that answers fewer bytes than rustls offered ends the
call the same way, once what it brought is processed and drained, because the
receive queue is empty and another `recv` could only answer EAGAIN
([#1602](https://github.com/sozu-proxy/sozu/issues/1602)). A call whose buffer
that plaintext fills still answers `Continue`, and the proof outlives it:
`FrontRustls::recv_memory` (`RecvMemory`, `lib/src/socket.rs`) records that a
`recv` answered short or EAGAIN, so the later call that finds the plaintext
empty answers `WouldBlock` without a `recv` — once per request on a
multiplexed connection before
[#1609](https://github.com/sozu-proxy/sozu/issues/1609). The memory is
cleared by every event the event loop delivers for the frontend token
(`HttpsSession::update_readiness` calls `FrontRustls::readiness_delivered`,
whatever the session state), because a byte that arrived after the proof raised
an edge that is either still pending in epoll or already delivered; HUP or ERROR
turns it off for good, so the EOF behind a short read is still read. EOF and
`close_notify` are answered `Closed` by the first call that finds no plaintext
left, after the frames that preceded them; a TCP FIN behind a short read is
read by the next call, which `update_readiness_after_read` makes once HUP was
seen.
`Error` is sticky: once `process_new_packets` fails, `FrontRustls::tls_fatal`
makes every later call answer `(0, Error)` without serving plaintext decrypted
from the records before the bad one.

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
| `Discard` | skips payload bytes: a refused stream's HEADERS payload (HPACK still decoded), or the unread rest of a DATA payload whose stream was removed mid-frame, read in pieces and credited to the connection window (`skip_orphaned_data_payload`, #1597) |

### handle_header_state()

Parses a 9-byte frame header from `self.zero`, validates the stream (new, existing,
closed, or idle), creates new streams for HEADERS on odd-numbered IDs, and
transitions to `H2State::Frame(header)` for payload reading.

`self.zero` is parsed from its first byte, and it holds nothing but input:
since #1604 our own output — the PING and SETTINGS acknowledgements, the
backend `end_stream` RST_STREAM(CANCEL) (#1597), the tail of a stalled
WINDOW_UPDATE or RST_STREAM flush — waits in the separate output queue, so a
frame header read can no longer land on it (#1600). Reading goes on while
output is queued, up to the output read cap (`poll_read_target`), where it
stops until the queue drains, as HAProxy's demux stops on a full `mbuf`.

Key decisions in this method:
- MAX_CONCURRENT_STREAMS enforcement: queues RST_STREAM(REFUSED_STREAM) and
  transitions to `Discard` state to skip the HEADERS payload
- Buffer exhaustion: same treatment as MAX_CONCURRENT_STREAMS. The core asks
  its `BufferSource` (`buffer_source.rs`) rather than a pool directly, and a
  source that answers `None` is reporting a transient shortage — so the answer
  is a refusal of that one stream, never a connection error. See
  `BufferSource`'s own documentation for why that distinction is the contract
  rather than a convention
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

```rust lib/src/protocol/mux/h2.rs:8854-8858
pub fn writable<E, L>(&mut self, context: &mut Context<L>, endpoint: E) -> MuxResult
where
    E: Endpoint,
    L: ListenerHandler + L7ListenerHandler,
{
```

`writable` sits beside `readable` and `write_streams` in the SHELL impl block,
and it is the only one of the three that reaches the socket exclusively through
the named TLS seams rather than through `self.socket`. It performs every seam
call its own body used to make inline — the preamble flush and the two queries
around it, the stalled-drain re-arm query, and the `H2State::GoAway` arm's flush
and post-flush query — and the `(H2State, Position)` dispatch between them takes
no `Front`, no `Context` and no `Endpoint`. Two seam reads reached on the same
pass are not `writable`'s: the one inside `flush_output_to_socket`, which
`flush_pending_control_frames` still performs and whose lift is a separate step,
and the one inside `force_disconnect`, which is `h2_close`'s third close site.

1. Adopts `context.now` and runs `prune_inactive_streams_while_closing`
2. Calls `flush_pending_control_frames()` as preamble, and performs the
   `ensure_tls_flushed` re-arm its `H2ControlFlushTarget::Stalled` answer asks
   for
3. Attempts the unconditional TLS flush: read `tls_wants_write`, and
   `flush_tls_records` if it says yes. It pushes bytes for every state, which
   is why it is here and not inside a close arm
4. Reads `tls_wants_write` a SECOND time — that read is the preamble flush's
   post-flush answer, and the pre-flush question both close arms decide on —
   and hands it to `dispatch_writable_state(tls_wants_write)`
5. Performs what the `H2WritableStateTarget` answers:
   - `Done(result)` is the pass's result
   - `Flush` — the `H2State::GoAway` arm, the only arm that asks for a second
     flush — means `flush_tls_records()`, a THIRD read of `tls_wants_write`,
     and `dispatch_writable_state_after_flush(tls_wants_write)`
   - `WriteStreams` means `write_streams(context, endpoint)`

Each read is its own `let`, and the third shadows the second inside the `Flush`
arm alone. That is the whole guard against the confusion
`h2_close::TlsFlushPhase` exists to prevent: "does rustls hold records" and
"did the kernel take them" are two questions, the second cannot be read off the
first, and a pass that reused one answer for both would close a TLS connection
with records still pending — the peer reads a truncated response and cannot
tell it from an attack. The flush and the re-query happen within the SAME
`writable()` call rather than over two ticks;
`a_flush_that_succeeds_closes_within_one_writable_call` pins that.

`dispatch_writable_state` dispatches on `(H2State, Position)`:

- Handshake states: serializes client preface, SETTINGS, connection WINDOW_UPDATE
- `(H2State::Error, Position::Server)`: `h2_close::error_close_action` on the
  answer handed in. No flush of its own — the preamble already issued this
  pass's
- `(H2State::GoAway, _)`: `h2_close::goaway_close_action` at
  `TlsFlushPhase::BeforeFlush`, which may answer `H2WritableStateTarget::Flush`
- Proxying states: answer `WriteStreams`, which the caller turns into
  `write_streams(context, endpoint)`

The proxying arms are a state test, not a content test. A connection whose only
queued output is the PING or SETTINGS acknowledgement the preamble just drained
is in `H2State::Header` like any other, so reaching `write_streams` never meant
"application data is being written" — which is why the connection-level idle
deadline is armed per transmit inside `handle_write` and not on the way in here
(LIFECYCLE.md §9 invariant 9, sozu-proxy/sozu#1489).

### flush_pending_control_frames()

Queues control data behind the ordered output and flushes it before
application frames, in order:

1. **Frontend-hung-up-while-draining cleanup**: if
   `frontend_hung_up_while_draining()` (`Position::Server`, draining, AND a
   HUP/ERROR readiness event — all three), clears `expect_write`, the output
   queue, the pending WINDOW_UPDATE queue and `pending_rst_streams` — the peer
   is gone, so nothing queued for it will ever reach it. It clears output
   only: `zero` is input, and `Ready::HUP` is `is_read_closed() ||
   is_write_closed()` (`command/src/ready.rs`), which mio documents as true on
   a TCP half-close too, where data the peer already sent can still be sitting
   unread in the kernel receive queue. `drive_frontend_shutdown_io` (`mod.rs`)
   force-calls `readable()` for H2 on every `shutting_down()` poll, so a
   CONTINUATION frame split across TCP segments landing a HUP event alongside
   its first segment is the ordinary soft-stop path. Review of this stage's
   first version (`e1c3c2fb`, sozu-proxy/sozu#1423) proved that clearing
   `zero` there corrupts exactly that case (`StringDecodingError(NotEnoughOctets)`
   / decoder desync); before #1604 split the output out, the stage had to
   guard its clear on `!header_block_reassembly_in_progress()` instead
2. **SETTINGS ACK timeout check**: If peer hasn't ACK'd within 5 seconds,
   sends GOAWAY(SETTINGS_TIMEOUT)
3. **WINDOW_UPDATE frames**: `H2FlowControl::drain_window_updates_into`
   (`h2_flow_control.rs`) serializes every queued entry into room reserved at
   the end of the output queue and removes what it wrote — coalescing already
   happened at queue time (`queue_window_update`, keyed by stream ID; `0` is
   the connection-level entry). Drain order is the map's ascending stream-id
   order, deterministic across processes — see that module's doc comment
4. **Pending RST_STREAM frames**: Asks `H2ControlTx::drain_rst_streams_into`
   (`h2_control_tx.rs`) to serialize every queued frame into room reserved at
   the end of the output queue, with flood detection (`MAX_PENDING_RST_STREAMS`
   cap). Proxy-emitted RSTs (DATA-on-closed, `refuse_stream_and_discard`,
   `reset_stream`, `cancel_timed_out_streams`) are queued via the canonical
   `ConnectionH2::enqueue_rst` helper, which delegates to
   `H2ControlTx::enqueue_rst` — dedupes through the wire-map's `rst_sent` set
   (`H2StreamTable`, `h2_stream_table.rs`), bumps the lifetime counter, and
   arms WRITABLE. The same `MAX_PENDING_RST_STREAMS` bounds the queue at the
   insert: once that queue holds 200 entries a further
   `enqueue_rst` queues nothing and returns `h2.rst_stream_dropped` plus an
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
   whenever the queue is non-empty, so a pass that ends before this stage
   re-runs on the next tick rather than stranding the queued RST.
5. **One output flush**: when the output queue holds anything — what the
   stages above queued, behind whatever was already waiting (an ACK, the rest
   of a stream frame a partial write cut) — the walk answers `FlushOutput`.

Before #1604 each stage serialised into `zero`, which was also the read
landing zone, so the WINDOW_UPDATE, RST_STREAM and GOAWAY stages had to defer
while `header_block_reassembly_in_progress()` was true and could not
serialise while an earlier flush still owned the buffer. The output queue
shares nothing with the read side, so no stage waits for a block to complete.
The last one that did — a deferred initial GOAWAY, kept as a drain policy after
#1604 — was removed in #1637: `ConnectionH2::graceful_goaway` queues the
advisory GOAWAY in the drain pass itself, because deferring it let a drain
close a session whose only stream was mid-block without any GOAWAY (see the
module doc of `lib/src/protocol/mux/h2_drain.rs`).

Answers `H2ControlFlushTarget`: `Done(MuxResult)` when a GOAWAY already ended
the pass, `Proceed` when `writable()` should carry on into its state dispatch,
`FlushOutput` when the output queue holds bytes, and — through
`handle_control_flush` — `Stalled` when that flush stalled, in which case
`writable` reads `tls_wants_write` and hands it to `ensure_tls_flushed`. The
query has to happen AFTER the write that stalled, so it cannot be an input,
and the shell that owns the socket is the layer that makes it.
`flush_output_to_socket` itself stays inside the shell: it is a byte mover on
the control-frame path, not a TLS seam.

### The ordered output queue (`h2_output.rs`)

`ConnectionH2::output` is the connection's single, ordered output queue, and
the answer to #1604. Every byte the connection sends that is not still owned
by a stream goes through it, in wire order: the control frames it serialises
and the unsent rest of the one stream frame a partial write cut. Each entry is
a WHOLE frame when it is queued.

Stream frames stay zero-copy. A stream transmit
(`ConnectionH2::gather_transmit`, `h2_transmit::gather_after`) offers the
kernel the queue first, as one descriptor, then the stream's `kawa.out`
blocks, as `IoSlice`s straight into `kawa.storage`. What the socket took is
split at the queue's length: the queue's part goes back to
`ConnectionH2::consume_output`, the rest is the stream's and is confirmed on
its `kawa` exactly as before. When the write stopped inside a stream frame,
`h2_transmit::frame_tail` reads the frame headers in the gathered descriptors
and measures the rest of that frame — extended through the CONTINUATION
frames of an unfinished header block, since RFC 9113 §6.10 allows no other
frame in between — and `ConnectionH2::handle_write` has the queue adopt it
(`H2Output::adopt_tail`): the bytes are copied, consumed from the stream, and
counted to the stream right away. A stream therefore only ever owns frames the
wire has not started, and removing it — a peer RST, `end_stream`, expiry,
`prune_inactive_streams_while_closing` — drops whole unsent frames, never the
second half of one the peer is already parsing. That keeps the framing whole.
The HPACK tables are kept by a separate mechanism: an unsent header block was
already encoded, so dropping it resets the encoder's table (see "A dropped
header block resets both tables" below,
[#1627](https://github.com/sozu-proxy/sozu/issues/1627)).

The copy is bounded by one frame or one header block and happens only on a
partial write. HAProxy owns its in-flight bytes by copying every frame into
its `mbuf` ring (`h2c_ack_settings`, `h2c_send_ping` append with `br_tail`),
or by swapping a whole stream buffer into the ring in `h2s_make_data`'s
zero-copy path; `h2` holds an in-flight DATA payload by value in
`FramedWrite`'s `Encoder::next` until it is written, its `Bytes` reference
count keeping it alive. Sōzu's stream storage is a pool buffer that a removed
stream gives back, with no reference count, so the in-flight rest is copied.
The queue itself holds 128 bytes inline — a frontend's whole server preface is
79 — and spills to the heap only past that, so a connection that never meets
backpressure allocates nothing for its output, as the pool buffer `zero` it
replaces did not.

Reading is independent of the queue, but bounded by it: past one buffer of
queued output (`output_read_cap`, the size of `zero`) the peer is not reading
what it asked for, so `poll_read_target` withdraws READABLE until
`consume_output` drains the queue below the cap — HAProxy's
`H2_CF_DEM_MROOM`.

### write_streams(), poll_write_target() and handle_write()

The main data-plane write path is a **drive loop**, the write-side mirror of
`poll_read_target` / `handle_read`. It sits beside `readable` in the shell
impl, on `H2Shell`, and it is the one member of this quartet that stays
PRIVATE.
`poll_write_target` and `handle_write` are `pub`, re-exported with
`H2WriteTarget` and `H2WritePass`; `write_streams` is not, because the
`Vec<IoSlice<'static>>` bracket must not become splittable by a caller this
crate cannot enumerate — see the gather/confirm section below.
`write_streams` is the shell: it drives the
`Vec<IoSlice<'static>>` and the only `socket_write_vectored` call on the
stream-write path, and it does nothing else but answer what the core asks for —
including the TLS flush triple the pass ends on. The vector is `H2Shell`'s
private `io_slices` field, kept for the connection's lifetime: it is empty
outside the gather/confirm bracket, and only its capacity survives from one
pass to the next, so a warm transmitting pass allocates nothing for it. As a
per-pass local it cost one heap allocation per transmitting pass, at least one
per response.

```rust
loop {
    match self.poll_write_target(context, &mut endpoint, &mut pass) {
        H2WriteTarget::Done(result) => return result,
        H2WriteTarget::Finalize { socket_write, bytes_written } => {
            let tls_wants_write = self.tls_wants_write();
            match self.finalize_write(tls_wants_write, socket_write, bytes_written, context) {
                H2FinalizeTarget::Done(result) => return result,
                H2FinalizeTarget::Flush => { self.flush_tls_records(); }
                H2FinalizeTarget::SkipFlush => {}
            }
            let tls_wants_write = self.tls_wants_write();
            return self.finalize_write_after_flush(tls_wants_write);
        }
        H2WriteTarget::Transmit { stream_id } => { /* gather, write, confirm */ }
    }
}
```

It is a loop and not a `match`, which is the one forced divergence from the
read side: a read pass performs exactly one `socket_read`, while a write pass
performs an unbounded number of vectored writes. `H2WriteTarget::Finalize`
carries `bytes_written` as well as `socket_write` because `finalize_write`
reads it as `made_progress`, and that alone selects `RetainPendingBack` over
`Quiesce` (LIFECYCLE §9 invariant 16); three sites end a pass **without**
finalizing and are `Done(MuxResult)` instead — the resume path's stall, the
MadeYouReset emitted-RST cap trip, and the close-frontend GOAWAY.

`poll_write_target` walks the pass through `H2WritePhase`, and the phase is
what makes a re-entry after a transmit different from a first entry:

1. `Start` — reads `stream_table.expect_write()` and goes to `Resume` or
   straight to the scheduler pass.
2. `Resume` — flushes the stream a previous pass parked. It pre-computes
   `byte_totals` for overhead distribution (in `H2WritePass::new`), and a
   stall here ends the pass with `Done` before the scheduler pass begins.
   The rest of the frame the parking write cut already waits in the output
   queue, so it leaves first — in the preamble flush or ahead of this
   resume's transmit — and a PING or SETTINGS ACK, GOAWAY or RST queued
   meanwhile follows it, never inside it (#1600, #1604). The park only keeps
   the stream's whole remaining frames first in line.
3. The `Resume`/`Start` → `Prepare` transition (`begin_scheduler_pass`) opens
   a `H2ConverterPass` holding the reusable scratch and the pending RFC 7541
   §6.3 size-update signal, and asks `H2Scheduler::begin_pass` for the pass
   order (urgency, then stream_id, then the rotated incremental tail) and its
   ready-incremental census.
4. `Prepare { cursor }` — ONCE per stream: the eligibility gate,
   `kawa.prepare`, the window debit and `census.note_fired(urgency, ...)`.
5. `Flush { cursor, .. }` — yields one `Transmit` per round until the queue
   drains or the socket stalls, then recycles the stream if it completed and
   advances the cursor back to `Prepare`.
6. `End` — returns the scratch buffers to `self.hpack` and shrinks the three
   converter buffers if they grew beyond 16KB
   (`HpackState::shrink_converter_buffers`), accounts the deferred RSTs, ends
   the pass with `H2Scheduler::end_pass` (which takes the order buffer back
   and commits every bucket's RFC 9218 §4 round-robin cursor), then cleans up
   `dead_streams` via `remove_dead_stream` (evicting the `H2StreamTable` wire
   mapping, `rst_sent`, and the activity/fc-stall caches together, plus the
   scheduler's priority entry via `H2Scheduler::remove_stream`), distributes
   overhead and emits access logs.
7. `Ended` — terminal. `End` releases the three scheduler-pass values as its
   FIRST statement, so re-entering it would `expect` on three empty `Option`s;
   `Ended` answers `H2WriteTarget::Done` instead. `write_streams` never
   re-polls, but `poll_write_target` is `pub`, so its callers are no longer
   enumerable by reading this crate.

`handle_write` is the second half of the protocol and the pre-image flush
loop's body: it logs the socket I/O, counts the bytes into the phase's own
counter, re-arms `Ready::READABLE` when a cross-parked read has room again,
arms the connection-level idle deadline when the transmit moved a real stream's
bytes, and writes `pass.stalled`. **It is the only place on the write core that
sees a `SocketResult`**, in that one statement — see "the round-again" below.

The deadline arm is gated on BOTH halves — an `H2StreamId::Other` id and
`size > 0` — because this is the whole of the write side's share of LIFECYCLE.md
§9 invariant 9: outbound application data is activity, an acknowledgement is
not. `arm_timeout` reads `self.now`, the snapshot `writable()` adopted at pass
entry, so arming from here lands on the same instant the top of the pass would
have computed; the gate is the change, not the moment.

**Where the pass's own state lives**: the counters, flags and scratch vectors
one pass carries — the phase, the byte totals, the resume counter, the
socket-write flag, the pass byte total, `freshly_emitted_rsts`,
`completed_streams`, and the per-stream `consumed` / `stream_bytes` — are
fields of `H2WritePass` (`h2_write_pass.rs`), a plain struct built at the top
of `write_streams` and dropped when it returns. It is a local threaded by
`&mut`, never a field on `ConnectionH2`: resumption ACROSS calls is
`H2StreamTable::expect_write`, not this struct.

Three values the pass carries are `Option` fields **late-initialised** at the
transition into `Prepare` rather than built with the pass — the
`H2ConverterPass`, the scheduler's loaned `order` buffer and its
`ReadyIncrementalCensus` — and they are excluded for two different reasons.
`order` and `census`, because `H2Scheduler::begin_pass` enumerates
`stream_table.streams()` and the `expect_write` resume path that runs before
it can retire a stream out of that same map through `remove_dead_stream`, so
building them at pass start would change which streams the pass visits. The
`H2ConverterPass`, because its constructor `mem::take`s three scratch buffers
out of `HpackState` and only `into_buffers` hands them back — a converter
built at pass start would be dropped on every stalled resume, and the pool
would lose its buffers permanently. What guarantees they come back is that
`poll_write_target` has exactly one `return` between the transition and the
first statement of `End` — the `Transmit` yield — which the shell's loop
always answers; `H2WritePass`'s `Drop` carries the matching `debug_assert!`,
and the `Ended` phase closes the re-entry-after-answering case.

**One ordering question the split raises, and how it is answered.**
`Stream::state` is read BEFORE the flush, in `Start` and in `Prepare`, and
carried on the phase through the transmits rather than re-read afterwards. Its
consumer is `handle_1xx_reset`, which decides whether to re-arm
`Ready::READABLE` on the linked token, so a stream retired mid-flush must not
change which state that check sees — the pre-image read it before the flush and
this keeps that timing. `H2Scheduler::priority` is carried for a different
reason: once per stream per pass is what the pre-image did, and re-reading it
in `Flush` would be a second `HashMap` lookup on the write hot path.

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
tick, handing it the `tls_wants_write` answer it read itself. After the RFC 9113
§6.8 graceful-GOAWAY check (draining with every stream gone), the rest is one
decision taken in `h2_close::finalize_action`; the four readiness answers are
performed here and the two TLS answers are handed back to `write_streams` as an
`H2FinalizeTarget`, whose second call is `finalize_write_after_flush`:

| answer | what it means |
|---|---|
| `Flush` | rustls holds records and the pass wrote nothing: the caller performs `ConnectionH2::flush_tls_records`, re-reads `tls_wants_write`, and re-asks with `TlsFlushPhase::AfterFlush` |
| `SkipFlush` | rustls holds records but `socket_write_vectored` already attempted this pass's flush: the caller re-reads `tls_wants_write` without a second empty-buffer write |
| `Parked` | a partial write set `expect_write`: it owns the next tick, no bit moves |
| `RetainPendingBack` | LIFECYCLE §9 invariant 16: progress plus queued response bytes, so `Ready::WRITABLE` survives (the absence of the withdrawal below) |
| `ArmControlQueue` | a deferred RST_STREAM / WINDOW_UPDATE is still queued: `Readiness::arm_writable` |
| `Quiesce` | nothing owed anywhere: withdraw `Ready::WRITABLE` interest |
| `ReArm` | post-flush: records survived, re-arm the edge-triggered WRITABLE event |
| `Settled` | post-flush: the kernel took everything |

Two things about that list are worth knowing before changing it. The pre-flush
answers and the post-flush answers are disjoint, and the two halves match their
own set exhaustively with named-impossible arms rather than a wildcard — a `_`
arm would turn a new variant into a release-mode panic on the write path instead
of a compile error. And `ConnectionH2::flush_tls_records`' `(size, status)` is
discarded at its `write_streams` call site, as it is at the close sites: the
post-flush `ConnectionH2::tls_wants_write` query is how this path learns whether
the flush landed, so no `SocketResult` reaches the decision. `flush_output_buffer()` is the one site on the write path that does
consume a status, through `update_readiness_after_write`.


### The gather/confirm pair (`h2_transmit.rs`)

A stream's bytes do not reach the socket in one call. Each round is one
`H2WriteTarget::Transmit` answered by the shell:

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

**A second caller: the H1 write pass.** `ConnectionH1::writable` walks the
same `kawa.out` the same way for the one stream it carries, so it calls this
pair instead of keeping its own loop. It keeps the `Vec<IoSlice<'static>>` as
the connection's `io_slices` field, so only the vector's capacity survives a
pass, never a descriptor, and a warm pass allocates nothing for it. As a
per-pass local it cost one allocation plus one `realloc` per doubling, and an
H1 header block is many `Store`s: `intentrace` shows 45 descriptors in the
`writev(2)` of a `curl` request header and 32 in that of a
`python3 -m http.server` response. Its window is wider than the shell's three
statements: between the write and `confirm` it also copies the accepted bytes
into the replay capture, which writes to the stream's `retry_buffer` and never
to `kawa`, and its one early `return` inside the window is taken only when the
vector is empty.

**The first pass of a connection.** A reused field still starts empty on every
new connection, and pushing one descriptor per block doubled it from four up to
the block count: 5 allocations for a frontend's 64 descriptors and 4 for a
backend's 32, 9 per H1 request served on its own connection. `gather` now
reserves `kawa.out.len()` descriptors before it pushes, an upper bound on what
it can push, so a fresh connection sizes the vector in one allocation and a
vector that already holds the capacity is left alone
([#1610](https://github.com/sozu-proxy/sozu/issues/1610)).
`a_cold_h1_write_pass_sizes_its_descriptor_vector_once` (`h1.rs`) pins it on
both sides.

**Why the pair is exported asymmetrically.** `poll_read_target`,
`handle_read`, `poll_write_target` and `handle_write` were widened so a driver
outside this crate can eventually stand where `readable` and `write_streams`
stand, and `h2_transmit` is `pub mod` so that driver can perform the write.
`writable`'s own pair, `dispatch_writable_state` and
`dispatch_writable_state_after_flush`, is deliberately NOT widened: both stay
private and `H2WritableStateTarget` stays `pub(super)`, because the
connection-level write pass names no buffer for an out-of-crate driver to fill
and the `flush_pending_control_frames` ahead of it still moves bytes itself.
But
`gather` could not follow them as a plain `pub fn`: it PUSHES `IoSlice<'static>`
borrowed out of its `&Kawa<T>` argument, so three lines of safe out-of-crate
code — call `gather`, drop the `Kawa`, read the first descriptor — would be
undefined behaviour with no `unsafe` written anywhere in them, and `kawa` is a
published crate, so not one Sōzu internal is needed to reach it. A `'static` in
a safe signature does not merely fail to state the obligation; it denies there
is one.

It is therefore `pub unsafe fn gather`, with a `# Safety` section naming both
halves of the obligation: `kawa` must not be dropped or mutated while a
descriptor is readable, and `io_slices` must be emptied before that first
mutation. `confirm` is a plain `pub fn` and deliberately not `unsafe`: it
produces no descriptor, and everything it does — `Vec::clear`, which drops
`IoSlice` values without dereferencing them, then the safe `Kawa::consume` —
is well defined even on descriptors that already dangle. Handing `consume` too
large a `size` is a truncation bug, not undefined behaviour. Marking it
`unsafe` would be decoration and would blunt what `unsafe` means at the one
call site where it means "a lifetime is being asserted here".

**The round-again, and the trap in it.** A partial write is ordinary: `size`
may be the whole offer, less than it, or zero. `update_readiness_after_write`
classifies a pass as stalled **iff `size == 0`** — the `status` only clears the
WRITABLE event bit — so `(size > 0, WouldBlock)`, which `FrontRustls` returns
structurally whenever rustls accepted plaintext while the kernel was full, is
NOT a stall and `poll_write_target` must yield another `Transmit` for the same
stream. A machine keyed on `status != Continue` would end the pass there and
truncate the response. That is why `handle_write` writes `pass.stalled` in one
statement and `poll_write_target` never takes a `SocketResult` at all: adding
one has to be a visible signature change. Note that a stalled pass is not fair
to the streams it did not reach — see the module header and LIFECYCLE
invariant 26 for why the trailing urgency buckets are the ones that suffer.

### flush_output_to_socket()

```rust lib/src/protocol/mux/h2.rs:8179
fn flush_output_to_socket(&mut self) -> bool {
```

Writes the ordered output queue to the socket in a loop. Returns `true` if the
socket stalled (WouldBlock), `false` when fully drained. Counts the queue's
own bytes as `overhead_bout` (`ConnectionH2::consume_output`); the adopted
rest of a stream frame was counted to its stream already.

When `ConnectionH2::output_flush_closes_connection` answers yes — a server
connection in `H2State::GoAway` whose queue ends with the final GOAWAY, with
no stream, WINDOW_UPDATE or RST_STREAM left to write after it — the loop writes
through `SocketHandler::socket_write_then_close` instead of `socket_write`, and
sets `close_notify_sent` once the whole queue was taken. `FrontRustls` hands
rustls the GOAWAY, queues `close_notify` behind it and flushes both records
with one `write_tls`, so the close costs one `writev(2)` instead of two
([#1607](https://github.com/sozu-proxy/sozu/issues/1607)); `H2Shell::close`
then finds nothing left to drain. The alert still follows the GOAWAY on the
wire, as RFC 8446 §6.1 requires it before the write side closes. Other
handlers write, then call `socket_close`, which is what they did before.

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
- `Mux::shutting_down_inner()` keeps a draining H2 session open while the peer
  is still sending a header block (`ConnectionH2::peer_header_block_in_progress`)
  and while a complete request awaits its backend link (`StreamState::Link`),
  not only for `Linked` / non-quiesced `Unlinked` streams: closing there lost a
  request the advisory GOAWAY invited, with no final GOAWAY
  (sozu-proxy/sozu#1647). When the `h2_graceful_shutdown_deadline_seconds`
  budget elapses, `ConnectionH2::goaway_before_forced_close` queues the final
  GOAWAY before the forced close; its `last_stream_id` excludes a stream whose
  opening block never completed.
- A GOAWAY received from the client (`ConnectionH2::handle_goaway_frame`,
  `Position::Server`) retires no stream: its `last_stream_id` bounds the
  streams the receiver initiated (RFC 9113 §6.8), and sozu pushes none. It
  marks the connection draining, so new client streams are refused, every
  in-flight request completes, and the final GOAWAY follows once no stream
  remains. A soft-stop on such a connection arms the graceful-shutdown budget
  that the peer GOAWAY did not arm. Each received GOAWAY counts as a glitch. Only a GOAWAY from an H2 backend (`Position::Client`) retires the
  streams above its `last_stream_id`: re-linked, answered `503`, or reset.
- `ConnectionH2::prune_inactive_streams_while_closing()` removes H2 stream-ID
  mappings for streams that never became active before a connection-level close
  (for example, partial or oversized HEADERS blocks that were abandoned during
  GOAWAY). Without this pruning, shutdown can wait forever on idle entries that
  no longer correspond to useful work.
- `peer_gone_after_final_goaway()` is the terminal shutdown condition for the
  frontend H2 connection. Once the final GOAWAY has been queued, all stream
  mappings are gone, and the peer has already hung up, the remaining rustls
  backlog is no longer deliverable and the session may close immediately.
- While the peer is still there, a close after a final GOAWAY(NO_ERROR)
  lingers instead (`H2Shell::linger_instead_of_closing`): `close_notify` is
  flushed, the write side is shut down, and what the client still sends is
  read and dropped until its EOF, 4 MiB, or `request_timeout`. Closing with
  those frames unread made Linux reset the connection and discard response
  bytes the client had not read yet. See `LIFECYCLE.md` §8.1.
- `FrontRustls::peer_disconnected` suppresses new TLS writes after EOF/HUP so
  the close path does not keep retrying application writes to a dead peer.
- HTTPS uses `shutdown(Write)` rather than `shutdown(Both)`. On Linux,
  `shutdown(Both)` discards unread receive-buffer data and can convert an
  otherwise clean post-drain close into a TCP RST, truncating bytes that the
  drain loop already flushed. It is skipped once the client has closed
  (`mux::shutdown_write`, `FrontRustls::peer_disconnected`): the `close()`
  that follows sends the same FIN or RST, and after the client's
  `close_notify` the call only failed with `ENOTCONN`
  ([#1603](https://github.com/sozu-proxy/sozu/issues/1603)). The
  `close_notify` itself is still sent.

### A backend stream the backend never saw

On a backend connection (`Position::Client`) a stream exists in sozu before it
exists on the wire. `ConnectionH2::start_stream` allocates the id and registers
it as soon as the router links the request; the HEADERS leave with a later
write pass, once the preface is out and the handshake reached `H2State::Header`,
and the socket may take none of them, leaving the block parked in `front.out`.
`front.consumed` is the discriminator: on this position it is exactly "a request
byte reached this socket", and the first frame of a stream is its HEADERS.

A stream retired before that — the client reset or left, the stream or backend
timed out — is idle for the backend. RFC 9113 §5.1 and §6.4 make any frame but
HEADERS or PRIORITY on an idle stream, RST_STREAM included, a connection error
(hyperium/h2 answers GOAWAY(PROTOCOL_ERROR) from `Recv::ensure_not_idle`,
HAProxy from `h2_frame_check_vs_state`), and a frame queued before the preface
breaks it (§3.4). `Router::plan_connect` multiplexes the concurrent streams of
one frontend connection on one backend connection of its `Mux` session, still
connecting included, so one cancelled request used to cost all of them
([#1631](https://github.com/sozu-proxy/sozu/issues/1631)). The
`Position::Client` arm of `ConnectionH2::end_stream` and
`ConnectionH2::cancel_timed_out_streams` now queue nothing for such a stream,
as hyperium/h2 drops a stream still pending open. The same holds for a request
that fails while its backend stream is registered and its HEADERS have not
left — an H1 frontend rejecting a malformed body answers 400 itself: the
`H2WritePhase::Prepare` arm no longer hands it to the converter, whose
`initialize` would have queued the RST. Its id stays burnt: the next
HEADERS on a higher id closes it implicitly (§5.1.1), which is harmless because
the backend never processed it. A block parked unsent goes with the stream,
and the next write pass resets the HPACK encoder's table for it (see "A dropped
header block resets both tables" below).

The handshake is covered the same way. A backend connection is `Connected` from
its first writable, before its preface is written, and the router links streams
onto it from then on; `start_stream` arms a write for each. In
`H2State::ServerSettings` (preface out, server SETTINGS not read) that write used
to hit `Unexpected combination` and force-disconnect. It is now withdrawn, and
the SETTINGS ACK the read queues re-arms it, so a request linked during the
handshake goes out right after it. The `ClientPreface` arm debug-asserts that
nothing was queued ahead of the preface.

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
`lib/src/protocol/kawa_h1/editor.rs`), whose `HttpContext::otel` field is:

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

At access log emission time (`Stream::generate_access_log`, in
`lib/src/protocol/mux/stream.rs`):

```rust lib/src/protocol/mux/stream.rs:882-885
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

```rust lib/src/protocol/mux/pkawa.rs:596-598
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

```rust lib/src/protocol/mux/h2.rs:6782-6784
self.hpack.set_decoder_max_allowed_table_size(
    self.local_settings.settings_header_table_size as usize,
);
```

On receiving the peer's own SETTINGS, in the `SETTINGS_HEADER_TABLE_SIZE` arm:

```rust lib/src/protocol/mux/h2.rs:6796-6802
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

The peer may change the size more than once before that block, for example
to 0 and then back to 4096. The encoder evicted everything at 0, so
announcing 4096 alone would leave the peer's decoder holding entries the
encoder no longer has. `HpackState::set_encoder_max_table_size` therefore calls
`Encoder::change_max_table_size`, which records the smallest size set since the
last block beside the last one, and the converter emits through
`Encoder::encode_size_updates_into`: the smallest size first when it is below
the last, then the last (RFC 7541 §4.2) — at most the two updates a block may
open with. `ConnectionH2::pending_table_size_update` still holds the last size
only; it says whether a signal is owed, the encoder says what it is.

### A dropped header block resets both tables

The encoder changes its dynamic table when it ENCODES a block, in
`kawa.prepare`, not when the block reaches the wire: a field whose name is in
no table is sent with incremental indexing and inserted at once, and a size
update is taken from the pending signal. Every block the encoder produced must
therefore reach the peer, in order (RFC 7541 §2.2, RFC 9113 §4.3). Two paths
used to drop one unsent
([#1627](https://github.com/sozu-proxy/sozu/issues/1627)):

- a stream parked with an encoded HEADERS/CONTINUATION block still in its
  `kawa.out` is removed (a peer RST_STREAM, `end_stream`, expiry,
  `prune_inactive_streams_while_closing`), or keeps its park while its
  `kawa.out` is cleared (`forcefully_terminate_answer`, a default answer);
- `H2BlockConverter::check_header_capacity` clears a block that outgrew
  `MAX_HEADER_LIST_SIZE`, after its first fields and its size update were
  encoded;
- `H2BlockConverter::finalize` clears a field block left unfinished. kawa's H1
  parser queues one `Block::Header` per trailer line as it reads it and the
  closing `Flags { end_header }` only with the final CRLF, and
  `ParsingPhase::Trailers` counts as a main phase, so a write pass between the
  two reads encoded the first trailer lines and dropped them: the trailers
  were lost as well.

The damage is not limited to a decoding error. Entries are numbered from the
newest (RFC 7541 §2.3.3), so each insertion the peer missed moves every older
entry one index down in the encoder's table only, and a later block naming an
older entry makes the peer read a different field with no error at all:
`hpack::tests::a_dropped_block_substitutes_fields_until_the_table_is_reset`
encodes `x-c: 3` and the peer reads `x-a: 1`. On a backend connection, where
the concurrent streams of one frontend connection share one encoder, that is
one request's field delivered in another request of the same frontend
connection. A backend connection belongs to one `Mux` session (`Mux::router`)
and is never shared across client connections, so the field of a different
user is reachable only behind an intermediary that multiplexes several users on
one frontend connection.

The repair is the one RFC 7541 §4.2 provides: "This mechanism can be used to
completely clear entries from the dynamic table by setting a maximum size of 0,
which can subsequently be restored." `Encoder::reset_table` empties the
encoder's table and records the size updates `0`, then the current maximum,
and the caller re-arms `pending_table_size_update`, so the next block opens
with both and the peer's decoder empties its table too. Both tables are then
equal whatever the peer missed; the cost is the entries the next blocks would
have reused, once per dropped block. No frame is sent for the dead stream.

It is sound because no block encoded before the reset can reach the wire after
the block that carries it. A write pass flushes each stream it prepares before
it prepares the next and ends at the first stall, so only the stream parked in
`expect_write` can hold unsent frames, and nothing is encoded until that stream
is resumed or gone:

- `ConnectionH2::parked_header_block` records, where a pass parks a stream,
  whether its `kawa.out` still holds a HEADERS, PUSH_PROMISE or CONTINUATION
  frame (`h2_transmit::holds_header_frame`, a walk over the frame headers that
  runs only on a park). The next pass's `H2WritePhase::Start` takes the flag
  and, when the park is gone or its `kawa.out` is empty, calls
  `ConnectionH2::reset_encoder_table` before anything is encoded.
- A field block is encoded only once its closing flags are queued
  (`header_block_closing`): the converter puts the first field back
  and ends the `prepare`, and the backend read that completes the trailers
  wakes the writer again, so the trailers go out whole. Waiting must always
  end: a block the next queued flags do not close was cut short and is dropped
  unencoded, and `ConnectionH2::end_stream`'s close-delimited arm ends a
  chunked response its H1 backend closed mid-body, trailers included, or a
  `Content-Length` response short of its length, with RST_STREAM (the
  truncation `ConnectionH1::terminate_close_delimited` already reports the same
  way, RFC 9112 §6.3 and §7.1) and drops its unencoded blocks. Only a body with
  neither `Content-Length` nor chunked coding ends cleanly at the backend close
  (sozu-proxy/sozu#1633).
- Every converter path that still throws encoded bytes away
  (`check_header_capacity`, the `StatusLine::Unknown` abort, `finalize`) goes
  through `H2BlockConverter::discard_encoded_block`, which calls
  `Encoder::reset_table` and re-arms the converter's pending signal, so the
  next stream of the same pass opens with the updates; the stream's own earlier frames, already queued, go out before
  that stream is prepared. `H2WritePhase::End` then takes the pass's remaining
  signal (`H2ConverterPass::pending_table_size_update`) as the connection's,
  instead of clearing it whenever some block carried the old one.

The reference implementations avoid the hazard differently. `h2` (hyperium)
encodes a HEADERS frame only when `FramedWrite::buffer`
(`src/codec/framed_write.rs`) writes it into the connection's output buffer,
and a reset stream's queued frames are dropped before that
(`Prioritize::clear_queue`), so they never touched the table. HAProxy's
encoder (`hpack_encode_header`, `src/hpack-enc.c`) keeps no dynamic table: it
names static entries only, so a block it drops cannot shift an index it
uses. Encoding at send time would move the
encoding out of `kawa.prepare`, which the converter and the zero-copy output
queue are built around; the reset keeps both and sends nothing for a dead
stream.

### An encoded request belongs to its backend connection

The encoding changes the connection twice when a backend's write pass prepares
a request: the HPACK encoder's table (see above), and the request itself.
`kawa.prepare` pops the request's `StatusLine`/`Header`/`Flags` blocks and
leaves HEADERS/CONTINUATION frames in `front.out`, carrying the stream id this
connection allocated and a field block only this connection's peer can decode.
If the socket takes none of it and the connection is then lost, or refuses the
stream with a GOAWAY below it, the request still reads `front.consumed ==
false`, which used to mean "untouched, retry it elsewhere"
([#1632](https://github.com/sozu-proxy/sozu/issues/1632)). Re-linked, its
stale frames went out first on the new connection. `Router::plan_connect`
prefers an existing connection of the cluster, usually shared by the other
streams of the same frontend connection: a stream id above that connection's
highest opens a stream whose field block resolves its dynamic indexes against
another table, so the backend rebuilds a request carrying fields those other
requests inserted there; any other id, or an index out of range, is
a PROTOCOL_ERROR or COMPRESSION_ERROR that ends every stream on it.

`Stream::front_bound_to_backend` marks the request once an H2 backend has
encoded it (one store in the `Position::Client` prepare, no allocation), and
the three places that could send it elsewhere check it:
`end_stream_decision` answers `SendDefault(502)` instead of `Reconnect`;
`ConnectionH2::handle_goaway_frame` answers a whole `503` (refused and not
processed, RFC 9110 §15.6.4) instead of re-linking — not REFUSED_STREAM, whose
empty error answer an H1 frontend cannot write;
and `ConnectionH2::start_stream` refuses any request whose `front.out` already
holds output, before it allocates an id. Encoding it again for the new
connection would need the blocks `kawa.prepare` popped, so every request would
keep a copy on the nominal path; hyperium/h2 has no such state because it
encodes at send time. An H1 backend's prepared output is plain HTTP/1.1 bytes,
valid on any fresh H1 connection, so it never sets the flag and keeps its
reconnect.

### Buffer shrinking after large headers

`converter_buf`, `lowercase_buf` and `cookie_buf` live in `HpackState`, which
acquires all three from the connection's `BufferSource` at construction
(`buffer_source.rs`) rather than allocating them itself — which is why
`HpackState::new` returns an `Option`. A
write pass takes all three out by value, hands them to `H2ConverterPass`, and
`H2ConverterPass::converter` / `::reclaim` move them into and out of each
per-`prepare` `H2BlockConverter` — a `Vec` move, never a copy of the bytes.
The pass gives them back at the end, and `HpackState::shrink_converter_buffers`
then caps each one:

```rust lib/src/protocol/mux/hpack_state.rs:138-148
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

### The sans-io codec (`hpack/`)

The codec both coders of `HpackState` are built from lives in
`lib/src/protocol/mux/hpack/`, written from RFC 7541. It replaced the
`loona-hpack` crate (sozu-proxy/sozu#1616), whose decoder built a 257-entry
`HashMap` for every Huffman-coded string and kept every dynamic-table entry as
two owned `Vec<u8>`.

**No I/O.** `Decoder::decode_with_cb` takes one complete field block as
`&[u8]` and calls back with `(name, value)` for each field;
`Encoder::encode_header_into` appends to a `Vec<u8>` the caller owns. The
state is explicit: the dynamic table, the table-size bound, and one scratch
buffer. Errors are the typed `DecoderError`, every variant a
COMPRESSION_ERROR. The block must be complete: `h2_header_reassembly.rs`
joins HEADERS and CONTINUATION fragments before `pkawa.rs` decodes them.

**Zero copy, zero allocation once warm.** The callback's two `Cow`s are
always borrowed:

| Source of the octets | Where the callback's slice points |
|---|---|
| String sent raw | the block itself |
| Indexed field or indexed name | the static table, or the dynamic table |
| Huffman string | the decoder's scratch buffer, cleared per field |

Huffman decoding reads a 256-state × 16-nibble transition table that a
`const fn` derives from RFC 7541 Appendix B while the crate compiles, the
layout of nghttp2's `huff_decode_table`. A string decodes without building
or allocating anything. The dynamic table stores all its entries back to
back in one byte buffer, with a ring of offsets beside it. Eviction moves a
start offset, and the live bytes are compacted to the front only when the
evicted prefix is at least as long as them. The three buffers (scratch,
table bytes, offset ring) grow to a high-water mark and are then reused; a
steady-state block allocates nothing.
`hpack::tests::a_steady_state_block_allocates_nothing` and
`hpack::tests::huffman_decoding_allocates_nothing` hold that at zero.
Against `loona-hpack` the same two measurements were 257 and 64
allocations. `HpackState::reclaim_idle_buffers` releases the scratch buffer
on the quiet-time path with the other scratch buffers.

**Guards.** Every one is a `DecoderError`, never a panic:

- an integer longer than five octets;
- a block ending inside an integer or a string;
- index 0, or an index past both tables;
- a size update above the advertised `SETTINGS_HEADER_TABLE_SIZE`, after a
  field, beyond the two §4.2 allows, or ending the block;
- EOS inside a Huffman string, padding that is not all ones, or padding
  longer than seven bits.

The header-list budget stays in `decode_headers_with_budget`, which counts
what the callback receives. The decoder decodes the whole block past that
budget, because the table must stay in step with the peer's encoder.

**Encoder policy.** `Representation::Proxy` is what sozu sends, and it
reproduces `loona-hpack`'s choices so the wire does not change:

- a field found whole is indexed;
- a name found is a literal without indexing, naming the *last* entry that
  holds that name;
- a new name is a literal with incremental indexing and enters the table;
- strings are never Huffman-coded.

The other representations (`IncrementalIndexing`, `WithoutIndexing`,
`NeverIndexed`, with or without Huffman coding) reproduce RFC 7541
Appendix C, and let a test peer or a fuzz target produce every shape a
decoder meets.

**Tests.** From the codec alone to the running proxy:

| Level | Where | What |
|---|---|---|
| Unit | `hpack/tests.rs` | Appendix C.1 to C.6 in decoding and encoding; one test per malformed input and its error; blocks split at every boundary; size changes between blocks; quickcheck round trips; the two allocation criteria; a seeded simulation of one encoder and one decoder whose tables are compared after every block (`SOZU_HPACK_SIM_SEED` replays a seed) |
| Fuzz | `fuzz_hpack_decoder`, `fuzz_hpack_roundtrip` | Arbitrary blocks; scripted encoder → decoder round trips |
| Simulation | `sim/tests/h2_simulation.rs`, `h2_hpack_dynamic_table_stays_in_step_under_fragmentation` | The H2 core decodes dynamic-table references and size updates across fragmented reads and CONTINUATION splits, against a table model written from the RFC |
| E2E | `e2e/src/tests/h2_hpack_tests.rs` | A running Sōzu: indexed fields, table reuse across requests, `SETTINGS_HEADER_TABLE_SIZE` changes, Huffman and raw strings, a list past the limit, invalid blocks answered with GOAWAY(COMPRESSION_ERROR) |

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
| `e2e/src/tests/h2_hpack_tests.rs` | HPACK through a running Sōzu: indexed fields, dynamic-table reuse over fragmented blocks, table-size changes, Huffman, size limit, COMPRESSION_ERROR |
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

```rust lib/src/protocol/mux/h2.rs:24-27
const _: () = assert!(
    std::mem::size_of::<usize>() >= 4,
    "sozu requires at least 32-bit pointers"
);
```
