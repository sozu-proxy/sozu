# Kawa H1 — Session Workflow and Per-Stream Lifecycle

Reference document for maintainers of the HTTP/1.1 frontend / backend pairing
under `lib/src/protocol/kawa_h1/`. Companion to `lib/src/protocol/mux/LIFECYCLE.md`
(H2) and `lib/src/protocol/proxy_protocol/LIFECYCLE.md` (PROXY-v2 ingress).

Every claim is anchored to a concrete `file.rs:LINE`. Line numbers were last
refreshed against the `docs/feat-h2-mux-audit` branch tip on 2026-04-26 — when
a refactor moves them, please update the citations in the same changeset; stale
pointers here are treated as broken documentation.

Scope: the server-side Kawa H1 session is the primary subject. Where the
behaviour diverges between an H1 frontend and the H1 backend connection wired
underneath an H2 frontend (via the mux), the difference is called out.

---

## 1. Architecture Overview

### 1.1 Where Kawa H1 sits

Kawa H1 owns the HTTP/1.1 wire-level state machine plus the request/response
mutation pipeline. It is consumed by:

- the standalone H1 frontend (`lib/src/http.rs`, `lib/src/https.rs` after TLS
  handshake completes with ALPN `http/1.1` or no ALPN);
- the H2 mux backend path (`lib/src/protocol/mux/h1.rs`), which speaks H1 to
  origin servers regardless of the frontend protocol — Sōzu currently does
  not speak H2 to backends.

### 1.2 Module layout

| Module                | Path                                                     | Responsibility                                                 |
|-----------------------|----------------------------------------------------------|----------------------------------------------------------------|
| `mod.rs`              | `lib/src/protocol/kawa_h1/mod.rs`                        | `Http<Front, L>` session state + `SessionState` impl + readiness pumping |
| `editor.rs`           | `lib/src/protocol/kawa_h1/editor.rs`                     | `HttpContext`, header rewrites (`Forwarded`, `X-Forwarded-*`, `Sozu-Id`), `log_context()` |
| `parser.rs`           | `lib/src/protocol/kawa_h1/parser.rs`                     | `Method` enum, hostname/port helper, tolerant-vs-strict charset split |
| `answers.rs`          | `lib/src/protocol/kawa_h1/answers.rs`                    | `Template`, `HttpAnswers`, `DefaultAnswerStream` for synthesised 4xx/5xx replies |
| `diagnostics.rs`      | `lib/src/protocol/kawa_h1/diagnostics.rs`                | Hex-dump + phase rendering for parse-failure access logs       |

### 1.3 Key types

| Type                  | Declaration                                              | Purpose                                                        |
|-----------------------|----------------------------------------------------------|----------------------------------------------------------------|
| `Http<Front, L>`      | `lib/src/protocol/kawa_h1/mod.rs:189`                    | Top-level H1 session state                                     |
| `DefaultAnswer`       | `lib/src/protocol/kawa_h1/mod.rs:111`                    | Catalogue of synthesised replies (301/400/404/413/502/503/504/507) |
| `ResponseStream`      | `lib/src/protocol/kawa_h1/mod.rs:183`                    | `BackendAnswer(Kawa<…>)` vs `DefaultAnswer(…)` split            |
| `TimeoutStatus`       | `lib/src/protocol/kawa_h1/mod.rs:176`                    | Used by the supervisor to attribute idle vs response timeouts  |
| `HttpContext`         | `lib/src/protocol/kawa_h1/editor.rs:116`                 | Per-request mutable state used by Kawa parser callbacks         |
| `Method`              | `lib/src/protocol/kawa_h1/parser.rs:39`                  | Owned-string-free method enum                                   |
| `HttpAnswers`         | `lib/src/protocol/kawa_h1/answers.rs:339`                | Listener + cluster template registry                           |
| `DefaultAnswerStream` | `lib/src/protocol/kawa_h1/answers.rs:36`                 | `Kawa<SharedBuffer>` carrying a rendered default answer         |

---

## 2. Per-Stream Lifecycle

### 2.1 Accept and session creation

A new H1 session is constructed by the proxy layer (`lib/src/http.rs`,
`lib/src/https.rs`) once a TCP/TLS connection has produced a Kawa pair of
front/back buffers and a `HttpContext`. `Http::new` (`lib/src/protocol/kawa_h1/mod.rs:228`)
seeds the session with:

- `frontend_readiness.interest = READABLE | HUP | ERROR`;
- `backend_readiness.interest = HUP | ERROR` (no backend yet);
- `request_stream` / `response_stream` Kawa parsers in their initial phase;
- the `container_frontend_timeout` driving idle disconnect.

`reset()` (`lib/src/protocol/kawa_h1/mod.rs:299`) is the keep-alive entry
point — it reuses the same `Http` instance across pipelined requests, clearing
the per-request fields while preserving connection-level state.

### 2.2 Request parsing

`Http::readable` (`lib/src/protocol/kawa_h1/mod.rs:355`) is invoked when the
event loop sees the frontend socket as readable. It:

1. Resets the frontend timeout (`mod.rs:357`); failure to reset is logged but
   not fatal.
2. Refuses to read while a `DefaultAnswer` is already queued
   (`mod.rs:367-376`) — the only legal next step is `writable` to flush the
   synthesised reply.
3. Detects a full request buffer and either pushes the parser forward (if we
   are mid-body) or escalates to a `DefaultAnswer::Answer413`
   (`mod.rs:379-392`).
4. Calls `socket_read` and feeds the bytes to the Kawa request parser.

When the request transitions out of the parsing phase, the parser callbacks in
`HttpContext` (see §3) capture the `:method`, authority, path, and rewrite the
`Forwarded` / `X-Forwarded-*` header chain.

### 2.3 Routing — `cluster_id_from_request`

`cluster_id_from_request` (`lib/src/protocol/kawa_h1/mod.rs:1340`) extracts the
authority and path from the Kawa stream and calls back into the listener
(`L7ListenerHandler`) to resolve the request to a `cluster_id`. It is also the
gate that:

- enforces the SNI-vs-`:authority` strict binding when `tls_server_name` is
  present (`HttpContext::strict_sni_binding`, see `editor.rs:189`) — mismatch
  yields a default answer rather than a backend connection (CWE-346 / CWE-444);
- turns route-not-found into `DefaultAnswer::Answer404`.

### 2.4 Backend connect — `connect_to_backend`

`connect_to_backend` (`lib/src/protocol/kawa_h1/mod.rs:1462`) reuses the
existing backend when the cluster has not changed and the TCP connection is
healthy (`mod.rs:1485-1500`); otherwise it allocates a fresh socket via
`backend_from_request` (`mod.rs:1397`). The resulting `BackendConnectAction`
is bubbled back to the supervisor so it can register the new socket with mio.

### 2.5 Response streaming and keep-alive

After the backend handshake completes, `Http::backend_writable`
(`mod.rs:701`) flushes the request body and `Http::backend_readable`
(`mod.rs:773`) ingests the response. Once `response_stream.parsing_phase`
reaches the terminal phase and the body is fully consumed:

- if both sides set `Connection: keep-alive` (`HttpContext.keep_alive_frontend`
  + `keep_alive_backend`), `Http::reset` (`mod.rs:299`) is called and the
  session waits for the next request on the same TCP/TLS connection;
- otherwise the session shuts down (see §6).

---

## 3. Editor — `HttpContext` and the parser callbacks

`HttpContext` (`lib/src/protocol/kawa_h1/editor.rs:116`) is the per-request
mutable companion to the Kawa parser. Its `kawa::h1::ParserCallbacks` impl
(`editor.rs:217`) fires:

- `on_headers` (`editor.rs:218`) — split between request and response by
  `stream.kind`;
- `on_request_headers` (`editor.rs:287`) — captures the `:method`, authority,
  path; copies `X-Forwarded-For` into `xff_chain` for the access log; appends
  the configured `Forwarded`/`X-Forwarded-*` hop; injects the `Sozu-Id`
  correlation header named by `sozu_id_header`;
- `on_response_headers` (`editor.rs:574`) — captures `:status`, `:reason`,
  optionally rewrites `Set-Cookie` for sticky sessions.

`HttpContext::log_context` (`editor.rs:687`) is the canonical helper for
producing the `LogContext { session_id, request_id, cluster_id, backend_id }`
record consumed by every `log_context!` macro in this module — prefer it over
hand-rolling a struct literal (per repo `CLAUDE.md`).

Notable security-relevant fields on `HttpContext`:

- `tls_server_name` (`editor.rs:188`) — SNI captured at handshake; the routing
  layer enforces an exact match against the HTTP authority when
  `strict_sni_binding` is set.
- `strict_sni_binding` (`editor.rs:195`) — mirrors
  `HttpsListenerConfig::strict_sni_binding`; defends against
  cross-tenant authority spoofing (CWE-346 / CWE-444).
- `xff_chain` (`editor.rs:147`) — verbatim upstream `X-Forwarded-For` snapshot
  taken before Sōzu appends its own hop, so the access log records the
  attested chain even when Sōzu mutates the live header.
- `x_request_id` (`editor.rs:141`) — universal correlation token; populated
  unconditionally in `on_request_headers` so the access log always has a
  cross-component join key.

---

## 4. Default Answers

Synthesised 4xx/5xx responses are not handcrafted byte buffers — they are
template-rendered Kawa streams. The relevant pieces:

- `DefaultAnswer` enum (`lib/src/protocol/kawa_h1/mod.rs:111`) lists every
  variant Sōzu can emit (301 redirect, 400, 404, 413, 502, 503, 504, 507) and
  carries the per-call diagnostic strings.
- `Template` (`answers.rs:78`), `Replacement` (`answers.rs:72`), and
  `TemplateVariable` (`answers.rs:57`) drive the substitution engine:
  variables can be plain (text) or typed (URL/path/integer) — typed
  substitutions go through the corresponding sanitizer to avoid log /
  header injection.
- `HttpAnswers` (`answers.rs:339`) holds the per-listener registry; the
  ListenerAnswers / ClusterAnswers split (`answers.rs:307`/`:334`) allows a
  cluster to override a listener-level template.
- `Http::set_answer` (`mod.rs:1055`) is the one chokepoint that promotes a
  `ResponseStream::BackendAnswer` to `ResponseStream::DefaultAnswer` and
  arms the readiness flags so `writable` flushes the synthesised reply
  next.

Status mapping `DefaultAnswer → u16` lives at `mod.rs:157`.

---

## 5. Diagnostics and Error Envelope

`diagnostics::diagnostic_400_502` (`lib/src/protocol/kawa_h1/diagnostics.rs:49`)
and `diagnostic_413_507` (`diagnostics.rs:95`) render the parsing phase, the
charset rule (`diagnostics.rs:14-17`, switches on the `tolerant-http1-parser`
feature), and a hex-dump window into the offending buffer region. Their output
is what populates the `message` field on `DefaultAnswer::Answer400` and is
echoed to the access log so a parse failure is debuggable without a packet
capture.

`Http::log_request` (`mod.rs:992`) is the canonical access-log emitter; the
`log_request_success` / `log_default_answer_success` / `log_request_error`
shortcuts (`mod.rs:1036`/`:1041`/`:1044`) categorise the call site.

---

## 6. Invariants

These rules are load-bearing — break them and you risk a truncated response,
a wedged session, or a security regression.

1. **Never panic on network-facing input.** Parser failures, oversized bodies,
   bad TLS handshake state, lost backends — all funnel into `DefaultAnswer` +
   metric + log. `unwrap`/`expect`/`panic!` are reserved for hard internal
   invariants. Per repo `CLAUDE.md`, the H1 parser, editor, default-answer,
   and diagnostics paths are explicitly listed under
   "no panic on network-facing input".

2. **Write-only shutdown on TLS frontends.** Closing the frontend socket with
   `Shutdown::Both` discards any unread receive data and elicits a TCP RST,
   truncating the already-queued response. The canonical write-up lives at
   `lib/src/https.rs:650-655`. Backends speak plaintext H1 today, so
   `Shutdown::Both` is permitted on the backend socket — but the
   `// SAFETY (TLS-truncation invariant)` comment at
   `lib/src/protocol/kawa_h1/mod.rs:1219-1225` flags the migration to
   `Shutdown::Write` when backend TLS lands.

3. **Readiness lifecycle is owned by the editor, not the supervisor.** The
   per-stream readiness pumping (`frontend_readiness.interest.insert(WRITABLE)`,
   `backend_readiness.interest.insert(READABLE)`, etc.) is driven from
   `Http::readable` / `writable` / `backend_readable` / `backend_writable`
   (`mod.rs:355` / `:520` / `:773` / `:701`). The mux-style
   `signal_pending_write` discipline does NOT apply to Kawa H1 — H1 sessions
   own a single in-flight request at a time, and the readable/writable
   methods toggle interest directly under the mio level-triggered semantics
   the H1 listener uses.

4. **Default answer is one-shot per session.** Once `set_answer` promotes
   `response_stream` to `ResponseStream::DefaultAnswer`, `readable` refuses to
   accept further bytes (`mod.rs:367-376`). The next legal transition is
   `writable` followed by either `reset` (keep-alive) or close.

5. **`tolerant-http1-parser` is opt-in.** The strict parser is the default;
   the tolerant variant is enabled only via the `tolerant-http1-parser`
   feature on `sozu-lib` and `sozu-bin` (`lib/Cargo.toml:86`,
   `bin/Cargo.toml`). Tolerant mode relaxes hostname charset rules
   (`parser.rs:110-123`) and switches the diagnostics charset label
   (`diagnostics.rs:14-17`). It must not be enabled in security-sensitive
   deployments without measuring the risk against the upstream backends'
   strictness.

6. **`HttpContext` outlives a single request when keep-alive is in play.**
   `reset` clears the per-request fields but preserves the per-connection
   ULID (`session_id`), the SNI-derived TLS state, and the rendered
   `sozu_id_header` label. The `request_id` (`HttpContext::id`,
   `editor.rs:161`) IS rotated per request to keep the access log
   correlatable.

---

## 7. Cross-References

- `lib/src/protocol/mux/LIFECYCLE.md` — H2 frontend lifecycle. The H2 mux
  speaks H1 to the backend via this very module.
- `lib/src/protocol/proxy_protocol/LIFECYCLE.md` — PROXY-v2 ingress that
  precedes H1 on PROXY-aware listeners.
- `bin/src/command/LIFECYCLE.md` — supervisor view; explains how listener
  reloads (which can swap `HttpAnswers` templates) propagate into running
  sessions.
- `doc/lifetime_of_a_session.md` — operator-facing prose introduction; this
  document is the maintainer-facing deep dive it links into.
- `doc/configure.md` — listener / cluster / answer configuration reference.
