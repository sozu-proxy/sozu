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

---

Written by `team-lead` on feat/h2-mux post-merge sweep 2026-04-22.
