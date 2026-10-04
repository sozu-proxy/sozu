# TCP continuity during a binary upgrade

This diagnostic starts the supplied old binary and opens a TCP session carrying
acknowledged heartbeat traffic. It replaces the executable, invokes `sozu upgrade`,
checks that the new worker accepts a new connection, and observes whether the
original connection remains usable until the client closes it. It also requires
process retirement and shutdown to complete.

Sōzu closes TCP sessions immediately on worker soft stop, as documented in
`doc/lifetime_of_a_session.md`. This executable scenario records that accepted
limit as `TCP_LONG_LIVED_OUTCOME=closed_during_upgrade`; it does not classify the
closure as a regression or claim that TCP drain is supported. The harness exits
zero when its orchestration, process-retirement, new-session, and shutdown
invariants hold for either recorded session outcome. HTTP, WebSocket and UDP are
separate scenarios.

`OLD_WORKER_STOPPING_OBSERVED` is telemetry rather than an assertion: the old
worker can finish its short soft-stop path between two status snapshots. The
harness instead asserts its eventual exit and that exactly one replacement
worker remains active.

```sh
BASE_BINARY=/absolute/path/to/old/sozu \
CANDIDATE_BINARY=/absolute/path/to/new/sozu \
timeout 180s e2e/lifecycle/tcp_upgrade/run.sh
```

Requires Linux, Bash, Python 3, jq and the usual coreutils. Each run allocates
loopback ports and writes evidence beneath a unique temporary directory printed
on exit. Only processes owned by the invocation are cleaned up. The client marker
is written after an acknowledged backend round trip, so the upgrade starts only
once the old worker has a live session. Binary hashes, version output, status
snapshots and backend acknowledgements identify the actual test inputs and results.

The harness also exposes a bounded oracle self-test. It passes its own live PID
to the process-retirement assertion and must exit exactly 1 after printing
`live witness pid ... is still live`:

```sh
set +e
e2e/lifecycle/tcp_upgrade/run.sh --self-test-live-pid-assertion
status=$?
set -e
test "$status" -eq 1
```

This mode needs neither binary input and starts no proxy process. Its expected
failure validates the harness assertion; it is not a Sōzu product failure.
