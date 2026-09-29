# Kawa H1 — Shared HTTP/1.1 Vocabulary

Reference document for maintainers of `lib/src/protocol/kawa_h1/`. Companion to
`lib/src/protocol/mux/LIFECYCLE.md` (the H1/H2 datapath that consumes this
module) and `lib/src/protocol/proxy_protocol/LIFECYCLE.md` (PROXY-v2 ingress).

Every claim is anchored to code by symbol — a type, a function, a method or a
field, plus its file — never by line number, because a line number does not
survive an edit above it (converted on 2026-09-28; see `CLAUDE.md`). A stale
symbol here is treated as broken documentation.

**Scope changed on 2026-09-20.** This module used to own an `Http<Front, L>`
session state machine and this document used to describe its lifecycle. That
session was removed (sozu#1346): neither `HttpStateMachine` (`lib/src/http.rs`,
`Expect | Mux | WebSocket`) nor `HttpsStateMachine` (`lib/src/https.rs`,
`Expect | Handshake | Mux | WebSocket`) had a variant holding one, and
`Http::new` had no code caller under either module spelling
(`crate::protocol::kawa_h1::` or the `crate::protocol::http::` re-export,
`kawa_h1 as http` in `lib/src/protocol/mod.rs`). An unconditional
`panic!("PROBEALWAYS …")` planted at the top of `Http::new` and
`save_http_status_metric` fired 0 times across four real proxied e2e sessions,
while the same binary panicked immediately under the function's own unit test.
`TimeoutStatus`, `ResponseStream`, `save_http_status_metric`, **this module's**
`handle_connection_result` (`lib/src/tcp.rs` keeps its own separate copy,
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
| `DefaultAnswer`       | `lib/src/protocol/kawa_h1/mod.rs`             | Catalogue of synthesised replies (301/302/308/400/401/404/408/413/421/429/502/503/504/507) |
| `GenericHttpStream`   | `lib/src/protocol/kawa_h1/mod.rs`             | `kawa::Kawa<Checkout>` — the pooled-buffer parser stream            |
| `HttpContext`         | `lib/src/protocol/kawa_h1/editor.rs`          | Per-request mutable state used by Kawa parser callbacks             |
| `HeaderEditMode` / `HeaderEditSnapshot` | `lib/src/protocol/kawa_h1/editor.rs`        | Per-frontend header-edit programme and its pre-edit snapshot |
| `Method`              | `lib/src/protocol/kawa_h1/parser.rs`          | Owned-string-free method enum                                       |
| `HttpAnswers`         | `lib/src/protocol/kawa_h1/answers.rs`         | Listener + cluster template registry                                |
| `DefaultAnswerStream` | `lib/src/protocol/kawa_h1/answers.rs`         | `Kawa<SharedBuffer>` carrying a rendered default answer             |

Note that `mod.rs`'s `impl kawa::AsBuffer for Checkout` (`mod.rs`) is the
crate's only impl **for `Checkout`** — the orphan rule permits no second copy —
so `mux` depends on it even though `mux` declares its own `GenericHttpStream`
alias. (`answers.rs` carries a separate `impl AsBuffer for SharedBuffer`, the
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
  correlation header named by `sozu_id_header`. The hop is rendered once per
  connection (`ForwardingHop`, `HttpContext::forwarding_hop`, `editor.rs`)
  from its only inputs — the protocol, the public address and the peer
  address — and rendered again only when one of them changes: a synthesised
  `X-Forwarded-For`, `Forwarded`, `X-Real-IP` or `X-Forwarded-Port` shares
  that rendering (`kawa::Store::Shared`), while a client-supplied chain,
  which is request-scoped, is extended into an exact-size copy of its own.
  It also ends a request that declares no body after its headers should
  kawa not have done so already — defense in depth since kawa 0.7.2 (§2.2);
- `on_response_headers` (`editor.rs`) — captures `:status`, `:reason`,
  optionally rewrites `Set-Cookie` for sticky sessions. The reason is kept
  for the access log as the `'static` phrase RFC 9110 §15 registers for the
  code (`standard_reason`, `editor.rs`) when the backend sent exactly that
  phrase, and copied otherwise; the forwarded status line is kawa's own and
  never reads it. It also clears `keep_alive_backend` on a backend
  `Connection: close`, and forwards that header unchanged. That flag is what
  sends a backend EOF into
  `ConnectionH1::terminate_close_delimited` (`lib/src/protocol/mux/h1.rs`),
  where only a body with neither `Content-Length` nor chunked coding ends
  cleanly: a body the close cut short ends in `ParsingPhase::Error`,
  RST_STREAM to an H2 client and a closed connection to an H1 one
  (`lib/src/protocol/mux/LIFECYCLE.md` §8.4). It is also what closes an H1
  client connection after the response, cleanly ended or not:
  `ConnectionH1::writable` keeps the connection only while both
  `keep_alive_frontend` and `keep_alive_backend` hold (RFC 9112 §9.6,
  sozu-proxy/sozu#1642). The callback never clears `keep_alive_frontend`
  itself: `HttpContext` also serves H2 frontends, where that flag sends a
  GOAWAY (`ConnectionH2::write_streams`, `lib/src/protocol/mux/h2.rs`).

  The status line carries Sōzu's own version, not the backend's (RFC 9110
  §6.2, sozu-proxy/sozu#16). kawa's H1 converter writes `HTTP/1.1` for
  `Version::V11` and `Version::V20` but `HTTP/1.0` for `Version::V10`, so
  `on_response_headers` rewrites an HTTP/1.0 response to `Version::V11`; the
  H2 converter ignores the version. HTTP/1.1 is persistent by default where
  HTTP/1.0 is not (RFC 9112 §9.3), so the same branch clears
  `keep_alive_backend` for an HTTP/1.0 response that lacks a `keep-alive`
  connection option, has a close-delimited body, or carries
  `Transfer-Encoding` (faulty framing in HTTP/1.0, RFC 9112 §6.1). It then
  merges the response's `Connection` options into one line ending in
  `close` (RFC 9112 §9.6): every option that nominates a hop-by-hop field
  survives (RFC 9110 §7.6.1), and only the `keep-alive` the close
  contradicts is dropped. That flag then does what a backend
  `Connection: close` does above: the backend EOF ends a close-delimited
  body, and the H1 client connection closes after the response, so a
  backend that answers HTTP/1.0 without `keep-alive` costs its H1 clients a
  new connection per response. The H2 converter drops the merged line with
  every connection-specific one, and the H2 connection stays open. A 1xx is
  left alone apart from its version: its persistence belongs to the final
  response, which runs the callback again, and a 101 keeps its
  `Connection: Upgrade`. A persistent HTTP/1.0 response
  (`Connection: keep-alive` with a length) keeps both connections and its
  header. The `close` option is matched as a list token, for every version,
  so `Connection: keep-alive, close` closes the backend too. Covered by
  `an_http10_keep_alive_response_is_forwarded_as_http11`,
  `an_http10_response_without_keep_alive_is_forwarded_as_http11_with_close`,
  `a_close_option_in_a_connection_list_closes_the_backend`,
  `a_non_persistent_http10_response_keeps_its_connection_options`,
  `a_chunked_http10_response_closes_the_backend`,
  `an_http10_101_keeps_its_connection_upgrade`,
  `an_http10_100_leaves_persistence_to_the_final_response` (unit, in
  `editor.rs`), the `test_h1_http10_*` rows of
  `e2e/src/tests/mux_tests.rs` and the `test_h2_http10_*` rows of
  `e2e/src/tests/h2_correctness_tests.rs`.

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
`callbacks.on_headers` is called at all. Two consequences, read from kawa's
`src/protocol/h1/parser/mod.rs` rather than measured here (0.7.1 and 0.7.2, the
version this workspace locks, resolve Transfer-Encoding identically):

- a leading `Transfer-Encoding: chunked` line can no longer latch chunked framing
  while a later `identity` line rides along. That latch was a kawa **0.7.0**
  shape, and it is the shape the second and third clauses below were written
  against;
- the residual gap is the mirror of it: a chunked-final LAST line with a
  differently-framed EARLIER one — `identity` then `chunked` — parses clean under
  kawa >= 0.7.1, and forwarding both lines is what the count clause refuses.

The guard therefore folds over every non-elided `Transfer-Encoding` header in
`request.blocks` (the `te_count` fold at the top of
`HttpContext::on_request_headers`, `lib/src/protocol/kawa_h1/editor.rs`),
producing `te_count` and `te_all_suffix_chunked` — the latter true only when EVERY such value's literal
trailing bytes are `chunked` (`compare_no_case` over the last seven bytes). The
rejection predicate is exactly (the `if` that follows the fold):

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
  can fire on a request kawa >= 0.7.1 accepted**, in the `identity`-then-`chunked`
  shape above;
- one surviving TE header whose final coding is not `chunked`
  (`!te_all_suffix_chunked`), e.g. `Transfer-Encoding: chunked, gzip`;
- one surviving TE header while kawa did not adopt chunked framing
  (`body_size != BodySize::Chunked`).

The last two cannot fire under kawa >= 0.7.1: a TE line that survives to this point
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

#### Content-Length value clause

Right after the Transfer-Encoding predicate, `on_request_headers` rejects a
request whose non-elided `Content-Length` value is not exclusively ASCII
digits (`has_non_digit_content_length`, `editor.rs`). RFC 9110 §8.6 defines
`Content-Length = 1*DIGIT` and forbids a sender to forward a message whose
value does not match it; RFC 9112 §6.3 rule 5 makes it an unrecoverable
framing error (400 for a request, 502 for a response received by a proxy).

kawa 0.7.1's `process_headers` read the value with `nom::ParseTo`, i.e.
`usize::from_str`, which accepts one leading `+`, and left the field line
in place with its original spelling (CleverCloud/kawa#25). Without the
clause, `Content-Length: +5` framed a 5-byte body and reached the backend as
`+5`: a backend that refuses or re-reads that spelling takes the body for the
start of the next request, never routed nor Basic-auth checked (CWE-444,
sozu-proxy/sozu#1652).

kawa **>= 0.7.2** checks the value against `1*DIGIT` itself, before parsing it
and before `callbacks.on_headers` is called
([CleverCloud/kawa#26](https://github.com/CleverCloud/kawa/pull/26)), so every
non-digit spelling is refused by kawa with `Invalid Content-Length field
value`, and the parse error is still answered 400 (502 for a response). The
clause no longer fires on any traffic kawa accepts: it stays as defense in
depth against a kawa regression, and the `FRONTEND_CONTENT_LENGTH_INVALID` /
`BACKEND_CONTENT_LENGTH_INVALID` counters stay at zero while kawa holds —
such a message is counted in `http.frontend_parse_errors` /
`http.backend_parse_errors` instead. Do not delete the clause on that basis.

| Value | kawa 0.7.1 | kawa >= 0.7.2 (locked) |
|---|---|---|
| `+5`, `+0` | accepted by kawa; rejected by this clause (400) | refused by kawa (400); the clause is defense in depth |
| `-0`, `+`, `5 5`, `0x5`, `5.0`, overflow, non-ASCII digits, empty | refused by kawa (400) | refused by kawa (400) |
| `5, 5` | refused by kawa (400) | refused by kawa (400); RFC 9110 §8.6 lets a recipient reject or collapse a list of equal values, and Sōzu rejects |
| `005` | `1*DIGIT`: accepted and forwarded as sent, as the H2 path forwards it | same |

Only non-elided lines are judged, because they are the lines the H1
serializer forwards: kawa elides a second line equal to the first and
every `Content-Length` beside a `Transfer-Encoding` (RFC 9110 §6.3). Under
kawa 0.7.1, `5` then `+5` forwarded `5` (the second line parsed equal and was
elided) while `+5` then `5` was rejected by this clause; kawa >= 0.7.2 judges
every line against `1*DIGIT` first, so both orders are refused by kawa. The guard
increments `names::http::FRONTEND_CONTENT_LENGTH_INVALID`, logs a `warn!`
and errors the parse, like the predicate above. The clause cannot fire for an
H2 request: `pkawa::write_regular_header` already refuses a non-digit
`content-length` with `RejectReason::DuplicateCl` before this callback runs.

`on_response_headers` applies the same check first, before the 204/304/1xx
override of `body_size`, and increments
`names::http::BACKEND_CONTENT_LENGTH_INVALID`. The failed response parse makes
`ConnectionH1::readable` end the backend stream, and
`shared::end_stream_decision` answers the client 502 because no byte of the
response was consumed.

Covered by `a_request_content_length_that_is_not_only_digits_is_rejected`,
`a_forwarded_content_length_is_only_digits`,
`a_response_content_length_that_is_not_only_digits_is_rejected` and
`the_content_length_helper_judges_every_non_digit_value` (unit, in
`editor.rs`) and by `test_h1_signed_content_length_request_rejected`,
`test_h1_signed_content_length_response_rejected` and
`test_h1_leading_zero_content_length_forwarded` in
`e2e/src/tests/h1_security_tests.rs`. Under kawa >= 0.7.2 the parse-driven
tests and the e2e tests pin the end-to-end outcome — kawa's refusal and the
clause together — so deleting the clause alone leaves them green. The only
test that still goes red on the clause by itself is
`the_content_length_helper_judges_every_non_digit_value`, which calls the
helper directly. To see the parse-driven tests red on the clause, pin
kawa 0.7.1 (`kawa = { version = "=0.7.1", … }` in the root `Cargo.toml`, then
`cargo update -p kawa --precise 0.7.1`) and delete the clause: the `+5`
rows then parse clean.

The resulting `ParsingPhase::Error` is observed by the mux H1 connection in
`ConnectionH1::readable` (`lib/src/protocol/mux/h1.rs`), which checks
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
(`lib/src/protocol/mux/pkawa.rs`).

### 2.2 A request without Content-Length or Transfer-Encoding has no body

RFC 9112 §6.3 rule 7: a request with neither header has a zero-length body,
whatever its method or version. Read-until-close (rule 8) belongs to
responses only. kawa 0.7.1 did not make that distinction: after
`process_headers`, `kawa::h1::parse` mapped `BodySize::Empty` to
`ParsingPhase::Body` with `expects = 1` for both kinds, and its `Body` arm
takes every byte left in the buffer when `body_size` is `Empty`
(CleverCloud/kawa#23). Left alone, a request pipelined behind a plain `GET`
became that `GET`'s body and was forwarded raw to its backend — never routed,
never checked by `check_basic`, without `Sozu-Id` or `X-Forwarded-*`
(CWE-444, sozu-proxy/sozu#1650).

kawa **>= 0.7.2** applies rule 7 itself
([CleverCloud/kawa#27](https://github.com/CleverCloud/kawa/pull/27)): for a
request, `kawa::h1::parse` maps `BodySize::Empty` to
`ParsingPhase::Terminated` before calling `callbacks.on_headers`, pushes the
end-of-headers `Flags` block with `end_stream: true`, and leaves the bytes
that follow in `unparsed_data()`. Responses keep the close-delimited mapping.
The request is therefore forwarded complete, `ConnectionH1::readable` stops
reading the frontend, and the pipelined request stays unparsed until the
keep-alive branch of `ConnectionH1::writable` (`lib/src/protocol/mux/h1.rs`)
parses, routes and edits it on its own. `body_size` stays `Empty`: nothing is
injected on the wire, the phase alone ends the message.

`HttpContext::on_request_headers` (`editor.rs`) still sets
`ParsingPhase::Terminated` when it finds `parsing_phase == ParsingPhase::Body`
and `body_size == BodySize::Empty`. Under kawa >= 0.7.2 that branch never
fires — the H1 parser no longer calls back in that state — and it stays as
defense in depth against a kawa regression; do not delete it on that basis.
kawa pushes the end-of-headers `Flags` block right after the callback with
`end_stream: kawa.is_terminated()`, which is why a phase set there would
still end the stream.

- **The `ParsingPhase::Body` conjunct confines the rule to the H1 parser.**
  `pkawa::handle_header` (`lib/src/protocol/mux/pkawa.rs`) calls the same
  `on_headers` for an H2 request while `body_size` is still `Empty` and the
  phase still the initial one, then frames a request whose DATA follows as
  chunked — unless the callback terminated it. `Empty` alone would drop those
  bodies (`an_h2_request_is_left_for_pkawa_to_frame`).
- **Responses keep their framing.** `on_response_headers` never runs this
  rule, so a response without length stays close-delimited, ended by
  `ConnectionH1::terminate_close_delimited`. Only an HTTP/1.0 response has
  its version and `Connection` header edited (§2), never its length.
- **Interactions.** `Expect: 100-continue` with no length has no body to wait
  for. A WebSocket `GET` with `Upgrade` has no body either; the 101 still
  switches to the pipe. Bytes a client sends behind the upgrade request
  before the 101 (which RFC 6455 §4.1 forbids) are held and reach the
  backend after the 101, through the pipe, whether they share the request's
  segment or not; they used to be forwarded before the 101 as the request's
  body. Measured on 2026-09-28 with a temporary e2e probe on a clear
  listener. `CONNECT` is not tunnelled by Sōzu: its request ends
  after its headers like any other, where it used to stream client bytes to
  the backend as a close-delimited "body". HTTP/1.0 follows the same rule.

Covered by `a_request_without_length_ends_after_its_headers`,
`a_framed_request_and_an_unframed_response_keep_their_bodies` (unit, in
`editor.rs`) and the `test_h1_unframed_request_then_*` rows of
`e2e/src/tests/h1_security_tests.rs`, which pipeline a second request behind
an unframed `GET`, `HEAD`, `DELETE` or `POST` in one write, to the same
cluster, another cluster and a Basic-auth-gated cluster, over clear and TLS
listeners. Under kawa >= 0.7.2 these tests pin kawa's rule 7 and the Sōzu
branch together, so deleting the branch alone leaves them green; to see
them red on the branch, pin kawa 0.7.1 (`kawa = { version = "=0.7.1", … }` in
the root `Cargo.toml`, then `cargo update -p kawa --precise 0.7.1`) and
delete it.

---

## 3. Default Answers

Synthesised 3xx/4xx/5xx responses are not handcrafted byte buffers — they are
template-rendered Kawa streams. The relevant pieces:

- `DefaultAnswer` enum (`lib/src/protocol/kawa_h1/mod.rs`) lists every
  variant Sōzu can emit (301/302/308 redirects, 400, 401, 404, 408, 413, 421,
  429, 502, 503, 504, 507) and carries the per-call diagnostic strings.
- `Template`, `Replacement` and `TemplateVariable` (`answers.rs`) drive the
  substitution engine:
  variables can be plain (text) or typed (URL/path/integer) — typed
  substitutions go through the corresponding sanitizer to avoid log /
  header injection.
- `HttpAnswers` (`answers.rs`) holds the registry; its `cluster_answers` /
  `listener_answers` split (`HttpAnswers::cluster_answers`,
  `HttpAnswers::listener_answers`) lets a cluster override a
  listener-level template. `HttpAnswers::get` (`answers.rs`) is the
  selection chokepoint: its lookup key is derived from the `DefaultAnswer`
  variant, so only the built-in code names ("301" … "507") and the bundled
  fallback are ever selectable — an operator's custom answer under an
  unrecognised name is compiled but never chosen.
- `mux::answers::set_default_answer` (`lib/src/protocol/mux/answers.rs`)
  is the one chokepoint that queues a rendered answer onto a `Stream` and arms
  the readiness flags so the writable pass flushes it. It replaces a response
  only while none of it has left: `end_stream_decision`
  (`lib/src/protocol/mux/shared.rs`) no longer picks it for a failed response
  whose kawa is `consumed`, which would put the answer behind bytes the client
  already has. The mux fills the
  parse-detail fields (`message`, `phase`, `successfully_parsed`, …) with
  neutral placeholders (`default_answer_for_code`, `mux/answers.rs`): it
  has no H1 parse state to report, which is why the `kawa_h1::diagnostics`
  hex-dump renderer had no live consumer and was removed with the `Http`
  session on 2026-09-20.

Status mapping `DefaultAnswer → u16` lives in `impl From<&DefaultAnswer> for u16`
(`mod.rs`), and the status
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
   truncating the already-queued response. The canonical write-up is the
   comment above the `mux::shutdown_write` call in `HttpsSession::close`
   (`lib/src/https.rs`). Backends speak plaintext H1 today, so
   `Shutdown::Both` is permitted on the backend socket; that carve-out must be
   revisited when backend TLS lands.

3. **`tolerant-http1-parser` is opt-in.** The strict parser is the default;
   the tolerant variant is enabled only via the `tolerant-http1-parser`
   feature on `sozu-lib` and `sozu-bin` (the `tolerant-http1-parser` entry of
   the `[features]` table in `lib/Cargo.toml` and `bin/Cargo.toml`). Tolerant
   mode relaxes the hostname charset rules (the two `cfg`-gated
   `is_hostname_char` in `parser.rs`). It must not be enabled in security-sensitive
   deployments without measuring the risk against the upstream backends'
   strictness.

4. **`HttpContext` outlives a single request when keep-alive is in play.**
   `HttpContext::reset` (`editor.rs`) clears the per-request fields but
   preserves the per-connection ULID (`session_id`, `editor.rs`), the
   SNI-derived TLS state, the connection's rendered forwarding values
   (`forwarding_hop`, `editor.rs`), and the rendered `sozu_id_header` label
   (`editor.rs`). The request id (`HttpContext::id`, `editor.rs`) IS
   rotated per request: `reset` takes the next request's id as its argument,
   and the keep-alive branch of `ConnectionH1::writable`
   (`lib/src/protocol/mux/h1.rs`) mints it with `Context::next_request_id`
   (`lib/src/protocol/mux/mod.rs`), the per-session source that also numbers
   H2 streams — no syscall, no allocation. `Sozu-Id` (request and response),
   a generated `X-Request-Id`, `%REQUEST_ID` and the access log's
   `request_id` therefore agree within a request and differ between two
   requests of one connection. A client-supplied `X-Request-Id` is still
   forwarded verbatim. Pinned by `header_editing_output_is_byte_exact_across_keep_alive_requests`
   (`editor.rs`) and `test_keep_alive_rotates_request_id`
   (`e2e/src/tests/tests.rs`).

5. **`impl kawa::AsBuffer for Checkout` is unique to this module.** It lives at
   `mod.rs`; the orphan rule permits no second impl **for `Checkout`**, so
   `mux` depends on this one through its own `GenericHttpStream` alias. Do not
   move it without moving every `kawa::Kawa<Checkout>` user with it. The
   sibling `impl AsBuffer for SharedBuffer` (`answers.rs`) is a different
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
