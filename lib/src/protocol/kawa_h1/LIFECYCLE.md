# Kawa H1 — Shared HTTP/1.1 Vocabulary

Reference document for maintainers of `lib/src/protocol/kawa_h1/`. Companion to
`lib/src/protocol/mux/LIFECYCLE.md` (the H1/H2 datapath that consumes this
module) and `lib/src/protocol/proxy_protocol/LIFECYCLE.md` (PROXY-v2 ingress).

Every claim is anchored to a concrete `file.rs:LINE`. Line numbers were last
refreshed on 2026-09-20 — when a refactor moves them, please update the
citations in the same changeset; stale pointers here are treated as broken
documentation.

**Scope changed on 2026-09-20.** This module used to own an `Http<Front, L>`
session state machine and this document used to describe its lifecycle. That
session was removed (sozu#1346): neither `HttpStateMachine` (`lib/src/http.rs`,
`Expect | Mux | WebSocket`) nor `HttpsStateMachine` (`lib/src/https.rs`,
`Expect | Handshake | Mux | WebSocket`) had a variant holding one, and
`Http::new` had no code caller under either module spelling
(`crate::protocol::kawa_h1::` or the `crate::protocol::http::` re-export
declared at `lib/src/protocol/mod.rs:25-27`). An unconditional
`panic!("PROBEALWAYS …")` planted at the top of `Http::new` and
`save_http_status_metric` fired 0 times across four real proxied e2e sessions,
while the same binary panicked immediately under the function's own unit test.
`TimeoutStatus`, `ResponseStream`, `save_http_status_metric`, **this module's**
`handle_connection_result` (`lib/src/tcp.rs:2426` keeps its own separate copy,
which is live), this module's `log_context!` macro (and with it the `KAWA-H1`
log tag) and the whole `diagnostics.rs` module went with it, because nothing
else reached them. sozu#1347 — a frontend timeout consumed without
re-arming — was closed by that removal rather than patched.

**For the H1 session lifecycle read `lib/src/protocol/mux/LIFECYCLE.md`.**
Accept, request parsing, routing, backend connect, response streaming,
keep-alive, timeouts and access logs all live in `lib/src/protocol/mux/`
(`ConnectionH1` in `mux/h1.rs`, `Router` in `mux/router.rs`, `Stream` in
`mux/stream.rs`).

---

## 1. What this module still provides

`kawa_h1` is the HTTP/1.1 vocabulary the mux builds on: the parser callbacks
that rewrite headers, the answer templates, the method enum, and the catalogue
of synthesised replies. It has no `SessionState` implementation.

### 1.1 Consumers

| Item | Consumed by |
|---|---|
| `editor::HttpContext` | `mux/stream.rs`, `mux/router.rs`, `mux/answers.rs`, `mux/h2.rs` |
| `editor::HeaderEditMode`, `editor::HeaderEditSnapshot` | `mux/shared.rs`, `mux/router.rs`, `router/mod.rs` |
| `answers::HttpAnswers` | `lib/src/http.rs`, `lib/src/https.rs`, `mux/answers.rs` |
| `answers::DefaultAnswerStream`, `answers::merge_legacy_into_map` | `mux/answers.rs`, `lib/src/http.rs`, `lib/src/https.rs` |
| `parser::Method`, `parser::hostname_and_port`, `parser::compare_no_case` | `lib/src/https.rs`, `lib/src/http.rs`, `mux/auth.rs`, `router/mod.rs` |
| `DefaultAnswer` (in `mod.rs`) | `protocol/pipe.rs`, `lib/src/http.rs`, `mux/answers.rs` |

### 1.2 Module layout

| Module       | Path                                      | Responsibility                                                                             |
|--------------|-------------------------------------------|--------------------------------------------------------------------------------------------|
| `mod.rs`     | `lib/src/protocol/kawa_h1/mod.rs`          | `DefaultAnswer` catalogue, its `u16` mapping, the `GenericHttpStream` alias and the crate's single `kawa::AsBuffer for Checkout` impl |
| `editor.rs`  | `lib/src/protocol/kawa_h1/editor.rs`       | `HttpContext`, header rewrites (`Forwarded`, `X-Forwarded-*`, `Sozu-Id`), `log_context()`     |
| `parser.rs`  | `lib/src/protocol/kawa_h1/parser.rs`       | `Method` enum, hostname/port helper, tolerant-vs-strict charset split                        |
| `answers.rs` | `lib/src/protocol/kawa_h1/answers.rs`      | `Template`, `HttpAnswers`, `DefaultAnswerStream` for synthesised 3xx/4xx/5xx replies          |

### 1.3 Key types

| Type                  | Declaration                                   | Purpose                                                            |
|-----------------------|-----------------------------------------------|--------------------------------------------------------------------|
| `DefaultAnswer`       | `lib/src/protocol/kawa_h1/mod.rs:40`          | Catalogue of synthesised replies (301/302/308/400/401/404/408/413/421/429/502/503/504/507) |
| `GenericHttpStream`   | `lib/src/protocol/kawa_h1/mod.rs:28`          | `kawa::Kawa<Checkout>` — the pooled-buffer parser stream            |
| `HttpContext`         | `lib/src/protocol/kawa_h1/editor.rs`          | Per-request mutable state used by Kawa parser callbacks             |
| `HeaderEditMode` / `HeaderEditSnapshot` | `lib/src/protocol/kawa_h1/editor.rs`        | Per-frontend header-edit programme and its pre-edit snapshot |
| `Method`              | `lib/src/protocol/kawa_h1/parser.rs:38`       | Owned-string-free method enum                                       |
| `HttpAnswers`         | `lib/src/protocol/kawa_h1/answers.rs:503`     | Listener + cluster template registry                                |
| `DefaultAnswerStream` | `lib/src/protocol/kawa_h1/answers.rs:44`      | `Kawa<SharedBuffer>` carrying a rendered default answer             |

Note that `mod.rs`'s `impl kawa::AsBuffer for Checkout` (`mod.rs:30`) is the
crate's only impl **for `Checkout`** — the orphan rule permits no second copy —
so `mux` depends on it even though `mux` declares its own `GenericHttpStream`
alias. (`answers.rs:34` carries a separate `impl AsBuffer for SharedBuffer`, the
storage behind a rendered default answer.)

---

## 2. Editor — `HttpContext` and the parser callbacks

`HttpContext` (`lib/src/protocol/kawa_h1/editor.rs`) is the per-request
mutable companion to the Kawa parser. Its `kawa::h1::ParserCallbacks` impl
(`editor.rs`) fires:

- `on_headers` (`editor.rs`) — split between request and response by
  `stream.kind`;
- `on_request_headers` (`editor.rs`) — captures the `:method`, authority,
  path; copies `X-Forwarded-For` into `xff_chain` for the access log; appends
  the configured `Forwarded`/`X-Forwarded-*` hop; injects the `Sozu-Id`
  correlation header named by `sozu_id_header`;
- `on_response_headers` (`editor.rs`) — captures `:status`, `:reason`,
  optionally rewrites `Set-Cookie` for sticky sessions.

`HttpContext::extract_route` (`editor.rs`) hands the mux router the
authority, path and method it needs, and `HttpContext::log_context`
(`editor.rs`) is the canonical helper for producing the
`LogContext { session_id, request_id, cluster_id, backend_id }` record consumed
by every `log_context!` macro that has an `HttpContext` in scope — prefer it
over hand-rolling a struct literal (per repo `CLAUDE.md`).

Notable security-relevant fields on `HttpContext`:

- `tls_server_name` (`editor.rs`) — SNI captured at handshake (lowercased,
  trailing dot stripped). Used for logging and as a fallback exact-match check
  when `tls_cert_names` is unavailable.
- `tls_cert_names` (`editor.rs`) — `Option<Arc<Vec<String>>>` snapshot of
  the SAN dNSName entries of the certificate Sōzu actually served on this TLS
  session (RFC 6125 §6.4.4: when the SAN extension contains at least one
  dNSName entry, those entries are authoritative and the Common Name is
  ignored — Sōzu only honours CN as a fallback when SAN is absent or has no
  dNSName entry). Captured once at handshake from the cert resolver, frozen
  for the connection lifetime, `Arc`-shared across every per-stream
  `HttpContext` so H2 fan-out allocates once. The routing layer matches
  `:authority` against this set with RFC 6125 §6.4.3 wildcard handling,
  accepting browser connection coalescing while preserving the
  CWE-346 / CWE-444 trust boundary (operator-defined SAN scope). `None` when
  the resolver fell back to the default cert — routing then fall-backs to
  legacy SNI exact-match.
- `strict_sni_binding` (`editor.rs`) — mirrors
  `HttpsListenerConfig::strict_sni_binding`; gates the `tls_cert_names` check
  on/off. Defends against cross-tenant authority spoofing (CWE-346 / CWE-444).
- `xff_chain` (`editor.rs`) — verbatim upstream `X-Forwarded-For` snapshot
  taken before Sōzu appends its own hop, so the access log records the
  attested chain even when Sōzu mutates the live header.
- `x_request_id` (`editor.rs`) — universal correlation token; populated
  unconditionally in `on_request_headers` so the access log always has a
  cross-component join key.

### 2.1 CL.TE framing guard (`on_request_headers`)

The very first thing `HttpContext::on_request_headers` does (`editor.rs`, before
capturing `:method`/authority/path) is reject requests whose Transfer-Encoding
framing is ambiguous (RFC 9110 §7.6 / RFC 9112 §6.1; reopen of
[#726](https://github.com/sozu-proxy/sozu/issues/726)). An intermediary must not
forward a message whose framing is ambiguous.

The guard runs once per request, after kawa's own header pass (`process_headers`)
has finalised `body_size` but before `HttpContext` captures anything from the
request. kawa never elides the `Transfer-Encoding` header, so every TE field line
is still there to be forwarded, and `body_size` alone cannot tell you how many
there were — which is why the guard folds over the blocks rather than reading the
aggregate.

kawa **>= 0.7.1** resolves the combined Transfer-Encoding from the LAST TE field
line, per RFC 9110 §5.3 combining, and errors the parse for a REQUEST whose
combined final coding is not `chunked` (RFC 9112 §6.3), returning before
`callbacks.on_headers` is called at all. Two consequences, read from kawa 0.7.1's
`src/protocol/h1/parser/mod.rs` rather than measured here:

- a leading `Transfer-Encoding: chunked` line can no longer latch chunked framing
  while a later `identity` line rides along. That latch was a kawa **0.7.0**
  shape, and it is the shape the second and third clauses below were written
  against;
- the residual gap is the mirror of it: a chunked-final LAST line with a
  differently-framed EARLIER one — `identity` then `chunked` — parses clean under
  0.7.1, and forwarding both lines is what the count clause refuses.

The guard therefore folds over every non-elided `Transfer-Encoding` header in
`request.blocks` (`editor.rs:638-658`), producing `te_count` and
`te_all_suffix_chunked` — the latter true only when EVERY such value's literal
trailing bytes are `chunked` (`compare_no_case` over the last seven bytes). The
rejection predicate is exactly (`editor.rs:659-662`):

```rust
te_count > 1
    || (te_count == 1
        && (!te_all_suffix_chunked || request.body_size != kawa::BodySize::Chunked))
```

which rejects three distinct shapes:

- `te_count > 1` — more than one non-elided TE header. RFC 9112 §6.1 requires
  `chunked` be applied once and be the final coding; multiple TE field lines
  cannot be safely reconciled here, and forwarding them all hands the backend the
  combining problem sōzu just declined to solve. **This is the only clause that
  can fire on a request kawa 0.7.1 accepted**, in the `identity`-then-`chunked`
  shape above;
- one surviving TE header whose final coding is not `chunked`
  (`!te_all_suffix_chunked`), e.g. `Transfer-Encoding: chunked, gzip`;
- one surviving TE header while kawa did not adopt chunked framing
  (`body_size != BodySize::Chunked`).

The last two cannot fire under kawa 0.7.1: a TE line that survives to this point
is chunked-final, or kawa already refused the request, and `body_size` is then
`Chunked`. They stay as defense in depth against a kawa regression — do not read
either as closing a live hole, and do not delete them on that basis. The
`not-final-coding` and `multi-line-chunked-then-identity` rows of `e2e`'s
`TE_SMUGGLING_CASES` do still answer 400, but through kawa's own refusal rather
than through this predicate.

**An OWS-obfuscated coding is NOT rejected by this guard.** kawa >= 0.7.1
excludes leading/trailing OWS from every field value (RFC 9112 §5), so
`chunked\t` and `chunked ` read as `chunked`: `te_all_suffix_chunked` stays
true, kawa frames the message as chunked and elides the Content-Length, and the
request is legal. What keeps it safe is that the coding sozu framed on is the
coding it forwards — the obfuscated spelling never reaches the backend — which
`e2e`'s `test_h1_te_ows_forwarded_canonically` pins on the forwarded bytes.
(kawa 0.7.0 framed on the trimmed reading but forwarded the raw field line; a
backend that did not itself trim then saw no recognised coding and no length,
and read the chunked body as a pipelined request. That is the TE.TE desync
`chunked\t` used to be refused for.) The `trailing-tab` / `trailing-space`
entries in `TE_SMUGGLING_CASES` still 400, but for their invalid chunked
**body** — `Hello` is not a chunk — not through this predicate.

If the predicate holds, the guard increments
`names::http::FRONTEND_TE_SMUGGLING`, logs a `warn!`, and calls
`request.parsing_phase.error(...)` before returning early. It does **not**
short-circuit anything else in kawa — the very next line back in
`kawa::h1::parse`'s loop re-checks `parsing_phase`, sees `Error`, and returns.

The resulting `ParsingPhase::Error` is observed by the mux H1 connection in
`ConnectionH1::readable` (`lib/src/protocol/mux/h1.rs:383`), which checks
`kawa.is_error()` immediately after `kawa::h1::parse` and, on the server side,
calls
`set_default_answer(..., 400, ...)` and returns — before routing or the
per-frontend Basic-auth check (the `check_basic` guard in
`Router::route_from_request`, `mux/router.rs`, calling
`mux/auth.rs::check_basic`) run, so an ambiguously-framed request is
rejected before it reaches routing. Note that `crate::protocol::http` is a
`pub use ... kawa_h1 as http` re-export, not a separate type, so a grep for
consumers must search both spellings.

Mirrors the equivalent HTTP/2 → H1 defense, `RejectReason::ClTeConflict`
(`lib/src/protocol/mux/pkawa.rs:287`).

---

## 3. Default Answers

Synthesised 3xx/4xx/5xx responses are not handcrafted byte buffers — they are
template-rendered Kawa streams. The relevant pieces:

- `DefaultAnswer` enum (`lib/src/protocol/kawa_h1/mod.rs:40`) lists every
  variant Sōzu can emit (301/302/308 redirects, 400, 401, 404, 408, 413, 421,
  429, 502, 503, 504, 507) and carries the per-call diagnostic strings.
- `Template` (`answers.rs:89`), `Replacement` (`answers.rs:83`), and
  `TemplateVariable` (`answers.rs:67`) drive the substitution engine:
  variables can be plain (text) or typed (URL/path/integer) — typed
  substitutions go through the corresponding sanitizer to avoid log /
  header injection.
- `HttpAnswers` (`answers.rs:503`) holds the registry; its `cluster_answers` /
  `listener_answers` split (`answers.rs:504-505`) lets a cluster override a
  listener-level template. `HttpAnswers::get` (`answers.rs:1302`) is the
  selection chokepoint: its lookup key is derived from the `DefaultAnswer`
  variant, so only the built-in code names ("301" … "507") and the bundled
  fallback are ever selectable — an operator's custom answer under an
  unrecognised name is compiled but never chosen.
- `mux::answers::set_default_answer` (`lib/src/protocol/mux/answers.rs:195`)
  is the one chokepoint that queues a rendered answer onto a `Stream` and arms
  the readiness flags so the writable pass flushes it. The mux fills the
  parse-detail fields (`message`, `phase`, `successfully_parsed`, …) with
  neutral placeholders (`default_answer_for_code`, `mux/answers.rs:134`): it
  has no H1 parse state to report, which is why the `kawa_h1::diagnostics`
  hex-dump renderer had no live consumer and was removed with the `Http`
  session on 2026-09-20.

Status mapping `DefaultAnswer → u16` lives at `mod.rs:110`, and the status
bucket / per-code metrics are emitted once, from
`mux::stream::generate_access_log` (`lib/src/protocol/mux/stream.rs`).

---

## 4. Invariants

These rules are load-bearing — break them and you risk a truncated response,
a wedged session, or a security regression.

1. **Never panic on network-facing input.** Parser failures, oversized bodies,
   bad TLS handshake state, lost backends — all funnel into `DefaultAnswer` +
   metric + log. `unwrap`/`expect`/`panic!` are reserved for hard internal
   invariants. Per repo `CLAUDE.md`, the H1 parser, editor and default-answer
   paths are explicitly listed under "no panic on network-facing input". A
   status line is wire data, not an invariant: kawa parses `HTTP/1.1 000 …`
   into status `0` with no range check, so no bucketer may assert a
   `100..=999` range — locked by
   `mux::stream::tests::a_backend_status_line_below_100_is_bucketed_not_asserted`
   and `answers::tests::an_unrecognised_custom_answer_may_carry_an_out_of_range_status`.

2. **Write-only shutdown on TLS frontends.** Closing the frontend socket with
   `Shutdown::Both` discards any unread receive data and elicits a TCP RST,
   truncating the already-queued response. The canonical write-up lives at
   `lib/src/https.rs:1008-1015`. Backends speak plaintext H1 today, so
   `Shutdown::Both` is permitted on the backend socket; that carve-out must be
   revisited when backend TLS lands.

3. **`tolerant-http1-parser` is opt-in.** The strict parser is the default;
   the tolerant variant is enabled only via the `tolerant-http1-parser`
   feature on `sozu-lib` and `sozu-bin` (`lib/Cargo.toml:121`,
   `bin/Cargo.toml:100`). Tolerant mode relaxes the hostname charset rules
   (`parser.rs:158-181`). It must not be enabled in security-sensitive
   deployments without measuring the risk against the upstream backends'
   strictness.

4. **`HttpContext` outlives a single request when keep-alive is in play.**
   `HttpContext::reset` (`editor.rs`) clears the per-request fields but
   preserves the per-connection ULID (`session_id`, `editor.rs`), the
   SNI-derived TLS state, and the rendered `sozu_id_header` label
   (`editor.rs`). The `request_id` (`HttpContext::id`, `editor.rs`) IS
   rotated per request to keep the access log correlatable.

5. **`impl kawa::AsBuffer for Checkout` is unique to this module.** It lives at
   `mod.rs:30`; the orphan rule permits no second impl **for `Checkout`**, so
   `mux` depends on this one through its own `GenericHttpStream` alias. Do not
   move it without moving every `kawa::Kawa<Checkout>` user with it. The
   sibling `impl AsBuffer for SharedBuffer` (`answers.rs:34`) is a different
   type and unaffected.

---

## 5. Cross-References

- `lib/src/protocol/mux/LIFECYCLE.md` — the H1 and H2 session lifecycle. Every
  question this document used to answer about accept, parse, route, connect,
  forward and close now belongs there.
- `lib/src/protocol/proxy_protocol/LIFECYCLE.md` — PROXY-v2 ingress that
  precedes the mux on PROXY-aware listeners.
- `bin/src/command/LIFECYCLE.md` — supervisor view; explains how listener
  reloads (which can swap `HttpAnswers` templates) propagate into running
  sessions.
- `doc/lifetime_of_a_session.md` — operator-facing prose introduction; this
  document is the maintainer-facing deep dive it links into.
- `doc/configure.md` — listener / cluster / answer configuration reference.
- `e2e/COVERAGE.md > Out of e2e reach by construction` — the reachability
  measurement that retired the `Http` session, kept as the worked example.
