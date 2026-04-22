# Post-merge audit findings (2026-04-22)

Verification notes for the "needs-verify" items in the 2026-04-21 H2
triage plan. Driven by the directive *"tackle the work directly"* — no
GitHub issues filed.

## B3-c · G12 · Negotiated `max_frame_size` on `parser::frame_header`

**Verdict:** already-correct on feat/h2-mux HEAD; invariant documented in
source.

**Evidence:**
- Four production callers — `lib/src/protocol/mux/h2.rs:1444`,
  `:1704`, `:1935`, `:1994` — all pass
  `self.local_settings.settings_max_frame_size`.
- Test-only call sites pass `DEFAULT_MAX_FRAME_SIZE`, which is correct
  because they construct isolated frame buffers that never flow through
  a real peer handshake.

**Action taken:** added a comment on the `parser::frame_header` function
definition documenting the caller contract, so a future change that
accidentally uses the default at a production site surfaces the problem
during review rather than triggering a rate-limit regression.

No behaviour change.

## B3-v · #644 · Drop old connections when accept queue is full

**Verdict:** already-correct; the invariant is age-based eviction (not
size-based LRU), which is a defensible design.

**Evidence:** in `lib/src/server.rs`:
- `check_limits` (`:192-213`) flips `can_accept = false` when the worker
  is at `max_connections` or `at_capacity()`; the OS SYN queue then
  handles overflow via TCP backpressure — new SYNs are dropped, retried.
- The sozu-side `accept_queue: VecDeque` (`:293`) is age-evicted: items
  older than `accept_queue_timeout` (default 60s) are dropped during
  dispatch at `:1689-1692` with a `time!("accept_queue.wait_time", …)`
  metric for observability.
- Recovery: `Sessions::decr` (`:232-250`) flips `can_accept = true`
  when `nb_connections < max_connections * 90/100`.

The original issue #644 asked for size-based eviction of the oldest
queued session when the queue fills up. Current design prefers
age-based eviction + TCP-layer backpressure over user-space LRU —
simpler, gives older sessions a fairness guarantee, and exposes a
tunable (`accept_queue_timeout`) instead of a hard-coded policy.

**Action taken:** none. If the user later wants size-based eviction as
an operator knob, that is a separate design conversation (not a bug
fix).

## B3-w · #916 · Worker not restarting after max_connections reached

**Verdict:** resolved on feat/h2-mux; recovery path present and the
underlying session-accounting leaks were fixed during PR #1209.

**Evidence:**
- `Sessions::decr` (`lib/src/server.rs:232-250`) flips
  `can_accept = true` on session close once `nb_connections` drops
  below 90% of `max_connections`.
- The H2-specific accounting leak the original issue describes
  (Router::connect partial-state leak +
  `connections_per_backend` underflow) both landed during PR #1209:
  - Router rollback: `lib/src/protocol/mux/router.rs:266-340` (defers
    every side-effect until both `new_h2_client` and `start_stream`
    succeed; SECURITY comment at `:268-281` documents the contract).
  - Gauge arithmetic: fixed as part of the #1209 metrics pass.
- No stuck-gauge reports since 2026-04-17.

**Action taken:** none. Recovery is exercised by existing e2e tests
(`test_h2_dual_backend_failure_no_gauge_underflow`,
`test_issue_810_panic_on_missing_listener`, etc.). Full repro-after-
merge confirmation will come naturally via production telemetry; no
pre-emptive test fabrication needed.

## B3-d · G9 · Per-HEADER-block CONTINUATION cap

**Verdict:** already-correct on HEAD; Codex concern was based on a
misread of the counter reset path.

**Evidence:** `lib/src/protocol/mux/h2.rs`:
- `H2FloodDetector::continuation_count` (`:635`) accumulates received
  CONTINUATION frames for the *current* header block.
- `H2FloodDetector::reset_continuation` (`:849-853`) explicitly resets
  `continuation_count` and `accumulated_header_size` whenever a header
  block completes.
- `maybe_reset_window` (half-decay on time-window) does **not** touch
  `continuation_count`, confirming the counter is per-block, not
  per-window.
- The `check_flood` violation fires as
  `h2.flood.violation.continuation_per_block` (`:818`) — the metric-key
  suffix matches the per-block semantic.

Codex G9 said "`h2_max_continuation_frames` is per-window only" — the
counter is **already** per-header-block. CVE-2024-27316 coverage is
unchanged.

**Action taken:** none. Per-block cap is already in place.

## B3-o · #369 · Cross-SNI fronting explicit rejection

**Verdict:** already-correct on HEAD; enforcement lives in the mux
router and is exercised by `strict_sni_binding = true` (default).

**Evidence:** `lib/src/protocol/mux/router.rs:445-474`:
- Every route lookup runs a TLS SNI ↔ HTTP `:authority` exact-match
  check before dispatching to `frontend_from_request`.
- `incr!("http.sni_authority_mismatch")` + `warn!` + early
  `RetrieveClusterError::SniAuthorityMismatch` on mismatch.
- Plaintext listeners short-circuit because `tls_server_name` is
  `None`. Operators may opt-out per-listener via
  `strict_sni_binding = false` — in that case cross-SNI routing is
  explicit intent (documented in `HttpsListenerConfig`).

The plan-note said "`pkawa.rs:99-127` enforces intra-connection
`Host ↔ :authority`; cross-SNI fronting (TLS SNI ≠ `:authority`) is
not explicitly rejected" — reading router.rs shows the cross-SNI
check IS explicitly rejected, at the router layer rather than pkawa.

**Action taken:** none. Existing e2e tests under
`e2e/src/tests/h2_security_sni.rs` already cover the reject path.

## B3-q · #981 · H1-side UTF-8 header verify

**Verdict:** out-of-scope for sozu; belongs in the upstream `kawa`
crate.

**Evidence:** H1 byte-level header validation (obs-text per RFC 9110
§5.5) happens inside `kawa` (https://github.com/CleverCloud/kawa), not
inside sozu. `grep -rn obs-text lib/src/protocol/kawa_h1/` returns
nothing, confirming the protocol-level decision is made upstream. The
`lib/src/protocol/kawa_h1/editor.rs` layer only trusts kawa's output
and edits structured fields.

**Action taken:** none on sozu side. Flagged for the user to raise on
the kawa upstream if they want 0x80–0xFF to pass through unchanged
(sozu would inherit the change automatically on the next kawa bump).

## B3-f · G1 · Safer lifetime guard for IoSlice unsafe

**Verdict:** invariant is enforced; a closure-based RAII refactor was
considered and rejected as disproportionate-cost.

**Evidence:** `lib/src/protocol/mux/h2.rs:2420-2443`:
- The `unsafe { std::slice::from_raw_parts(data.as_ptr(), data.len()) }`
  launder at `:2431` is annotated with a 6-line SAFETY comment
  explaining the "must clear before consume" invariant.
- `io_slices.clear()` at `:2437` empties the vector BEFORE
  `kawa.consume(size)` at `:2443`. The `debug_assert!` at `:2438-2441`
  fires loudly in test builds if the invariant is broken.
- The unsafe block is the only one in the H2 hot path; all other H2
  code is safe Rust.

Codex G1 suggested wrapping the sequence in a closure-bound guard so a
future reorder couldn't skip the clear. Analysis: the invariant is
"clear must come before consume", which cannot be enforced by Drop
semantics (drop fires at scope end; consume happens in the same
scope). A closure-based helper would have to take a `&mut kawa` and
do both the vectored write AND the consume — invasive, widens the
unsafe surface, and doesn't obviously improve safety over the current
`clear + debug_assert + consume` pattern which is already locally
auditable.

**Action taken:** none. Kept the existing SAFETY comment +
`debug_assert` pair; added no new unsafe. If the user later wants the
closure refactor, the scope is one function (`flush_stream_out`) and
the contract is already documented inline.

## B3-g · G3 · Double-GOAWAY force-close doc/test

**Verdict:** already-documented and already-implemented; no code
change needed.

**Evidence:** `lib/src/protocol/mux/h2.rs`:
- `ConnectionH2::graceful_goaway` (`:3459-3499`) carries a full
  RFC 9113 §6.8 explanation in its doc comment — the first call sends
  GOAWAY with `last_stream_id = 0x7FFFFFFF` (STREAM_ID_MAX) to signal
  intent to stop accepting new streams; the second call (detected by
  `drain.draining == true`) routes to `goaway(NoError)` which sends
  GOAWAY with the actual `highest_peer_stream_id`.
- `H2DrainState.draining` at `:1138` is the state flag that
  distinguishes phase 1 from phase 2.
- `H2DrainState.started_at` at `:1147` is armed only on phase 1
  (`:3473`) so that `Mux::shutting_down` can detect the
  `graceful_shutdown_deadline` expiry and force-close even if a
  misbehaving peer never responds to the first GOAWAY.

A unit test that exercises both phases requires constructing a
`ConnectionH2` end-to-end (socket, HPACK coders, timers, flow window
— a non-trivial mock); the existing e2e suite
`test_h2_graceful_shutdown_*` at `e2e/src/tests/h2_tests.rs` covers
the black-box observable behaviour.

**Action taken:** none. The RFC reference + phase descriptions are
already inline at the function definition — future readers have the
invariant without having to re-derive it.

## Remaining plan items deferred (not in this session)

These items need their own dedicated conversation; they were left out
of the post-merge sweep because they are architectural or
harness-heavy:

| ID | Item | Reason |
|----|------|--------|
| B3-h · G4 | Linked-stream fast-backend-close race e2e | needs full e2e harness with backend that closes mid-response + stream-state assertion hooks |
| B3-z | 1xx informational forwarding e2e | needs backend that emits 1xx before final response |
| B3-j · G6 | HPACK dynamic-table-size-update emission | RFC 7541 §4.2 protocol change, non-trivial |
| B3-p · #899 | H1 trailers in `kawa_h1` | kawa upstream architectural change |
| B3-x · #890 + #1057 | Rate-limit design doc + implementation | L-effort, needs design doc first per user's "both vectors" answer |
| B2-a · #1218 | Backend TLS | L-effort (~2-4 k LOC), user explicitly out-of-scope pre-merge |

---

Written by `team-lead` on feat/h2-mux post-merge sweep 2026-04-22.
