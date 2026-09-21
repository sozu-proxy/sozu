# E2E protocol-pair matrix — coverage spec

This document is the index for the four-cell `(frontend × backend)`
protocol-pair matrix that v2.x e2e tests opt into. The harness already
exists at `e2e/src/tests/tests.rs::try_tls_cardinality_cell`; this
document captures **how to use it**, **where to apply it**, and the
**applicable-cells-only** rule.

## The matrix

| Cell     | Frontend        | Backend       | Mock backend                 | Notes                                                                         |
| -------- | --------------- | ------------- | ---------------------------- | ----------------------------------------------------------------------------- |
| `h1-h1`  | h1 + TLS        | h1 cleartext  | `AsyncBackend::http_handler` | Baseline; covered by `try_tls_endpoint`, `try_tls_rsa_2048`, `try_tls_ecdsa`. |
| `h1-h2c` | h1 + TLS        | h2c cleartext | `H2Backend::start`           | Covered by `try_tls_cardinality_h1_h2_*`.                                     |
| `h2-h1`  | h2 + TLS (ALPN) | h1 cleartext  | `AsyncBackend::http_handler` | Covered by `try_tls_cardinality_h2_h1_*`.                                     |
| `h2-h2c` | h2 + TLS (ALPN) | h2c cleartext | `H2Backend::start`           | Covered by `try_tls_cardinality_h2_h2_*`.                                     |

Backend **TLS** (h1-over-TLS, h2-over-TLS) is **not** in the current
release; the follow-up enhancement is tracked at
[#1218](https://github.com/sozu-proxy/sozu/issues/1218). At that point
the matrix grows to 8 cells (each backend axis gains a TLS variant).

## How the harness works

```rust
// Existing API in e2e/src/tests/tests.rs (around line 660):
pub fn try_tls_cardinality_cell(
    test_name: &str,
    cert_pem: &str,
    key_pem: &str,
    tls_versions: Option<Vec<TlsVersion>>,
    frontend_h2: bool,   // false = H1+TLS, true = H2+TLS
    backend_h2: bool,    // false = h1 cleartext, true = h2c cleartext
) -> State { ... }
```

For each cell:

- Frontend ALPN preference is set indirectly via the chosen Hyper client
  (`build_https_client` for H1, `build_h2_client` for H2). rustls
  negotiates the cell's protocol.
- Backend protocol is selected by `cluster.http2 = Some(true|false)` plus
  the appropriate mock (`H2Backend` for h2c, `AsyncBackend` for H1).

Existing 4-cell wrappers (RSA + ECDSA cert kinds × 2 cardinality cells
that aren't trivially covered by the H1-baseline tests):

- `try_tls_cardinality_h1_h2_rsa` / `…_ecdsa` — H1 frontend + h2c backend
- `try_tls_cardinality_h2_h1_rsa` / `…_ecdsa` — H2 frontend + H1 backend
- `try_tls_cardinality_h2_h2_rsa` / `…_ecdsa` — H2 frontend + h2c backend

The H1+TLS / H1-backend cell is the legacy baseline — `try_tls_endpoint`
covers it.

## "Applicable cells only" rule

Not every test makes sense in every cell. Tests opt into the cells they
actually exercise. The decision matrix:

| Feature under test               | h1-h1                | h1-h2c | h2-h1 | h2-h2c | Notes                                                               |
| -------------------------------- | -------------------- | ------ | ----- | ------ | ------------------------------------------------------------------- |
| HTTP Basic auth                  | ✓                    | ✓      | ✓     | ✓      | All cells; auth gate is router-layer.                               |
| 301/302/308 redirect             | ✓                    | ✓      | ✓     | ✓      | All cells; redirect renders same answer-template.                   |
| X-Real-IP injection              | (✓)                  | ✓      | (✓)   | ✓      | H1-backend cells (h1-h1, h2-h1) assert frontend forwarding only — `AsyncBackend` does not currently expose request bytes for the strip/inject assertion (tracked as a future `AsyncBackend` enhancement). H2 trailer-elision needs H2 frontend cells specifically. |
| Per-IP `429` limit               | ✓                    | ✓      | ✓     | ✓      | All cells; one of each cert kind is enough.                         |
| `evict_on_queue_full`            | ✓                    | ✓      | ✓     | ✓      | All cells.                                                          |
| Custom answer template           | ✓                    | ✓      | ✓     | ✓      | All cells.                                                          |
| RFC 9218 priority                | ✗                    | ✗      | ✓     | ✓      | H2 frontend only — RFC 9218 is HTTP/2 priorities.                   |
| HPACK rejection counters         | ✗                    | ✗      | ✓     | ✓      | H2 frontend only — HPACK is H2 wire.                                |
| H2 flood detector                | ✗                    | ✗      | ✓     | ✓      | H2 frontend only.                                                   |
| HTTP/1 pipelining                | ✓                    | ✓      | ✗     | ✗      | H1 frontend only — H2 multiplexing replaces pipelining.             |
| H2 trailer header elision        | ✗                    | ✗      | ✓     | ✓      | H2 frontend only — trailers are H2 frames.                          |
| RFC 7239 `Forwarded` (planned)   | ✓                    | ✓      | ✓     | ✓      | All cells; header semantic is protocol-agnostic.                    |
| Backend TLS / mTLS-up (planned)  | (new TLS-axis cells) |        |       |        | Adds 4 TLS-backend cells; matrix grows to 8.                        |
| ACME HTTP-01 challenge (planned) | ✓                    | ✓      | ✓     | ✓      | Challenge route is HTTP/1.1 GET on port 80; ALPN-agnostic.          |
| OCSP stapling (planned)          | ✓                    | ✓      | ✓     | ✓      | Stapling is TLS-handshake, before HTTP.                             |

When adding a test, declare the applicable cells in a comment and use
the relevant `try_tls_cardinality_*` wrapper(s).

## Convenience pattern

`e2e/src/tests/protocol_pair_matrix.rs` exposes a `protocol_pair_matrix!`
macro that emits the four wrapper functions plus their `#[test]`
harnesses for a single feature cell function:

```rust
// Cell function: takes (frontend_h2, backend_h2), returns State.
fn try_basic_auth_cell(frontend_h2: bool, backend_h2: bool) -> State { ... }

// Macro emits `pub mod basic_auth { ... }` containing
// try_h1_h1 / try_h1_h2 / try_h2_h1 / try_h2_h2 wrappers plus
// matching test_h1_h1 / test_h1_h2 / test_h2_h1 / test_h2_h2
// `#[test]` harnesses that wrap the cell in `repeat_until_error_or`.
// `cargo test basic_auth` filters all four cells.
protocol_pair_matrix!(basic_auth, try_basic_auth_cell, "basic auth");
```

Hand-authored wrappers (e.g. the cardinality smoke tests in `tests.rs`)
remain valid for cases that need a custom `repeat` count or a non-cell
shape. The macro covers the common case where each cell is one
`(frontend_h2, backend_h2)` boolean pair.

## Priority-1 backfill

Landed in `e2e/src/tests/protocol_pair_matrix.rs` (16 tests, four cells
each):

- **Basic auth** (`required_auth = true` + `authorized_hashes`) — three
  arms per cell: missing header → 401, wrong creds → 401, correct
  creds → 200 from the backend.
- **301 redirect** (`RedirectPolicy::Permanent`) — backend never
  contacted; the response carries `:status 301` (H2) /
  `HTTP/1.1 301` (H1) plus a `Location` header.
- **Custom answer template** (`Cluster.answers."503"`) — operator's
  503 template renders verbatim (verified via load-bearing
  `X-Sozu-Stamp` header that the listener default does not emit).
- **X-Real-IP elide + send** (`with_elide_x_real_ip` +
  `with_send_x_real_ip` on the HTTPS listener) — client-supplied
  spoof stripped, proxy-generated header reaches the backend.
  Initial-HEADERS only; H2 trailer-frame elision is a separate
  targeted test scaffolded at `tests::test_x_real_ip_elide_h2_trailer`.

Deferred from matrix coverage (existing single-cell tests stay
authoritative until connection-pinning helpers land):

- **Per-IP `429` limit** — `cluster_ip_limit_tests.rs` exercises the
  H1 cleartext path. Matrix coverage needs sustained TLS + HTTP/2
  connection holds while a second connection races; the current
  Hyper client pool reuses connections opaquely, so saturating the
  per-IP slot from a parallel Hyper client is unreliable.
- **`evict_on_queue_full`** — `eviction_tests.rs` exercises the H1
  cleartext path with raw TCP socket holds. Same connection-pinning
  prerequisite as 429.

Each feature × cell pair runs in `repeat_until_error_or(2, ...)` so a
single transient failure surfaces as a stable fail — the harness
matches the existing redirect/auth tests' retry budget.

## Out of e2e reach by construction

Not every gap is a backfill. Some code cannot be reached from this suite
at all, and the reason is structural rather than a missing helper — a
test written against it would pass for the wrong reason. The mechanism
is recorded here so the next person does not re-derive it.

- **`protocol::kawa_h1::Http` — resolved by deletion on 2026-09-20, kept
  here as the worked example.** The `Http` session state machine, its
  `SessionState` impl, `TimeoutStatus`, `ResponseStream`,
  `save_http_status_metric`, `handle_connection_result` and the whole
  `kawa_h1::diagnostics` module were unreachable in **any** binary: no
  session state machine had a variant holding one — `HttpStateMachine` is
  `Expect | Mux | WebSocket` (`lib/src/http.rs`) and `HttpsStateMachine`
  is `Expect | Handshake | Mux | WebSocket` (`lib/src/https.rs`) — and
  `Http::new` had zero code callers under either module spelling
  (`crate::protocol::kawa_h1::` and the `crate::protocol::http::`
  re-export). That was measured, not inferred: an unconditional
  `panic!("PROBEALWAYS …")` planted at the top of both
  `save_http_status_metric` and `Http::new` fired 0 times across four real
  proxied HTTP/HTTPS e2e sessions, while the same planted binary panicked
  immediately under the function's own unit test (the positive control).
  sozu#1346 removed all of it; sozu#1347, a frontend timeout consumed
  without re-arming, was closed by that removal rather than patched.

  The rest of the module is *not* dead and *is* e2e-reachable:
  `kawa_h1::editor::HttpContext`, `kawa_h1::answers` (`HttpAnswers`,
  `DefaultAnswerStream`, `merge_legacy_into_map`), `kawa_h1::parser`
  (`Method`, `hostname_and_port`) and the `DefaultAnswer` enum are what
  `mux` builds on.

  Consequence for coverage, and the reusable lesson: the unit test that
  guarded the dead bucketer,
  `kawa_h1::tests::a_backend_status_line_below_100_is_bucketed_not_asserted`,
  was ported onto the live path as
  `mux::stream::tests::a_backend_status_line_below_100_is_bucketed_not_asserted`
  rather than deleted with the code — a unit test on an unreachable
  function reads as protocol coverage and is not. Its
  control-plane-reachable sibling, an operator answer template carrying an
  out-of-range status, *is* e2e-covered by
  `tests::h1_security_tests::test_h1_custom_answer_with_out_of_range_status_does_not_kill_the_worker`.
  Before writing a test for a defect on a quiet path, plant the `panic!`
  and run the suite: it settles reachability in one run.

- **Rendered log content, including the `peer=` slot of a `MUX-H2` line
  — no longer out of reach.** This entry used to say the harness could
  not observe a worker's log output *at all*, and named the change that
  would falsify it. That change has been made, so what follows is the
  reachable surface, its price, and the residue that is still out of
  reach.

  Three mechanisms used to stand in the way, each sufficient on its own.
  The worker's log target was the hardcoded string `"stdout"`
  (`setup_default_logging(false, "error", &thread_name)`);
  `LoggerBackend::Stdout` writes through a `std::io::Stdout` handle
  (`command/src/logging/logs.rs:280`), not the `print!` path libtest
  captures, so even `--nocapture` yielded the test no `String`; and
  `LOGGER` is a `thread_local!` (`logs.rs:24`) with a one-shot
  `initialized` guard (`logs.rs:191`), so the worker thread's logger is
  not the test thread's.

  Only the first was load-bearing. `Worker::start_new_worker_with_logging`
  and `Worker::start_new_worker_owned_with_logging`
  (`e2e/src/sozu/worker.rs`) take a `target_to_backend` target string and
  a `parse_logging_spec` level spec, and install them on the worker's own
  thread; `WorkerLogCapture` (`e2e/src/sozu/log_capture.rs`) owns a
  `tempfile::TempDir`, hands out the matching `file://` target and reads
  the lines back. The third mechanism turns into a *property*: each
  worker owns its logger, so a per-worker file needs no `serial_test`
  serialisation. The additions are purely additive — `start_new_worker`
  and `start_new_worker_owned` still make the same
  `setup_default_logging` call they always did, so every other worker in
  the suite keeps target `"stdout"`, level `"error"` and `RUST_LOG`
  semantics unchanged.

  Worked example:
  `tests::h2_log_context_tests::test_h2_proxy_protocol_peer_is_the_advertised_client`
  drives a real PROXY-v2 header through the real accept path
  (`upgrade_expect` → `upgrade_handshake` → `FrontRustls::configured_peer`
  → `SocketHandler::peer_addr`) and asserts that every `peer=` slot of
  the `MUX-H2` lines the worker wrote names the PROXY-advertised client
  and none names the connection's raw TCP peer. Under the pre-fix macro
  it measures 27 `peer=` slots, 0 advertised, 26 raw and one `peer=None`
  — the `ENOTCONN` rendering the fix also removes.

  Second worked example, taking the other branch of cost 1 below:
  `tests::socket_log_context_tests::test_tls_socket_log_peer_is_the_advertised_client`
  pins the `peer=` slot of a `SOCKET` line on the same kind of frontend.
  Every `log_socket_context!` expansion in `lib/src/socket.rs` is an
  `error!`, and each needs an abnormal condition, so a healthy TLS
  session emits no `SOCKET` line at ANY level and raising one cannot
  help. That test therefore stays at plain `"error"` and provokes
  instead: it establishes the session, then writes one undecryptable TLS
  record straight onto the TCP socket, which reaches
  `FrontRustls::socket_read`'s `process_new_packets` arm while the
  connection underneath is still `ESTABLISHED`. Keeping the connection
  healthy is the point — it makes `getpeername(2)` succeed and answer the
  wrong address, which is the half of the defect that is not `ENOTCONN`.
  Under the pre-fix macro it measures 1 `SOCKET` line, 0 advertised, 1
  raw. Keeping the connection alive is load-bearing for WHICH half is
  proven rather than for redness: a dead-socket provocation would still
  redden the test, since `peer=None` also fails the "every slot names the
  advertised client" check; what only a live connection buys is the
  negative assertion that no slot names the raw TCP peer. The `ENOTCONN`
  half is pinned by the unit test
  `socket::tests::log_socket_context_renders_the_cached_peer_when_the_live_lookup_fails`
  instead, staged with a never-connected socket because only that refuses
  `getpeername(2)` deterministically.
  One claim here is reasoned, not measured, and is flagged as such in the
  test's own module note: that a corrupt record sent before the server
  has read the client's `Finished` would fail inside `protocol/rustls.rs`
  and log `RUSTLS` rather than `SOCKET`. That is read off the state
  machine; no test drives it.

  **What it costs.** Four things, all measured:

  1. *The level.* A HEALTHY H2 session emits no `MUX-H2` line at
     `"error"` at all: every `log_context!` expansion on a clean
     request/response path is a `trace!`, and the `debug!`/`warn!`/
     `error!` ones each need an abnormal condition. Either raise the
     captured level or provoke an error path, deliberately. Scope the
     raise — `"error,sozu_lib::protocol::mux=trace"` keeps the capture in
     the tens of kilobytes, because a directive name is matched against
     the call site's `module_path!()` by prefix.
  2. *The build profile.* `debug!` and `trace!` are compiled in under
     `any(debug_assertions, feature = "logs-debug"/"logs-trace")`. Every
     CI cell runs `cargo test` in the dev profile, so they are present; a
     `--release` e2e run without those features captures nothing below
     `info`, and a capture test should say so in its failure message
     rather than look like a logic failure.
  3. *When to read.* The `file://` backend is a `MultiLineWriter` with a
     4096-byte buffer that flushes on overflow and on `Drop`. That `Drop`
     is the worker thread's thread-local teardown, so read the capture
     only after `Worker::wait_for_server_stop`, which joins it.
  4. *Not the UDP drain.* Do **not** reuse `capture_test_logs_at_level`
     (`lib/src/lib.rs:1682`): it drains only once the run has finished
     and so depends on the kernel socket buffer having held every
     datagram — true for the handful of lines a unit test emits, false
     under an H2 conversation at `trace`. It drops lines silently and
     produces exactly the load-sensitive flake this suite already has a
     family of. `file://` has no loss mode.

  **What is still out of reach.** The target and level must be chosen at
  spawn time and cannot be changed afterwards — the one-shot
  `initialized` guard is untouched, and nothing retargets a running
  worker. The override sets the main log target only; `access_logs_target`
  stays `None` on this path, so access-log *content* remains
  unobservable from e2e. And a `log_context_lite!` line (`h2.rs:139`)
  carries the `MUX-H2` tag with no session block, so an assertion over
  `peer=` must select the lines that have the slot rather than every
  tagged line — the worked example counts 28 tagged lines and 27 slots.

  Consequence for coverage: a defect whose only observable is a log line
  is now guardable end-to-end, at the price of naming a level. The unit
  tests that render the macro directly
  (`protocol::mux::h2::tests::log_context_renders_the_cached_peer_not_a_live_lookup`,
  its `log_context_stream` sibling, and
  `https::tests::front_rustls_peer_snapshot_is_the_session_peer_not_the_accepted_socket`)
  stay: they are faster and they pin the seeding directly. The wire
  choreography around them is covered by
  `tests::h2_tests::test_h2_with_proxy_protocol_v2` and the
  `tests::proxy_protocol_local_tests` pair — still do not bolt a `peer=`
  claim onto one of those, because they capture nothing; write the claim
  where the capture is.

## Clock-driven behaviour: what the wire can falsify

A deadline or a rate window has no wire representation of its own — only
its *consequences* do. That makes a whole class of change untestable from
e2e by construction, and a neighbouring class very testable, and the two
are easy to confuse.

**Not falsifiable end-to-end: which clock a deadline reads.** The H2 core
samples `Context::now` once per `Mux::ready` pass and `ConnectionH2`
mirrors it into `self.now`, so a burst of frames is weighed against ONE
instant instead of one `Instant::now()` per frame. Reverting any of those
reads to a fresh `Instant::now()` yields the same elapsed time to within
an event-loop pass, so every e2e test stays green. A test claiming to
guard that property guards nothing; the single-snapshot invariant belongs
to the unit tests that inject an instant
(`h2.rs::tests::{graceful_shutdown_deadline_is_evaluated_against_the_connection_snapshot,
settings_ack_deadline_is_evaluated_against_the_connection_snapshot}`).

**Falsifiable end-to-end: the behaviour the clock drives.** Four
properties of a deadline show on the wire, and each has a one-line
production mutation that reddens the test:

| Property | Covered by | Reddened by |
| --- | --- | --- |
| A rate window really decays | `h2_clock_tests.rs::test_h2_flood_window_decays_between_bursts` | `FLOOD_WINDOW_DURATION` → one hour: 120 sub-threshold PINGs accumulate and the 101st draws `GOAWAY(ENHANCE_YOUR_CALM)` |
| A deadline fires at all | `h2_clock_tests.rs::test_h2_settings_ack_timeout_goaways_the_frontend` | `SETTINGS_ACK_TIMEOUT` → one hour: no GOAWAY in 25 s |
| It does not fire early | same test's early probe | flipping either `>= SETTINGS_ACK_TIMEOUT` guard to `<`: GOAWAY at 2.5 s |
| It carries the right error code | same test | any GOAWAY reason other than `SETTINGS_TIMEOUT` (0x4) |

The flood tests in `h2_tests.rs` (CVE-2019-9512 PING, CVE-2019-9515
SETTINGS, CVE-2024-27316 CONTINUATION) only ever prove a threshold
*trips*. Their negative space — a peer that stays under the threshold in
every window but exceeds it cumulatively — is what catches a window that
stopped advancing, which is the false-positive half of the CVE control
and the failure mode a frozen clock produces.

**A deadline needs an event to be observed.** The SETTINGS-ACK watchdog
is evaluated inside `readable()` and `flush_pending_control_frames()`
only, so a silent connection is never re-examined and the GOAWAY does not
arrive on its own. The test pokes with a PING every 500 ms past the
budget; a test that merely waits on a quiet socket would time out and
read as a missing deadline. Check where a deadline is evaluated before
concluding it did not fire.

## Status assertions: what a HEADERS block can falsify

An H2 response status is a decode, not a search. The seven `:status` rows
of the RFC 7541 static table (indices 8..=14 → 200, 204, 206, 304, 400,
404, 500) are emitted as a single indexed byte; every other status is a
literal over one of those same name indices, three unhuffmanned ASCII
digits long. RFC 9113 §8.3.2 puts the field first in the block, so
`decode_status` (`e2e/src/tests/h2_utils.rs`) reads byte 0 and stops.
Nothing later in the block can be mistaken for the status.

**What the old byte scan could not falsify.** Until 2026-09-20 three
copies of `payload.windows(3).any(|w| w == b"421")` stood in this suite.
That question has no negative space: a plain 200 answers yes whenever the
digits happen to sit side by side anywhere in the field block, and one
header guarantees they eventually will. Every Sōzu response carries
`Sozu-Id`, the session's 26-character Crockford base-32 ULID
(`HttpContext::on_response_headers`, `lib/src/protocol/kawa_h1/editor.rs`),
whose alphabet is `0-9A-Z`-minus-`ILOU` and which the encoder writes as plain ASCII — a
captured example is `01M2Z5AGKTYKJM9MFQY89EJMJ9`. Any given 3-digit
needle hits about once in 1400 ULIDs. The first ten characters are the
generation timestamp in milliseconds, so a hit there is not independent
between runs: it holds for 1 ms, 32 ms, ~1 s, ~33 s or ~17.5 min
depending on which triple it occupies — and ~9.3 h, ~12.4 d or ~1.1 y for
the three slowest. All 24 windows are a priori equally likely — the slow
ones are not rarer per draw, their characters are simply fixed for a
whole era, so a hit there is a property of the epoch rather than of a
run. Every response generated inside the window carries it.

That is the whole of issue #1353. `strict-off FAIL: infra_ok=true
got_ok=true got_421=true metric_stable=true foo_reqs=0 bar_reqs=1` is not
a race between a 421 and a proxied response — it is one proxied 200 whose
correlation id contained `421`. It is **not** a flake and does not belong
on a flake list: the wire was right, the reading was wrong, and the
reading is fixed. Contrast the genuinely load-sensitive entries in this
suite, which assert on a wall-clock budget rather than on content.

**The dangerous copy was not the one in the reported test.**
`try_strict_sni_binding_toggle` (`e2e/src/tests/listener_update_tests.rs`)
ran the same scan and fed its result into
`got_rejection_or_421 = got_421 || contains_goaway(..)`, which gates
`phase1_ok` — the assertion that a `strict_sni_binding=true` listener
*rejects* a mismatched `:authority`. A ULID false positive there turns a
security assertion silently green rather than red. Direction matters when
pricing one of these: #1353 cost a re-run, this one would have cost the
guard. Both are decoded now, along with `h2_tests.rs`, whose status check used
to be ORed with a match on the answer body (`""status_code": 404"`).
That disjunct widened the needle rather than guarding it and has been
removed: `test_h2_default_answer_terminates_stream` passes on the
decoded `:status` alone.

**Keep the negative half — and check that you have one.**
`h2_status_checks_decode_the_status_field_not_any_matching_bytes` asserts
in both directions, but only after a correction worth recording. Its
first fixture, a captured 200 whose `Sozu-Id` contains `421`, falsifies
the digit scan. Its second, a captured 421, was *not* enough to falsify
the companion `payload.contains(&0x88)` scan in `headers_ok_response`,
because that captured block is pure ASCII and so holds no `0x88` byte.
Reverting that one function left the test green — an assertion that
looked like a guard and was not. The third fixture,
`stray_indexed_200`, is a captured 421 block with a trailing `0x88`.
When a `To SEE THIS RED:` names two mutations, run both.

**`0x88` in a block does not mean `:status 200`.** An earlier draft of
this section justified that third fixture by claiming the byte was
unreachable off the wire. It is not, and the correction matters more
than the fixture. RFC 7541 §5.1 encodes a string length ≥ 127 as a
7-bit prefix plus continuation octets of `(len % 128) + 128`, i.e.
`0x80..=0xFF` by construction: a 263-byte header value writes
`7f 88 01`, and 1542 distinct lengths below 100 000 put a `0x88` octet
in the block. A raw value byte ≥ `0x80` is a second route —
`lib/src/protocol/mux/converter.rs:388-391` rejects only
`0x00..=0x08 | 0x0A..=0x1F | 0x7F` and passes everything else through
verbatim. `headers_ok_response` runs on *proxied* responses in the
strict-off and coalescing tests, where backend headers are arbitrary, so
a 263-byte `Location`, `Set-Cookie` or CSP header was enough to make the
old scan report a 2xx on a 421. The narrow claim — `0x88` cannot occur
inside an unhuffmanned ASCII *value* — is the only true one.

**Still scanning, and no longer rated low risk.** Measured on
2026-09-20, 19 indexed-status byte probes remain: 2 × `contains(&0x88)`
and 17 × `contains(&0x8D)`, in `e2e/src/tests/h2_security_header_injection.rs`
(3) and `e2e/src/tests/h2_security_tests.rs` (16). They are the same
class as #1353 and carry the same exposure — a 268-byte value writes
`7f 8d 01` — and several of them sit on the permissive side of an `||`
(`rejected = got_rst || got_goaway || got_400`,
`h2_security_tests.rs:589`), which is the inverted direction again.
Converting them to `decode_status` is a follow-up, not chased here.

Two things to settle before that conversion. First, `0x8D` is static
index **13 = `:status 404`**, not 400: `:status 400` is index 12 =
`0x8C`. Every one of those 17 sites, and the comments above them
(`h2_security_tests.rs:577`, `:658`,
`h2_security_header_injection.rs:73`, `:542`), names it 400, and the
variable is `got_400` at all 15 sites in `h2_security_tests.rs`. So
either the assertion or its label is wrong at every one of them, and a
mechanical `headers_status_matches(.., b"400")` rewrite would change
behaviour rather than preserve it. Second, `decode_status` returns
`None` on a size-update-prefixed block, which is fail-closed for a
`got_X` used positively and fail-**open** for one used as `|| !got_X`.

## Backend-TLS expansion (preview)

When backend TLS lands ([#1218](https://github.com/sozu-proxy/sozu/issues/1218)),
the cardinality helper's `backend_h2: bool` axis grows to a
`backend: BackendKind { H1Cleartext, H2cCleartext, H1Tls, H2Tls }`
variant. Tests already opting into the matrix get the new axis "for
free" via the macro proposed above.

## Mock backend taxonomy

| Mock                    | h1-h1 | h1-h2c | h2-h1 | h2-h2c | Notes                                                            |
| ----------------------- | ----- | ------ | ----- | ------ | ---------------------------------------------------------------- |
| `AsyncBackend`          | ✓     | ✗      | ✓     | ✗      | H1 only on the wire.                                             |
| `SyncBackend`           | ✓     | ✗      | ✓     | ✗      | Synchronous H1.                                                  |
| `H2Backend`             | ✗     | ✓      | ✗     | ✓      | h2c only.                                                        |
| `RawH2ResponseBackend`  | ✗     | ✓      | ✗     | ✓      | Adversarial H2 — for security tests, not protocol-pair backfill. |
| `ChunkedFlushH1Backend` | ✓     | ✗      | ✓     | ✗      | H1-only chunked-encoding repros.                                 |
| `SingleReadH1Backend`   | ✓     | ✗      | ✓     | ✗      | H1-only single-read repros.                                      |

Tests opt into mocks that match their target cells. Don't pair
`H2Backend` with an `h1-h1` cell — the cluster still has
`http2 = false` and would reject the connection preface bytes.

## Where the matrix lives in CI

The CI feature-coverage matrix at `.github/workflows/ci.yml` runs each
crypto provider × full features (`opentelemetry,splice,simd`) over the
same e2e suite. The 4-cell protocol matrix runs **inside each CI cell**.
Total:

```
CI cells × tested protocol-pair cells × applicable-features ⊆ matrix surface
```

Coverage gaps are listed in the priority-1 backfill table above. A test
covering a feature that ships in a release without any matrix backfill
is a release-blocker.

## Related

- `e2e/src/tests/tests.rs::try_tls_cardinality_cell` — harness implementation.
- `e2e/src/mock/` — mock backends.
