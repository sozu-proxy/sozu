# TCP Passthrough SNI+ALPN Preread — Session Lifecycle

Reference document for maintainers of `lib/src/protocol/tcp_preread/` (the pure
sans-io core + nom parser) and `lib/src/tcp.rs` (the I/O shell/orchestration).
Companion to `lib/src/protocol/udp/LIFECYCLE.md` (UDP flow lifecycle, the
template this document mirrors), `lib/src/protocol/mux/LIFECYCLE.md` (H2), and
`lib/src/protocol/proxy_protocol/` (PROXY ingress, reused verbatim here for the
`expect_proxy` case).

Every claim is anchored to a concrete `file.rs:LINE`; line numbers were last
refreshed against the `feat/tcp-sni-preread` branch tip on 2026-07-10.
Implements issue [sozu-proxy/sozu#1279](https://github.com/sozu-proxy/sozu/issues/1279)
(TCP passthrough SNI+ALPN preread routing). `doc/configure.md` is the
operator-facing counterpart (config keys, CLI, metrics); this document is the
maintainer-facing internals reference.

---

## 1. What Problem This Solves

A `protocol = "tcp"` listener forwards raw bytes without terminating TLS — the
backend, not Sōzu, holds the certificate. Before this feature, one TCP
listener could only route to exactly one cluster (`TcpListener.cluster_id`,
`tcp.rs:1896`): there was no way to fan a single `address:port` out to
multiple backend clusters by *virtual host*, the way HTTPS routing does by SNI
at the TLS layer.

The fix reads the TLS ClientHello's `server_name` (RFC 6066 §3) and
`application_layer_protocol_negotiation` (RFC 7301 §3.1) extensions **before**
any byte is forwarded — a technique HAProxy calls `req.ssl_sni` / `tcp-request
inspect-delay` — then routes to a cluster by `(sni, alpn)`, all while never
decrypting or terminating the connection. The backend still sees the
untouched ClientHello and completes its own TLS handshake with the client.

This is a **preread**, not a proxy: no bytes are consumed, transformed, or
re-serialized. The parsed ClientHello is discarded once routing is decided;
only the *decision* (cluster, byte offset, parsed SNI/ALPN for logging)
survives. The accumulated bytes — ClientHello and anything coalesced after it
— are replayed byte-for-byte to the backend once connected.

---

## 2. Two-Half Sans-IO Split (mirrors the UDP/H2 pattern)

| Half | Module | Owns |
|------|--------|------|
| **Pure core** | `lib/src/protocol/tcp_preread/mod.rs` + `parser.rs` | the `decided`/`deadline` latch, the routing decision (SNI/ALPN match against the trie), PROXY-v2 stripping, the nom-based TLS record/ClientHello parser |
| **I/O shell** | `lib/src/protocol/tcp_preread/shell.rs` | the frontend socket, the accumulating `Checkout` buffer, the read-size cap enforcement, decision metrics/logs |
| **Orchestration** | `lib/src/tcp.rs` | the `TcpStateMachine` enum, session construction, the `ACTIVE`/`DURATION` gauge lifecycle, the four `proxy_protocol` upgrade branches, the SNI route table (`TrieNode`) per listener |

The core performs **no I/O**: no socket, no `Instant::now()` on its own, no
`rand`, no `Arc<Mutex>` (`mod.rs:1-9`). Time is injected as `now: Instant` on
every [`Input`] (`mod.rs:79-87`); the route table is injected borrowed via
[`PrereadConfig`] (`mod.rs:56-75`). This is what makes
`sim/tests/tcp_preread_sim.rs` (the `sozu-sim` crate, moonpool-driven)
possible — see `doc/testing.md` §5 and §6.

Unlike the UDP manager (one long-lived flow table stepped by many actions),
[`SniPrereadCore`] (`mod.rs:154`) is a small, **near-stateless, per-connection**
state machine: two fields (`decided: Option<Output>`, `deadline:
Option<Instant>`, `mod.rs:159-163`), constructed fresh per `TcpSession` and
dropped once routed or rejected.

The narrow message contract (`mod.rs:77-146`):

- **`Input<'a>`** (`mod.rs:79`) — `Bytes { buf, now }` (the FULL accumulated
  window from wire offset 0, re-fed on every call, never a delta),
  `Timeout { now }`, `FrontClosed`.
- **`Output`** (`mod.rs:91`) — `NeedMore { deadline }`, `Routed { cluster,
  content_offset, proxy_source, sni, alpn }`, `Reject(RejectReason)`.

---

## 3. The Decided Latch: Exactly One Terminal Verdict, Ever

`SniPrereadCore::decide` (`mod.rs:376-392`) is the only place `self.decided`
is ever set, and it `debug_assert!`s it is called **at most once**
(`mod.rs:377-380`). Every subsequent `handle_input` call — on `Bytes`,
`Timeout`, or `FrontClosed`, regardless of what bytes arrive — replays the
SAME latched `Output` verbatim (`on_bytes` `mod.rs:219-221`, `on_timeout`
`mod.rs:272-274`, `on_front_closed` `mod.rs:279-281`). This is verified by
`decided_latch_replays_the_same_terminal_forever` (`mod.rs:716`) and by the
fuzz target's own invariant (`fuzz_tcp_clienthello.rs`: "once a terminal
`Output` is latched, every later call ... returns that SAME terminal").

The deadline is symmetric: armed exactly once, on the first `Input::Bytes`
(`mod.rs:212-213`), and `handle_input`'s own post-condition
(`mod.rs:192-205`) asserts it never rewinds.

**Why this matters operationally**: a route decision, once made, cannot be
un-made by a later or malformed input. A client cannot re-open the routing
question by drip-feeding garbage after a valid ClientHello, nor can a
duplicate/racing timeout override an already-routed session.

---

## 4. State Lifecycle

```
accept()
   │  create_session (tcp.rs:2416): listener has SNI routes
   │  → new_sni_preread (tcp.rs:279), gauge_add(sni_preread.active, +1) (tcp.rs:321)
   ▼
┌─────────────────────────┐
│      SniPreread          │  readable() (shell.rs:174) re-feeds the FULL
│  (tcp.rs:65,             │  accumulated Checkout to SniPrereadCore on every
│   shell.rs:76)           │  read; on_timeout/on_front_closed feed the other
│                           │  two Input variants (tcp.rs:1768-1772, 417-421)
└───────────┬───────────────┘
            │
            ├─ Output::Reject(reason) ─────────────────────────────┐
            │    metric tcp.sni_preread.rejected.<reason>          │
            │    (shell.rs:336-342), session closes                │
            │                                                       ▼
            │                                              close() (tcp.rs:1630):
            │                                              gauge -1 + duration
            │                                              (tcp.rs:1674-1682)
            │
            └─ Output::Routed{cluster, content_offset, ...} ───────┐
                 arm backend-writable (shell.rs:326)                │
                 sync TcpSession::cluster_id (tcp.rs:485-490)       │
                 connect_to_backend (gated, §7)                    │
                 backend connects → back_writable → upgrade()      │
                 (tcp.rs:549-567) → upgrade_sni_preread            │
                 (tcp.rs:683-806): gauge -1 + duration              │
                 (tcp.rs:724-728), dispatch by proxy_protocol (§6)  │
                                                                     ▼
                                                    SendProxyProtocol │ Pipe
                                                    (transient)      (terminal)
```

The two gauge/duration decrements (reject path in `close()`, upgrade path in
`upgrade_sni_preread`) are structured as **mutually exclusive exits**:
`upgrade_sni_preread` transitions `self.state` away from `SniPreread` *before*
`close()` can ever observe that marker again (`tcp.rs:690-693`), so a session
nets to exactly one `-1` for its one `+1`, on precisely one of reject /
upgrade / teardown. See `every_reject_reason_has_a_distinct_metric_name`
(`shell.rs:376`) and the gauge-contract comments at `tcp.rs:313-321` and
`tcp.rs:678-682`.

---

## 5. The `RejectReason` Taxonomy (11 reasons)

Defined at `mod.rs:113-146`, mapped to metric names by the **total function**
`names::tcp::sni_preread::rejected_name` (`lib/src/metrics/names.rs:348-364` —
a new `RejectReason` variant without a matching arm fails to compile, so the
metric surface can never silently drop a reason).

| Reason | Meaning | Typical cause |
|---|---|---|
| `NotTls` | First TLS record's `ContentType` wasn't `handshake` (22) | Plain-TCP client on an SNI-enabled listener, or a non-TLS health check sending bytes |
| `MalformedRecord` | Bad record framing (length lies, or a non-handshake record interrupts an in-progress ClientHello) | Corrupted/adversarial input, RFC 8446 §5.1 violation |
| `MalformedHandshake` | Not a ClientHello, or a length-prefixed field inside it lies about available bytes | Adversarial input, or a non-ClientHello handshake message first |
| `Fragmented` | Preread deadline fired before a terminal verdict | Slow client, or an attacker deliberately drip-feeding one byte at a time |
| `TooLarge` | Accumulated window reached `max_bytes` without a complete PROXY header + ClientHello | Oversized ClientHello (many extensions/certs in early data) exceeding `sni_preread_max_bytes`, or `expect_proxy` misconfigured against a listener not actually behind a PROXY-emitting LB |
| `NoSni` | `server_name` extension absent, empty, or its `host_name` entry missing | Client didn't send SNI (older/non-conformant TLS client) on a listener with SNI-scoped frontends |
| `EchOuterAbsent` | `encrypted_client_hello` (`0xfe0d`) present but no usable outer SNI | ECH-enabled client hiding the real SNI in the encrypted inner hello — distinguished from `NoSni` because it is a different operational signal (§8) |
| `SniUnmatched` | Normalized SNI matched no route in the trie | Client requested a hostname with no configured TCP frontend |
| `AlpnUnmatched` | SNI matched a route, but no entry's `AlpnMatcher` accepted the client's offered ALPN (and no `Any` catch-all present) | Client's ALPN offer doesn't overlap any `--alpn` set on that `(address, sni)` |
| `ProxyHeaderInvalid` | The PROXY-v2 header itself failed to parse | `expect_proxy = true` on a listener not actually receiving PPv2 from its LB |
| `FrontClosed` | Frontend closed before a decision was reached | Bare TCP health check (SYN/ACK/FIN, no bytes — silent, no metric, see §9) or a genuine mid-preread abort (bytes seen — metered) |

`NoSni` vs. `EchOuterAbsent` is a deliberate operational distinction
(`mod.rs:129-135`): both mean "no usable outer SNI", but the first is an
ordinary non-SNI client while the second flags a client actively using
Encrypted Client Hello, which is a different traffic-shape signal for
operators tracking ECH adoption.

`TooLarge` is asymmetric by design: it only fires on a window that is still
**incomplete** at the cap (`need_more_or_too_large`, `mod.rs:291-301`). A
COMPLETE ClientHello routes regardless of total window length, because in
passthrough the client keeps sending after its hello (rest of handshake,
early data) — see `complete_hello_with_trailing_bytes_over_cap_routes`
(`mod.rs:629`) and Defect D in §10.

---

## 6. The Four `proxy_protocol` Handoff Paths

Once `SniPrereadCore` returns `Routed`, `upgrade_sni_preread`
(`tcp.rs:683-806`) looks up the ROUTED cluster's `proxy_protocol` config
(`tcp.rs:744-749`) — not the listener's `expect_proxy`, which only gates
whether an *inbound* PPv2 header preceded the ClientHello on the wire — and
dispatches one of four ways. `content_offset` is how many leading bytes of
the accumulated buffer are the already-parsed inbound PROXY-v2 header
(`Output::Routed::content_offset`, `mod.rs:98-99`); the doc comment at
`tcp.rs:657-676` is the canonical statement of wire-order per branch:

| Cluster's `proxy_protocol` | What happens to the inbound PPv2 prefix (if any) | Wire order to backend | Code |
|---|---|---|---|
| `Some(SendHeader)` | Consumed (`frontend_buffer.consume(content_offset)`, `tcp.rs:760`) | `[synth PPv2 header][ClientHello...]` — Sōzu synthesizes its OWN header | `upgrade_sni_preread` → `TcpStateMachine::SendProxyProtocol` (`tcp.rs:759-775`), then one more `ready()` cycle through `upgrade_send` (`tcp.rs:569-612`) into `Pipe` |
| `Some(ExpectHeader)` \| `None` | Consumed (`tcp.rs:777`) | `[ClientHello...]` only — Sōzu terminates the inbound header locally; the backend never sees a PROXY header at all (a `None`-proxy_protocol cluster on an `expect_proxy` listener must not leak a stray inbound prefix either — this is the one case this implementation infers beyond a literal reading of the original design text) | `build_pipe_from_preread` (`tcp.rs:809`), directly into `Pipe` |
| `Some(RelayHeader)` | **NOT** consumed | `[inbound PPv2 header][ClientHello...]` — the already-parsed inbound header IS the header this backend expects, replayed verbatim | `build_pipe_from_preread` (`tcp.rs:792-804`), directly into `Pipe` |

Three of the four branches reach `Pipe` in the SAME `ready()` cycle that
decided the route (`ExpectHeader`/`RelayHeader`/`None` all call
`build_pipe_from_preread` synchronously, `tcp.rs:776-805`); only `SendHeader`
detours through the transient `SendProxyProtocol` state for one more cycle,
because it must synthesize and send its own header before the pipe exists.

`build_pipe_from_preread` (`tcp.rs:809-862`) restores the `SniPreread`
readiness events onto the new `Pipe` via `restore_readiness_events`
(`pipe.rs:219-223`) rather than a bare field assignment: that helper sets both
`.event`s and THEN re-runs `arm_inherited_buffer_writes` (`pipe.rs:210-217`),
so a synthetic backend-writable event survives even when the restored
`backend_event` alone wouldn't carry `WRITABLE` — the byte-for-byte drain of
the inherited accumulator must not depend on that. `upgrade_send`
(`tcp.rs:569-612`) does the identical restore for the `SendHeader` branch's
own eventual `Pipe`.

Access-log tagging: `routed_sni` / `routed_alpn_label` (`tcp.rs:144-151`) are
stashed at `upgrade_sni_preread` (`tcp.rs:741-742`) and consumed
(`Option::take`) at the point the session actually reaches `Pipe` —
immediately via `build_pipe_from_preread` (`tcp.rs:858`) for
Expect/Relay/None, or one cycle later via `upgrade_send` (`tcp.rs:599-601`)
for `SendHeader`.

---

## 7. Two-Phase Connection Admission

A TCP passthrough session's cluster is unknown until the SNI decision lands —
unlike HTTP, where the `Host` header arrives in the same read that resolves
routing. Admission control is therefore split into two phases:

**Phase 1 — pre-routing, cluster-agnostic** (checked regardless of whether the
listener uses SNI routing at all): buffer-pool exhaustion at accept
(`create_session`, `tcp.rs:2433-2446`), and the global session-memory cap
(`sessions.borrow().at_capacity()`, `tcp.rs:1522-1524`).

**Phase 2 — post-routing, per-`(cluster, source-IP)`**: the backend connect
attempt itself is GATED on a route decision existing —
`attempt_backend_connect_if_needed` (`tcp.rs:1186-1195`) returns `None`
(does nothing) `if matches!(&self.state, TcpStateMachine::SniPreread(preread)
if !preread.is_routed())`. Only once that gate passes does
`connect_to_backend` (`tcp.rs:1479`) run its per-(cluster, source-IP) limit
check (`tcp.rs:1526-1561`, the same gate the HTTP/HTTPS mux `Router` applies,
reused here for raw TCP) against the NOW-KNOWN cluster. A limit hit produces
`BackendConnectionError::TooManyConnectionsPerIp`, which closes the session
with a graceful TCP FIN via `handle_connection_result` (`tcp.rs:2062-2078`) —
TCP has no HTTP envelope to carry a 429/`Retry-After`.

The gate is re-checked opportunistically: a route decision that completes
INSIDE a `readable()` call gets a same-cycle connect attempt via the second
`attempt_backend_connect_if_needed` call right after `self.readable()`
returns (`tcp.rs:1300-1313`), so a session never stalls waiting for an
unrelated readiness event to re-enter `ready_inner` and reach the top-of-loop
connect gate.

---

## 8. Splice Interaction: Buffered Bytes Drain Before the Fast Path Engages

Once a session reaches `Pipe` with `Protocol::TCP`, the kernel-`splice(2)`
zero-copy fast path is available if the `splice` feature is compiled in and
the target is Linux (`SplicePipe::new()` allocated unconditionally for
`Protocol::TCP`, `pipe.rs:196-201`). But `Pipe::backend_writable`
(`pipe.rs:786-796`) gates entry into `splice_backend_writable` on
`self.frontend_buffer.available_data() == 0`:

```rust
#[cfg(all(target_os = "linux", feature = "splice"))]
if self.protocol == Protocol::TCP
    && self.splice_pipe.is_some()
    && self.frontend_buffer.available_data() == 0
{
    return self.splice_backend_writable(metrics);
}
```

This is load-bearing for SNI-routed sessions specifically: the accumulated
ClientHello (+ any coalesced payload) inherited from `SniPreread` sits in
`frontend_buffer`, a plain userspace `Checkout`, NOT the kernel pipe splice
drains. Without the gate, `backend_writable` would dispatch straight to
`splice_backend_writable`, which only drains `splice_in_pending()` — zero for
a session that never spliced anything yet — and return immediately, silently
dropping the inherited preread bytes. The regression guard is
`backend_writable_drains_inherited_frontend_buffer_before_splice_engages`
(`pipe.rs:1786`).

---

## 9. Frontend-Close Handling: Silent Health Checks vs. Metered Aborts

`SniPreread::front_gone` (`shell.rs:279-294`) and its two callers —
`readable`'s own `SocketResult::Closed`/`Error` arm (`shell.rs:241-249`) and
`on_front_closed` (`shell.rs:273-275`, invoked from `TcpSession::front_hup`,
`tcp.rs:417-421`) — branch on `has_received_bytes()` (`shell.rs:148-150`,
`frontend_buffer.available_data() > 0`):

- **Bytes were seen**: feeds `Input::FrontClosed` into the core, which
  latches `Reject(FrontClosed)` and increments
  `tcp.sni_preread.rejected.front_closed` — a genuine mid-preread abort.
- **Zero bytes ever arrived**: silent — `trace!` only, no rejection metric
  (`shell.rs:284-293`). This is the bare TCP health-check pattern (SYN → ACK
  → FIN, no payload), mirrored from the identical guard in
  `protocol/proxy_protocol/expect.rs` — counting every health-check probe as
  a routing rejection would make the reject-reason metrics useless as a
  signal for actual client-facing failures.

---

## 10. Hardening Notes

1. **Never `consume()` the accumulator while undecided.** `frontend_buffer`
   only ever grows (via `fill`, `shell.rs:214`) until a route decision either
   consumes the PROXY-v2 prefix (`content_offset` bytes, at the point of
   upgrade) or the session closes. Byte-for-byte replay depends on this: the
   shell must be able to hand the SAME bytes to the backend it fed to the
   core, verbatim.

2. **Defect D — the read cap must be enforced by the SHELL, not just the
   core's `NeedMore` path.** `SniPreread::readable` (`shell.rs:174-249`) caps
   each socket read at `effective_max_bytes - already_buffered`
   (`shell.rs:198-202`), not just the checked-out buffer's free space. The
   core's own `max_bytes` cap (`need_more_or_too_large`, `mod.rs:291-301`)
   only fires on a would-be-`NeedMore` window; it does NOT re-check a window
   that turns out to be a COMPLETE ClientHello (by design — see §5's
   `TooLarge` note). A `Checkout` from the buffer pool is sized to the full
   pool buffer (>= 16 KiB), far larger than a tight `sni_preread_max_bytes`;
   without capping the READ ITSELF, the shell would drain the whole kernel
   socket buffer in one call, and an oversized-but-complete ClientHello would
   be read in full and ROUTED instead of rejected `TooLarge`. Regression
   guard: `readable_caps_the_accumulator_at_effective_max_bytes`
   (`shell.rs:436`).

3. **`effective_max_bytes` is `min(configured, buffer_capacity)`, always.**
   `effective_sni_preread_max_bytes` (`tcp.rs:2085-2087`) — the core has no
   independent backstop of its own; `PrereadConfig::max_bytes` is trusted
   entirely (`mod.rs:64-68`), so the shell must never hand it a cap the
   checked-out buffer cannot actually hold, regardless of what the listener
   config says. `TcpListener::preread_config` (`tcp.rs:1958-1970`) takes this
   already-derived `effective_max_bytes` as a parameter rather than
   re-deriving it, so core cap and shell cap can never drift apart within one
   session.

4. **Defect C — half-close must drain in-flight bytes before closing.** See
   §8's sibling issue in `Pipe::readable`: a frontend EOF
   (`SocketResult::Closed`, `pipe.rs:640-668`) used to return `Close`
   unconditionally, discarding whatever sat in `frontend_buffer` for the
   backend. The fix transitions to the half-closed `WriteOpen` status exactly
   like the `WouldBlock`/`Continue` path, arms backend-writable to flush the
   queue, and defers teardown to `check_connections` (`pipe.rs:453-...`),
   which only reports "closeable" once nothing is in flight
   (`request_is_inflight` / `response_is_inflight`, `pipe.rs:470-475`). The
   reproducer named in the fix's own comments is a payload coalesced with the
   SNI ClientHello (this feature's exact shape — one read delivering hello +
   application data before the frontend closes), but the defect could hit any
   plain-TCP upload racing a frontend close, independent of SNI preread.
   Regression guard: `frontend_eof_flushes_queued_backend_bytes_before_closing`
   (`pipe.rs:1857`).

5. **Route lookup + ALPN resolution is asserted deterministic.**
   `SniPrereadCore::route` (`mod.rs:306-371`) re-runs the SAME trie lookup +
   `resolve_alpn` immediately after matching and `debug_assert_eq!`s the
   result (`mod.rs:346-358`) — routing is a pure function of `(sni,
   accept_wildcard, alpn)` at a given instant; a divergence would mean the
   trie or `resolve_alpn` has a hidden side effect or iteration-order
   nondeterminism.

6. **No panic on adversarial input.** The parser (`parser.rs`) uses
   `nom::*::streaming` combinators for the outer record/handshake framing
   (genuine incompleteness must surface as `NeedMore`) and `nom::*::complete`
   combinators for the inner ClientHello body (by the time it runs, framing
   already proved the body is fully present, so any further shortfall is a
   lying length field — `MalformedHandshake`, never a panic or an infinite
   `NeedMore`). Every reachable `RejectReason` has a dedicated unit test
   (`mod.rs` tests: `not_tls_is_reachable_through_the_core`,
   `malformed_record_is_reachable`, `malformed_handshake_is_reachable`,
   `no_sni_is_reachable`, `ech_outer_absent_is_reachable`,
   `sni_unmatched_is_reachable`, `alpn_unmatched_is_reachable`,
   `proxy_header_invalid_is_reachable`, plus the two cap tests and the two
   close/timeout tests) and a directed generator in
   `sim/tests/tcp_preread_sim.rs` (`generate_scenario`) that constructs one
   per required variant BY CONSTRUCTION rather than hoping randomness finds
   it.

---

## 11. Cross-References

- `lib/src/protocol/tcp_preread/mod.rs` — the core: `Input`/`Output` contract,
  `PrereadConfig`, `RejectReason`, `SniPrereadCore`, `resolve_alpn`,
  `normalize_sni`.
- `lib/src/protocol/tcp_preread/parser.rs` — the nom-based TLS record /
  ClientHello wire parser (RFC 8446 §5.1/§4.1.2, RFC 6066 §3, RFC 7301 §3.1).
- `lib/src/protocol/tcp_preread/shell.rs` — the I/O shell: `SniPreread`,
  `readable`/`on_timeout`/`on_front_closed`, the effective-cap enforcement,
  decision metrics/logs.
- `lib/src/tcp.rs` — `TcpStateMachine`, `TcpSession::{new_sni_preread,
  upgrade_sni_preread, build_pipe_from_preread}`, `TcpListener::{preread_config,
  insert_sni_route, remove_sni_route}`, the gauge/duration lifecycle, the
  two-phase connect gate.
- `lib/src/protocol/pipe.rs` — `Pipe::{backend_writable, readable,
  check_connections, restore_readiness_events, arm_inherited_buffer_writes}`;
  the splice interaction (§8) and the close-before-flush fix (§10.4).
- `lib/src/metrics/names.rs` — `tcp::sni_preread::{ROUTED, ACTIVE, DURATION,
  rejected_name}`.
- `command/src/config.rs` — TOML config surface, SNI pattern validation, ALPN
  overlap / mixing-ban validation (config-load time only — see the caveat in
  `doc/configure.md`'s Runtime CLI surface section).
- `command/src/command.proto` — `RequestTcpFrontend.{sni, alpn}`,
  `TcpListenerConfig.{sni_preread_timeout, sni_preread_max_bytes}`.
- `sim/tests/tcp_preread_sim.rs` — the moonpool-driven deterministic
  simulation of `SniPrereadCore` (`doc/testing.md` §5).
- `fuzz/fuzz_targets/fuzz_tcp_clienthello.rs` — the cargo-fuzz target.
- `e2e/src/tests/tcp_sni_tests.rs` — end-to-end coverage (routing by cert,
  mTLS, byte-for-byte replay, reject paths).
- `doc/configure.md` — operator-facing TCP listener / frontend config, CLI
  surface, and the `tcp.sni_preread.*` metrics.
- `lib/src/protocol/udp/LIFECYCLE.md` — the sans-io core/shell split pattern
  this document mirrors.
