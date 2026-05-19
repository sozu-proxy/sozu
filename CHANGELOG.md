# Changelog

## [Unreleased]

The v1.1.1 → v2.0.0 entries below are grouped by Keep-a-Changelog category.
Multiple Keep-a-Changelog blocks of the same kind appear in sequence — bullets
stay where they were authored; they will be merged into a single block per
category at tag time.

### 💥 Breaking changes (1.1.x → 2.0.0)

The 2.0.0 release carries breaking changes that operators must address before
upgrade. See `doc/upgrade/1.x-to-2.0.md` for the full migration guide.

- **Renamed metrics ([#1196](https://github.com/sozu-proxy/sozu/pull/1196))**:
  `client.connections_percentage` → `client.connections_percent`;
  `client.max_connections` → `client.connections_max`; `buffer.number` →
  `buffer.in_use`. Dashboards scraping the old names need updating.
- **`http.errors` now labelled with `(cluster_id, backend_id)`
  ([#1196](https://github.com/sozu-proxy/sozu/pull/1196))**.
- **Pattern-trie segment regex anchoring**: every segment-level regex inserted
  into the routing trie is now wrapped with `\A...\z` at insert time. Operators
  who relied on partial-segment matching (e.g. `cdn[0-9]+` rule inadvertently
  matching `cdn123xxx`) need to widen their patterns explicitly.
- **HAProxy-style `del-header` semantic**: empty `Header.val` now removes the
  named header. Operators who already configured custom headers with the empty
  string as a value should set a whitespace-only value instead.
- **Sub-16 393 `buffer_size` rejected at config load when any HTTPS listener
  advertises H2 ALPN**. Operators wanting a smaller buffer must drop `"h2"` from
  the affected listeners' `alpn_protocols`.
- **Cipher list constant rename
  ([#1191](https://github.com/sozu-proxy/sozu/pull/1191))**: internal
  `DEFAULT_RUSTLS_CIPHER_LIST` renamed to `DEFAULT_CIPHER_LIST`. The TOML key
  (`cipher_list`) is unchanged. `P-521` removed from default `groups_list`
  (rustls 0.23 does not support it).
- **Renamed metric `http.301.redirect_template.compile_error` →
  `http.redirect_template.compile_error`**: inline-template compile failures now
  surface under a single status-agnostic counter (302 and 308 templates can fail
  the same way; the per-status form would have left those failures unmonitored).
  Dashboards scraping the old per-status name need updating; the new key is
  documented in `doc/configure.md` alongside the new `http.302.redirection` and
  `http.308.redirection` counters.

### ⛑️ Fixed

- **H2 connection coalescing accepted on certificate SANs (RFC 7540 §9.1.1 /
  RFC 9113 §9.1.1, RFC 6125 §6.4.3 wildcard handling)**: the strict
  `:authority == TLS SNI` check at `lib/src/protocol/mux/router.rs` rejected
  Firefox / Chrome coalesced H2 streams on wildcard certificates as **421
  Misdirected Request**. Browsers reuse one H2 connection for any origin
  covered by the served certificate's SAN set (Firefox
  `nsHttpConnectionMgr::FindCoalescableConnection*` →
  `Http2Session::RealJoinConnection`; Chrome
  `SpdySession::VerifyDomainAuthentication` →
  `X509Certificate::VerifyNameMatch`); both retry cleanly on 421. Sōzu now
  matches each request's `:authority` (H2) / `Host` (H1) against the SAN+CN
  set of the certificate it actually served on the TLS session — captured
  once at handshake into `Context::tls_cert_names` and `Arc`-shared across
  every per-stream `HttpContext`. The 421 path remains for genuine SAN
  misses (RFC 9110 §15.5.20). The default-cert path serves an explicit
  empty SAN set so every `:authority` is still rejected. Existing
  `strict_sni_binding = false` opt-out is unchanged. A new
  `h2.coalescing.accepted` counter records legitimate coalesced acceptances
  (matched SAN ≠ initial SNI) for multi-tenant observability, alongside a
  `debug!` log on the same path. Public-suffix wildcard blocking (e.g.
  `*.com`) is intentionally not implemented — that is a browser-policy
  concern, not a reverse-proxy responsibility.

- **Response-side header edits no longer re-injected on every prepare
  cycle (`H2BlockConverter::finalize` "out buffer not empty" leak)**:
  `apply_response_header_edits` was called inconditionally before
  every `kawa.prepare(...)` pass for a stream
  (`lib/src/protocol/mux/h2.rs` and `lib/src/protocol/mux/h1.rs`).
  When `write_streams` re-entered the same stream for a follow-on
  body chunk (multi-frame body, flow-control yield, RFC 9218
  same-urgency round-robin), the helper re-injected each
  `headers_response` edit AFTER the
  `Block::Flags { end_header: true }` anchor had already been
  consumed — `end_of_headers_index` returned `None` and the helper
  fell back to `kawa.blocks.len()`, appending the edit past every
  remaining DATA block. The next prepare cycle encoded the orphan
  `Block::Header` into `H2BlockConverter.out` with no closing flags
  block to flush it as a HEADERS frame, and `finalize` tripped the
  "out buffer not empty (38 bytes remaining), clearing"
  defense-in-depth log on every re-entry. 38 bytes is the
  static-table HPACK encoding of a typical
  `strict-transport-security: max-age=…; includeSubDomains` header,
  which is how the symptom surfaced in production once
  listener-default HSTS reached a non-trivial share of frontends on
  a Clever Cloud `cleverapps.io` shared node. Beyond the spam, the
  orphan encode also mutated the HPACK encoder's dynamic table
  without the peer ever receiving the matching wire frame —
  silent decoder desync that would have manifested as decode errors
  on subsequent requests sharing the H2 connection. On H1 the same
  pattern would have surfaced as duplicate `Strict-Transport-Security`
  headers on the wire (RFC 6797 §6.1 — UAs MAY ignore the response
  when more than one STS header is present). The fix drains
  `parts.context.headers_response` via `mem::take` at both apply
  sites so the injection runs exactly once per response. Regression
  test in `e2e/src/tests/hsts_tests.rs::try_hsts_multi_frame_body_no_leak_no_duplicate_sts`
  asserts a 256 KiB body (~16 H2 DATA frames) returns intact and
  carries STS exactly once.
- **Listener-default HSTS now refreshes "no policy" frontends too**:
  `Router::refresh_inheriting_hsts` (`lib/src/router/mod.rs`) previously
  walked only `Route::Frontend` entries — but the routing fast path in
  `add_http_front_with_hsts_origin` stores frontends without ANY
  per-frontend policy field (no redirect, no rewrite, no headers, no
  auth, no `[hsts]` block) under the lightweight `Route::ClusterId` /
  `Route::Deny` shapes, NOT as `Route::Frontend`. As a result, an
  `UpdateHttpsListenerConfig.hsts` patch silently skipped every
  "no-policy" inheriting frontend on the listener — observed on a
  Clever Cloud `cleverapps.io` shared node where only ~60 of 91 k+
  inheriting frontends were refreshed per patch (~0.07 % of the
  fleet), leaving 99 %+ of HTTPS responses without the
  `Strict-Transport-Security` header that the operator had just
  configured. The walk now visits all three `Route` variants: existing
  `Route::Frontend` entries follow the original path-1 rebuild, and
  `Route::ClusterId(id)` / `Route::Deny` entries are promoted in place
  to a minimal `Route::Frontend` carrying just the listener-default
  HSTS edit (`Frontend::minimal_forward` / `Frontend::minimal_deny`)
  with `inherits_listener_hsts = true` so subsequent patches keep
  refreshing them. Routing semantics are preserved — the promoted
  `Frontend` forwards (resp. denies) identically to the lightweight
  variant. Promotion is gated on `would_emit_hsts(new_hsts)` so a
  patch that resolves to "no HSTS" (None / `enabled = Some(false)` /
  malformed `enabled = true` with no `max_age`) is a no-op on
  lightweight routes — no allocation is created just to hold an empty
  edit. `http.hsts.frontend_refreshed` now counts path-1 refreshes +
  path-2 promotions combined. Regression tests in
  `lib/src/router/mod.rs::tests`:
  `refresh_inheriting_hsts_promotes_clusterid_on_enable`,
  `refresh_inheriting_hsts_promotes_deny_on_enable`,
  `refresh_inheriting_hsts_skips_lightweight_on_disable`,
  `refresh_inheriting_hsts_promoted_entry_refreshes_on_subsequent_patches`,
  `refresh_inheriting_hsts_promoted_entry_loses_hsts_on_disable_patch`.
- **`LoadState` accepts JSON state files from older `sozu-command-lib` clients
  (`1.1.1` forward-compat)**: `bin/src/command/requests.rs::load_state` reads
  each `\n\0`-separated JSON record via
  `command::parser::parse_several_requests::<WorkerRequest>`, which calls
  `serde_json::from_slice` per record. The `command/build.rs` prost-build config
  attaches `Serialize`/`Deserialize` derives to every generated message but did
  not attach `#[serde(default)]` anywhere, so missing `repeated`/`map` fields
  rejected the record (`Vec<T>` and `BTreeMap<K, V>` are required-by-serde
  without an explicit default). Post-1.1.1 schema additions (`Cluster.answers`,
  `Cluster.authorized_hashes`, `RequestHttpFrontend.headers` from #1231;
  `HttpListenerConfig.answers`, `HttpsListenerConfig.alpn_protocols` /
  `answers`, `TcpListenerConfig.answers` from the same wave) therefore broke
  `LoadState` for any older client (e.g. `proxy-manager` pinned to
  `sozu-command-lib = "1.1.1"`): the first `AddCluster` or `AddHttpFrontend`
  failed to deserialize, `parse_several_requests`'s `many0(complete(...))` left
  the unparsed bytes as the remainder, and the read loop in `load_state`
  reported `"Error consuming load state message"` to the client at EOF. Each
  post-1.1.1 `repeated`/`map` field on a state-file-emittable message now
  carries `#[serde(default)]` via `command/build.rs` `field_attribute(...)`, so
  missing fields default to empty (mirroring the protobuf wire-format default)
  while required scalars stay strict. Without strictness on scalars,
  `ConfigState::add_cluster` would silently insert a `""`-keyed entry — the
  field-level annotation preserves that defense-in-depth. Forward additions are
  guarded by `command/tests/state_compat_v1_1_1.rs`, which pins both the 1.1.1
  fixture round-trip and the missing-required-scalar rejection contract; a
  documentation comment at the top of `command/src/command.proto` and at the
  relevant block in `command/build.rs` codifies the new rule for future
  `repeated`/`map` field additions.
- **Certificate chain dedup: drop the leaf when ACME `fullchain.pem` is supplied
  as the chain ([#1135](https://github.com/sozu-proxy/sozu/issues/1135),
  [#1148](https://github.com/sozu-proxy/sozu/issues/1148))**:
  `lib/src/tls.rs::TryFrom<&AddCertificate> for CertifiedKeyWrapper` previously
  stored the cert chain as `[leaf, ...cert.certificate_chain]` without
  inspecting whether the supplied chain already contained the leaf. Many ACME
  clients (Certbot's `fullchain.pem`, lego, acme.sh) emit the leaf at the START
  of the chain file, so the on-wire chain became
  `[leaf, leaf, intermediate, root]` — accepted by browsers but rejected by
  stricter validators (Node.js → `UNABLE_TO_VERIFY_LEAF_SIGNATURE`, the symptom
  in #1148). Two robustness passes are now applied: each chain entry is split
  through `split_certificate_chain` so a single string carrying multiple PEM
  blocks (operators concatenating fullchain contents into one chain entry,
  common via the runtime API) is fanned out into one entry per CA before parsing
  (without the split, `parse_pem` only consumes the first PEM block in the
  string and silently drops the rest), and each split entry's DER bytes are
  compared against the leaf's DER and dropped on match. Regression tests
  `certificate_chain_dedup_drops_duplicate_leaf` and
  `certificate_chain_handles_multi_pem_single_entry` in `lib/src/tls.rs::tests`.
  An `info!` log fires once per add when at least one duplicate is dropped.
- **Clusterless `RedirectPolicy::Permanent` frontends now emit 301 instead of
  401**: `Router::route_from_request` in `lib/src/protocol/mux/router.rs`
  previously short-circuited every `cluster_id == None` route to
  `UnauthorizedRoute` BEFORE the `RedirectPolicy::Permanent` branch, so a
  frontend declared with `redirect = permanent` and no backing cluster (the
  canonical "this hostname has moved, no service remains" shape from the
  original proposal in [#1161](https://github.com/sozu-proxy/sozu/issues/1161))
  returned 401 instead of the documented 301. The `Permanent` branch is now
  resolved before the clusterless-deny short-circuit. The `Permanent` block is
  data-flow safe for clusterless callers because the cluster-derived knobs
  (`https_redirect_port`, `www_authenticate`, `authorized_hashes`) default to
  safe sentinels at the cluster lookup when `cluster_id` is `None`. Regression
  test `try_clusterless_permanent_redirect_emits_301` in
  `e2e/src/tests/redirect_rewrite_auth_tests.rs`.
- **Non-trie host regex (`DomainRule::Regex`) is now anchored at both ends**:
  `convert_regex_domain_rule` in `lib/src/router/mod.rs` previously emitted an
  unanchored regex string. The `regex` crate's `Regex::is_match` is unanchored
  by default, so an operator's `/example\.com/` regex matched any hostname
  containing `example.com` as a substring — including
  `attacker.example.com.evil.org` — letting an attacker-controlled domain reach
  a frontend that should only serve `example.com` (CWE-1023, routing bypass).
  The compiled regex now opens with `\A` and ends with `\z` so only full-host
  matches succeed. The trie path (`lib/src/router/pattern_trie.rs`) was already
  anchored. Regression test `regex_domain_rule_rejects_suffix_and_prefix` in
  `lib/src/router/mod.rs`.
- **Multi-segment regex hostnames are no longer mis-parsed by
  `convert_regex_domain_rule`**: the inner `/`-finding loop did not `break`
  after the first matching `/`, so for a hostname like `/seg1/.foo./seg2/.com`
  every later `/` overwrote the segment index and the literal `.` separators
  between regex segments were swallowed into the regex segment. The expected
  output `seg1\.foo\.seg2\.com` (anchored: `\Aseg1\.foo\.seg2\.com\z`) is now
  produced correctly. Regression test
  `regex_domain_rule_multi_segment_segments_are_isolated` in
  `lib/src/router/mod.rs`.
- **`%STATUS_CODE` documentation/code drift removed**: `doc/configure.md`, the
  `redirect-template` CLI help in `bin/src/cli.rs`, and the `redirect_template`
  doc comments in `command/src/command.proto` and `command/src/config.rs`
  advertised a `%STATUS_CODE` placeholder that was never defined in the
  template-variable table at
  `lib/src/protocol/kawa_h1/answers.rs::HttpAnswers::template`. References to
  the placeholder are removed from docs and CLI help so operators no longer see
  a promised variable that silently no-ops. A future change can introduce the
  variable for real once 302/308 redirect support lands
  ([#1009](https://github.com/sozu-proxy/sozu/issues/1009)).
- **TLS certificate hot-rotation no longer drops the working certificate on
  failure ([#1202](https://github.com/sozu-proxy/sozu/pull/1202))**:
  `CertificateResolver::replace_certificate` in `lib/src/tls.rs` now adds the
  new certificate before removing the old one. A parse or signing-key failure on
  the new certificate leaves the previous certificate in place, so the listener
  keeps serving traffic across rotation hiccups instead of going cert-less for
  the affected name. Closes
  [#774](https://github.com/sozu-proxy/sozu/issues/774).
- **`name_fingerprint_idx` no longer leaks empty entries on certificate removal
  ([#1202](https://github.com/sozu-proxy/sozu/pull/1202))**:
  `CertificateResolver::remove_certificate` in `lib/src/tls.rs` now drops the
  index entry when its last fingerprint is removed, instead of leaving an empty
  `Vec` behind. Prevents a slow memory and key-cardinality leak on long-running
  listeners with high certificate churn. The `drain(..).filter(...).collect()`
  pattern is replaced with `Entry::Occupied` + `retain` + explicit empty-entry
  deletion.
- **Fail-open routing keeps the retry-policy gate
  ([#1191](https://github.com/sozu-proxy/sozu/pull/1191))**:
  `BackendList::next_available_backend` in `lib/src/backends.rs` now filters
  fail-open candidates with
  `status == Normal && retry_policy.can_try() == OKAY`, instead of only
  `status == Normal`. Backends in exponential-backoff after repeated connect
  failures are skipped during their back-off window — hammering a backend at
  line rate during its own back-off would defeat the purpose of the back-off
  itself. When zero candidates remain after the retry gate, the function returns
  `None` and the upstream caller emits the configured 503 default-answer.
- **Fail-open `warn!` is latched once per regime
  ([#1191](https://github.com/sozu-proxy/sozu/pull/1191))**: previously the
  `fail-open: all backends unhealthy` warning fired on every routing decision;
  under universal outage (the exact scenario fail-open targets) that produced
  thousands of identical lines per worker per second. The warning is now latched
  on regime entry and a paired `info!` log fires once on regime exit when the
  cluster recovers. The per-request signal is preserved as a counter
  (`backends.fail_open`) so dashboards keep a quantitative measure that doesn't
  drown logs.
- **`health_check.healthy_backends` gauge emits on every result update including
  0 ([#1191](https://github.com/sozu-proxy/sozu/pull/1191))**:
  `lib/src/health_check.rs` previously only emitted the gauge when
  `healthy > 0 && healthy * 2 <= total`, so when all backends went unhealthy and
  fail-open kicked in the gauge retained its last non-zero value and operators
  could not detect the universal-outage state on dashboards. The gauge now
  updates after every probe result for clusters with at least one configured
  backend; the "critically low" warning (≤50% healthy) keeps its existing
  condition.
- **`--version` ANSI escape codes only emitted on a TTY
  ([#1191](https://github.com/sozu-proxy/sozu/pull/1191))**:
  `bin/src/cli.rs::parse_args` previously hard-coded `\x1b[32m` / `\x1b[31m`
  around the `+`/`-` feature-flag indicators. Redirected output
  (`sozu --version > file`, package-metadata capture, systemd journal) carried
  raw escape codes. The `+`/`-` colouring is now gated on
  `std::io::stdout().is_terminal()` — colour on an interactive terminal, plain
  text everywhere else.
- **`bin/src/cli.rs` `TLS_V13` parsing now returns `TlsVersion::TlsV13`
  ([#1191](https://github.com/sozu-proxy/sozu/pull/1191))**: the CLI/TOML parser
  had `"TLS_V13" => Ok(TlsVersion::TlsV12)`, silently downgrading any operator
  who configured `TLS_V13` in their cipher list. Adjacent `eprintln!`
  deprecation warnings for `TLS_V10`/`TLS_V11` keep the configured behaviour but
  cite RFC 8996 explicitly.
- **`sozu --version` reports the effective crypto provider via the lib's
  precedence chain**: previously the four crypto-provider flags (`crypto-ring`,
  `crypto-aws-lc-rs`, `crypto-openssl`, `fips`) reported the literal Cargo
  feature activation set, so `cargo build --features fips` printed
  `+crypto-ring -crypto-aws-lc-rs -crypto-openssl +fips` (default `crypto-ring`
  still on, bin's own `crypto-aws-lc-rs` not propagated by
  `fips = ["sozu-lib/fips"]`). The runtime provider is selected by
  `lib/src/crypto.rs::default_provider()` via the chain
  `fips > ring > aws-lc-rs > openssl`, so the banner did not match what rustls
  actually loaded. `bin/Cargo.toml` now declares
  `fips = ["crypto-aws-lc-rs", "sozu-lib/fips"]` (mirroring the lib's
  `fips = ["crypto-aws-lc-rs", "rustls/fips"]`) so the bin-level feature graph
  stays consistent with the lib's, and `bin/build.rs` post-processes the four
  crypto-provider flags through the same precedence chain. Only the effective
  provider is emitted as `+`, the rest are forced to `-`; because `fips` is a
  build-mode layered on top of `crypto-aws-lc-rs`, when `fips` wins the chain
  `crypto-aws-lc-rs` is also reported as `+` (rustls still loads aws-lc-rs
  underneath, just driven through its FIPS profile). Result for
  `cargo build --features fips`:
  `-crypto-ring +crypto-aws-lc-rs -crypto-openssl +fips`. Other features
  (`jemallocator`, `simd`, `splice`, etc.) are unaffected and continue to report
  literal feature activation.
- **`lib/src/tcp.rs` echo test reads in a loop
  ([#1191](https://github.com/sozu-proxy/sozu/pull/1191))**: the existing
  TCP-proxy echo test asserted on a single `read()` of the second response,
  which is not guaranteed to return all bytes when the upstream round trip is
  still in flight. The test now reads in a loop until the expected payload
  length is satisfied, eliminating a flake that surfaced under load on
  `sozu-e2e`.
- **FreeBSD and NetBSD: stop bundling jemalloc
  ([#1076](https://github.com/sozu-proxy/sozu/issues/1076))**: the
  `jemallocator` Cargo dep in `bin/Cargo.toml` is now declared under
  `[target.'cfg(not(any(target_os = "freebsd", target_os = "netbsd")))'.dependencies]`
  and the `#[global_allocator]` binding in `bin/src/main.rs` carries the same
  `not(any(target_os = "freebsd", target_os = "netbsd"))` guard. Default builds
  on FreeBSD/NetBSD therefore link neither `jemalloc-sys` nor a Rust-side global
  allocator and let libc's `malloc(3)` (which has shipped jemalloc since FreeBSD
  7.0 in 2008 and NetBSD 5.0 in 2009) handle every allocation — eliminating the
  two-jemalloc-heaps redundancy and saving roughly 190 KiB of binary text plus
  the `~47 MiB` `libjemalloc.a` build artefact. The `--version` banner reports
  `-jemallocator` on those platforms to match the actual link graph. Linux,
  macOS, OpenBSD, DragonFly BSD, and Windows builds are unchanged — they
  continue to use the bundled jemalloc by default. `--features jemallocator,...`
  invocations from CI / Dockerfile / linux-rpm / archlinux continue to work
  everywhere; on FreeBSD/NetBSD the feature is silently a no-op.
- **`health_check.healthy_backends` gauge is now labelled with `cluster_id`
  ([#892](https://github.com/sozu-proxy/sozu/issues/892))**:
  `lib/src/health_check.rs` previously emitted the gauge unlabelled, so the
  value was overwritten across clusters every health-check tick — a
  multi-cluster operator dashboard could only ever observe the last-checked
  cluster. Each emission now carries `Some(cluster_id)` and is dispatched
  through the same `filter_labels_for_detail` path as `incr!`/`count!`, so
  default `Process` detail keeps the legacy aggregate while `Cluster`/`Backend`
  detail surfaces per-cluster values. No metric rename, no template change —
  dashboards configured for the unlabelled metric keep working at `Process`
  detail. Closes [#892](https://github.com/sozu-proxy/sozu/issues/892)
  (alongside the new cluster-availability surface listed under `### 🌟 Added`).

### 🌟 Added

- **Pre-built binaries for tagged releases
  ([#1089](https://github.com/sozu-proxy/sozu/issues/1089))**: a new
  `.github/workflows/release.yml` triggers on tag push (`X.Y.Z` and
  `X.Y.Z-rc.N`) and publishes 12 `sozu-${VERSION}-${TARGET}-${PROVIDER}.tar.gz`
  archives covering four Linux targets (`x86_64`/`aarch64` × `gnu`/`musl`)
  crossed with the four crypto providers (`crypto-ring` and `crypto-aws-lc-rs`
  on all 4 targets; `crypto-openssl` and `fips` on the two gnu targets only —
  `rustls-openssl` needs vendored OpenSSL on musl, and the `aws-lc-fips-sys`
  precompiled module assumes glibc). The archives are attached to a draft GitHub
  Release together with a `SHA256SUMS` file. Each tarball ships the stripped
  `sozu` binary, `LICENSE-AGPL3`, `LICENSE-LGPL3`, `README.md`, `CHANGELOG.md`,
  the production-shaped `os-build/config.toml`, both shipped systemd units, and
  a `SOURCE.txt` corresponding-source pointer (AGPL §6 / LGPL §4) that records
  the cell's target and crypto provider. The release workflow signs the
  aggregated `SHA256SUMS` keyless via sigstore (cosign + GitHub OIDC), recording
  the certificate in Rekor and shipping `SHA256SUMS.sig` + `SHA256SUMS.pem`
  alongside the archives, and emits SLSA build provenance via
  `actions/attest-build-provenance`. Builds set `SOURCE_DATE_EPOCH` and a
  deterministic `tar` invocation for best-effort reproducibility. Stable tags
  also push `clevercloud/sozu:${VERSION}` and `clevercloud/sozu:latest` (built
  with the default `crypto-ring` feature set); RC tags are marked as GitHub
  prereleases and skip both the `:latest` move and the version-pinned Docker
  image. The pre-existing `ci.yml` `dockerhub` job is gated off on tag pushes —
  version-pinned and `:latest` images now come exclusively from the release
  workflow; `:${SHA}` aliases continue to be produced from branch and PR pushes.
  Closes [#1089](https://github.com/sozu-proxy/sozu/issues/1089).
- **`command/LICENSE-LGPL3` shipped alongside `sozu-command-lib`**: the
  canonical LGPL-3.0 text now lives next to the LGPL-3.0-or-later crate that
  needs it. `cargo publish -p sozu-command-lib` carries it onto crates.io, the
  GitHub source archive carries it in `command/`, and the release workflow
  copies it into every binary tarball as `LICENSE-LGPL3` so AGPL §6 / LGPL §4
  obligations are satisfied without an external URL fetch.
- **Typed HSTS (HTTP Strict Transport Security, RFC 6797) policy with
  listener-default + per-frontend override**: a new `[hsts]` block under an
  HTTPS `[[listeners]]` entry (listener default) or
  `[clusters.<cluster>.frontends.hsts]` (per-frontend override) lets operators
  emit
  `Strict-Transport-Security: max-age=N[; includeSubDomains][; preload]` on
  every HTTPS response — backend-served, redirect 3xx, auth-deny 401, and
  proxy-generated 5xx alike. Knobs: `enabled` (required when the block is
  present so partial-update semantics distinguish preserve / explicit-disable
  / enable), `max_age` (defaults to `31_536_000` = 1 year, matches the HSTS
  preload list minimum), `include_subdomains`, `preload`. Validation refuses
  HSTS on plain-HTTP listeners (RFC 6797 §7.2 — at TOML config-load via
  `ConfigError::HstsOnPlainHttp` and at the worker IPC entry via
  `ProxyError::HstsOnPlainHttp`), allows `max_age = 0` silently as the §11.4
  kill switch, and warns on sub-day `max_age` and on `preload = true` without
  `include_subdomains` or with insufficient `max_age` (Chrome HSTS preload
  list rules). Per-frontend `enabled = false` explicitly suppresses an
  inherited listener default. Backend-supplied `Strict-Transport-Security`
  headers pass through unchanged via the new `HeaderEditMode::SetIfAbsent`
  semantics on `apply_response_header_edits` so RFC 6797 §6.1 single-header
  expectations hold. Implementation: new `HstsConfig` proto message attached
  to `HttpsListenerConfig` (tag 46), `UpdateHttpsListenerConfig` (tag 41),
  and `RequestHttpFrontend` (tag 16); `Frontend::new` materialises a single
  `HeaderEdit` with key `strict-transport-security`, value `render_hsts(cfg)`,
  and mode `SetIfAbsent` into `headers_response` at routing-table-build time;
  the `mux/router.rs` snapshot copy is hoisted above the redirect / auth-deny
  early returns and gated on `context.protocol == Protocol::HTTPS` so
  HTTPS-served default answers carry HSTS (RFC 6797 §8.1) while plaintext HTTP
  listeners can never leak the header (defense in depth on top of config-load
  validation).
- **`sozu listener https update` exposes the HSTS knobs**: the
  `UpdateHttpsListenerConfig.hsts` partial-update path is now reachable from
  the CLI without crafting a typed IPC message. `bin/src/cli.rs` adds
  `--hsts-max-age`, `--hsts-include-subdomains`, `--hsts-preload`,
  `--hsts-force-replace-backend`, and the kill-switch `--hsts-disabled` (with
  `conflicts_with_all` against the four enabling flags) on
  `HttpsListenerCmd::Update`; the request builder reuses the existing
  `build_hsts_from_cli` helper so mutual-exclusion validation and the
  `DEFAULT_HSTS_MAX_AGE = 31_536_000` substitution stay in lock-step with
  `frontend https add`. Supplying any flag flips the listener's policy and
  triggers the existing `Router::refresh_inheriting_hsts` reflow on every
  frontend that did not declare a per-frontend `[hsts]` block at add time;
  `--hsts-disabled` substitutes the `enabled = Some(false)` block. Coverage:
  five new clap-parse tests under `bin/src/cli.rs::tests` cover the
  enabling-flags happy path, the `--hsts-disabled`-alone path, the
  no-flag-at-all sanity case (existing surface unchanged), and the two
  conflict cases (`--hsts-disabled` with `--hsts-max-age` /
  `--hsts-force-replace-backend`).
- **Per-cluster availability surface
  ([#892](https://github.com/sozu-proxy/sozu/issues/892))**: closes the
  long-standing observability gap in which a cluster losing every backend
  produced no log line, no counter, and no recovery signal — the only proxy-wide
  flag swallowed every other cluster's transition the moment any single cluster
  tripped it. Worker now tracks per-cluster availability on `BackendList` and
  emits, exactly once per state flip:
  - `error!("cluster X: all N backends are down")` on the `Available -> AllDown`
    transition, paired with the existing `EventKind::NoAvailableBackends` event
    (now per-cluster) and a new `cluster.no_available_backends` counter.
  - `info!("cluster X: backends recovered (i/N available)")` on the
    `AllDown -> Available` transition, paired with a new
    `EventKind::ClusterRecovered = 29` worker event and a
    `cluster.available_recovered` counter.
  - Per-cluster gauges `cluster.available_backends` and `cluster.total_backends`
    published on every routing decision and every health-check tick (label-aware
    via `filter_labels_for_detail`, so default `Process` detail collapses to the
    proxy-wide aggregate).
  - Per-(cluster, backend) `backend.available` boolean gauge emitted only at
    transition sites (active health check up/down arms, passive connect-success
    and connect-failure handlers in `kawa_h1`, mux, and tcp paths), so
    per-request cost is unchanged. Recovery is detected on the next routing call
    against the cluster, on the next health-check tick, or when a single backend
    successfully serves traffic — whichever comes first. The previous
    process-wide `BackendMap.available` flag is removed.
- **Access-log timeout discriminator
  ([#892](https://github.com/sozu-proxy/sozu/issues/892))**: timeout-driven
  408/504 default-answer responses now surface a stable, structured token in the
  existing `RequestRecord.message` field so operator pipelines can distinguish a
  timeout from a backend-error response with the same status code. Vocabulary
  aligned with HAProxy `cR`/`cH`/`sH`/`sD` and Envoy `DT`/`UT`:
  - `client_timeout` — frontend timeout, request not fully received (status
    `408`).
  - `client_timeout_during_response` — frontend timeout while awaiting backend
    response (status `504`).
  - `backend_timeout` — backend silent before first response byte (status
    `504`).
  - `backend_response_timeout` — backend stalled mid-response-body (status `504`
    or H2 `RST_STREAM(INTERNAL_ERROR)`). Set inside `kawa_h1::Http::timeout` and
    `MuxState::timeout` on a new static-string `HttpContext.access_log_message`
    field; consumed by `log_default_answer_success` (H1) and
    `Stream::generate_access_log` (mux). Caller-supplied parser-error messages
    keep precedence. Non-timeout sessions emit `message: None` exactly as
    before.
- **HTTP 302 / 308 redirects via `RedirectPolicy`
  ([#1009](https://github.com/sozu-proxy/sozu/issues/1009))**: the
  `RedirectPolicy` proto enum gains `FOUND` (302, RFC 9110 §15.4.3 — temporary
  redirect; user agents may rewrite POST → GET on follow) and
  `PERMANENT_REDIRECT` (308, RFC 9110 §15.4.9 — permanent redirect that
  PRESERVES the request method). The router (`lib/src/protocol/mux/router.rs`)
  sets `HttpContext.redirect_status` per policy (`Permanent → 301`,
  `Found → 302`, `PermanentRedirect → 308`); the answer engine renders the
  matching `http.{301,302,308}.redirection` template with the same
  `(REDIRECT_LOCATION, ROUTE, REQUEST_ID)` variable schema. Default templates
  `default_302` / `default_308` ship in-tree (`Connection: close`,
  `Sozu-Id: %REQUEST_ID`, single `Location: %REDIRECT_LOCATION` header).
  Operator-supplied templates flow through the renamed
  `HttpAnswers::render_inline_redirect(code, …)` (the legacy `render_inline_301`
  is preserved as a thin wrapper). New per-status counters
  `http.302.redirection` and `http.308.redirection` (cluster + backend
  labelled). Closes [#1009](https://github.com/sozu-proxy/sozu/issues/1009).
  Regression tests: `try_redirect_found_h1_emits_302` and
  `try_redirect_permanent_redirect_h1_emits_308` in
  `e2e/src/tests/redirect_rewrite_auth_tests.rs`.
- **systemd `sd_notify` integration
  ([#228](https://github.com/sozu-proxy/sozu/issues/228))**: the master process
  now sends `READY=1` to systemd via `$NOTIFY_SOCKET` only AFTER its initial
  workers spawn and any saved state replays — fixing the long-standing
  `Type=simple` race where `After=sozu.service` peers could start hitting sozu
  before traffic was being accepted. Graceful shutdown sends `STOPPING=1`, and
  the hot-upgrade path (`bin/src/upgrade.rs::begin_new_main_process`) sends
  `MAINPID=<new-pid>` followed by `READY=1` so systemd tracks the post-exec
  master correctly. Implementation is std-only
  (`std::os::unix::net::UnixDatagram` against `$NOTIFY_SOCKET`); no extra
  dependency. The shipped `os-build/systemd/sozu.service` and
  `os-build/systemd/sozu@.service` units now declare `Type=notify` +
  `NotifyAccess=main`. The helper is a no-op when `$NOTIFY_SOCKET` is unset
  (e.g. operator running `sozu start` from a shell), so non-systemd deployments
  are unaffected. Optional `WatchdogSec` ping is documented in the helper but
  not yet wired into the master event loop.
- **`backends.fail_open` counter
  ([#1191](https://github.com/sozu-proxy/sozu/pull/1191))**: new counter
  incremented per routing decision that lands on the fail-open path (no backend
  passed the regular `can_open()` gate, the load balancer fell back to backends
  in `Normal` status with retry policy `OKAY`). Pairs with the latched `warn!`
  to give operators a per-request quantitative signal for universal-outage
  routing. Documented in `doc/metrics.md`.
- **Compile-time crypto provider selection
  ([#1191](https://github.com/sozu-proxy/sozu/pull/1191))**: Crypto provider
  selection: `crypto-ring` (default), `crypto-aws-lc-rs`, `crypto-openssl`, and
  `fips` (implies `crypto-aws-lc-rs` and activates `rustls/fips`). At least one
  must be active. When several are enabled together (e.g. `--all-features`), the
  precedence chain `fips > ring > aws-lc-rs > openssl` selects one — `fips`
  always wins, so `cargo build --features fips` runs aws-lc-rs in FIPS mode even
  with the default `crypto-ring` enabled. New `lib/src/crypto.rs` provides a
  LazyLock-cached `default_provider()` and the helpers
  `cipher_suite_by_name`/`kx_group_by_name`/`any_supported_type` that thread
  through the active provider. `cipher_suite_by_name` filters its result through
  `default_provider().cipher_suites` so a ChaCha-inclusive `cipher_list` cannot
  silently downgrade an FIPS build (`ServerConfig::fips()` would otherwise
  return `false`). `groups_list` is now per-listener via `ListenerBuilder`, with
  `X25519MLKEM768` as the first-preference post-quantum hybrid where supported.
  Documented under `doc/configure.md` and `doc/getting_started.md`.
- **Active backend health checks with HTTP/2 probes and fail-open
  ([#1191](https://github.com/sozu-proxy/sozu/pull/1191))**: new
  `lib/src/health_check.rs` runs probes inside the existing single-threaded mio
  event loop (no extra threads, no async runtime). Probes use a bounded token
  namespace `[1<<24, 1<<24 + 1<<16)` with a modulo-allocator that skips
  in-flight slots and refuses allocation rather than colliding with the mux
  GOAWAY sentinel `Token(usize::MAX)`. Threshold-based state machine, jittered
  intervals, 4 KB response buffer cap. URIs are validated at every boundary that
  mutates the active health-check config (CLI, master
  `ConfigState::add_cluster` + `set_health_check`, worker `AddCluster` +
  `SetHealthCheck` handlers) by
  `sozu_command::config::validate_health_check_config` (rejecting empty
  thresholds, missing leading `/`, and any CR/LF/NUL/C0 byte) so off-channel
  paths cannot smuggle invalid configs through `SaveState`/`LoadState`.
  Fail-open routing in
  `lib/src/backends.rs::BackendList::next_available_backend`: when ALL backends
  for a cluster fail the regular `can_open()` gate, route across `Normal`-status
  backends whose retry policy is OKAY rather than returning 503. The probe wire
  format follows the cluster's `http2` flag — `cluster.http2 = false` sends
  HTTP/1.1, `cluster.http2 = true` sends the HTTP/2 connection preface plus an
  HPACK-encoded HEADERS frame on stream 1 (encoded via `loona_hpack::Encoder`
  and decoded via `loona_hpack::Decoder`, so the probe parser inherits the
  workspace's existing HPACK fuzz coverage and handles HEADERS+CONTINUATION
  fragmentation). The probe and the data-plane backend connection share the same
  `cluster.http2` switch so they cannot diverge. HTTPS probes (h2 over TLS)
  remain a follow-up. Removing a health check via
  `sozu cluster health-check remove` now also resets each backend's
  `HealthState` so the load balancer can route again. New CLI verbs
  `sozu cluster health-check {set,remove,list}`. Configurable per-cluster via
  `[clusters.<id>.health_check]` (TOML) or `Cluster.health_check` (proto field
  15). New events `HEALTH_CHECK_HEALTHY = 27` / `HEALTH_CHECK_UNHEALTHY = 28`
  and metrics `health_check.{success,failure,up,down,healthy_backends}`. See
  `doc/health_checks.md`.
- **Per-(cluster, source-IP) connection limit
  ([#1193](https://github.com/sozu-proxy/sozu/pull/1193))**: a new global
  `max_connections_per_ip` (default `0` = disabled) caps the simultaneous
  frontend connections one source IP may hold against a given cluster. The
  source IP is taken from the parsed PROXY-protocol header when present (both
  `ExpectHeader` and `RelayHeader` modes are now address-aware — preserving the
  pre-existing source IP through the pipe phase), and falls back to `peer_addr`
  otherwise. The limit is enforced after cluster resolution and before backend
  connect, so a 401/421/redirect frontend is never gated. Enforcement points
  cover the unified H1+H2 mux (`lib/src/protocol/mux/router.rs`) and raw TCP
  (`lib/src/tcp.rs`):
  - HTTP/HTTPS clients (H1 and H2) hitting the limit receive
    `429 Too Many Requests` with an optional `Retry-After` header. `0` (or unset
    `retry_after`) elides the header — `Retry-After: 0` invites an immediate
    retry that defeats the limit. The new `Answer429` variant lands in the
    unified template engine alongside the existing `Answer4xx`/`Answer5xx`,
    complete with `%RETRY_AFTER` substitution and per-cluster custom-template
    support via `CustomHttpAnswers.answer_429` (proto tag 12).
  - TCP clients see a graceful FIN before any backend connect — no SO_LINGER
    trick, no RST.
  - H2 sessions multiplex many streams over one frontend connection; the
    SessionManager keeps a per-token set of `(cluster, ip)` pairs so multiple
    streams to the same cluster from the same source consume a single slot,
    matching the per-connection semantics operators expect.
  - Each cluster can override the global default via
    `Cluster.max_connections_per_ip` and `Cluster.retry_after` (proto tags
    13/14): `None` inherits, `Some(0)` is explicit unlimited, `Some(n > 0)`
    overrides.
  - New CLI verb `sozu connection-limit {set|remove|show}` patches the global
    limit at runtime via `SetMaxConnectionsPerIp` (proto tag 50) /
    `QueryMaxConnectionsPerIp` (proto tag 51) / `MaxConnectionsPerIpLimit`
    (response tag 14). Setter is non-sticky — operators must mirror the change
    in TOML to make it durable.
  - New metric `connections.rejected_per_cluster_ip` (counter, labelled by
    `cluster_id`) for SOC dashboards. Operators wanting per-IP attribution
    should pair this with the existing `client.connect.per_source.bucket_*`
    telemetry. Closes [#890](https://github.com/sozu-proxy/sozu/issues/890),
    [#1057](https://github.com/sozu-proxy/sozu/issues/1057).
- **Zero-copy TCP forwarding via Linux `splice(2)`
  ([#1194](https://github.com/sozu-proxy/sozu/pull/1194))**: when
  `Protocol::TCP` listeners are used and the new `splice` feature flag is
  enabled, the `Pipe` protocol moves bytes between frontend and backend through
  two `pipe2(O_NONBLOCK | O_CLOEXEC)` kernel pipes (one per direction) using
  `splice(2)` with `SPLICE_F_NONBLOCK | SPLICE_F_MOVE` instead of the userspace
  buffer pool. The `SplicePipe` RAII type owns all four fds and closes them in
  `Drop`. The `check_connections` half-close state machine and `backend_hup`
  accounting both observe the new `in_pipe_pending` / `out_pipe_pending` byte
  counts so half-closed sessions stay alive while data is still in kernel —
  mirroring HAProxy's `CF_SHUTR` / `CF_SHUTW` model. Linux-only (the cfg gate is
  `all(target_os = "linux", feature = "splice")`); other platforms compile to
  the buffered path. Forwarded through both `sozu-lib` and `sozu` (binary)
  `[features]`. The kernel-pipe capacity is operator-tunable via the new
  `splice_pipe_capacity_bytes` field on `ServerConfig` (TOML key of the same
  name): `None` keeps the kernel default of 64 KiB; values are applied via
  `fcntl(F_SETPIPE_SZ)` and clamped at `/proc/sys/fs/pipe-max-size` (default 1
  MiB unprivileged), with the realised capacity read back via
  `fcntl(F_GETPIPE_SZ)` and used as the per-call `splice_in` length. Coverage:
  `tcp_tests::test_tcp_proxy_concurrent_async`,
  `test_tcp_proxy_multiple_requests`, `test_tcp_soft_stop_with_active_sessions`,
  plus the in-tree `splice::tests::splice_roundtrip` unit test.
- **Per-status-code HTTP counters across H1 and H2
  ([#1196](https://github.com/sozu-proxy/sozu/pull/1196))**: in addition to the
  bucket counters `http.status.{1xx,…,5xx,other,none}`, Sōzu now emits
  `http.status.<code>` for an eighteen-code short-list — `200/201/204`,
  `301/302/304`, `400/401/403/404/408/413/429`, `500/502/503/504/507`. The list
  covers Sōzu's default-answer codes and the upstream codes operators routinely
  chart. Codes outside the list contribute only to their bucket so the metric
  keyspace stays bounded. The lookup lives in
  `lib/src/metrics/mod.rs::http_status_code_metric_name` and is shared by H1
  (`save_http_status_metric`) and H2 (`mux::stream::generate_access_log`).
  Closes [#937](https://github.com/sozu-proxy/sozu/issues/937),
  [#1146](https://github.com/sozu-proxy/sozu/issues/1146).
- **HTTP/2 mux access log: populate `client_rtt` and `server_rtt`
  ([#937](https://github.com/sozu-proxy/sozu/issues/937))**: the H2 / mux path
  now emits the same TCP `TCP_INFO`-derived round-trip-time fields that the H1,
  Pipe, and TCP-frontend paths already produce. Each access-log emission
  boundary (Mux session close in `mux/mod.rs`, H1
  `Upgrade`/`EarlyHint`/`Complete` in `mux/h1.rs`, H2
  `Complete`/`IdleTimeout`/`ResetFrame`/`Reset` in `mux/h2.rs`) snapshots both
  RTTs from the sockets it can reach and threads them as parameters into
  `Stream::generate_access_log`, matching the inline pattern already used by
  `kawa_h1`, `pipe`, and the TCP frontend. The capture is gated to
  `Position::Server` on the H2 paths so backend H2 connections cannot poison the
  shared `Stream.metrics` with an upstream RTT mis-labelled as `client_rtt`; the
  snapshot also runs before `endpoint.end_stream(...)` so the `server_rtt`
  lookup does not depend on `EndpointClient::end_stream` continuing to leave
  entries in `Router.backends`. The `Endpoint` trait gains a small
  `socket(token) -> Option<&TcpStream>` method so a frontend connection can read
  the peer-side socket through the existing endpoint adapter without holding a
  `Router` reference; the implementation is one line in each of the two existing
  impls (`EndpointServer` returns the frontend socket, `EndpointClient` looks up
  the backend by token). Schema unchanged (`ProtobufAccessLog` tags 9 and 10).
  TCP_INFO via `socket::stats::socket_rtt`; non-Unix stubs return `None` exactly
  as the H1 path already does.
- **Buffer pool gauges
  ([#1196](https://github.com/sozu-proxy/sozu/pull/1196))**: the run loop now
  samples `buffer.in_use` (renamed from `buffer.number`), `buffer.capacity`, and
  `buffer.usage_percent` once per iteration alongside `process.uptime_seconds` /
  `server.live`. `buffer.in_use` is still emitted on checkout/drop in
  `lib/src/pool.rs` for high-resolution dashboards.
- **Slab utilisation gauges
  ([#1196](https://github.com/sozu-proxy/sozu/pull/1196))**: `slab.capacity`,
  `slab.usage_percent` (against `slab.capacity()`), and
  `slab.accept_threshold_percent` (against the historical `at_capacity()` gate
  `10 + 2 * max_connections`). The two percent gauges are kept independent
  because `slab_entries_per_connection` can make configured slab capacity larger
  than the accept gate, and operators want to chart both frontiers.
- **Optional eviction of least-recently-active sessions when the accept queue is
  full ([#1192](https://github.com/sozu-proxy/sozu/pull/1192))**: new
  `evict_on_queue_full` global config knob (proto field on `ServerConfig`,
  default `false`). When the accept queue saturates and
  `SessionManager::check_limits` refuses, the worker can now evict the oldest 1%
  of non-listener sessions (`max_connections / 100`, floored at 1) instead of
  refusing new accepts. Selection is O(n) average via
  `select_nth_unstable_by_key` on `Session::last_event()`; eviction is skipped
  during graceful `shutting_down` and on small `max_connections < 100` configs
  the operator gets a config-load `warn!` because the 1% batch clamps to 1 (so
  the per-round share grows). A new `sessions.evicted` counter is emitted when
  the mitigation fires. Defaults to `false` because during a DDoS the existing
  connections are more likely to be legitimate clients than the queued ones;
  enable when overload is dominated by normal traffic spikes. Complementary to
  the `accept_queue.saturated_seconds`, `accept_queue.backpressure`, and
  `client.connect.per_source.bucket_*` telemetry that report the saturation
  condition. Closes [#644](https://github.com/sozu-proxy/sozu/issues/644),
  [#916](https://github.com/sozu-proxy/sozu/issues/916).
- **Listener-scoped `X-Real-IP` injection and anti-spoof elision**
  ([#1113](https://github.com/sozu-proxy/sozu/issues/1113)): two new
  `HttpListenerConfig` / `HttpsListenerConfig` flags, both defaulting to
  `false`:
  - `elide_x_real_ip`: strip any client-supplied `X-Real-IP` header from
    forwarded requests (anti-spoofing). Honoured on H1 initial request headers,
    H2 initial HEADERS frames, and H2 trailer HEADERS frames (the
    trailer-elision plumbing in `pkawa::handle_trailer` closes a code-path gap
    that would otherwise let a naive H2 client smuggle `x-real-ip` through a
    trailer).
  - `send_x_real_ip`: append a proxy-generated `X-Real-IP` header carrying
    `session_address.ip()` — i.e. the original client IP after PROXY-v2 unwrap.
    Both flags are independently combinable (anti-spoof only, send only, both,
    or neither) and are runtime-patchable via `UpdateHttpListenerConfig` /
    `UpdateHttpsListenerConfig`. Patches apply immediately to H1 sessions and to
    new H2 connections; already-open H2 connections continue using the values
    captured at their handshake (same connection-scoped capture as
    `strict_sni_binding`). New `with_elide_x_real_ip` / `with_send_x_real_ip`
    builder methods; new commented examples in `bin/config.toml`; documented
    under `doc/configure.md`.

### 🔄 Changed

- **Cipher list constant rename
  ([#1191](https://github.com/sozu-proxy/sozu/pull/1191))**: the internal
  `DEFAULT_RUSTLS_CIPHER_LIST` constant in `command/src/config.rs` is renamed to
  `DEFAULT_CIPHER_LIST`. The TOML key (`cipher_list`) is unchanged — operator
  configurations are not affected. Also drops the latent `DEFAULT_CIPHER_SUITES`
  (OpenSSL-style names that rustls never matched) and removes `P-521` from the
  default `groups_list` (rustls 0.23 does not support it).

### 🔄 Changed (potentially dashboard-breaking)

- **`EventKind::NoAvailableBackends` now fires per cluster
  ([#892](https://github.com/sozu-proxy/sozu/issues/892))**: was a process-wide
  latch (`BackendMap.available: bool`); the first cluster to trip it silently
  swallowed every other cluster's all-down transition. The event is now emitted
  on the per-cluster `Available -> AllDown` transition and is paired with the
  new `EventKind::ClusterRecovered = 29` for the inverse transition. Subscribers
  tracking the proxy-wide regime as a single signal need to aggregate across
  `cluster_id` themselves.
- **Renamed metrics ([#1196](https://github.com/sozu-proxy/sozu/pull/1196))**:
  - `client.connections_percentage` → `client.connections_percent`
  - `client.max_connections` → `client.connections_max`
  - `buffer.number` → `buffer.in_use`

  The new names are emitted from the run loop alongside the rest of the proxy
  gauges; per-event emission of `client.connections_*` from
  `SessionManager::incr/decr` is dropped (the high-resolution
  `client.connections` signal is preserved). Dashboards scraping the old names
  need to be updated.

- **`http.errors` is now labelled with `(cluster_id, backend_id)`
  ([#1196](https://github.com/sozu-proxy/sozu/pull/1196))**: emitted with labels
  in both `kawa_h1::log_request_error` and `mux::stream::generate_access_log`.
  The labels are filtered centrally by `metrics.detail`
  (`metrics/mod.rs::filter_labels_for_detail`): `process` / `frontend` collapse
  to a proxy-wide counter (no behaviour change for default deployments that
  already used `process`), `cluster` (the documented default) attributes errors
  per cluster, `backend` keeps the per-backend split. No double-counting: the
  prior unlabeled emission was replaced, not duplicated. Closes
  [#597](https://github.com/sozu-proxy/sozu/issues/597).

### 🔐 Security

- **Worker auto-restart race against on-disk binary replacement
  ([#515](https://github.com/sozu-proxy/sozu/issues/515))**: `bin/src/worker.rs`
  (worker spawn / auto-restart) previously called
  `Command::new(executable_path).exec()` where `executable_path` was a string
  captured at master startup via `read_link("/proc/self/exe")`. If a package
  upgrade replaced the on-disk binary between master startup and a worker
  auto-restart, `execve(2)` resolved the path string as a normal filesystem
  lookup and started the **new** binary, incompatible with the running master's
  protocol expectations — leading to crashes, partial state, or refusals to
  handshake. The new helper `crate::util::get_executable_exec_path()` opens
  `/proc/self/exe` with `O_PATH | O_CLOEXEC` and returns `/proc/self/fd/<n>` for
  `execve(2)` to resolve through the magic symlink to the **original** inode
  (`O_CLOEXEC` closes the fd on successful exec; on failed exec the fd persists
  harmlessly for the master's lifetime). Workers spawned via the helper
  therefore always match the running master's version. Linux-only via
  `cfg(target_os = "linux")`; FreeBSD / macOS keep the path-string approach (out
  of scope for #515). The worker exec site fails soft on `O_PATH` open failure
  (log + fall back to path-string exec). **The master hot-upgrade path at
  `bin/src/upgrade.rs` is unchanged**: `sozuctl upgrade-main` still re-execs the
  new on-disk binary by path, because the operator's intent is precisely to
  switch to a different version (the worker / master split is documented in the
  rustdoc on `bin/src/util.rs::get_executable_exec_path()`). New unit tests in
  `bin/src/util.rs::tests`
  (`get_executable_exec_path_returns_proc_self_fd_on_linux`,
  `get_executable_exec_path_distinct_calls_yield_distinct_fds`) cover the
  helper's shape; an e2e upgrade-race test in
  `e2e/src/tests/upgrade_race_tests.rs` covering
  binary-replacement-during-worker-restart is a follow-up.
- **Worker panic on short hostnames vs `Wildcard` rules
  ([#1223](https://github.com/sozu-proxy/sozu/issues/1223))**:
  `DomainRule::Wildcard::matches` in `lib/src/router/mod.rs` computed
  `hostname.len() - s.len() + 1` unconditionally, panicking with
  `attempt to subtract with overflow` in debug builds when the incoming hostname
  was shorter than the configured wildcard pattern (CWE-191 → CWE-754,
  remote-reachable DoS). In release builds the wrap is masked by the `&&`
  short-circuit on `ends_with(suffix)`, so the false-positive in §1 of the issue
  was rare; debug builds always panicked. The arm now uses
  `<[u8]>::strip_suffix` and validates the leftmost prefix on the returned slice
  — there is no length arithmetic to underflow, and short hostnames return
  `false` without panic. Boundary case `hostname == suffix` (empty leftmost
  label, e.g. `.foo.example.com` against `*.foo.example.com`) is now rejected as
  an invalid DNS label per RFC 1035 §3.1. Reachable from H1
  (`HttpListener::frontend_from_request`, `lib/src/http.rs:658`), H2
  (single-byte `:authority`, `lib/src/https.rs:865`), and operator-driven
  `Router::has_hostname` introspection. Affected versions: all releases
  including `sozu-lib` 1.1.1 and earlier (bug introduced in `a9dd46e9`,
  2019-06-04). Regression tests
  `match_domain_rule_wildcard_short_hostname_does_not_panic` and
  `router_lookup_wildcard_pre_rule_short_hostname_does_not_panic` in
  `lib/src/router/mod.rs`.

### 🌟 Added

- **Flexible HTTP answer templates per listener and per cluster**: the proto
  schema for `HttpListenerConfig`, `HttpsListenerConfig`, and `Cluster` gains a
  `map<string, string> answers` field keyed by HTTP status code. The
  listener-level map is the global default; the cluster-level map overrides it
  for the matching status code on requests routed to that cluster. The
  deprecated `optional CustomHttpAnswers http_answers` field is preserved on the
  wire so existing state files round-trip (the runtime merges both for one
  minor). Each map value is **either an inline literal body (the default) or
  `file://<path>` to load off disk** — the inline form skips disk I/O entirely,
  useful for short canned responses, secrets-free containers, and test rigs
  (`sozu cluster add --answer 503='HTTP/1.1 503 ...'` or
  `[listeners.https.answers] "503" = "..."` for inline,
  `--answer 503=file:///etc/sozu/503.http` for path). The new template engine
  (`lib/src/protocol/kawa_h1/answers.rs`) supports variable substitution via
  `%ROUTE`, `%REQUEST_ID`, `%CLUSTER_ID`, `%BACKEND_ID`, `%DURATION`,
  `%CAPACITY`, `%PHASE`, `%SUCCESSFULLY_PARSED`, `%PARTIALLY_PARSED`, `%INVALID`
  (repeatable), `%REDIRECT_LOCATION`, `%MESSAGE`, `%TEMPLATE_NAME` (single-use),
  `%WWW_AUTHENTICATE` (header-only, elides the line when the realm is empty).
  When the template carries a literal `Content-Length`, the engine recomputes
  the value from the actual rendered body size after `%`-substitutions — closing
  the RFC 9110 §8.6 / RFC 7230 §3.3.2 anti-smuggling drift. Templates that omit
  `Content-Length` keep their byte-for-byte shape; nothing is synthesised.
  Per-cluster overrides hot-swap via `AddCluster` / `RemoveCluster`. HAProxy
  parallel: `errorfile NNN /path`.

- **URL rewrite, redirect, and per-frontend custom headers**: new
  `RequestHttpFrontend` fields (`redirect`, `redirect_scheme`,
  `redirect_template`, `rewrite_host`, `rewrite_path`, `rewrite_port`,
  `headers`, `required_auth`) drive frontend-level routing decisions.
  `RedirectPolicy::PERMANENT` emits a 301 with a resolved `Location` header
  built from `redirect_scheme` (USE_SAME / USE_HTTP / USE_HTTPS) and the
  cluster's optional `https_redirect_port`. `RedirectPolicy::UNAUTHORIZED` (or a
  clusterless `FORWARD`) emits a 401. `Header { position, key, val }` carries
  request- or response-side header edits; an empty `val` deletes the header by
  name (HAProxy `del-header` parity).

- **HTTP Basic authentication on a per-frontend basis**: clusters gain
  `authorized_hashes: repeated string` (entries shaped
  `username:hex(sha256(password))`) and `www_authenticate: optional string`
  (realm). Frontends with `required_auth = true` invoke
  `mux::auth::check_basic`, which extracts the `Authorization: Basic` header,
  base64-decodes the token, hashes the password with SHA-256, and compares
  against every entry in `authorized_hashes` using `subtle::ConstantTimeEq` over
  the full list (no early exit). Failure produces a 401 with a
  `WWW-Authenticate: Basic realm="<realm>"` header rendered through the
  answer-template engine. The credential boundary is hardened against the timing
  side-channel that PR #1210's review fix (`171a31ce` upstream) was designed to
  prevent. New workspace dependencies `base64 = ^0.22.1` and `subtle = ^2.6.1`.

  The maximum decoded credential length is operator-tunable via the new
  main-config field `basic_auth_max_credential_bytes` (default 4096). Lower
  values (256 / 512) bound the per-failed-auth allocation tighter for hardened
  tenants. Setting `0` is a no-op so an explicit zero cannot disable the cap by
  accident. The config validator emits a `warn!` at boot when the configured cap
  is `>= buffer_size / 3` — informational only, but flags the back-pressure
  trade-off (a single failed auth pinning ~33% of the per-frontend buffer).

- **Pattern-trie segment regex anchoring with capture propagation**: every
  segment-level regex inserted into the routing trie is now wrapped with
  `\A...\z` at insert time so a pattern like `cdn[0-9]+` matches `cdn1` exactly
  but not `cdn1xxx`. A new `lookup_with_path` helper records the non-literal
  segments matched during traversal (`TrieSubMatch::Wildcard` /
  `TrieSubMatch::Regexp`) so the routing layer can re-run `Regex::captures` on
  the matched bytes and feed the captures into rewrite templates (`$HOST[n]` /
  `$PATH[n]` placeholders).

- **`Frontend` and `RouteResult` types in the routing layer**: the bare
  `Route::ClusterId` / `Route::Deny` decision is replaced by a richer `Frontend`
  (per-frontend policy, cached header-edit slices, capture capacity counts) and
  `RouteResult` (resolved policy plus substituted rewritten host/path). The
  `L7ListenerHandler::frontend_from_request` trait return type changes to
  `Result<RouteResult, _>`. Legacy `Route::ClusterId` and `Route::Deny` variants
  remain so existing match arms still compile; a follow-up will retire them.

### 🔄 Changed

- `HttpAnswers::new` now takes `&BTreeMap<String, String>` instead of
  `&Option<CustomHttpAnswers>`. `HttpAnswers::get` returns
  `(u16, bool, DefaultAnswerStream)` so the rendered template's `Connection`
  header can drive the frontend keep-alive bit (templates that ship
  `Connection: close` flip the flag; operators can opt back into keep-alive by
  shipping a custom template without that header). All in-tree call sites update
  in lockstep; legacy state files merge the populated `CustomHttpAnswers` fields
  into the new map at load.

- `mux/answers.rs::default_answer_for_code` becomes a pure builder that takes
  the redirect URL and `WWW-Authenticate` realm as parameters.
  `set_default_answer` reads them from `HttpContext.redirect_location` and
  `HttpContext.www_authenticate` (stashed by the routing layer) before falling
  back to the legacy `https://<authority><path>` shape.

- `CachedTags` derives `PartialEq + Eq + Clone` so `RouteResult` can derive
  `PartialEq` for assertions in router unit tests.

### 🌟 Added

- **`sozu listener {http,https,tcp} update` — in-place runtime patch verb**:
  Operators can now tune non-bind-only listener settings on a running proxy
  without cycling the listening socket. Patchable fields include H2 flood
  thresholds (CVE-2023-44487 / CVE-2024-27316 / CVE-2025-8671 mitigations), SNI
  binding enforcement (`strict_sni_binding`), protocol-downgrade protection
  (`disable_http11`), ALPN preference, per-stream idle timeout,
  graceful-shutdown deadline, stream-0 WINDOW_UPDATE cap, correlation-header
  name (`sozu_id_header`), custom HTTP answers, and all session timeouts. The
  patch is field-masked: omitted fields are preserved. Existing sessions keep
  their configuration snapshot; only new sessions, connections, or TLS
  handshakes pick up the new values. CVE mitigations can now be tightened under
  attack without any connection disruption.

- **HTTP/2 Multiplexing (Mux layer)**: Introduced a unified `Mux` protocol
  handler (`lib/src/protocol/mux/`) that replaces the separate H1 and H2 session
  handlers. The new layer supports full HTTP/2 multiplexing over a single
  connection, with shared stream state, flow control, and HPACK compression via
  the `loona-hpack` crate, see
  [`f7d1b659`](https://github.com/sozu-proxy/sozu/commit/f7d1b659865e7d92e732fcf8c52783713140f6a8),
  [`b6bffd36`](https://github.com/sozu-proxy/sozu/commit/b6bffd364355c8001da7403066c2dbcbc04445a1).

- **ALPN Protocol Negotiation**: Added per-listener ALPN configuration so Sōzu
  can advertise `h2` and `http/1.1` during the TLS handshake and route each
  connection to the correct Mux path, see
  [`4f67d07d`](https://github.com/sozu-proxy/sozu/commit/4f67d07d6179c2af61108a24d1e1d57eec663787).

- **`cluster h2 enable` / `cluster h2 disable` CLI commands**: New `sozuctl`
  sub-commands and a `--http2` flag allow operators to toggle HTTP/2 support on
  a cluster at runtime without restarting workers, see
  [`2a65aa78`](https://github.com/sozu-proxy/sozu/commit/2a65aa78313f359d22d38c1c828c1960bb411bd8).

- **End-to-end test suite for H2 and Mux**: Added `e2e/src/tests/h2_tests.rs`
  and `e2e/src/tests/mux_tests.rs` (2 500+ lines) covering multiplexed streams,
  request pipelining, WebSocket upgrades, and mixed H1/H2 scenarios, see
  [`f7d1b659`](https://github.com/sozu-proxy/sozu/commit/f7d1b659865e7d92e732fcf8c52783713140f6a8).

- **RFC 9218 Extensible Priorities**: Stream scheduling now respects RFC 9218
  priority headers (`u=N, i` format). Higher-priority streams (lower urgency
  value) are written before lower-priority streams. The `Prioriser` struct
  tracks urgency and incremental flags per stream, see
  [`4c02e918`](https://github.com/sozu-proxy/sozu/commit/4c02e918).

- **Configurable flood detection thresholds**: Six H2 flood detection limits
  (RST_STREAM, PING, SETTINGS, empty DATA, CONTINUATION frames, glitch count)
  are now configurable per-listener via protobuf config fields
  (`h2_max_rst_stream_per_window`, `h2_max_ping_per_window`, etc.), falling back
  to compile-time defaults when not set, see
  [`03942c11`](https://github.com/sozu-proxy/sozu/commit/03942c11).

- **OpenTelemetry trace propagation**: Behind the `opentelemetry` feature flag,
  Sozu now extracts `traceparent` and `tracestate` HTTP/2 headers during stream
  creation and propagates the `SpanContext` into access log entries for
  distributed tracing correlation, see
  [`7da4f580`](https://github.com/sozu-proxy/sozu/commit/7da4f580).

- **30 H2 security e2e tests**: `e2e/src/tests/h2_security_tests.rs` covering
  desync vectors, connection-specific headers, Content-Length fuzzing, cookie
  splitting, trailer forwarding, pseudo-header in trailers, half-closed streams,
  HPACK table size zero, and connection reuse.

- **Per-listener H2 connection tuning**: Three new optional config fields —
  `h2_initial_connection_window` (default 1MB), `h2_max_concurrent_streams`
  (default 100), `h2_stream_shrink_ratio` (default 2) — allow operators to tune
  connection-level flow control, stream concurrency, and memory reclamation per
  listener. Values are validated and clamped to safe bounds.

- **Fuzz test harness**: `e2e/src/tests/fuzz_tests.rs` integrates `cargo-fuzz`
  targets (`fuzz_frame_parser`, `fuzz_hpack_decoder`) as `#[ignore]`d e2e tests.
  Run with `cargo test -p sozu-e2e -- --ignored fuzz` (requires nightly +
  cargo-fuzz).

- **Customer H2 truncation regression guards** (cleverapps.io 2026-04 ticket):
  two e2e tests lock in the multi-fix chain that closed a report of ~7.4 MB
  gzipped chunked JSON responses truncating at ~3 MB across an H1-backend →
  H2-frontend path. `test_h2_large_gzipped_chunked_drains_fully` serves a
  deterministic XorShift-seeded 7.76 MB payload gzipped through
  `ChunkedFlushH1Backend` + `Content-Encoding: gzip` and asserts sha256
  byte-identity of the wire body within 8 s using a Chromium-146 request profile
  (H2 preface + SETTINGS with `INITIAL_WINDOW_SIZE=6_291_456`, one-shot
  `WINDOW_UPDATE(0, 15_663_105)`, per-stream `WINDOW_UPDATE(sid, 32 KiB)`
  cadence, `priority: u=3, i`). `test_h2_large_chunked_7mb_drains_fully` is the
  scale-only smoke (same total bytes, `b'Z'` fill, no gzip). New helpers in
  `e2e/src/tests/h2_utils.rs` — `h2_handshake_chromium_146`,
  `build_chrome146_get_headers`, `drain_h2_stream_streaming`,
  `advance_one_frame` — plus three additive `ChunkedFlushConfig` knobs (`body`,
  `extra_response_headers`, `content_type`). Stream-0 `WINDOW_UPDATE` is issued
  once at handshake only so the drain stays below
  `DEFAULT_MAX_WINDOW_UPDATE_STREAM0_PER_WINDOW=100`
  (`lib/src/protocol/mux/h2.rs:259`). Adds `flate2` as a workspace dependency
  and wires `sha2` into the `sozu-e2e` crate.

#### Telemetry & observability

- **H2 flood-detector exposure** (12 metrics under `h2.flood.violation.*`):
  every CVE mitigation in the H2 family — Rapid Reset (CVE-2023-44487),
  MadeYouReset (CVE-2025-8671), CONTINUATION flood (CVE-2024-27316), the PING /
  SETTINGS / empty-DATA flood family (CVE-2019-9512/9515/9518), header overflow,
  and the generic glitch budget — now emits a per-kind counter through the
  `handle_flood_violation` chokepoint. SIEM dashboards can window the trip rate
  without parsing logs.

- **H2 GOAWAY and RST_STREAM by error code** (4 counter families):
  `h2.goaway.{sent,received}.<code>` and `h2.rst_stream.{sent,received}.<code>`
  for every RFC 9113 §7 error variant, plus
  `h2.rst_stream.received.pre_response_start` (the canonical Rapid Reset
  signature, emitted alongside the per-code counter). A compile-time helper
  macro keeps the breakdown in lock-step with the H2Error enum.

- **H2 frame-type counters**: `h2.frames.rx.<kind>` for every received frame at
  the `handle_frame` dispatch chokepoint (11 keys), plus
  `h2.frames.tx.{settings,window_update,rst_stream,goaway,ping_ack}` at the
  control-frame serializer sites. HEADERS / DATA tx remain a follow-up (they
  travel through the H2 block converter).

- **HPACK header-rejection counters by reason** (17 metrics under
  `h2.headers.rejected.*`): every site in the H2→H1 HPACK decoder that rejects a
  header now emits `h2.headers.rejected.total` (aggregate) plus
  `h2.headers.rejected.<reason>` for one of 16 bounded reasons (CRLF/NUL/CTL
  injection, CL/TE conflict, duplicate or malformed pseudo-headers, RFC 9113
  §8.2.2 connection-specific headers, etc.). The first externally visible signal
  for request-smuggling probes against the H2 stack.

- **TLS handshake telemetry**: `tls.handshake.failed.<reason>` (12 RustlsError
  buckets, with `other` fallback for future variants), `tls.handshake_ms` (HDR
  histogram of handshake duration), and `tls.cert.min_expires_at_seconds` (gauge
  of the soonest-expiring loaded certificate, recomputed on cert add/remove
  only).

- **`x-request-id` propagation + access-log field**: incoming `x-request-id` is
  preserved verbatim end-to-end (`http.x_request_id.propagated`); when absent,
  Sōzu generates one from the request ULID and injects it
  (`http.x_request_id.generated`). The header value is also surfaced as a new
  `x_request_id` field on `RequestRecord` and `ProtobufAccessLog` (proto tag
  #24, wire-compatible append) — universal correlation key across Envoy /
  HAProxy / Sōzu hops.

- **`process.uptime_seconds` and `server.live` runtime gauges**: seconds since
  worker start (never reset on hot upgrade), and a `1`/`0` liveness signal that
  flips to `0` once a graceful shutdown begins. Standard SRE inputs for "is this
  worker stale?" and "drain this worker before terminating" L4 health-check
  decisions.

- **Control-plane audit log**: 22 `EventKind` variants reachable on the existing
  `SubscribeEvents` bus — CLUSTER_ADDED/REMOVED, FRONTEND_ADDED/REMOVED,
  CERTIFICATE_ADDED/REMOVED/REPLACED,
  LISTENER_ADDED/ACTIVATED/DEACTIVATED/UPDATED/REMOVED, STATE_LOADED/SAVED,
  CONFIGURATION_RELOADED, WORKER_KILLED/RELAUNCHED/UPGRADED,
  LOGGING_LEVEL_CHANGED, METRICS_CONFIGURED, SOZU_STOP_REQUESTED, MAIN_UPGRADED,
  EVENTS_SUBSCRIBED. Each privileged mutation emits a `config.<verb>` counter
  and a structured audit log line at `info!` level in the MUX-family layout —
  `[session_ulid request_ulid cluster_id|- backend_id|-]\tAUDIT\tCommand(verb=…, actor_uid=…, actor_gid=…, actor_pid=…, actor_user=…, actor_comm=…, client_id=…, socket=…, target=…, result=…[, error_code=…, reason=…, elapsed_ms=…, fanout=…, workers=<ok>/<err>/<expected>, request_sha256=…], sozu_version=…)`,
  filterable alongside existing `MUX` / `MUX-ROUTER` / `RUSTLS` / `PIPE` / `TCP`
  lines (full format in `doc/observability.md`). Worker-fanning verbs emit
  **two** audit lines — attempt-time (accepted by main state) and
  completion-time (applied across workers, carries
  `fanout=ok|partial|timeout|local_only` + per-worker counts + wall-clock
  `elapsed_ms` + `error_code`). `UpdateHttp/Https/TcpListener` lines now show
  `field=old→new` for every patched field (looked up from the pre-patch listener
  state). `ReplaceCertificate` now records both old and new cert fingerprints.
  Actor identity is captured via `SO_PEERCRED` on the unix command socket,
  enriched with `/proc/<pid>/comm` and NSS `getpwuid_r(uid)` at accept time, and
  tagged with the command socket path. Every free-form field (`target`,
  `actor_comm`, `actor_user`, `socket`, `reason`) is sanitized at render time
  (`sozu_command_lib::sessions::sanitize_for_audit`) so attacker-influenced
  input cannot forge additional audit lines via embedded `\t` / `\n` / ANSI
  escapes. Requests that go through the worker fan-out carry a truncated SHA-256
  fingerprint (`request_sha256=<16hex>`, first 64 bits) for dedupe / replay
  correlation.

- **Dedicated audit log sinks — `audit_logs_target` (text) +
  `audit_logs_json_target` (JSON)**: two new optional config paths next to
  `log_target` / `access_logs_target`. Each sink opens
  `O_APPEND | O_CREAT | chmod 0640` so the audit trail can be separated from the
  main log stream and protected via `audit`-group ACL (PCI-DSS 10.5). The text
  sink mirrors the human-readable `Command(...)` line with ANSI escapes
  stripped; the JSON sink writes one stable-schema JSON object per line for SIEM
  ingest (Wazuh, Elastic, Loki, Splunk HEC) without bespoke parser code. Both
  sinks are independent and best-effort — write failures log an error but never
  block the mutation.

- **Audit log enrichment fields** (closing Codex P3s): `ts=<RFC3339 UTC>`
  (in-body timestamp via std-only Hinnant date conversion — no `time`/`chrono`
  dep), `actor_role=root|system|user|unknown` (bucketing on the conventional
  0/<1000/≥1000 UID floor), `connect_ts=<RFC3339 UTC>` (per-session accept time
  stamped on `ClientSession`), `boot_generation=<u32>` (stamped on every line,
  persisted across `MAIN_UPGRADED` re-execs via `UpgradeData` so SOC tooling can
  disambiguate post-upgrade sessions through PID reuse), and
  `build_git_sha=<short>` (12-char short SHA embedded by `bin/build.rs` at
  compile time, falls back to `unknown` outside a git tree).
  `doc/observability.md` documents the JSON schema and the recommended retention
  policy (PCI-DSS 10.7 ≥ 1 year, 3 months hot, 9 months cold — operator-managed
  via logrotate, sōzu does not rotate audit files itself).

- **`rustls-webpki` bumped to 0.103.13** — closes RUSTSEC-2026-0104 (reachable
  panic in CRL parsing).

- **Backend H2 mux pool + flow-control telemetry**:
  `backend.pool.{hit,miss,size}` for connection reuse vs new dial decisions in
  `Router::connect` (the H2 mux makes least-loaded selection across the
  per-cluster connection map, which IS the pool). `backend.flow_control.paused`
  from the converter stall path. Symmetry guaranteed by colocating the gauge
  sites with the existing `backend.connections` accounting.

- **Aggregate H2 connection gauges (correctness fix)**: the three per-connection
  absolute gauges (`h2.active_streams`, `h2.connection_window`,
  `h2.pending_window_updates`) suffered from last-writer-wins semantics under
  multi-connection load and were misleading on dashboards. Replaced with
  `gauge_add!` lifecycle deltas under a clearer `h2.connection.*` namespace so
  values aggregate across all live H2 connections. `impl Drop for ConnectionH2`
  rebalances each connection's contribution to zero on teardown — symmetric
  independent of which close path runs (graceful_goaway, force_disconnect,
  panic-unwind, …), making the underflow class of bug structurally impossible.

- **Per-source accept telemetry**: `listener.accepted.total`,
  `client.connect.per_source.<bucket>` (256 bounded buckets via `LazyLock`
  static-string table — fixed ~10 KB heap regardless of attacker effort, OWASP
  A05 / NIST SP 800-92 cardinality-safe), `accept_queue.saturated_seconds` (1 Hz
  tick distinguishing brief spikes from sustained wedged-at-cap),
  `listener.connection_capped` (refusal counter). The accept-queue tuple grows
  to carry the peer SocketAddr; one extra `peer_addr()` syscall per accept on a
  path that already costs 1–2.

- **Five new TLS / forwarding fields on the access log**: `tls_version`,
  `tls_cipher`, `tls_sni`, `tls_alpn`, `xff_chain` plumbed across H1, H2 mux,
  TCP, and post-upgrade pipe sessions. Wire tags 25–29 on `ProtobufAccessLog`
  (wire-compatible append). Direct enabler for ECS-aligned ingestion and every
  modern TLS-aware SOC playbook.

- **`metrics.detail` cardinality knob (foundation)**: `MetricDetail` proto
  enum + `MetricDetailLevel` config enum
  (`process | frontend | cluster | backend`, mirroring HAProxy's
  `extra-counters` opt-in). Operators can write `detail = "frontend"` in TOML
  today; the value is plumbed through `ServerMetricsConfig` with
  backwards-compatible default. Wiring labels into the existing `incr!`/`gauge!`
  call sites lands as a dedicated follow-up MR.

- **Two emitted-but-undocumented metrics now in `doc/configure.md`**:
  `https.alpn.rejected.http11_disabled` (H2-only listener refusing http/1.1
  ALPN) and `http.sni_authority_mismatch` (CWE-346 cross-tenant defence).

- **OpenTelemetry doc rewrite**: the section now correctly identifies the
  feature as W3C `traceparent` passthrough (no SDK, no spans, no OTLP exporter).
  The previous `Limitations` text falsely claimed H2 requests were not
  propagated — verified false against the on-branch source (the H1 editor runs
  on H2 frames via `pkawa.rs`, then re-encoded by `H2BlockConverter`).

### 🔄 Changed

- **Backend retry-budget exhaustion no longer emits `ERROR`**: TCP / HTTP/1 /
  HTTP/2 paths all demote the per-session retry-budget exhaustion
  (`CONN_RETRIES = 3` consecutive backend-connect failures) from `error!` to
  `warn!`, aligning the three protocols with the branch severity taxonomy
  (peer-driven transport conditions stay below `ERROR`). A new
  `backend.connect.retries_exhausted{cluster_id, backend_id}` counter is emitted
  at all three gates — operators alerting on this class should rate the counter
  instead of grepping logs. The TCP caller-echo line
  (`Error connecting to backend: …`) is deduplicated to `trace!` when caused by
  `MaxConnectionRetries` (already logged + metered at the gate). Retry budget,
  session-close, and 503-answer behaviour are unchanged.

- **`HttpAnswers::replace_defaults` preserves runtime cluster overrides on
  listener update**: When `sozu listener update` patches the listener-default
  HTTP answer bodies, per-cluster `answer_503` templates that were registered at
  runtime (via `sozu cluster add`) are no longer silently discarded. The new
  `replace_defaults` method rewrites only the listener-default template strings
  in place, leaving the `cluster_custom_answers` map untouched.

- **Slab capacity multiplier doubled to 4× per connection**: The internal slab
  allocator now reserves 4× the per-connection slot count (previously 2×) to
  accommodate the higher stream concurrency introduced by HTTP/2 multiplexing.

- **`hpack` dependency replaced by `loona-hpack`**: The HPACK encoder/decoder
  used for HTTP/2 header compression has been migrated from the `hpack` crate to
  `loona-hpack`, see
  [`Cargo.toml`](https://github.com/sozu-proxy/sozu/blob/feat/h2-mux/lib/Cargo.toml).

- **Proportional overhead distribution**: Connection-level overhead bytes
  (SETTINGS, PING, WINDOW_UPDATE) are now distributed to streams proportional to
  their bytes transferred instead of equally, see
  [`4c02e918`](https://github.com/sozu-proxy/sozu/commit/4c02e918).

- **ConnectionH2 decomposed into sub-structs**: Fields grouped into
  `H2FlowControl`, `H2ByteAccounting`, `H2DrainState`, and `H2FloodConfig` for
  maintainability, see
  [`7917feeb`](https://github.com/sozu-proxy/sozu/commit/7917feeb).

- **readable()/writable() method extraction**: Large monolithic functions
  decomposed into `handle_header_state()`, `handle_continuation_header_state()`,
  and `write_streams()` private methods. `readable()` reduced from ~420 to ~213
  lines, `writable()` from ~310 to ~75 lines, see
  [`fc606d23`](https://github.com/sozu-proxy/sozu/commit/fc606d23).

- **Log-tag coverage on listener and proxy-protocol surfaces**:
  `lib/src/http.rs`, `lib/src/https.rs`, `lib/src/tls.rs`, and
  `lib/src/protocol/proxy_protocol/{expect,relay,send}.rs` now emit log lines
  through per-module `log_module_context!()` and (where a session is in scope)
  `log_context!($self)` macros. Operators can grep new tags `HTTP`, `HTTPS`,
  `TLS-RESOLVER`, `PROXY-EXPECT`, `PROXY-RELAY`, and `PROXY-SEND` alongside the
  existing `MUX-*`, `RUSTLS`, `PIPE`, `KAWA-H1`, and `TCP` tags for the same
  session. Three previously-untagged `trace!` sites in
  `lib/src/protocol/pipe.rs` (`Pipe::new`, `Pipe::readable`,
  `Pipe::backend_writable`) are now wrapped in the `PIPE` envelope. Format
  strings include the literal substring of the previous body verbatim, so
  existing grep predicates continue to match.

- **H2 close-drain log severity tiered by stream-count and close-state**: The
  `TLS buffer NOT fully drained on close` line at
  `lib/src/protocol/mux/h2.rs:5141` now downgrades from `error!` to `warn!` when
  stream count is zero AND the connection was already in a `GoAway` or `Error`
  state. This covers the operator-noisy "peer-initiated `GOAWAY(PROTOCOL_ERROR)`
  followed by TCP RST" path (no application data was queued to lose) without
  dropping the alarm on truly loss-bearing closes. `streams != 0` (live streams
  at close) and `streams == 0` from any other state stay at `error!`. Composes
  orthogonally with the send-side `H2Error`-variant tier added by `649af790`.
  See `lib/src/protocol/mux/LIFECYCLE.md` §11 for the tier table.

- **Config-load validation rejects sub-16 393 `buffer_size` when any HTTPS
  listener advertises H2 ALPN**: previously a TOML typo such as
  `buffer_size = 8192` started Sōzu cleanly and only manifested as an H2 mux
  deadlock under traffic on full-size frames (no panic, no obvious log).
  `ConfigBuilder::into_config` now scans `https_listeners` for `"h2"` in
  `alpn_protocols` and returns
  `ConfigError::BufferSizeTooSmallForH2 { buffer_size, minimum, listeners }` at
  boot when the global buffer is below `H2_MIN_BUFFER_SIZE = 16_393` (RFC 9113
  §6.5.2 + §4.1: 16 384-byte max frame payload + 9-byte frame header). Operators
  who deliberately want a smaller buffer must drop `"h2"` from the affected
  listeners' `alpn_protocols`. Documented in `doc/configure.md` "Buffer size for
  HTTP/2".

### ⛑️ Fixed

- **`command_allowed_uids` UID allowlist on the unix command socket**: the
  command socket previously authenticated callers via the `0o600` mode alone.
  The new optional `command_allowed_uids: Vec<u32>` config field rejects
  `SO_PEERCRED` UIDs not in the list at the top of
  `Server::handle_client_request`, so operators can restrict mutating verbs to a
  specific UID even when other same-UID daemons coexist.

- **Command channel hardening**: parser-level `MessageTooLarge` reject when the
  8-byte length prefix exceeds `max_buffer_size` (defense in depth against
  attacker-driven allocation pressure), `register_client` no longer inserts a
  session whose mio registration failed, and `read_message_blocking_timeout`
  raises its per-iteration socket timeout from 10 ms to 100 ms (drops idle
  blocking-read syscall rate ~10×). Audit attribution: `peer_user` caches
  `getpwuid_r` in a 16-entry process-local LRU (defangs an SSSD/LDAP wedge
  against the main event loop), and `peer_comm` opens `/proc/<pid>/stat` first
  so a recycled PID returns `None` rather than the new owner's comm.

- **TLS surface**: SNI absolute-form (`example.com.`) is normalised by stripping
  a single trailing dot in `https.rs::upgrade_handshake`; the "unsupported ALPN"
  arm now emits `https.alpn.rejected.unsupported`;
  `MutexCertificateResolver::resolve` replaces `try_lock` with blocking `lock`
  (silent default-cert fallback on contention is an attacker-detectable chain
  mismatch); `ListenerBuilder::to_tls` rejects the `disable_http11=true` +
  `alpn_protocols` containing `"http/1.1"` combination at config load (the
  listener would advertise http/1.1 then refuse every connection that negotiates
  it).

- **H2 protocol-level hardening**: refuse new peer-initiated HEADERS while in
  graceful drain (RFC 9113 §6.8); cap the trailer-only HPACK budget at
  `MAX_TRAILER_BYTES = 8 KiB` (HEADERS and trailers no longer each consume the
  full `SETTINGS_MAX_HEADER_LIST_SIZE` budget); reject duplicate identical
  `host:` headers (RFC 9110 §7.2 is strict on COUNT not on VALUE); reclaim
  per-connection scratch Vecs at quiet-time intervals
  (`cancel_timed_out_streams` runs `Vec::shrink_to(SCRATCH_BUF_RETAIN)` when
  capacity exceeds `64 KiB`).

- **PRIORITY_UPDATE field-value cap raised to 1024 bytes** (LOW-1 from prior
  batch — recap so the operator-facing `priority_field_value` cap matches the
  spec citation in `parser.rs:PRIORITY_UPDATE_MAX_VALUE` and aligns with nghttp2
  / `h2`-Rust). RFC 9218 §7.1 SF-Items are ASCII-only; 1024 leaves headroom
  while denying a per-frame 16 KiB allocation primitive.

- **Audit log JSON sink** runs every attacker-influenced free-form field
  (`verb`, `target`, `actor.user`, `actor.comm`, `socket`, `extras.reason`)
  through `sanitize_for_audit` before `serde_json::json!()` (INFO-1 follow-up —
  closes the SIEM-to-TSV column-smuggling primitive that JSON escaping alone
  does not catch).

- **Command channel client buffer is now bounded by `max_command_buffer_size`
  (default 2 MB) instead of `u64::MAX`**: previously
  `bin/src/command/server.rs::register_client` constructed every unix-socket
  client `Channel` with `(initial = 4096, max = u64::MAX)`. Combined with the
  doubling growth strategy in `Channel::readable()` (`command/src/channel.rs`)
  and the absence of `Vec::try_reserve` in `Buffer::grow`, any process that can
  connect to the command socket (mode `0o600`, so same-UID local processes)
  could OOM the main process by sending a single oversized length-prefixed
  message. The new ceiling matches the value worker channels at
  `fork_main_into_worker` already use; operators who legitimately need a larger
  ceiling raise `max_command_buffer_size` in the global config. CWE-770
  hardening.

- **`tls.cert.min_expires_at_seconds` no longer publishes 0 on an empty
  resolver**: at process boot before any `AddCertificate` lands the cert map is
  empty; the prior emit wrote 0 and any SOC alert "page when min-expires-at < N
  seconds" would fire on every restart with the same severity as a real
  expired-cert event. The gauge now skips the emit entirely when the cert set is
  empty so dashboards keep the last known good value (typically the previous
  worker's reading across a hot upgrade). Companion items — labelling per
  listener, refresh on remove_listener / soft_stop / hard_stop — are tracked as
  a follow-up resolver-lifecycle refactor.

- **`cancel_timed_out_streams` now routes through the `enqueue_rst`
  chokepoint**: the slow-multiplex idle-cancel guard at
  `lib/src/protocol/mux/h2.rs:3415-3475` previously pushed RST tuples directly
  into `self.pending_rst_streams` and inserted into `self.rst_sent` inline,
  bypassing the canonical `enqueue_rst` / `enqueue_rst_into` helpers. Three
  silent divergences: (a) `MAX_PENDING_RST_STREAMS = 200` queued cap and
  `total_rst_streams_queued` were not updated, so a peer that opens 200 idle
  streams could push past the cap silently — no GOAWAY(ENHANCE_YOUR_CALM)
  escalation; (b) double-cancel grew `pending_rst_streams` instead of
  short-circuiting on `rst_sent` membership; (c) invariant 15
  `Readiness::arm_writable` was hand-rolled. Routing through `enqueue_rst` fixes
  all three. Behaviour change: 200 cumulative idle-cancel RSTs from one peer now
  triggers GOAWAY(ENHANCE_YOUR_CALM); deliberate, since opening 200 streams that
  never make progress IS abusive (pinning MAX_CONCURRENT_STREAMS slots).

- **Audit log file mode is force-narrowed to `0o640` on pre-existing files; new
  parent directories use `0o750`**: `OpenOptions::mode(0o640)` is honoured by
  Linux only when the file is created by this open. The previous code at
  `bin/src/command/server.rs::open_audit_log_file` silently bypassed PCI-DSS
  10.5 when `audit_logs_target` pointed at a file that already had wider perms
  (touch + chmod 0644 from a sloppy install, logrotate without copytruncate,
  prior package upgrade). After `OpenOptions::open` succeeds,
  `set_permissions(0o640)` is now called so pre-existing files get narrowed at
  boot, with a `warn!` line recording the pre-existing mode before the chmod so
  SOC tooling sees the transition. `create_dir_all` is replaced with
  `DirBuilder::new().mode(0o750)` so newly created parents are owner+group only
  (pre-existing parents are not re-permissioned — operators may have a
  deliberate ACL).

- **PRIORITY_UPDATE `priority_field_value` capped at 1024 bytes
  (`PRIORITY_UPDATE_MAX_VALUE`)**: `priority_update_frame` at
  `lib/src/protocol/mux/parser.rs::priority_update_frame` previously stored the
  priority value verbatim via `value_bytes.to_vec()` with no upper bound beyond
  `SETTINGS_MAX_FRAME_SIZE` (16 384 bytes default). RFC 9218 §7.1 priority field
  values are SF-Items (RFC 8941) — ASCII-only, typically ~30 bytes. The new cap
  mirrors `nghttp2`'s `NGHTTP2_MAX_PRIORITY_VALUE_LEN` and the `h2` Rust crate's
  upper bound; overflow returns `H2Error::ProtocolError`. Two new unit tests
  cover the cap and the boundary.

- **H2 overlong DATA + Content-Length violation no longer starves
  connection-level flow control**: when a peer sent DATA frames whose cumulative
  payload exceeded the response's declared `Content-Length` (RFC 9113 §8.1.1),
  `handle_data_frame` reset and removed the offending stream **before**
  crediting the connection-level `WINDOW_UPDATE` for the wire payload bytes
  (`lib/src/protocol/mux/h2.rs:4064-4124`). Because RFC 9113 §6.9 + §5.2 measure
  flow control against the full wire payload (pad-length + padding included),
  the bad bytes consumed the peer's send window without it being credited back;
  repeated bad streams could permanently shrink the connection window and stall
  **unrelated** streams sharing the same H2 connection. The connection-level
  credit + arming of pending `WINDOW_UPDATE`s is now done **before** the
  Content-Length early-return; per-stream credit remains below the gate (the
  violating stream is being RST'd, so its window is moot per §6.9).

- **`Router::connect` rolls back gauge / slab / `active_requests` state if mio
  `register_socket` fails**: the new-backend dial path explicitly defers all
  stateful side effects until after `new_h2_client`/`start_stream` succeed
  (a650ad69), but `register_socket` failure (e.g. fd exhaustion at the kernel)
  was logged-and-continued at `lib/src/protocol/mux/router.rs:406-417`. The
  function then armed the connect timeout, inserted into `self.backends`, linked
  the stream and returned `Ok(())` with a backend that was visible to gauges
  (`backend.connections`, `backend.pool.size`, `connections_per_backend`), to
  the slab session list, and to `Backend.active_requests` (bumped in
  `Connection::start_stream` → `pre_start_stream_client_bookkeeping`) — but
  **not** to mio. Under EMFILE/ENFILE bursts this poisoned capacity dashboards
  until the connect timeout fires. The failure path now decrements all three
  gauges, calls `proxy.borrow().remove_session(token)` so the regular
  session-drop path releases `active_requests`, and returns
  `BackendConnectionError::MaxSessionsMemory`. CWE-400 hardening.

- **H2 close-drain log missing `MUX-H2` envelope**: The TLS-drain warning at
  `lib/src/protocol/mux/h2.rs:5141` (`Position::Server` arm of
  `ConnectionH2::close`) emitted as a raw `error!` without the canonical
  `MUX-H2 Session(...)` envelope, so operators could not grep-correlate this
  line against the preceding `WARN MUX-H2` GOAWAY-receipt line for the same
  session. The format string is now prefixed with `"{}"` and
  `log_context!(self)` is the first interpolation argument, matching the H1
  sibling at `mux/h1.rs:669-678` and the rest of `ConnectionH2`'s log surface.
  Regression coverage in `e2e/src/tests/h2_correctness_tests.rs`
  (`test_h2_peer_goaway_protocol_error_then_rst_clean_drain`,
  `test_h2_peer_goaway_no_error_clean_close`,
  `test_h2_peer_goaway_during_response_body`) plus a static
  `lib/tests/log_layout.rs` walker (echoed at compile time as `cargo:warning=`
  lines via `lib/build.rs`) keeps the rule honest going forward.

- We fixed 18 h2spec 2.0 conformance failures in which malformed-stream
  rejections never reached the wire. Two compounding gaps meant that every
  detected violation (RFC 9113 §5.3 self-dependent HEADERS, §6.9 WINDOW_UPDATE
  boundary, §8.1.2 uppercase/connection-specific/`TE` headers, §8.1.2.1-3
  pseudo-header allow-list, §8.1.2.6 content-length vs DATA reconciliation)
  fired its `RST_STREAM` byte into a kawa buffer that was already disconnected
  from the writable scheduler: (a) `forcefully_terminate_answer` raised the
  `Ready::WRITABLE` interest bit without the paired event bit, so edge-triggered
  epoll never scheduled `writable()`; (b) every `reset_stream` caller evicted
  the owning stream via `remove_dead_stream` before the converter path could
  serialise the RST bytes. Clients saw only the SETTINGS-handshake
  `WINDOW_UPDATE` before the ~6.8 s session timeout closed the TLS connection.
  Fixed by introducing a canonical `ConnectionH2::enqueue_rst` helper that
  routes every proxy-emitted RST through `self.pending_rst_streams` —
  independent of the owning stream's eviction — with `rst_sent` dedupe,
  `total_rst_streams_queued` MadeYouReset queued-cap bookkeeping, and a
  canonical `Readiness::arm_writable` pair. `finalize_write` now retains
  `Ready::WRITABLE` when the control queue is non-empty so a partial write that
  deferred the Phase-3 flush (gated on `expect_write.is_none()`) cannot strand
  the queued RST. `test_h2spec_conformance` un-ignored and running at
  145/145/0/0.

- We fixed HTTP/2 frontend stalling after an H1 backend response arrives on slow
  or per-chunk-flushed streams. The H1 read path was flipping the frontend
  `Ready::WRITABLE` interest bit at five sites in `lib/src/protocol/mux/h1.rs`
  without pairing it with `signal_pending_write()`; edge-triggered epoll then
  never re-fired and the queued body sat in sozu's buffers. Firefox and Chrome
  both reproduced on large PHP/Apache assets (~300 KiB chunked).

- We fixed the per-stream H2 idle timer firing mid-response on slow outbound
  deliveries. `stream_last_activity_at` refreshed only on inbound DATA /
  HEADERS, so a long-running response trickled at low bandwidth without inbound
  frames could be cancelled by `cancel_timed_out_streams` before completion.
  Outbound writes now refresh the timer alongside the existing inbound refresh
  sites in `lib/src/protocol/mux/h2.rs`.

- We fixed chunked-transfer backend EOF being silently reported as a clean
  `END_STREAM` with a truncated body. `terminate_close_delimited` in
  `lib/src/protocol/mux/h1.rs` now demotes mid-chunked EOF to
  `ParsingPhase::Error`, which the H2 converter surfaces as
  `RST_STREAM(InternalError)` to the client per RFC 9112 §7.1 — replacing silent
  truncation with a loud stream error.

- We fixed HTTP cleartext frontend socket shutdown using `Shutdown::Both`
  instead of `Shutdown::Write`. On Linux, `Shutdown::Both` discards unread
  receive-buffer data and can convert an otherwise clean close into a TCP RST.
  The HTTPS path already used `Shutdown::Write`.

- We fixed an `http.active_requests` gauge underflow where idle-timeout teardown
  and malformed-request rejection paths decremented the counter without a prior
  increment, causing the gauge to wrap to `u64::MAX`, see
  [`9b6f9987`](https://github.com/sozu-proxy/sozu/commit/9b6f99871f2a74f120cceb13a3c3ffeab5525515).

- We fixed a double-decrement of `http.active_requests` on `100 Continue` and
  `103 Early Hints` informational responses, which were incorrectly treated as
  terminal responses in the mux H2 path, see
  [`9b6f9987`](https://github.com/sozu-proxy/sozu/commit/9b6f99871f2a74f120cceb13a3c3ffeab5525515).

- We fixed `http.errors` over-counting on session teardown: the mux `close()`
  path was emitting an error metric for every open stream even when the session
  ended cleanly, see
  [`9b6f9987`](https://github.com/sozu-proxy/sozu/commit/9b6f99871f2a74f120cceb13a3c3ffeab5525515).

- We fixed the `local_drain` gauge wrapping to `u64::MAX` on negative values:
  the gauge is now clamped to zero rather than wrapping on unsigned underflow,
  see
  [`9b6f9987`](https://github.com/sozu-proxy/sozu/commit/9b6f99871f2a74f120cceb13a3c3ffeab5525515).

- We fixed the three H2 connection-level gauges (`h2.connection_window`,
  `h2.active_streams`, `h2.pending_window_updates`) clobbering across
  connections: each H2 connection emitted absolute `gauge!` snapshots, so under
  multi-connection load the dashboard saw the value from whichever connection
  wrote last instead of the aggregate. The metrics are renamed to
  `h2.connection.window_bytes`, `h2.connection.active_streams`, and
  `h2.connection.pending_window_updates`, and emitted as `gauge_add!` lifecycle
  deltas with a paired `Drop`-side decrement so the sum across all live H2
  connections is now correct (see `doc/configure.md` migration note).

- We fixed missing backend metrics (response time, request count, error rate) in
  the mux layer that were silently dropped compared to the legacy H1/H2
  handlers, see
  [`5a02f634`](https://github.com/sozu-proxy/sozu/commit/5a02f634e848600651a350a60e2f1fb39d0cc4dc).

- We fixed frontend timeout handling in the mux layer: timeouts are now re-armed
  after partial reads and H2 linked-stream wakeups are handled correctly to
  avoid spurious session drops, see
  [`560fac2f`](https://github.com/sozu-proxy/sozu/commit/560fac2f2cc249a66264a9233bb908059bb01ca0).

- **HPACK decoder table size sync**: Fixed HPACK decoder dynamic table
  synchronization on SETTINGS ACK per RFC 7541 §4.2, see
  [`21e7514d`](https://github.com/sozu-proxy/sozu/commit/21e7514d).

- **Stream recycling dedup**: Replaced inlined `complete_server_stream` with
  static method call, removing duplicated stream cleanup logic, see
  [`21e7514d`](https://github.com/sozu-proxy/sozu/commit/21e7514d).

- **1xx informational response handling (100 Continue, 103 Early Hints)**: Fixed
  the H2 write path to detect 1xx informational responses in both the resume and
  converter paths of `write_streams()`. Previously, the converter path recycled
  the stream after a 1xx response, killing it before the final 200 OK could
  arrive. Also strips END_STREAM/end_body flags on 1xx responses in the H1
  backend so the H2 frontend doesn't close the stream prematurely, see
  [`9f414492`](https://github.com/sozu-proxy/sozu/commit/9f414492).

- **Proxy Protocol v2 + H2 E2E test**: Fixed the test client's TCP read timeout
  (was 5s, causing rustls `complete_io` to block in `read_tls` and expire the
  server's SETTINGS ACK timer). Also added TCP_NODELAY, see
  [`9f414492`](https://github.com/sozu-proxy/sozu/commit/9f414492).

- **H1 pipelining error handling**: Added parse error handling and header
  validation (method/authority/path) to the pipelining parse in `writable()`,
  preventing silent hangs on malformed pipelined requests, see
  [`9f414492`](https://github.com/sozu-proxy/sozu/commit/9f414492).

- **Empty H2 header name rejection**: Moved empty header name check into
  `is_invalid_h2_header()` for defense-in-depth (RFC 9110 §5.1), see
  [`9f414492`](https://github.com/sozu-proxy/sozu/commit/9f414492).

- **Edge-triggered epoll TLS flush safety**: Added `ensure_tls_flushed()` helper
  and `Readiness::signal_pending_write()`/`signal_pending_read()` methods to
  properly handle edge-triggered epoll with TLS buffering, see
  [`9f414492`](https://github.com/sozu-proxy/sozu/commit/9f414492).

- **Proxy Protocol → TLS readiness transfer**: Fixed the PP → TLS upgrade to
  transfer the full `Readiness` struct (interest + event) instead of only the
  event, preventing lost interest bits, see
  [`9f414492`](https://github.com/sozu-proxy/sozu/commit/9f414492).

### 📚 Documentation

- **README / RELEASE / CONTRIBUTING refreshed for the post-`sozuctl`,
  post-Travis, GitHub Actions / `main`-branch reality.** Dead Travis + Gitter
  badges removed; copyright bumped to 2015-2026; workspace source map
  regenerated; remaining `sozuctl` / `master/` / `sozu-command-futures`
  references retired. See
  [`19828c2f`](https://github.com/sozu-proxy/sozu/commit/19828c2f1b586aa4847dab0fa644a8ff947cd28e),
  [`338c7d68`](https://github.com/sozu-proxy/sozu/commit/338c7d688a1a948438565530f00280e4814854f8).

- **`doc/` folder brought back in sync with `feat/h2-mux` HEAD.** Dropped the
  fictional `crypto-ring` / `crypto-aws-lc-rs` / `crypto-openssl` matrix from
  `doc/getting_started.md` (rustls is wired to `ring` via the workspace
  `Cargo.toml`); refreshed `doc/architecture.md` with current mux line counts
  (18 666 across 13 modules) and replaced SHA-pinned permalinks with `main`
  branch links; rewrote `doc/observability.md` audit examples to match the
  shipped `Command(...)` envelope; bumped `doc/configure.md` `buffer_size`
  example to `16393` so it matches the H2 minimum gate, and added a row for the
  `https.alpn.rejected.unsupported` counter. The `doc/README.md` index now links
  every previously orphaned page plus the new narratives below. See
  [`9f529fec`](https://github.com/sozu-proxy/sozu/commit/9f529fec),
  [`f35f580e`](https://github.com/sozu-proxy/sozu/commit/f35f580e),
  [`d72dc7fa`](https://github.com/sozu-proxy/sozu/commit/d72dc7fa),
  [`47cf05d3`](https://github.com/sozu-proxy/sozu/commit/47cf05d3),
  [`81a64409`](https://github.com/sozu-proxy/sozu/commit/81a64409).

- **Full rewrite of
  [`doc/lifetime_of_a_session.md`](doc/lifetime_of_a_session.md)** as the
  unified session-lifecycle entry point. Re-anchored on the current `mio` event
  loop, edge-triggered readiness discipline, and the `HttpProxy` / `HttpsProxy`
  / `TcpProxy` split; cross-links the new per-protocol
  [`mux/LIFECYCLE.md`](lib/src/protocol/mux/LIFECYCLE.md),
  [`kawa_h1/LIFECYCLE.md`](lib/src/protocol/kawa_h1/LIFECYCLE.md),
  [`proxy_protocol/LIFECYCLE.md`](lib/src/protocol/proxy_protocol/LIFECYCLE.md),
  and [`bin/src/command/LIFECYCLE.md`](bin/src/command/LIFECYCLE.md) siblings;
  replaces every reference to the removed `https_openssl.rs` /
  `protocol/http/mod.rs` modules; adds a metrics sidebar mapping
  `tls.handshake_ms`, `https.alpn.rejected.*`, `client.connections`,
  `backend.pool.size`, `h2.flood.violation.*`, and `h2.{goaway,rst_stream}.*` to
  the lifecycle stages where they fire. See
  [`0022b552`](https://github.com/sozu-proxy/sozu/commit/0022b552).

- **Module-level `//!` doc headers** added across the previously bare critical
  surfaces: `lib/src/protocol/mod.rs`,
  `lib/src/protocol/mux/{converter,h1,h2,parser,pkawa,serializer}.rs`,
  `lib/src/protocol/kawa_h1/{mod,answers,editor,parser,diagnostics}.rs`,
  `lib/src/protocol/proxy_protocol/{mod,expect,header,parser,relay,send}.rs`,
  `lib/src/protocol/{pipe,rustls}.rs`, `lib/src/{https,socket,metrics/mod}.rs`,
  `command/src/{channel,scm_socket,buffer/{fixed,growable,mod}}.rs`,
  `bin/src/upgrade.rs`, the `bin/src/command/{mod,server,sessions,requests}.rs`
  supervisor surface, and the two `fuzz/fuzz_targets/*.rs` entries. See
  [`c119838a`](https://github.com/sozu-proxy/sozu/commit/c119838a).

- **`// SAFETY:` annotations on the 63 previously-undocumented `unsafe { … }`
  blocks** across the worker, supervisor, and command-channel paths
  (`from_raw_fd` ownership transfers, `from_utf8_unchecked` ASCII proofs,
  `DuplicateOwnership` raw-pointer impls with explicit double-free obligations,
  overlap-safe `ptr::copy` calls in the buffer + pool families, and FFI /
  syscall sites including `getpid`, `fork`, `setrlimit`, `sched_setaffinity`,
  `getsockopt`, `sendmmsg`, and `env::set_var`). Annotation only — no block
  bodies changed, no new or removed `unsafe`. Dynamic parity check:
  `safety_n = unsafe_n + 1` (the +1 is the non-unsafe TLS-truncation invariant
  block at `lib/src/protocol/kawa_h1/mod.rs:1211`). See
  [`f8dbc476`](https://github.com/sozu-proxy/sozu/commit/f8dbc476).

- **In-source comments encoding load-bearing invariants** previously only
  present in `CLAUDE.md`: the 16-iteration backend-drain loop at
  `lib/src/protocol/mux/mod.rs:1402-1407` (constant rationale + invariant-15
  cross-link to `mux/LIFECYCLE.md`); the `socket_write` /
  `socket_write_vectored` structural symmetry contract at `lib/src/socket.rs`;
  the write-only TLS-frontend shutdown invariant at the three
  previously-undocumented call sites (`lib/src/protocol/mux/mod.rs:984`,
  `lib/src/protocol/mux/mod.rs:1604`, `lib/src/http.rs:448`); and the
  `serial_test::serial(force_h2_client_failure)` discipline for tests that
  toggle the process-wide `e2e-hooks` `AtomicBool`s (`e2e/src/tests/mod.rs`
  top-of-file `//!`).

- **New narrative documents.**
  [`lib/src/protocol/kawa_h1/LIFECYCLE.md`](lib/src/protocol/kawa_h1/LIFECYCLE.md)
  (~300 lines covering the H1 read / parse / route / connect / forward / close
  cycle with `file.rs:LINE` citations);
  [`lib/src/protocol/proxy_protocol/LIFECYCLE.md`](lib/src/protocol/proxy_protocol/LIFECYCLE.md)
  (~180 lines covering the PROXY v1/v2 expect → relay → send transitions,
  partial-header handling, oversized-header rejection, and TCP health-check
  carve-outs); [`bin/src/command/LIFECYCLE.md`](bin/src/command/LIFECYCLE.md)
  (~250 lines covering the supervisor + audit-log envelope +
  `command_allowed_uids` + PID-reuse-guarded `peer_comm` + boot-generation
  across hot upgrades); [`fuzz/README.md`](fuzz/README.md) covering the two
  `cargo-fuzz` targets, corpus management policy, regression repro workflow, and
  an OSS-Fuzz / ClusterFuzzLite integration roadmap (the `.clusterfuzzlite/`
  directory does not exist on this branch yet);
  [`doc/configure_admin_ops.md`](doc/configure_admin_ops.md) (~250 lines on the
  admin verbs, listener-update worked examples, the query-then-resubmit dance
  behind `cluster h2 enable|disable`, and operator-facing mechanism behind the
  recent `feat/h2-mux` knobs without duplicating the `doc/configure.md`
  reference tables). See
  [`31c522e2`](https://github.com/sozu-proxy/sozu/commit/31c522e2).

- **Comment-only `sozuctl` → `sozu` rename** across
  `bin/src/command/{server,sessions,requests}.rs` and the request-builder
  fixture: `/proc/comm` field strings flip to `"sozu"`; prose mentions become
  `sozu CLI` or `sozu command-socket client`. No source identifiers or public
  symbols renamed. See
  [`cb782a02`](https://github.com/sozu-proxy/sozu/commit/cb782a02).

### 🤖 CI

- **Replaced archived `actions-rs/toolchain@v1` and `actions-rs/cargo@v1`**
  (deprecated since 2023) with `dtolnay/rust-toolchain@<channel>` and direct
  `cargo` invocations across `.github/workflows/ci.yml`. The matrix `test` and
  `doc` jobs no longer depend on unmaintained third-party actions. See
  [`0b73c8a1`](https://github.com/sozu-proxy/sozu/commit/0b73c8a1).

- **`fmt` and `clippy` gates now enforced in CI.**
  `cargo +nightly fmt --all -- --check` and
  `cargo clippy --all-targets --all-features --locked -- -D warnings` were
  previously local-only conventions documented in `CLAUDE.md`; both now run as
  fail-fast jobs on every PR. See
  [`0b73c8a1`](https://github.com/sozu-proxy/sozu/commit/0b73c8a1).

- **MSRV (1.88.0) build gate added.** A dedicated job builds the workspace with
  `--all-features --locked` against the advertised minimum supported Rust
  version (`Cargo.toml rust-version = "1.88.0"`), aligned with the `1.88.0`
  everyday-development pin in `rust-toolchain`. The MSRV was bumped from
  `1.85.0` to match the toolchain pin; see
  [`0b73c8a1`](https://github.com/sozu-proxy/sozu/commit/0b73c8a1) for the
  gate's introduction.

- **Pre-flight clippy hygiene.** The `clippy::if_same_then_else` warning
  previously documented in `CLAUDE.md` for
  `e2e/src/tests/h2_correctness_tests.rs` is no longer reachable on this branch
  (the duplicated arm fell out of an earlier refactor), so the new `-D warnings`
  gate ships zero pre-flight warnings under
  `--all-targets --all-features --locked` and no Rust source change was required
  to enable it. See
  [`0b73c8a1`](https://github.com/sozu-proxy/sozu/commit/0b73c8a1).

### Changelog

The categorised list below covers every commit in the `main..HEAD` range for
`feat/h2-mux` as of this revision (`df500868..9cdf2c22`, 429 entries on
2026-04-27). Bullets in the narrative sections above curate the user-visible
behaviour; the bullets below give a one-line per-commit index for code review
and bisection.

#### 🌟 Features

- [
  [`09029d57`](https://github.com/sozu-proxy/sozu/commit/09029d5743673085edea1226be445975e4f02956)
  ] feat(command): optional command_allowed_uids enforcement on every verb
  [`Florentin Dubois`] (`2026-04-25`)
- [
  [`f2a10b85`](https://github.com/sozu-proxy/sozu/commit/f2a10b8502391bcb3dcce7433c48a38f9940ca62)
  ] feat(config): reject disable_http11=true + http/1.1 ALPN combination
  [`Florentin Dubois`] (`2026-04-25`)
- [
  [`9161dcb4`](https://github.com/sozu-proxy/sozu/commit/9161dcb4664776d77b82f7edc97f1dba7112e4cb)
  ] feat(config): expose slab_entries_per_connection knob (default 4, [2,32])
  [`Florentin Dubois`] (`2026-04-25`)
- [
  [`20095779`](https://github.com/sozu-proxy/sozu/commit/20095779eae0d2c9827dd6ce36c732a75af6184e)
  ] feat(config): reject sub-H2 buffer_size at config load when h2 ALPN listener
  present [`Florentin Dubois`] (`2026-04-25`)
- [
  [`1ed86a57`](https://github.com/sozu-proxy/sozu/commit/1ed86a5718051ffce190ecdbdcab306e5400114b)
  ] feat(e2e): add HEADERS-then-delay-then-DATA mode to raw_h2_response_backend
  [`Florentin Dubois`] (`2026-04-24`)
- [
  [`92ef16c3`](https://github.com/sozu-proxy/sozu/commit/92ef16c31d6db1888a22ac79f7c087b95ed7d084)
  ] feat(h2): parse and honour RFC 9218 §7.1 PRIORITY_UPDATE frames [`Florentin
  Dubois`] (`2026-04-24`)
- [
  [`01293da7`](https://github.com/sozu-proxy/sozu/commit/01293da7885c3734f2b1e6b928139376657eb6d4)
  ] feat(command): sozu listener {http,https,tcp} update runtime-patch verb
  [`Florentin Dubois`] (`2026-04-22`)
- [
  [`ac5a4199`](https://github.com/sozu-proxy/sozu/commit/ac5a4199814314f972d76eec74f0083b4acea916)
  ] feat(h2): emit HPACK dynamic-table-size-update on SETTINGS change
  [`Florentin Dubois`] (`2026-04-22`)
- [
  [`f936ec1e`](https://github.com/sozu-proxy/sozu/commit/f936ec1e4b8ece23a01813b5967c1afc8ee1d9b7)
  ] feat(listener): configurable Sozu-Id correlation header name [`Florentin
  Dubois`] (`2026-04-22`)
- [
  [`e801363b`](https://github.com/sozu-proxy/sozu/commit/e801363b7d3f67965fa655e82c1cf6eca1315f74)
  ] feat(h2): warn + counter on silent H2→H1 trailer drop [`Florentin Dubois`]
  (`2026-04-22`)
- [
  [`9aa29095`](https://github.com/sozu-proxy/sozu/commit/9aa2909507ef21845e94eaafebe48570f87cf5fa)
  ] feat(h2): add flood counter for connection-level WINDOW_UPDATE (stream 0)
  [`Florentin Dubois`] (`2026-04-22`)
- [
  [`0a98e039`](https://github.com/sozu-proxy/sozu/commit/0a98e039ee530ae0ba4c5d73e784a74ce071e283)
  ] feat(h2): RFC 9218 incremental flag round-robin scheduling [`Florentin
  Dubois`] (`2026-04-22`)
- [
  [`0906369b`](https://github.com/sozu-proxy/sozu/commit/0906369bcf1bfcc7afe363fdec91df5cd4fd5d04)
  ] feat(h2): make soft_stop graceful-shutdown deadline configurable [`Florentin
  Dubois`] (`2026-04-22`)
- [
  [`c357a73a`](https://github.com/sozu-proxy/sozu/commit/c357a73aca6a4cf3323c35aa9cf16df2d32947c0)
  ] feat(metrics): wire metrics.detail end-to-end (label-cardinality knob)
  [`Florentin Dubois`] (`2026-04-22`)
- [
  [`dbeb842d`](https://github.com/sozu-proxy/sozu/commit/dbeb842d796ce80f7fa038da101cfb474d42ce2f)
  ] feat(h2): per-type tx counters for HEADERS, CONTINUATION, DATA in the
  converter [`Florentin Dubois`] (`2026-04-22`)
- [
  [`e4d7114c`](https://github.com/sozu-proxy/sozu/commit/e4d7114cc0b73c4f681b822f2f276f3dab254577)
  ] feat(metrics): metrics.detail cardinality knob (foundation) [`Florentin
  Dubois`] (`2026-04-21`)
- [
  [`2db90ae9`](https://github.com/sozu-proxy/sozu/commit/2db90ae99acd9da7dd8382db40f18db4fca14d76)
  ] feat(server): per-source accept telemetry + saturation duration counters
  [`Florentin Dubois`] (`2026-04-21`)
- [
  [`2e7d9434`](https://github.com/sozu-proxy/sozu/commit/2e7d9434eb53e389b4a58406e18944a15f1802a7)
  ] feat(logging): surface TLS metadata and XFF chain on access logs [`Florentin
  Dubois`] (`2026-04-21`)
- [
  [`04cd6874`](https://github.com/sozu-proxy/sozu/commit/04cd68749f204764b36511977bde1964caad1a07)
  ] feat(command): wire audit EventKind emits + SO_PEERCRED actor attribution
  [`Florentin Dubois`] (`2026-04-21`)
- [
  [`394d8a07`](https://github.com/sozu-proxy/sozu/commit/394d8a07f2bf040570d1d4799851ea9ef514e96f)
  ] feat(backend): telemetry for H2 mux pool reuse and backend flow control
  [`Florentin Dubois`] (`2026-04-21`)
- [
  [`8a2a8282`](https://github.com/sozu-proxy/sozu/commit/8a2a8282a78f832925cf8114cdc0bb8bb5fb9634)
  ] feat(command): extend EventKind with control-plane mutation variants
  [`Florentin Dubois`] (`2026-04-21`)
- [
  [`14914857`](https://github.com/sozu-proxy/sozu/commit/1491485708e3a96f250c214f3547f4543b2045b7)
  ] feat(server): process.uptime_seconds + server.live runtime gauges
  [`Florentin Dubois`] (`2026-04-21`)
- [
  [`ed1966e1`](https://github.com/sozu-proxy/sozu/commit/ed1966e147d0e6dc759e96f557f1d3763957c248)
  ] feat(http): propagate x-request-id and log it on access log [`Florentin
  Dubois`] (`2026-04-21`)
- [
  [`ba65d2d9`](https://github.com/sozu-proxy/sozu/commit/ba65d2d900a2de75b9fd460164e88989dd9bf1d0)
  ] feat(tls): handshake failure counters by reason, duration histogram,
  cert-expiry gauge [`Florentin Dubois`] (`2026-04-21`)
- [
  [`a1a42012`](https://github.com/sozu-proxy/sozu/commit/a1a420120f2c74cb0fa1c1415c1ecd865d2994a1)
  ] feat(h2): expose HPACK header-rejection counters by reason [`Florentin
  Dubois`] (`2026-04-21`)
- [
  [`9227987b`](https://github.com/sozu-proxy/sozu/commit/9227987b67d6dbfafa68749b0320b7cab762c688)
  ] feat(h2): per-frame-type rx/tx counters at mux dispatch + serializer sites
  [`Florentin Dubois`] (`2026-04-21`)
- [
  [`0704a2e0`](https://github.com/sozu-proxy/sozu/commit/0704a2e06b32e609c02739dc1c78e4b60a629e02)
  ] feat(h2): metricise GOAWAY and RST_STREAM by RFC 9113 §7 error code
  [`Florentin Dubois`] (`2026-04-21`)
- [
  [`4aa8930f`](https://github.com/sozu-proxy/sozu/commit/4aa8930fd6857b6a044ec4bc1efae362c4fe33cb)
  ] feat(h2): expose H2 flood-detector trips as per-kind statsd counters
  [`Florentin Dubois`] (`2026-04-21`)
- [
  [`76359c1e`](https://github.com/sozu-proxy/sozu/commit/76359c1e6f1d5b25a3c71b9a5bf43e75a3fc70d0)
  ] feat(socket): cache configured peer on SessionTcpStream for
  ENOTCONN-resilient logging [`Florentin Dubois`] (`2026-04-21`)
- [
  [`369bf2a6`](https://github.com/sozu-proxy/sozu/commit/369bf2a64b206e78ecc2cbc61bca07af31451a94)
  ] feat(mux-router): render [session req cluster backend] + Session prefix on
  router logs [`Florentin Dubois`] (`2026-04-21`)
- [
  [`266559f5`](https://github.com/sozu-proxy/sozu/commit/266559f51f3419eb53d299685255c51eace6c228)
  ] feat(socket): render Session(peer, local, rtt, state) on module-level SOCKET
  logs [`Florentin Dubois`] (`2026-04-21`)
- [
  [`884c0d56`](https://github.com/sozu-proxy/sozu/commit/884c0d56bd1f5ef210a7459da72f3eed5e94a5bb)
  ] feat(logging): enrich protocol log-context prefixes with debug fields
  [`Florentin Dubois`] (`2026-04-20`)
- [
  [`f8a153c0`](https://github.com/sozu-proxy/sozu/commit/f8a153c0182ebe4ea4ac9b3ae4b93339d451adb3)
  ] feat(socket): session-context prefix on lib/src/socket.rs logs [`Florentin
  Dubois`] (`2026-04-20`)
- [
  [`46d37ba3`](https://github.com/sozu-proxy/sozu/commit/46d37ba3df92c90e42208e8fa1bbfdf8d7904359)
  ] feat(mux): propagate per-session ULID through the protocol stack +
  log-context schema [`Florentin Dubois`] (`2026-04-17`)
- [
  [`46db3604`](https://github.com/sozu-proxy/sozu/commit/46db3604172d0f3751740e2f1c57a5729f19b50f)
  ] feat(h2): expose h2_max_rst_stream_emitted_lifetime as a listener config
  knob [`Florentin Dubois`] (`2026-04-17`)
- [
  [`9cbaec75`](https://github.com/sozu-proxy/sozu/commit/9cbaec75ef2d961bbf379650af1d211c1fde909b)
  ] feat(h2): MadeYouReset (CVE-2025-8671) mitigation — lifetime cap on
  server-emitted RST_STREAM [`Florentin Dubois`] (`2026-04-17`)
- [
  [`74b89fdd`](https://github.com/sozu-proxy/sozu/commit/74b89fdda1d3d377d3cd02f4424cbe84a4fd5ee5)
  ] feat(logging): session-context prefix and ANSI color scheme across all
  protocol macros [`Florentin Dubois`] (`2026-04-17`)
- [
  [`55890245`](https://github.com/sozu-proxy/sozu/commit/5589024557b1879d1342a932923aeefa1d022a37)
  ] feat(config): add h2_max_header_table_size listener knob [`Florentin
  Dubois`] (`2026-04-16`)
- [
  [`e3608c7d`](https://github.com/sozu-proxy/sozu/commit/e3608c7dce92512d232f175338abeefbcb3f132d)
  ] feat(bin): expose simd feature for kawa SIMD parser [`Florentin Dubois`]
  (`2026-04-16`)
- [
  [`80da9aad`](https://github.com/sozu-proxy/sozu/commit/80da9aad62337975e205df76c7282859c6ae8f19)
  ] feat(https): reject ALPN None on H2-only listener [`Florentin Dubois`]
  (`2026-04-15`)
- [
  [`f68d4ec9`](https://github.com/sozu-proxy/sozu/commit/f68d4ec985ea20e211c0b536fc67ca5d29679fb8)
  ] feat(mux): introduce 421 Misdirected Request template for SNI-authority
  mismatch [`Florentin Dubois`] (`2026-04-15`)
- [
  [`ac66dfc0`](https://github.com/sozu-proxy/sozu/commit/ac66dfc09f5ca9ac6d5e12b188ba67c1b2bca8d9)
  ] feat(config): add strict_sni_binding listener knob for SNI-authority
  enforcement [`Florentin Dubois`] (`2026-04-15`)
- [
  [`6cdc262e`](https://github.com/sozu-proxy/sozu/commit/6cdc262e158e1d21b20ea57ca0b051ed43e22a6c)
  ] feat(config): expose H2 flood thresholds as per-listener config knobs
  [`Florentin Dubois`] (`2026-04-15`)
- [
  [`5c37f542`](https://github.com/sozu-proxy/sozu/commit/5c37f5428fc7820f6a1c255f1b920647c6005ee3)
  ] feat(mux): bind TLS SNI to HTTP :authority per stream [`Florentin Dubois`]
  (`2026-04-15`)
- [
  [`9de0494f`](https://github.com/sozu-proxy/sozu/commit/9de0494fd916bf04fe851ba91cf0a7dc5c81ad88)
  ] feat(h2): add per-listener connection_window, max_concurrent_streams,
  stream_shrink_ratio [`Florentin Dubois`] (`2026-03-24`)
- [
  [`ddbb2e36`](https://github.com/sozu-proxy/sozu/commit/ddbb2e368cb1e41ea481068eee4e9653366be36e)
  ] feat(h2): make flood detection thresholds configurable via listener config
  [`Florentin Dubois`] (`2026-03-14`)
- [
  [`a97bce33`](https://github.com/sozu-proxy/sozu/commit/a97bce335f7276f22589180f89f1ab5973be9754)
  ] feat(h2): implement RFC 9218 priorities and proportional overhead
  distribution [`Florentin Dubois`] (`2026-03-14`)
- [
  [`db7c33ec`](https://github.com/sozu-proxy/sozu/commit/db7c33ecc94faf5014bbd36ed991d45f72894b1f)
  ] feat(h2): propagate OTel trace context from H2 headers to access logs
  [`Florentin Dubois`] (`2026-03-14`)
- [
  [`94f3bbf3`](https://github.com/sozu-proxy/sozu/commit/94f3bbf3e23da09909f8e4967b681b9fa5f6c590)
  ] feat(h2): add flow control observability for timeout diagnosis [`Florentin
  Dubois`] (`2026-03-13`)
- [
  [`b272b541`](https://github.com/sozu-proxy/sozu/commit/b272b54107d543b1766e52ecf7f26e8d043640b6)
  ] feat(h2): wire cross-readiness wake-up, HashMap capacity,
  complete_server_stream helper [`Florentin Dubois`] (`2026-03-12`)
- [
  [`aa146391`](https://github.com/sozu-proxy/sozu/commit/aa146391bd1ae9a932291ba2769265844dca7730)
  ] feat(h2): add cross-readiness wake-up, connection window enlargement, and
  RST dedup [`Florentin Dubois`] (`2026-03-12`)
- [
  [`3374c03e`](https://github.com/sozu-proxy/sozu/commit/3374c03ed78258fd98ca07a31c0bd14f10462a9f)
  ] feat(h2): add SETTINGS ACK timeout enforcement (RFC 9113 §6.5) [`Florentin
  Dubois`] (`2026-03-12`)
- [
  [`4c4871fc`](https://github.com/sozu-proxy/sozu/commit/4c4871fc00ef007b57394e3f6def9a1faa221976)
  ] feat(h2): add flood detection for Rapid Reset, CONTINUATION, Ping, and
  Settings floods [`Florentin Dubois`] (`2026-03-12`)
- [
  [`e2569e1b`](https://github.com/sozu-proxy/sozu/commit/e2569e1bf6ef981d20cb7ad1b12eed2099a8fad2)
  ] feat(cli): add cluster H2 enable/disable commands and --http2 flag
  [`Florentin Dubois`] (`2026-03-10`)
- [
  [`464900d5`](https://github.com/sozu-proxy/sozu/commit/464900d50c7858d84f9a8556c0e039878a14365f)
  ] feat(config): add per-listener ALPN protocol configuration [`Florentin
  Dubois`] (`2026-03-10`)
- [
  [`3738b94b`](https://github.com/sozu-proxy/sozu/commit/3738b94bd63a93b766573b356d2cc9385cc8d7f5)
  ] feat(metrics): add missing metrics to mux H1/H2 paths and document all
  metrics [`Florentin Dubois`] (`2026-03-10`)
- [
  [`47d85d38`](https://github.com/sozu-proxy/sozu/commit/47d85d38e98364a4a5c7fa65bb1435e39a461f25)
  ] feat(h2): add H2 backend support and fix bidirectional end_of_stream
  tracking [`Florentin Dubois`] (`2026-03-09`)
- [
  [`9b4e4846`](https://github.com/sozu-proxy/sozu/commit/9b4e4846389dce4b8fa8e9e74cf9456b718bd749)
  ] feat(h2): integrate mux into HTTP/HTTPS state machines [`Florentin Dubois`]
  (`2026-03-09`)
- [
  [`dbdec91f`](https://github.com/sozu-proxy/sozu/commit/dbdec91f34409c69dbb86edcda037db9dad704fa)
  ] feat(h2): extract mux module from mux_v1 branch [`Florentin Dubois`]
  (`2026-03-09`)

#### ⛑️ Fixed

- [
  [`9cdf2c22`](https://github.com/sozu-proxy/sozu/commit/9cdf2c22960a931275e3dc61c6e5a5b97be2f27c)
  ] fix(mux): gate debug event import [`Florentin Dubois`] (`2026-04-27`)
- [
  [`ba144c02`](https://github.com/sozu-proxy/sozu/commit/ba144c0220ec4072fba8b78d61aeb973cb1db757)
  ] fix(ci): pin clippy to 1.88.0 to match local lint baseline [`Florentin
  Dubois`] (`2026-04-26`)
- [
  [`0ee347b7`](https://github.com/sozu-proxy/sozu/commit/0ee347b776bd92dacaeaa6dd0ab9fe80495a3a71)
  ] fix(ci): resolve fmt missing-mod + clippy toolchain-pin failures [`Florentin
  Dubois`] (`2026-04-26`)
- [
  [`1a2c1c2f`](https://github.com/sozu-proxy/sozu/commit/1a2c1c2f09c4c8ec3ce7214e95c88a4c402bbb02)
  ] fix(h2): refuse new peer streams while draining (RFC 9113 §6.8) [`Florentin
  Dubois`] (`2026-04-25`)
- [
  [`798d7ff4`](https://github.com/sozu-proxy/sozu/commit/798d7ff411ec852680fd13c74c71590a5c463cc6)
  ] fix(audit): apply sanitize_for_audit to JSON sink free-form fields
  [`Florentin Dubois`] (`2026-04-25`)
- [
  [`5c3d9f6f`](https://github.com/sozu-proxy/sozu/commit/5c3d9f6f09afd419b6e080e217547379167e2ca8)
  ] fix(audit): PID-reuse guard on peer_comm + cache peer_user lookups
  [`Florentin Dubois`] (`2026-04-25`)
- [
  [`85641a7e`](https://github.com/sozu-proxy/sozu/commit/85641a7ef3d8eafe7d9e16b85776873a93c5c480)
  ] fix(mux): shrink per-connection scratch Vecs at quiet-time intervals
  [`Florentin Dubois`] (`2026-04-25`)
- [
  [`5d13a816`](https://github.com/sozu-proxy/sozu/commit/5d13a8164f73247e5c69e3c1ca99c61cad85e74d)
  ] fix(mux): tighter trailer budget + duplicate Host strict reject [`Florentin
  Dubois`] (`2026-04-25`)
- [
  [`bdf2b865`](https://github.com/sozu-proxy/sozu/commit/bdf2b865e134c21936706a7034c4a8336b600108)
  ] fix(channel): bound message_len at the parser + drop blocking poll cadence
  [`Florentin Dubois`] (`2026-04-25`)
- [
  [`0b1c83c1`](https://github.com/sozu-proxy/sozu/commit/0b1c83c1b05946f8b7f5aba6e23f8fc3e6e64b6a)
  ] fix(command): drop unix-socket client when register() fails [`Florentin
  Dubois`] (`2026-04-25`)
- [
  [`c43ed5c0`](https://github.com/sozu-proxy/sozu/commit/c43ed5c02757083e3a8988ea5411c13382207333)
  ] fix(tls): normalise SNI trailing dot + meter unsupported ALPN refusals
  [`Florentin Dubois`] (`2026-04-25`)
- [
  [`5a7f3a26`](https://github.com/sozu-proxy/sozu/commit/5a7f3a26f4ca2f71c6f6d37cf1c792a33f0c2893)
  ] fix(tls): skip tls.cert.min_expires_at_seconds emit on empty resolver
  [`Florentin Dubois`] (`2026-04-25`)
- [
  [`24ea703a`](https://github.com/sozu-proxy/sozu/commit/24ea703a212cd1465d0709ddfaddc36b1785bc6d)
  ] fix(command): bound client channel max_buffer_size to
  max_command_buffer_size [`Florentin Dubois`] (`2026-04-25`)
- [
  [`652f523a`](https://github.com/sozu-proxy/sozu/commit/652f523a8c6d4d3c68299f585a11859465822d57)
  ] fix(h2): cap PRIORITY_UPDATE priority_field_value at 1024 bytes [`Florentin
  Dubois`] (`2026-04-25`)
- [
  [`bd198a68`](https://github.com/sozu-proxy/sozu/commit/bd198a68f2d388c92e0e1c46df98a725bcd8cc14)
  ] fix(audit): force-narrow audit log file mode + use 0o750 on new parents
  [`Florentin Dubois`] (`2026-04-25`)
- [
  [`0abec1f1`](https://github.com/sozu-proxy/sozu/commit/0abec1f1b8e75ef0b74b72d9a6cc10e6c3920fa6)
  ] fix(h2): route cancel_timed_out_streams through enqueue_rst chokepoint
  [`Florentin Dubois`] (`2026-04-25`)
- [
  [`624a7570`](https://github.com/sozu-proxy/sozu/commit/624a757014921b7a3fdbd1ac9ef6d808077d78c9)
  ] fix(mux): roll back Router::connect state if mio register_socket fails
  [`Florentin Dubois`] (`2026-04-25`)
- [
  [`54a9e451`](https://github.com/sozu-proxy/sozu/commit/54a9e45168377d32a2e0f8ad7cd086202be661be)
  ] fix(h2): credit connection flow control before Content-Length early-return
  [`Florentin Dubois`] (`2026-04-25`)
- [
  [`ad7fb86b`](https://github.com/sozu-proxy/sozu/commit/ad7fb86b3adbcebf0e5d8d5c7f42594921bdbfa7)
  ] fix(mux): wrap H2 close-drain log in MUX-H2 envelope and tier severity by
  stream-count and close-state [`Florentin Dubois`] (`2026-04-25`)
- [
  [`337d0a59`](https://github.com/sozu-proxy/sozu/commit/337d0a59b492eb9e5eba5ead4979a9fd72efed09)
  ] fix(h2): emit RST_STREAM eagerly from reset_stream [`Florentin Dubois`]
  (`2026-04-24`)
- [
  [`ad396615`](https://github.com/sozu-proxy/sozu/commit/ad396615867e563f38ec903551c99675f65a6b0b)
  ] fix(h2): pair signal_pending_write with peer WRITABLE in DATA/HEADERS
  handlers [`Florentin Dubois`] (`2026-04-24`)
- [
  [`1c7762c9`](https://github.com/sozu-proxy/sozu/commit/1c7762c9d55ac41ac1c3cd41be2de4eef07e1d2a)
  ] fix(h2): rearm WRITABLE after PRIORITY_UPDATE mutates scheduler state
  [`Florentin Dubois`] (`2026-04-24`)
- [
  [`9756ed8a`](https://github.com/sozu-proxy/sozu/commit/9756ed8a90ad7f149c43cb9a87e4897e0108c85d)
  ] fix(mux): pair signal_pending_write with WRITABLE insert in
  set_default_answer [`Florentin Dubois`] (`2026-04-24`)
- [
  [`68895fd5`](https://github.com/sozu-proxy/sozu/commit/68895fd571fe18f28b8d14a08932245a7f0c5c36)
  ] fix(h2): suppress incremental yield between last DATA and closing Flags
  [`Florentin Dubois`] (`2026-04-24`)
- [
  [`787c5e74`](https://github.com/sozu-proxy/sozu/commit/787c5e74927858438db68237cd8051a654524447)
  ] fix(h2): scope incremental_peer_count to same urgency bucket + ready peers
  [`Florentin Dubois`] (`2026-04-24`)
- [
  [`ed5cb5aa`](https://github.com/sozu-proxy/sozu/commit/ed5cb5aa1ed83c862badb621df3c062313c3ea0b)
  ] fix(h2): enforce LIFECYCLE invariant 16 in finalize_write +
  has_pending_write [`Florentin Dubois`] (`2026-04-24`)
- [
  [`d46e7c4b`](https://github.com/sozu-proxy/sozu/commit/d46e7c4bcbbe6eafb8ada522634c8859bb280beb)
  ] fix(mux): close H1→H2 wake-gap, outbound idle refresh, chunked-EOF
  classification [`Florentin Dubois`] (`2026-04-23`)
- [
  [`393abac1`](https://github.com/sozu-proxy/sozu/commit/393abac1552eea8e14547897d129b80ad330473e)
  ] fix(proxy): harmonise backend retry-budget-exhaustion log severity
  [`Florentin Dubois`] (`2026-04-23`)
- [
  [`6c3ee90c`](https://github.com/sozu-proxy/sozu/commit/6c3ee90c2db055999ec320012c84311375151958)
  ] fix(h2): skip RFC 9218 incremental yield for solo-bucket streams [`Florentin
  Dubois`] (`2026-04-23`)
- [
  [`882f2e1d`](https://github.com/sozu-proxy/sozu/commit/882f2e1dfacd673bf9c5cf6028bfd26ae6bf691f)
  ] fix(mux): inherit `back_timeout` for H2 stream idle default [`Florentin
  Dubois`] (`2026-04-23`)
- [
  [`ea3cb9de`](https://github.com/sozu-proxy/sozu/commit/ea3cb9de233617b1409fff11f4d1ca24fc0afb86)
  ] fix(mux): demote SETTINGS_TIMEOUT detection to warn! and stop re-firing
  [`Florentin Dubois`] (`2026-04-23`)
- [
  [`79c91f84`](https://github.com/sozu-proxy/sozu/commit/79c91f84dd78c8b697c1683bd5d95dbf6f3d406a)
  ] fix(logging): gate `debug!` dead-code arm on `logs-debug` [`Florentin
  Dubois`] (`2026-04-23`)
- [
  [`1af613de`](https://github.com/sozu-proxy/sozu/commit/1af613dec7386e8e34f3ce5cd075f04de1aa4a29)
  ] fix(listener): reset `active` on give_back_listener + repair 4 e2e tests
  [`Florentin Dubois`] (`2026-04-22`)
- [
  [`da7010fb`](https://github.com/sozu-proxy/sozu/commit/da7010fbd7d418644cef3807ecc9d60d23cbf23b)
  ] fix(command): post-review follow-ups on listener update verb [`Florentin
  Dubois`] (`2026-04-22`)
- [
  [`5c754a58`](https://github.com/sozu-proxy/sozu/commit/5c754a58b0be3a6a5e608882f4d4ce375f08baa5)
  ] fix(h2): stream-id exhaustion emits GOAWAY with tightened bound [`Florentin
  Dubois`] (`2026-04-22`)
- [
  [`c53e869f`](https://github.com/sozu-proxy/sozu/commit/c53e869f1ebe727c723ce91caa930c62127cd58a)
  ] fix(h2): strict Content-Length digit-only parse [`Florentin Dubois`]
  (`2026-04-22`)
- [
  [`6e3ee1de`](https://github.com/sozu-proxy/sozu/commit/6e3ee1dee317a19b0867f34ab4f58b30dae714fd)
  ] fix(telemetry): post-review hardening + audit_emit_inline arity bug + pkawa
  metric-key simplification [`Florentin Dubois`] (`2026-04-21`)
- [
  [`471f2f15`](https://github.com/sozu-proxy/sozu/commit/471f2f1536513937ede3187a5474853ab3099918)
  ] fix(h2): aggregate H2 connection gauges via gauge_add! + Drop teardown
  [`Florentin Dubois`] (`2026-04-21`)
- [
  [`3566ff27`](https://github.com/sozu-proxy/sozu/commit/3566ff27897fdc8335c8707af6d9f4c966b0718e)
  ] fix(h2): invalidate expect_write/read on stream eviction [`Florentin
  Dubois`] (`2026-04-20`)
- [
  [`1ec8fd03`](https://github.com/sozu-proxy/sozu/commit/1ec8fd03b774bcf30c29465a51e809625d915df3)
  ] fix(logging): make is_logger_colored() safe to call during active log
  emission [`Florentin Dubois`] (`2026-04-17`)
- [
  [`722a5d59`](https://github.com/sozu-proxy/sozu/commit/722a5d596ce2db61f1e8e4f1e5f3309943db71c7)
  ] fix(mux): drop duplicate log_context! prefix in H2 flood/goaway error paths
  [`Florentin Dubois`] (`2026-04-17`)
- [
  [`55345ae2`](https://github.com/sozu-proxy/sozu/commit/55345ae2a7c486241a1ffe8a0399ff7e34b13e21)
  ] fix(mux): Phase 4 security hardening — port comparison, arithmetic, flood
  counters [`Florentin Dubois`] (`2026-04-17`)
- [
  [`7176fa3a`](https://github.com/sozu-proxy/sozu/commit/7176fa3abf35ab1ed313fc1bf96db29e65841865)
  ] fix(mux): harden H2 header validation, shutdown, and converter security
  [`Florentin Dubois`] (`2026-04-17`)
- [
  [`29ad73ed`](https://github.com/sozu-proxy/sozu/commit/29ad73eddcc6f4ace5754cf972d347ddac85c1d8)
  ] fix(h2): convert per-stream idle timeout from lifetime cap to true idle
  timeout [`Florentin Dubois`] (`2026-04-17`)
- [
  [`79b96a1f`](https://github.com/sozu-proxy/sozu/commit/79b96a1f8b4e84f884811354e872ff9c5a88f70d)
  ] fix(mux): harden TLS vectored write, stream recycling, teardown, and CL
  validation [`Florentin Dubois`] (`2026-04-16`)
- [
  [`15871d95`](https://github.com/sozu-proxy/sozu/commit/15871d95f22ea4b86f8c3c880db9635606984577)
  ] fix(mux): harden SETTINGS, routing errors, and WINDOW_UPDATE [`Florentin
  Dubois`] (`2026-04-16`)
- [
  [`6f3769bc`](https://github.com/sozu-proxy/sozu/commit/6f3769bca0f4c8982cebcb54d6bdaff0ab426d4b)
  ] fix(mux): harden H2 header validation against request smuggling [`Florentin
  Dubois`] (`2026-04-16`)
- [
  [`a607cf8d`](https://github.com/sozu-proxy/sozu/commit/a607cf8d306747e58adb0cef864eec1be2919f4a)
  ] fix(mux): clear IoSlice refs before consume to prevent UB [`Florentin
  Dubois`] (`2026-04-16`)
- [
  [`8402ef0c`](https://github.com/sozu-proxy/sozu/commit/8402ef0c056afe8dec24f798236f4d4b7230e09e)
  ] fix(h2): drain default-answer streams [`Florentin Dubois`] (`2026-04-16`)
- [
  [`48a6fef5`](https://github.com/sozu-proxy/sozu/commit/48a6fef5f6a5514ff9fade016cd499d97314123b)
  ] fix(metrics): prevent connections_per_backend gauge underflow [`Florentin
  Dubois`] (`2026-04-16`)
- [
  [`45796cd9`](https://github.com/sozu-proxy/sozu/commit/45796cd9edde2cf5d8b2286ce42024a144ed65b3)
  ] fix(h2): discriminate initial HEADERS from trailers via read-side parse
  phase [`Florentin Dubois`] (`2026-04-15`)
- [
  [`0d0062de`](https://github.com/sozu-proxy/sozu/commit/0d0062ded6b7a112792a67190fd3a5bf0664c705)
  ] fix(h2): back-pressure MAX_CONCURRENT_STREAMS on repeated REFUSED_STREAM
  [`Florentin Dubois`] (`2026-04-15`)
- [
  [`4a798013`](https://github.com/sozu-proxy/sozu/commit/4a79801357e35f3d311ab98109c66b1695600536)
  ] fix(h2): reject standalone CONTINUATION not preceded by HEADERS [`Florentin
  Dubois`] (`2026-04-15`)
- [
  [`350e05ac`](https://github.com/sozu-proxy/sozu/commit/350e05ac4ae11dbbb7a6f81b2b654ffcf7eb7814)
  ] fix(socket): distinguish peer FIN from peer RST to preserve half-close
  writes [`Florentin Dubois`] (`2026-04-15`)
- [
  [`77b26265`](https://github.com/sozu-proxy/sozu/commit/77b26265c260dc83bd43516759978b0baf1a8781)
  ] fix(h2): count WINDOW_UPDATE on closed streams as glitch [`Florentin
  Dubois`] (`2026-04-15`)
- [
  [`6c200656`](https://github.com/sozu-proxy/sozu/commit/6c200656cf4e7a85e5865313fb71d9152db6d781)
  ] fix(h2-parser): cap SETTINGS frame entries at 64 [`Florentin Dubois`]
  (`2026-04-15`)
- [
  [`7e69b763`](https://github.com/sozu-proxy/sozu/commit/7e69b7637f68923ca5f11fbb860e1945640ba733)
  ] fix(h2-serializer): mask reserved bits when writing stream IDs [`Florentin
  Dubois`] (`2026-04-15`)
- [
  [`aac9d0b7`](https://github.com/sozu-proxy/sozu/commit/aac9d0b7d4d865733b14ca36b8631473fbe0545f)
  ] fix(h2): restrict PRIORITY map to currently-known streams [`Florentin
  Dubois`] (`2026-04-15`)
- [
  [`8afba086`](https://github.com/sozu-proxy/sozu/commit/8afba08694fc1acf954dad7d36a878b9185f3dbc)
  ] fix(h2): disconnect backend when client-initiated stream IDs exhausted
  [`Florentin Dubois`] (`2026-04-15`)
- [
  [`69874f6e`](https://github.com/sozu-proxy/sozu/commit/69874f6e2056b836a296ca33329fe23c12c99575)
  ] fix(h2): add per-stream idle timeout to prevent slow-multiplex Slowloris
  [`Florentin Dubois`] (`2026-04-15`)
- [
  [`4b8fbd3a`](https://github.com/sozu-proxy/sozu/commit/4b8fbd3a02f812a4b84982707e6e676dc6704608)
  ] fix(pkawa): reject or deduplicate host header that conflicts with :authority
  [`Florentin Dubois`] (`2026-04-15`)
- [
  [`bdb3dc34`](https://github.com/sozu-proxy/sozu/commit/bdb3dc3412e75e93c7d1f0233ee2a214ba17fa21)
  ] fix(h2): add Rapid Reset lifetime cap and tighten HEADERS stream-id gate
  [`Florentin Dubois`] (`2026-04-15`)
- [
  [`5261dbe5`](https://github.com/sozu-proxy/sozu/commit/5261dbe5e82ed7bf47ef50372464766fb06f73e4)
  ] fix(h2-parser): harden GOAWAY length, strip_padding bounds, unknown frame,
  and PUSH_PROMISE [`Florentin Dubois`] (`2026-04-15`)
- [
  [`c3f9e090`](https://github.com/sozu-proxy/sozu/commit/c3f9e0903cf98c05009c1ed8801eec56f6b21d12)
  ] fix(pkawa): reject CRLF/CTL in H2 header, cookie, trailer, and pseudo-header
  values [`Florentin Dubois`] (`2026-04-15`)
- [
  [`ba0f177c`](https://github.com/sozu-proxy/sozu/commit/ba0f177c51f34e86428007754bb2f7358c0cf2b8)
  ] fix(mux-router): defer backend side-effects until H2 client is fully
  established [`Florentin Dubois`] (`2026-04-15`)
- [
  [`1f84f86e`](https://github.com/sozu-proxy/sozu/commit/1f84f86e3b592a55f50bce61fa5139ee61f3338a)
  ] fix(socket): break worker loop on MAX_LOOP_ITERATIONS instead of logging
  only [`Florentin Dubois`] (`2026-04-15`)
- [
  [`1a9cf071`](https://github.com/sozu-proxy/sozu/commit/1a9cf07166782ac48ad559ea44665dad74916548)
  ] fix(h2-parser): reject padded HEADERS+PRIORITY with truncated body
  [`Florentin Dubois`] (`2026-04-15`)
- [
  [`1ec86ac4`](https://github.com/sozu-proxy/sozu/commit/1ec86ac48cebc02a703c5b15ef7f1f163af24d6f)
  ] fix(h2): always signal pending write from queue_window_update [`Florentin
  Dubois`] (`2026-04-14`)
- [
  [`26e74ce2`](https://github.com/sozu-proxy/sozu/commit/26e74ce2c2568b964704ecce718920218caaac0a)
  ] fix(h2): decrement Backend.active_requests on GOAWAY-refused stream
  retirement [`Florentin Dubois`] (`2026-04-14`)
- [
  [`449641d8`](https://github.com/sozu-proxy/sozu/commit/449641d85d7388e614e85fc1bb04e9a31a03e190)
  ] fix(h2): decrement Backend.active_requests on inbound RST_STREAM
  (Position::Client) [`Florentin Dubois`] (`2026-04-14`)
- [
  [`f41fe98e`](https://github.com/sozu-proxy/sozu/commit/f41fe98e45496409425659ddc88f033192f273b0)
  ] fix(h2): RST_STREAM oversized CONTINUATION blocks instead of stalling
  [`Florentin Dubois`] (`2026-04-14`)
- [
  [`7b8ef9fa`](https://github.com/sozu-proxy/sozu/commit/7b8ef9fa0f674ce4c10821efa2ba97ee7d75d14f)
  ] fix(h2): credit and advance padded DATA frames by full wire length
  [`Florentin Dubois`] (`2026-04-14`)
- [
  [`e8d5a121`](https://github.com/sozu-proxy/sozu/commit/e8d5a12188015b3e8492445b26082a93acf84345)
  ] fix(splice): document safety invariants for unsafe libc blocks [`Florentin
  Dubois`] (`2026-04-14`)
- [
  [`8a95b644`](https://github.com/sozu-proxy/sozu/commit/8a95b6449c7b968e9cdcd65f1437b985a9152259)
  ] fix(socket): restore symmetry between socket_write and socket_write_vectored
  [`Florentin Dubois`] (`2026-04-14`)
- [
  [`d0a55409`](https://github.com/sozu-proxy/sozu/commit/d0a55409b0c960f8987dd6a4cb6396c29f0ea742)
  ] fix(mux): filter saturated H2 backends by peer max_concurrent_streams during
  reuse [`Florentin Dubois`] (`2026-04-14`)
- [
  [`639134f7`](https://github.com/sozu-proxy/sozu/commit/639134f74cc494d04958662ca637418d60b16044)
  ] fix(h2): retire pre-reset streams and restrict router reuse to H2
  [`Florentin Dubois`] (`2026-04-14`)
- [
  [`e0ae5bf9`](https://github.com/sozu-proxy/sozu/commit/e0ae5bf94f89b2a08bc218584391e174bfabf8fe)
  ] fix(h2): address P2-P3 review findings from cross-model audit [`Florentin
  Dubois`] (`2026-04-13`)
- [
  [`2133d345`](https://github.com/sozu-proxy/sozu/commit/2133d345596d41e3489e4b0fbc08c3ba088d80f2)
  ] fix(h2): address P0-P3 review findings for connection config [`Florentin
  Dubois`] (`2026-03-24`)
- [
  [`1bd71aeb`](https://github.com/sozu-proxy/sozu/commit/1bd71aeba03afc340976f4eed832deb47c1521ca)
  ] fix(http): use Shutdown::Write instead of Both for frontend socket
  [`Florentin Dubois`] (`2026-03-24`)
- [
  [`77d64925`](https://github.com/sozu-proxy/sozu/commit/77d64925798a76cf8e13223586229f5287a0865e)
  ] fix(mux): prune stale h2 streams on shutdown [`Florentin Dubois`]
  (`2026-03-24`)
- [
  [`aa9add8e`](https://github.com/sozu-proxy/sozu/commit/aa9add8ee4dc8a88b3abb56b20d06d70b2aa70de)
  ] fix(h2): bound worker shutdown stalls [`Florentin Dubois`] (`2026-03-23`)
- [
  [`690b9fbb`](https://github.com/sozu-proxy/sozu/commit/690b9fbb3f7763c9c9e7f8dfc8fe7b19b2abc9eb)
  ] fix(mux): drive frontend writes during shutdown [`Florentin Dubois`]
  (`2026-03-23`)
- [
  [`44e28b73`](https://github.com/sozu-proxy/sozu/commit/44e28b73d508e4fc003f5b18d420ad61e8fc41fc)
  ] fix(h2): complete graceful shutdown and stabilize e2e drains [`Florentin
  Dubois`] (`2026-03-19`)
- [
  [`5e865466`](https://github.com/sozu-proxy/sozu/commit/5e86546615ae106fcc3829eae25cb4035d0a4437)
  ] fix(mux): force-close lingering H2 sessions after graceful shutdown timeout
  [`Florentin Dubois`] (`2026-03-19`)
- [
  [`cb6fcff9`](https://github.com/sozu-proxy/sozu/commit/cb6fcff9e99ae87ee40db318ab88f40dc9de5ce2)
  ] fix(mux): defer all session closes while TLS data pending and fix H2 stream
  removal ordering [`Florentin Dubois`] (`2026-03-18`)
- [
  [`0dc1f83a`](https://github.com/sozu-proxy/sozu/commit/0dc1f83a22f55dbba6ea7a579f5f027bb37090f0)
  ] fix(mux): initiate TLS close_notify on HUP before teardown for H1 and H2
  [`Florentin Dubois`] (`2026-03-18`)
- [
  [`9ab618b7`](https://github.com/sozu-proxy/sozu/commit/9ab618b71545d30c04655914a3bb91daf78996b5)
  ] fix(h2): prevent TCP RST on session close and preserve WRITABLE across
  read-close [`Florentin Dubois`] (`2026-03-18`)
- [
  [`20710090`](https://github.com/sozu-proxy/sozu/commit/2071009013c023911eda5469f1d9a064e2d93ef5)
  ] fix(h2): prevent TLS truncation on session close and harden proxy-protocol
  [`Florentin Dubois`] (`2026-03-18`)
- [
  [`016d6e72`](https://github.com/sozu-proxy/sozu/commit/016d6e72dc8e9b397ad356eead018279132c125f)
  ] fix(h2): signal_pending_write on WINDOW_UPDATE and fix send window
  accounting [`Florentin Dubois`] (`2026-03-17`)
- [
  [`79cda614`](https://github.com/sozu-proxy/sozu/commit/79cda614c34bd9e71de34d47083b00399e2fbea9)
  ] fix(h2): add retry loop to socket_write_vectored for TLS buffer saturation
  [`Florentin Dubois`] (`2026-03-17`)
- [
  [`6da87e74`](https://github.com/sozu-proxy/sozu/commit/6da87e748ca890d7871f3b4165f3397163459b82)
  ] fix(h2): resolve H1 backend read starvation under flow control pressure
  [`Florentin Dubois`] (`2026-03-17`)
- [
  [`0129daa6`](https://github.com/sozu-proxy/sozu/commit/0129daa6f544636d724151995b2ba68dc1d28fa2)
  ] fix(lib): make unit tests pass reliably in parallel execution [`Florentin
  Dubois`] (`2026-03-17`)
- [
  [`469509ed`](https://github.com/sozu-proxy/sozu/commit/469509ed725b5d6da85f5d5b9221eeb2e0f9779e)
  ] fix(h2): inject chunked transfer encoding for H2 requests without
  Content-Length [`Florentin Dubois`] (`2026-03-17`)
- [
  [`28bb49c9`](https://github.com/sozu-proxy/sozu/commit/28bb49c9da404ec9eb913459926447ae356bb968)
  ] fix(h2): silence expected log noise from rustls buffer and detached streams
  [`Florentin Dubois`] (`2026-03-16`)
- [
  [`0cd01a1d`](https://github.com/sozu-proxy/sozu/commit/0cd01a1d2b37c8d5942e40faac8ddeaeb3d0f53b)
  ] fix(h2): detect close-delimited backend EOF and dead backends in H1→H2 path
  [`Florentin Dubois`] (`2026-03-16`)
- [
  [`3c9e3b26`](https://github.com/sozu-proxy/sozu/commit/3c9e3b263e1c41928123e598d50d866ca4e89c9c)
  ] fix(e2e): use multi-attempt reads for flood detection tests [`Florentin
  Dubois`] (`2026-03-16`)
- [
  [`8f0188ee`](https://github.com/sozu-proxy/sozu/commit/8f0188ee10deb85bae95f3588c714495083fe137)
  ] fix(h2): store H2 cookies in detached jar for proper H1 concatenation
  [`Florentin Dubois`] (`2026-03-16`)
- [
  [`31ecc031`](https://github.com/sozu-proxy/sozu/commit/31ecc0319ec0f3cbe332176b8de84be1d0d9aa83)
  ] fix(h2): correct empty header name test and downgrade active stream logs
  [`Florentin Dubois`] (`2026-03-16`)
- [
  [`bd321b1c`](https://github.com/sozu-proxy/sozu/commit/bd321b1cf49ba099130093626fbb8a9d49ebbdd4)
  ] fix(h2): handle 1xx informational responses and proxy protocol v2 E2E tests
  [`Florentin Dubois`] (`2026-03-16`)
- [
  [`1e654a2d`](https://github.com/sozu-proxy/sozu/commit/1e654a2d1a9845e68ac5f0dbe2da22933cbe6937)
  ] fix(e2e): use String for old_fingerprint in certificate reload test
  [`Florentin Dubois`] (`2026-03-16`)
- [
  [`39cbf3fa`](https://github.com/sozu-proxy/sozu/commit/39cbf3faac61625bbb5729ac4e280010272f87a5)
  ] fix(h2): apply P1+P2+P3 review findings for correctness and safety
  [`Florentin Dubois`] (`2026-03-16`)
- [
  [`fb37d2a9`](https://github.com/sozu-proxy/sozu/commit/fb37d2a9d6c291551a3ce6ae09e7dc59ac3316a5)
  ] fix(h2): arithmetic overflow in distribute_overhead and idle stream
  detection on backend connections [`Florentin Dubois`] (`2026-03-14`)
- [
  [`0110b39c`](https://github.com/sozu-proxy/sozu/commit/0110b39c41c7481514e27e711f01b401b9581d6b)
  ] fix(h2): send RST_STREAM(STREAM_CLOSED) for DATA on closed streams
  [`Florentin Dubois`] (`2026-03-14`)
- [
  [`2834d94d`](https://github.com/sozu-proxy/sozu/commit/2834d94d6fda29e477c1616fb2a966d066d6044d)
  ] fix(h2): resolve all remaining H2 e2e test failures [`Florentin Dubois`]
  (`2026-03-13`)
- [
  [`47e6c457`](https://github.com/sozu-proxy/sozu/commit/47e6c457e5070263ccdfd49cf166405d0b078f7b)
  ] fix(h2): fix stream refusal payload discard and buffer pool exhaustion
  [`Florentin Dubois`] (`2026-03-12`)
- [
  [`678b3df9`](https://github.com/sozu-proxy/sozu/commit/678b3df95aaa41d918ac03e23dd2296288334f7f)
  ] fix(h2): address all review findings from PR #1209 audit [`Florentin
  Dubois`] (`2026-03-12`)
- [
  [`3a7f0b5a`](https://github.com/sozu-proxy/sozu/commit/3a7f0b5a461549178a2a1f8473a99d4a371f25f7)
  ] fix(h2): update connection window state and wire graceful_goaway into
  shutdown [`Florentin Dubois`] (`2026-03-12`)
- [
  [`a341b392`](https://github.com/sozu-proxy/sozu/commit/a341b392960955877aa5468816f8cf564273cc71)
  ] fix(h2): address P1/P2 review findings [`Florentin Dubois`] (`2026-03-12`)
- [
  [`03e8ad71`](https://github.com/sozu-proxy/sozu/commit/03e8ad71f4019a58bc6714f74924b416b36e2617)
  ] fix(mux): initialize settings_sent_at and rst_sent fields in H2 constructors
  [`Florentin Dubois`] (`2026-03-12`)
- [
  [`0172b863`](https://github.com/sozu-proxy/sozu/commit/0172b863d707f25221c819df4961cbafebe51b36)
  ] fix(session): perform full cleanup on failed protocol upgrades [`Florentin
  Dubois`] (`2026-03-12`)
- [
  [`a3f5cc10`](https://github.com/sozu-proxy/sozu/commit/a3f5cc10eae1001e9be74500d9d11ec07b94b01c)
  ] fix(mux): drain response buffer on close-delimited backend instead of
  setting HUP [`Florentin Dubois`] (`2026-03-12`)
- [
  [`8d654c77`](https://github.com/sozu-proxy/sozu/commit/8d654c77c998286dcd5a12ab578fc0a3b96d6f5e)
  ] fix(mux): fix byte tracking, flow control, GoAway retry, and TE header
  [`Florentin Dubois`] (`2026-03-12`)
- [
  [`3ad0e732`](https://github.com/sozu-proxy/sozu/commit/3ad0e732cecc4e5124a610a75b42747ec34f88cb)
  ] fix(metrics): restore configuration gauge updates after state mutations
  [`Florentin Dubois`] (`2026-03-12`)
- [
  [`a650ad69`](https://github.com/sozu-proxy/sozu/commit/a650ad69cfeef2d8cdfeeb9dd990f9e83d7c3c7b)
  ] fix(mux): prevent gauge underflow and error counter inflation [`Florentin
  Dubois`] (`2026-03-12`)
- [
  [`5f884551`](https://github.com/sozu-proxy/sozu/commit/5f884551ed82564d66423423862459bdd2f52be1)
  ] fix(mux): fix service_time inflation, gauge leak, and WebSocket upgrade
  accounting [`Florentin Dubois`] (`2026-03-11`)
- [
  [`1d794fff`](https://github.com/sozu-proxy/sozu/commit/1d794fffbb3acb3a625ef5cda65af343b29ab669)
  ] fix(mux): re-arm timeouts and handle H2 Linked streams on frontend timeout
  [`Florentin Dubois`] (`2026-03-11`)
- [
  [`deeba79c`](https://github.com/sozu-proxy/sozu/commit/deeba79c9ca3fe5320d3a6a2e810d21d2bee8dfc)
  ] fix(metrics): restore missing backend metrics in mux layer [`Florentin
  Dubois`] (`2026-03-11`)
- [
  [`d636a5c6`](https://github.com/sozu-proxy/sozu/commit/d636a5c680b21a5d38173abf29d21ed7b20bba9a)
  ] fix(logs): downgrade noisy mux/H1/H2 logs from warn/error to debug
  [`Florentin Dubois`] (`2026-03-10`)
- [
  [`4e5f8686`](https://github.com/sozu-proxy/sozu/commit/4e5f86863ea26edb55e5bdead69df52d35cc4b96)
  ] fix(h2): address review findings — closed stream detection, parser masks,
  formatting [`Florentin Dubois`] (`2026-03-10`)
- [
  [`56b6feea`](https://github.com/sozu-proxy/sozu/commit/56b6feea5652562fbca67b1de351f88635cea096)
  ] fix(h2): ignore RST_STREAM and WINDOW_UPDATE on closed streams [`Florentin
  Dubois`] (`2026-03-10`)
- [
  [`cb217793`](https://github.com/sozu-proxy/sozu/commit/cb217793fba6826c1b146dcf8caac3383d516c0e)
  ] fix(h2): fix H2→H2 concurrent streams and harden e2e test assertions
  [`Florentin Dubois`] (`2026-03-10`)
- [
  [`66311086`](https://github.com/sozu-proxy/sozu/commit/663110868f944b93b3da09bed894eba04b27f66c)
  ] fix(h2): protocol correctness — CONTINUATION, Content-Length, RST_STREAM,
  GoAway drain [`Florentin Dubois`] (`2026-03-10`)
- [
  [`da33ae18`](https://github.com/sozu-proxy/sozu/commit/da33ae18c54b86b86e714559199d2ff73b9feadc)
  ] fix(mux): eliminate all crash paths — replace panics with graceful error
  handling [`Florentin Dubois`] (`2026-03-10`)
- [
  [`3e8e0c97`](https://github.com/sozu-proxy/sozu/commit/3e8e0c977569d3123c89823387bbd4ebf614c67c)
  ] fix(mux): address review findings — response path, unwrap, unreachable
  [`Florentin Dubois`] (`2026-03-10`)
- [
  [`a11883a7`](https://github.com/sozu-proxy/sozu/commit/a11883a7e6a24e1cf3e7c8be42332d64ad92d231)
  ] fix(mux): remove dead answers field from sessions and fix parser
  anti-pattern [`Florentin Dubois`] (`2026-03-10`)
- [
  [`3e40240d`](https://github.com/sozu-proxy/sozu/commit/3e40240dc883df8bf66f81046b3450a3b1416dd2)
  ] fix(h2): fix critical review findings in HPACK decode and stream management
  [`Florentin Dubois`] (`2026-03-10`)
- [
  [`9a12c0a3`](https://github.com/sozu-proxy/sozu/commit/9a12c0a3b30d2735996e7dfd3326cdc8f34b2c97)
  ] fix(h2): implement remaining RFC 9113 compliance and performance fixes
  [`Florentin Dubois`] (`2026-03-10`)
- [
  [`28a80cff`](https://github.com/sozu-proxy/sozu/commit/28a80cffa2be2faef853098c53bb73e0d9443104)
  ] fix(h2): handle received GOAWAY frames with graceful connection shutdown
  [`Florentin Dubois`] (`2026-03-09`)
- [
  [`8180d5e2`](https://github.com/sozu-proxy/sozu/commit/8180d5e22d84366480acca1dd555e01f7ce084e7)
  ] fix(h2): derive :scheme pseudo-header from listener protocol instead of
  hardcoding https [`Florentin Dubois`] (`2026-03-09`)
- [
  [`5df6cac3`](https://github.com/sozu-proxy/sozu/commit/5df6cac304b4669f5690442fa342119a0ddff288)
  ] fix(h2): split data_received counter into per-direction fields [`Florentin
  Dubois`] (`2026-03-09`)
- [
  [`a2b68852`](https://github.com/sozu-proxy/sozu/commit/a2b688523218aa7307570b354f62be04317762b0)
  ] fix(h2): replace unreachable!() with graceful handling in close/end_stream
  [`Florentin Dubois`] (`2026-03-09`)
- [
  [`223024b5`](https://github.com/sozu-proxy/sozu/commit/223024b5f424d340693ec470acdadc798ea26133)
  ] fix(h2): harden HPACK decode callbacks against panics and invalid trailers
  [`Florentin Dubois`] (`2026-03-09`)
- [
  [`cd44d6f7`](https://github.com/sozu-proxy/sozu/commit/cd44d6f7db89ce7dc13411e7155d43c44b9f2a96)
  ] fix(h2): implement WINDOW_UPDATE flow control and fix review findings
  [`Florentin Dubois`] (`2026-03-09`)
- [
  [`29c2d01c`](https://github.com/sozu-proxy/sozu/commit/29c2d01c73df125747dec2b62cf4a05e22e35672)
  ] fix(h2): pass all h2spec HTTP/2 conformance tests [`Florentin Dubois`]
  (`2026-03-09`)
- [
  [`e6851ffb`](https://github.com/sozu-proxy/sozu/commit/e6851ffbaf685f2b17b5be6eff92b713e22042ea)
  ] fix(tls): return Continue from FrontRustls::socket_read when buffer is full
  [`Florentin Dubois`] (`2026-03-09`)
- [
  [`bd2bb357`](https://github.com/sozu-proxy/sozu/commit/bd2bb357697aac3277b5fe12abfde62f07a46d70)
  ] fix(mux): use listener HttpAnswers templates for default error responses
  [`Florentin Dubois`] (`2026-03-09`)
- [
  [`89d6884f`](https://github.com/sozu-proxy/sozu/commit/89d6884fa53d640bd3838ddecca9772761be1627)
  ] fix(h2): address review findings from PR #1209 [`Florentin Dubois`]
  (`2026-03-09`)
- [
  [`76404ce2`](https://github.com/sozu-proxy/sozu/commit/76404ce28a70c5611d73c5cb7db4e989c3a3b1eb)
  ] fix(h2): replace unmaintained hpack 0.3.0 with loona-hpack 0.4.3 [`Florentin
  Dubois`] (`2026-03-09`)

#### 🚀 Performance

- [
  [`cec41f54`](https://github.com/sozu-proxy/sozu/commit/cec41f54586c819602ad7eda21cbb25a1979eeed)
  ] perf(h2): keep per-bucket incremental count consistent mid-pass [`Florentin
  Dubois`] (`2026-04-24`)
- [
  [`b9c874c8`](https://github.com/sozu-proxy/sozu/commit/b9c874c81a429a3259501beb550184ff0c2a9681)
  ] bench(h2): add priority scheduling sort strategy benchmark [`Florentin
  Dubois`] (`2026-04-16`)
- [
  [`551333fa`](https://github.com/sozu-proxy/sozu/commit/551333fa854c621ed082874892c4715234bf5ee2)
  ] perf(mux): reuse IoSlice buffer in write hot path [`Florentin Dubois`]
  (`2026-04-16`)
- [
  [`3ae96601`](https://github.com/sozu-proxy/sozu/commit/3ae966014359e0ebc0698526ab6d479d4047677d)
  ] perf(mux): add backend-token reverse index for stream lookup [`Florentin
  Dubois`] (`2026-04-16`)
- [
  [`98ea27f8`](https://github.com/sozu-proxy/sozu/commit/98ea27f8d98957f7f65e3f1ed62da77dc53fee5a)
  ] perf(mux): replace Link scan with pending_links queue [`Florentin Dubois`]
  (`2026-04-16`)
- [
  [`ae7521e9`](https://github.com/sozu-proxy/sozu/commit/ae7521e9b46263b05b493adbd6c9ed00e0648a65)
  ] perf(h2): use Store::from_vec for HPACK output to avoid copy [`Florentin
  Dubois`] (`2026-04-16`)
- [
  [`c4ce4ab7`](https://github.com/sozu-proxy/sozu/commit/c4ce4ab7d0eba971d7ea0bfb3b54c72b88ccd976)
  ] perf(h2): reuse cookie buffer in H2BlockConverter [`Florentin Dubois`]
  (`2026-04-16`)
- [
  [`267e1993`](https://github.com/sozu-proxy/sozu/commit/267e199353660c766c847e68df0190042c0843b9)
  ] perf(h2): batch hot-path gauge updates via state-change helper [`Florentin
  Dubois`] (`2026-04-15`)
- [
  [`76958fe7`](https://github.com/sozu-proxy/sozu/commit/76958fe7f777024b690b1228a8158af83cc60126)
  ] perf(mux): first-byte dispatch in is_connection_specific_header [`Florentin
  Dubois`] (`2026-04-15`)
- [
  [`503dadaa`](https://github.com/sozu-proxy/sozu/commit/503dadaa7ed7d29c8d0d8b285f8668b87ebfec3d)
  ] perf(h2): use sort_by_cached_key for priorities buffer [`Florentin Dubois`]
  (`2026-04-15`)
- [
  [`4235f636`](https://github.com/sozu-proxy/sozu/commit/4235f636a9b4812abe41417001511844165654ee)
  ] perf(h2): replace flood-check slice with early-return chain [`Florentin
  Dubois`] (`2026-04-15`)
- [
  [`219d6eed`](https://github.com/sozu-proxy/sozu/commit/219d6eed950da7aacb00deea1b564588c687f42d)
  ] perf(h2): replace format!() with Vec<u8> + write!() in chunk hex path
  [`Florentin Dubois`] (`2026-03-17`)

#### 🚀 Refactored

- [
  [`f8935244`](https://github.com/sozu-proxy/sozu/commit/f89352440862a4585a47696a30c9f76bc1937f01)
  ] refactor: collapse actor display + pseudo-header dispatch boilerplate
  [`Florentin Dubois`] (`2026-04-25`)
- [
  [`1f631c90`](https://github.com/sozu-proxy/sozu/commit/1f631c9034c9075f92b89ce900bde82bccbc0fe7)
  ] refactor(parser): extract ensure_frame_size! macro for fixed-size frames
  [`Florentin Dubois`] (`2026-04-25`)
- [
  [`8b2769c7`](https://github.com/sozu-proxy/sozu/commit/8b2769c725741e71a835d129ff2ef302a79e200b)
  ] refactor(mux): collapse arm_writable + flood-check repetition [`Florentin
  Dubois`] (`2026-04-25`)
- [
  [`a2ae6006`](https://github.com/sozu-proxy/sozu/commit/a2ae600606993e8f64510ef1605f92f6b8c2bc88)
  ] refactor(lib): extract log-layout scanner shared between build.rs and
  integration test [`Florentin Dubois`] (`2026-04-25`)
- [
  [`e338e646`](https://github.com/sozu-proxy/sozu/commit/e338e646af672819310061087422b91747d2d11a)
  ] refactor(command): add Readiness::arm_writable convenience method
  [`Florentin Dubois`] (`2026-04-24`)
- [
  [`31d0d8d2`](https://github.com/sozu-proxy/sozu/commit/31d0d8d206527ec8cf822d0b57777f6183c9a4b8)
  ] refactor(command): finish audit log enrichment — ts, actor_role, connect_ts,
  boot_generation, build_git_sha, JSON sink [`Florentin Dubois`] (`2026-04-24`)
- [
  [`35de619d`](https://github.com/sozu-proxy/sozu/commit/35de619d5c7e550716a146b9e91dd3af6c383437)
  ] refactor(command): audit log P2+P3 — ucred/user/socket, dedicated sink, diff
  before→after, request sha256, cert-replace new fingerprint [`Florentin
  Dubois`] (`2026-04-23`)
- [
  [`c8acdde0`](https://github.com/sozu-proxy/sozu/commit/c8acdde07fda9cac5712f7b4e152d601db2655c4)
  ] refactor(command): enrich audit log — MUX layout, full ucred, fanout,
  injection defence [`Florentin Dubois`] (`2026-04-23`)
- [
  [`63c28bcf`](https://github.com/sozu-proxy/sozu/commit/63c28bcfadfd4a9e150e2862dd1ff58af08fdfe7)
  ] refactor(command): emit audit log at info level instead of debug [`Florentin
  Dubois`] (`2026-04-23`)
- [
  [`f566ed36`](https://github.com/sozu-proxy/sozu/commit/f566ed36a76cd8e567d377aa585db206bcd0a257)
  ] refactor(command): realign audit log to MUX `Session(...)` layout at debug
  level [`Florentin Dubois`] (`2026-04-23`)
- [
  [`366a502a`](https://github.com/sozu-proxy/sozu/commit/366a502a4c2c84a81d892d09fe0b7690642b429c)
  ] refactor(metrics): replace mem::uninitialized with MaybeUninit::zeroed
  [`Florentin Dubois`] (`2026-04-22`)
- [
  [`6847ba92`](https://github.com/sozu-proxy/sozu/commit/6847ba92af955ed0d8d99076c0e4f1deb8eae09b)
  ] refactor(mux): add Context::http_context accessor, drop streams[sid].context
  plumbing [`Florentin Dubois`] (`2026-04-21`)
- [
  [`34c197dd`](https://github.com/sozu-proxy/sozu/commit/34c197dd8301dc53c7a1fd8f15d81e0acc1deb91)
  ] refactor(logging): hoist ANSI prefix palette into
  sozu_command::logging::ansi_palette() [`Florentin Dubois`] (`2026-04-21`)
- [
  [`2a8d1be1`](https://github.com/sozu-proxy/sozu/commit/2a8d1be18bdf5e7dd193f01b2427bb898559250c)
  ] refactor(e2e): extract shared loop-read helpers into h2_utils [`Florentin
  Dubois`] (`2026-04-20`)
- [
  [`497cd256`](https://github.com/sozu-proxy/sozu/commit/497cd256bd442bbbf94a953b63f0f64b9d83ee76)
  ] refactor(h2-parser): apply take(payload_len) symmetrically in fixed-size
  frame parsers [`Florentin Dubois`] (`2026-04-15`)
- [
  [`2e6b0ef0`](https://github.com/sozu-proxy/sozu/commit/2e6b0ef08942c6d381da3bfdf6573fa644a5c5a7)
  ] refactor(mux): replace upgrade_mux .expect()/unreachable!() with graceful
  error return [`Florentin Dubois`] (`2026-04-15`)
- [
  [`e860bb67`](https://github.com/sozu-proxy/sozu/commit/e860bb67ec2049d023d2ee6683fa141e82ceafbc)
  ] refactor(mux): move Connection and Endpoint impls to connection.rs
  [`Florentin Dubois`] (`2026-04-15`)
- [
  [`7d8fb33c`](https://github.com/sozu-proxy/sozu/commit/7d8fb33c0f94bcc1b8da5e578da81ec166ade9ab)
  ] refactor(mux): move Router to router.rs [`Florentin Dubois`] (`2026-04-15`)
- [
  [`f27a449d`](https://github.com/sozu-proxy/sozu/commit/f27a449d0eda36627ee50e216f11ba0ded14216f)
  ] refactor(mux): move Stream types to stream.rs [`Florentin Dubois`]
  (`2026-04-15`)
- [
  [`9152766f`](https://github.com/sozu-proxy/sozu/commit/9152766fe660d36fac9b18db96c9dca84c21ebab)
  ] refactor(mux): move default-answer helpers to answers.rs [`Florentin
  Dubois`] (`2026-04-15`)
- [
  [`3fceee7a`](https://github.com/sozu-proxy/sozu/commit/3fceee7a113741e5029b6405aae6268b8372d261)
  ] refactor(mux): move DebugHistory and DebugEvent to debug.rs [`Florentin
  Dubois`] (`2026-04-15`)
- [
  [`f94e8bc4`](https://github.com/sozu-proxy/sozu/commit/f94e8bc4e79a8a4f0fc0b354809f3d7bfd9a19fe)
  ] refactor(h2): use named STREAM_ID_MAX constant in new_stream_id [`Florentin
  Dubois`] (`2026-04-15`)
- [
  [`4bfb3c70`](https://github.com/sozu-proxy/sozu/commit/4bfb3c70d534d10a5cd980b90e0a676b160dc3bc)
  ] refactor(mux): drop redundant MAX_HEADER_LIST_SIZE casts [`Florentin
  Dubois`] (`2026-04-15`)
- [
  [`bd4f4014`](https://github.com/sozu-proxy/sozu/commit/bd4f4014e12f56a47f25b0b09bc427a3e47487e9)
  ] refactor(h1): use end_stream_decision helper, normalize 502 for
  backend-closed-no-response [`Florentin Dubois`] (`2026-04-15`)
- [
  [`ebef96da`](https://github.com/sozu-proxy/sozu/commit/ebef96dac03b31c41e7a0ef124fd4d84929fc29b)
  ] refactor(h2): debug_assert unreachable Continuation in handle_frame dispatch
  [`Florentin Dubois`] (`2026-04-15`)
- [
  [`1059216a`](https://github.com/sozu-proxy/sozu/commit/1059216a00b38abae9fd68b9db06a78cb9ed4e08)
  ] refactor(h2): use named fields on H2StreamId::Other [`Florentin Dubois`]
  (`2026-04-15`)
- [
  [`51e3f244`](https://github.com/sozu-proxy/sozu/commit/51e3f24469dc7cf39b22c42cca8c21ceade65be8)
  ] refactor(h2): drop redundant error log in Prioriser::push_priority
  [`Florentin Dubois`] (`2026-04-15`)
- [
  [`cb38313e`](https://github.com/sozu-proxy/sozu/commit/cb38313e9fcfd17d6e605d17c200c68910ab4d92)
  ] refactor(h2): use StreamState::is_open consistently [`Florentin Dubois`]
  (`2026-04-15`)
- [
  [`c8de449b`](https://github.com/sozu-proxy/sozu/commit/c8de449ba2c427e7b90ef873483a4654756b26e0)
  ] refactor(serializer): extract gen_control_frame shell for control frames
  [`Florentin Dubois`] (`2026-04-15`)
- [
  [`3ebd0c46`](https://github.com/sozu-proxy/sozu/commit/3ebd0c46eda54176cef915d5c7fc31fcd1d66b02)
  ] refactor(h2): derive Copy on H2Settings [`Florentin Dubois`] (`2026-04-15`)
- [
  [`1282c78b`](https://github.com/sozu-proxy/sozu/commit/1282c78bddf517e8dc0baa579039a1ca5abc0a56)
  ] refactor(pkawa): extract decode_headers_with_budget shell [`Florentin
  Dubois`] (`2026-04-15`)
- [
  [`119ce736`](https://github.com/sozu-proxy/sozu/commit/119ce736269cb3e7e43f886b744253ca53ecf6cf)
  ] refactor(h2): change MAX_HEADER_LIST_SIZE to usize at declaration
  [`Florentin Dubois`] (`2026-04-15`)
- [
  [`cb80a595`](https://github.com/sozu-proxy/sozu/commit/cb80a595c44a320da3dccc24e461920396088be0)
  ] refactor(h2): extract end_stream_decision helper, normalize 502 for
  backend-closed-no-response [`Florentin Dubois`] (`2026-04-15`)
- [
  [`9758db46`](https://github.com/sozu-proxy/sozu/commit/9758db465a008bc1023105b043f1298c76e9e6b5)
  ] refactor(h1): use shared drain_tls_close_notify helper [`Florentin Dubois`]
  (`2026-04-15`)
- [
  [`1467644f`](https://github.com/sozu-proxy/sozu/commit/1467644f33b13aae3eca3ba1330bbefedd60030a)
  ] refactor(h2): use shared drain_tls_close_notify helper [`Florentin Dubois`]
  (`2026-04-15`)
- [
  [`bebde969`](https://github.com/sozu-proxy/sozu/commit/bebde969b2358794ccae0e446fa93094a81739f7)
  ] refactor(h2): factor flush_stream_out helper to unify write_streams paths
  [`Florentin Dubois`] (`2026-04-14`)
- [
  [`2d4c392c`](https://github.com/sozu-proxy/sozu/commit/2d4c392c67ba09e7edce4cac765b4b2e0a0bc2d2)
  ] refactor(mux): factor shared update_readiness helper for read/write halves
  [`Florentin Dubois`] (`2026-04-14`)
- [
  [`ccea54ef`](https://github.com/sozu-proxy/sozu/commit/ccea54ef45c3fd9113d2bc798ab28dd8e528c3ed)
  ] refactor(mux): inline single-use parse_error_answer helpers [`Florentin
  Dubois`] (`2026-04-14`)
- [
  [`ae744161`](https://github.com/sozu-proxy/sozu/commit/ae744161ce964616f2f77037166e47908bd271a7)
  ] refactor(splice): use libc crate FFI declarations and narrow dead_code allow
  [`Florentin Dubois`] (`2026-04-14`)
- [
  [`1cb632ef`](https://github.com/sozu-proxy/sozu/commit/1cb632efb7ca07f1bc7d8dc8178be8353684277e)
  ] refactor(mux): extract Position::Client backend bookkeeping helpers
  [`Florentin Dubois`] (`2026-04-14`)
- [
  [`85651e74`](https://github.com/sozu-proxy/sozu/commit/85651e7474fd79592c89113a7b0f93645466da63)
  ] refactor(mux): collapse Connection<Front> dispatch methods via forward macro
  [`Florentin Dubois`] (`2026-04-14`)
- [
  [`c8c47000`](https://github.com/sozu-proxy/sozu/commit/c8c4700002c8b94dcfe08ba34cdbda7866ba8457)
  ] refactor(h2): replace stream index sentinels with Option<GlobalStreamId>
  [`Florentin Dubois`] (`2026-03-16`)
- [
  [`7d384685`](https://github.com/sozu-proxy/sozu/commit/7d384685a8024f28439ad96a7fc36119554156ce)
  ] refactor(h2): extract post-write cleanup from write_streams [`Florentin
  Dubois`] (`2026-03-16`)
- [
  [`c75dc377`](https://github.com/sozu-proxy/sozu/commit/c75dc37783f7cf324d36a090bf4e346b33ed2c0b)
  ] refactor(h2): decompose handle_frame and extract stream recycling helper
  [`Florentin Dubois`] (`2026-03-16`)
- [
  [`d18e65ad`](https://github.com/sozu-proxy/sozu/commit/d18e65ad30bbe5df8c645a8d3a12e1d470029346)
  ] refactor(h2): consolidate error codes, extract helpers, simplify dead code
  [`Florentin Dubois`] (`2026-03-16`)
- [
  [`02530f91`](https://github.com/sozu-proxy/sozu/commit/02530f91dd2fa6214128696264e7207b1c3dd896)
  ] refactor(h2): extract handler methods from readable/writable for clarity
  [`Florentin Dubois`] (`2026-03-14`)
- [
  [`6f51c5e3`](https://github.com/sozu-proxy/sozu/commit/6f51c5e30ff666924e5caac1b7c1821d3747ce53)
  ] refactor(h2): decompose ConnectionH2 into sub-structs and simplify
  DefaultAnswer [`Florentin Dubois`] (`2026-03-14`)
- [
  [`ed53f0e1`](https://github.com/sozu-proxy/sozu/commit/ed53f0e12681c62bb645731224b2dc6a31d0c341)
  ] refactor(h2): round 3 - dedup stream recycling, fix HPACK decoder table sync
  [`Florentin Dubois`] (`2026-03-14`)
- [
  [`a1baaebb`](https://github.com/sozu-proxy/sozu/commit/a1baaebb7bee2503f6a983a3bdf46790397488d6)
  ] refactor(h2): round 2 review fixes, simplifications, and protocol compliance
  tests [`Florentin Dubois`] (`2026-03-14`)
- [
  [`ac88297a`](https://github.com/sozu-proxy/sozu/commit/ac88297a877944a379925a9f0e662152bceab768)
  ] refactor(h2): address review findings, simplify mux module, add security e2e
  tests [`Florentin Dubois`] (`2026-03-14`)
- [
  [`1c49462d`](https://github.com/sozu-proxy/sozu/commit/1c49462d47c54109ee717adda1837ff4b6fa5b52)
  ] refactor(mux): cleanup dead code, fix allocations, and reduce log noise
  [`Florentin Dubois`] (`2026-03-12`)
- [
  [`e00b5ca3`](https://github.com/sozu-proxy/sozu/commit/e00b5ca360f71e355b79a1399148229f68ec2a1d)
  ] refactor(h2): extract hardcoded values into named constants [`Florentin
  Dubois`] (`2026-03-10`)
- [
  [`a7553072`](https://github.com/sozu-proxy/sozu/commit/a7553072a986a244f1860bec49d7c0e032ed8a43)
  ] refactor(mux): remove #![allow] suppression from converter.rs and fix clippy
  [`Florentin Dubois`] (`2026-03-09`)
- [
  [`3b0bb2a9`](https://github.com/sozu-proxy/sozu/commit/3b0bb2a960af9ad5b4837ad19e6121067aa8e1ad)
  ] refactor(mux): remove #![allow] suppressions and fix clippy warnings
  [`Florentin Dubois`] (`2026-03-09`)

#### 🧪 Tests

- [
  [`d5b6d230`](https://github.com/sozu-proxy/sozu/commit/d5b6d2300f7df2d16ed7e623402b608f3ff736bd)
  ] test(lib): static log-layout integration test for sozu-lib [`Florentin
  Dubois`] (`2026-04-25`)
- [
  [`88f42525`](https://github.com/sozu-proxy/sozu/commit/88f425256352655e858d9b94ff29d6763edc432d)
  ] test(e2e): GOAWAY-then-close regression guards (peer-aborted, clean,
  in-flight) [`Florentin Dubois`] (`2026-04-25`)
- [
  [`158f029d`](https://github.com/sozu-proxy/sozu/commit/158f029db38ca999a4fdc44da462dbd53a688db4)
  ] test(e2e): serialise FIX-18/FIX-22 against shared injection AtomicBool
  [`Florentin Dubois`] (`2026-04-25`)
- [
  [`f1f57858`](https://github.com/sozu-proxy/sozu/commit/f1f5785898c5d85e38a5a19237ed729f3639b25e)
  ] test(e2e): make H2Backend::start synchronous on accept-loop readiness
  [`Florentin Dubois`] (`2026-04-25`)
- [
  [`54043d84`](https://github.com/sozu-proxy/sozu/commit/54043d8407d6a1503ec987ad391aa7bbb8dfe51e)
  ] test(e2e): raise large-body WU cadence to 1 MiB to stay below glitch cap
  [`Florentin Dubois`] (`2026-04-25`)
- [
  [`aa086648`](https://github.com/sozu-proxy/sozu/commit/aa08664821bd2397c70547189547418aef6aa36f)
  ] test(e2e): keep draining after a WU write races peer close [`Florentin
  Dubois`] (`2026-04-25`)
- [
  [`38d2b2a2`](https://github.com/sozu-proxy/sozu/commit/38d2b2a2451ee349397d06b07620aea5634621f0)
  ] test(e2e): stabilise drain helper + flood tests against peer-close races
  [`Florentin Dubois`] (`2026-04-25`)
- [
  [`903cd65b`](https://github.com/sozu-proxy/sozu/commit/903cd65bb4884221b6d4e4144384b425b876a142)
  ] test(e2e): un-ignore test_h2spec_conformance and tighten h2-security asserts
  [`Florentin Dubois`] (`2026-04-24`)
- [
  [`23f5f4f7`](https://github.com/sozu-proxy/sozu/commit/23f5f4f7be21d94165f7dfa761d497e279912f8a)
  ] test(e2e): customer-shape gzipped 7.7 MB H2 truncation regression guard
  [`Florentin Dubois`] (`2026-04-24`)
- [
  [`76063088`](https://github.com/sozu-proxy/sozu/commit/7606308809f66d81fa03868659574d2622a2a841)
  ] test(e2e): un-ignore fuzz targets, FIX-22 smoke, idle-timeout, 1 GB stress
  [`Florentin Dubois`] (`2026-04-24`)
- [
  [`4c81a9f1`](https://github.com/sozu-proxy/sozu/commit/4c81a9f1b17bf3e06745d1afb52952810145f04b)
  ] test(e2e): un-ignore H2 priority-rearm tests with real implementations
  [`Florentin Dubois`] (`2026-04-24`)
- [
  [`5e527073`](https://github.com/sozu-proxy/sozu/commit/5e527073ebc1f78f58faf8dea0a338131f065dcc)
  ] test(e2e): scaffolding for H2 priority rearm + peer-signal gaps [`Florentin
  Dubois`] (`2026-04-24`)
- [
  [`4a529dee`](https://github.com/sozu-proxy/sozu/commit/4a529dee5f0c5ad4bbc7334dd7fd2582c32d69aa)
  ] test(e2e): H2 body-stall repro — mixed-urgency incremental + HAR shape
  [`Florentin Dubois`] (`2026-04-24`)
- [
  [`235c1538`](https://github.com/sozu-proxy/sozu/commit/235c15383b1a47cd01df4ece897d346b8a01e776)
  ] test(e2e): H2 large-asset repro suite + ChunkedFlushH1Backend mock
  [`Florentin Dubois`] (`2026-04-23`)
- [
  [`6bb09d06`](https://github.com/sozu-proxy/sozu/commit/6bb09d06ecc0acec35c1e409d8225901edf1f2c3)
  ] test(e2e): RED-state test for solo RFC 9218 incremental H2 stream stall
  [`Florentin Dubois`] (`2026-04-23`)
- [
  [`3b843f19`](https://github.com/sozu-proxy/sozu/commit/3b843f1918fa95af026f69833880353385c94b4d)
  ] test(e2e): lock in H1 chunked-trailer forwarding through sozu (B3-p / #899)
  [`Florentin Dubois`] (`2026-04-22`)
- [
  [`b2ed12d6`](https://github.com/sozu-proxy/sozu/commit/b2ed12d611909bd2cf633402489a42596cf1e1ca)
  ] test(e2e): 30-iteration backend-disconnect stress verifies no stream-state
  leak [`Florentin Dubois`] (`2026-04-22`)
- [
  [`5b86c151`](https://github.com/sozu-proxy/sozu/commit/5b86c1518b480371a6043bf10976ff8dab9678da)
  ] test(e2e): raw H2 1xx informational-forwarding assertion [`Florentin
  Dubois`] (`2026-04-22`)
- [
  [`63057083`](https://github.com/sozu-proxy/sozu/commit/630570837a54b152b029d7d431ee95249c2bf2f3)
  ] test(h2): instrument ping-flood write path so future flakes are diagnosable
  [`Florentin Dubois`] (`2026-04-22`)
- [
  [`581ca982`](https://github.com/sozu-proxy/sozu/commit/581ca982d1b29f48409ef25ab6963e7f58f49681)
  ] test(h2): lock in response :status emptiness + absence rejection [`Florentin
  Dubois`] (`2026-04-22`)
- [
  [`5c020115`](https://github.com/sozu-proxy/sozu/commit/5c020115456c9f6d6c63d952f02e9e82a9bcb28e)
  ] test(e2e): assert H2-only listener rejects ALPN-absent clients [`Florentin
  Dubois`] (`2026-04-22`)
- [
  [`e7ff218b`](https://github.com/sozu-proxy/sozu/commit/e7ff218b7796cfcf2258797b7df5ac6d7c9f26bd)
  ] test(h2): instrument settings-flood write path so future flakes are
  diagnosable [`Florentin Dubois`] (`2026-04-20`)
- [
  [`77a65adc`](https://github.com/sozu-proxy/sozu/commit/77a65adcababc09accf9f446190b746025d18394)
  ] test(h2): reduce settings-flood iteration count to 1 to stop CI re-setup
  flake [`Florentin Dubois`] (`2026-04-20`)
- [
  [`d1ca708c`](https://github.com/sozu-proxy/sozu/commit/d1ca708c15240bf30baf8572680d31dec154de0e)
  ] test(e2e): loop-read four other single-read sites vulnerable to TCP
  segmentation [`Florentin Dubois`] (`2026-04-20`)
- [
  [`5311af3e`](https://github.com/sozu-proxy/sozu/commit/5311af3ee589bdf0b281957e48d82b6fc46f5445)
  ] test(e2e): loop-read PPV2 backend to absorb TCP segmentation [`Florentin
  Dubois`] (`2026-04-20`)
- [
  [`4ecd8507`](https://github.com/sozu-proxy/sozu/commit/4ecd85072d92938a7929733b02015e3fefca3d94)
  ] test(h2): widen settings-flood test read window for CI resilience
  [`Florentin Dubois`] (`2026-04-17`)
- [
  [`3939fc2e`](https://github.com/sozu-proxy/sozu/commit/3939fc2ee623b3ce2dfaf1f94cef754a047f8cf7)
  ] test(h2): distinguish graceful goaway [`Florentin Dubois`] (`2026-04-16`)
- [
  [`26c74df7`](https://github.com/sozu-proxy/sozu/commit/26c74df79472e94b585d5afbad44444295b546b9)
  ] test(e2e): align test_http_behaviors backend-closed assertion to 502
  [`Florentin Dubois`] (`2026-04-15`)
- [
  [`f609e08a`](https://github.com/sozu-proxy/sozu/commit/f609e08a27c83dcc95eaff2c133925bf7bbeda89)
  ] test(e2e): stream-id exhaustion graceful disconnect placeholder (FIX-22 /
  8afba086) [`Florentin Dubois`] (`2026-04-15`)
- [
  [`319b93bd`](https://github.com/sozu-proxy/sozu/commit/319b93bd3ca34761f048ebd3e60074bd50928136)
  ] test(e2e): client FIN vs RST half-close (FIX-21 / 350e05ac) [`Florentin
  Dubois`] (`2026-04-15`)
- [
  [`10e6ece0`](https://github.com/sozu-proxy/sozu/commit/10e6ece0de4d6369f08f542a99ffd26f343f1b02)
  ] test(e2e): HTTP upgrade backend-drop graceful (FIX-20 / 2e6b0ef0)
  [`Florentin Dubois`] (`2026-04-15`)
- [
  [`cc293884`](https://github.com/sozu-proxy/sozu/commit/cc293884de668f39451b7a5917c06110a800c4e1)
  ] test(e2e): stalled TLS peer starvation check (FIX-19 / 1f84f86e) [`Florentin
  Dubois`] (`2026-04-15`)
- [
  [`2ee3783f`](https://github.com/sozu-proxy/sozu/commit/2ee3783fa2c2b9857f8adebac7c164e07ffc8142)
  ] test(e2e): router-connect rollback leak check (FIX-18 / ba0f177c)
  [`Florentin Dubois`] (`2026-04-15`)
- [
  [`1ab59e5d`](https://github.com/sozu-proxy/sozu/commit/1ab59e5dd745b03df7e9069fa1a3c34c22b28faf)
  ] test(e2e): verify serializer masks reserved R-bit on outbound stream IDs
  [`Florentin Dubois`] (`2026-04-15`)
- [
  [`f20e7b7f`](https://github.com/sozu-proxy/sozu/commit/f20e7b7f995d7c4c4fde1d9ab9ebf74b6bc1e063)
  ] test(e2e): assert standalone CONTINUATION yields GOAWAY(PROTOCOL_ERROR)
  [`Florentin Dubois`] (`2026-04-15`)
- [
  [`fa1a84c7`](https://github.com/sozu-proxy/sozu/commit/fa1a84c794cf49666fa3be408cabe645727fd1af)
  ] test(e2e): assert PUSH_PROMISE from client yields GOAWAY(PROTOCOL_ERROR)
  [`Florentin Dubois`] (`2026-04-15`)
- [
  [`69742803`](https://github.com/sozu-proxy/sozu/commit/697428035b878bc8c64ffbc53396e6afca44da7b)
  ] test(e2e): cover unknown H2 frame types silently ignored [`Florentin
  Dubois`] (`2026-04-15`)
- [
  [`ccc4453f`](https://github.com/sozu-proxy/sozu/commit/ccc4453f6779ca2ce8dc5a548e1f338ea33b9160)
  ] test(e2e): cover GOAWAY length floor and strip_padding off-by-one
  [`Florentin Dubois`] (`2026-04-15`)
- [
  [`f10a5c1a`](https://github.com/sozu-proxy/sozu/commit/f10a5c1a6ced5ba97cbd0d5e6c564fb3ea6426b9)
  ] test(e2e): cover PADDED+PRIORITY HEADERS underflow regression (fuzz
  d7a34a0d) [`Florentin Dubois`] (`2026-04-15`)
- [
  [`5c098b03`](https://github.com/sozu-proxy/sozu/commit/5c098b03a9dcf38e539c3f92a38f3c92d052015c)
  ] test(e2e): add WINDOW_UPDATE-on-closed glitch flood E2E [`Florentin Dubois`]
  (`2026-04-15`)
- [
  [`d439413d`](https://github.com/sozu-proxy/sozu/commit/d439413d78faa5f89e8707b925f48f9e225e4196)
  ] test(e2e): add SETTINGS 64-entry cap E2E [`Florentin Dubois`] (`2026-04-15`)
- [
  [`72af0816`](https://github.com/sozu-proxy/sozu/commit/72af0816ab929c88a15fddfa3ee71232f414d3e5)
  ] test(e2e): add PRIORITY map cap adversarial E2E [`Florentin Dubois`]
  (`2026-04-15`)
- [
  [`43f96be0`](https://github.com/sozu-proxy/sozu/commit/43f96be0fcf9ebff05d28c476892411705d38e86)
  ] test(e2e): add per-stream idle timeout E2E (ignored) [`Florentin Dubois`]
  (`2026-04-15`)
- [
  [`4f687aca`](https://github.com/sozu-proxy/sozu/commit/4f687aca1964257f11b53b09e92c130c40a65074)
  ] test(e2e): add Rapid Reset abusive-lifetime E2E coverage [`Florentin
  Dubois`] (`2026-04-15`)
- [
  [`21791014`](https://github.com/sozu-proxy/sozu/commit/21791014019aea02604db85d0041d0b207c69299)
  ] test(e2e): SNI opt-out via strict_sni_binding=false [`Florentin Dubois`]
  (`2026-04-15`)
- [
  [`ce7edf91`](https://github.com/sozu-proxy/sozu/commit/ce7edf91ae0ab0cfc6a41ed47497cce55a089397)
  ] test(e2e): SNI plaintext listener skips the check [`Florentin Dubois`]
  (`2026-04-15`)
- [
  [`0f2cc68d`](https://github.com/sozu-proxy/sozu/commit/0f2cc68d74282db9f6c28401fde2bd8cf37376de)
  ] test(e2e): SNI wildcard :authority not expanded [`Florentin Dubois`]
  (`2026-04-15`)
- [
  [`1ebcf482`](https://github.com/sozu-proxy/sozu/commit/1ebcf482505ff88abe5ba435f81bd407aa140d18)
  ] test(e2e): SNI authority mismatch blocked with 421 + metric [`Florentin
  Dubois`] (`2026-04-15`)
- [
  [`0e439d3c`](https://github.com/sozu-proxy/sozu/commit/0e439d3c53d8be2f9a3e7f1f607b6d49b1d6ae1f)
  ] test(e2e): pseudo-header syntax enforcement — :scheme, :path,
  Content-Length, :status [`Florentin Dubois`] (`2026-04-15`)
- [
  [`c4a9f9cc`](https://github.com/sozu-proxy/sozu/commit/c4a9f9ccff2761b572540fcd5398296de1052eff)
  ] test(e2e): host/:authority reconciliation in H2→H1 downgrade [`Florentin
  Dubois`] (`2026-04-15`)
- [
  [`d6d7bb1a`](https://github.com/sozu-proxy/sozu/commit/d6d7bb1a9ec3fdf0a7b43624bd94f0b5a5fc808e)
  ] test(e2e): reject CRLF/CTL in H2 header and cookie values [`Florentin
  Dubois`] (`2026-04-15`)
- [
  [`75c3afc6`](https://github.com/sozu-proxy/sozu/commit/75c3afc65de0491878a276b358daac60d92240ee)
  ] test(e2e): add raw-byte H2 response mock backend for adversarial :status
  testing [`Florentin Dubois`] (`2026-04-15`)
- [
  [`f5e5246e`](https://github.com/sozu-proxy/sozu/commit/f5e5246e2c832769dea64e54000a78f069660136)
  ] test(e2e): add H2Frame::priority builder for raw PRIORITY frame tests
  [`Florentin Dubois`] (`2026-04-15`)
- [
  [`ff5063f3`](https://github.com/sozu-proxy/sozu/commit/ff5063f3326e16ad8ff138d57db48d28aaafcd81)
  ] test(e2e): parameterize SNI in raw_h2_connection helper [`Florentin Dubois`]
  (`2026-04-15`)
- [
  [`7e0c02ff`](https://github.com/sozu-proxy/sozu/commit/7e0c02ffcea0c9ec46baa175af1ac8a297e54ba9)
  ] test(assets): add multi-SAN certificate for SNI E2E tests [`Florentin
  Dubois`] (`2026-04-15`)
- [
  [`3627fa07`](https://github.com/sozu-proxy/sozu/commit/3627fa07d10d1c72c72cada07fca9148c6b91fef)
  ] test(mux): add cfg(test) hooks for router leak and stream-ID exhaustion
  scenarios [`Florentin Dubois`] (`2026-04-15`)
- [
  [`426b74e5`](https://github.com/sozu-proxy/sozu/commit/426b74e5da2601f8d00ff5c6a8a108460316ea19)
  ] test(converter): document H2→H1 trailer framing invariant [`Florentin
  Dubois`] (`2026-04-15`)
- [
  [`a00ecbf3`](https://github.com/sozu-proxy/sozu/commit/a00ecbf313866b49760ad23ef8cd0f4274a7dbdd)
  ] test(fuzz): seed regression input for padded HEADERS+PRIORITY underflow
  [`Florentin Dubois`] (`2026-04-15`)
- [
  [`2ccbf699`](https://github.com/sozu-proxy/sozu/commit/2ccbf69951e0a1b9796f6e128431f80bc5493183)
  ] test(parser): replace test unwraps with expect-with-context [`Florentin
  Dubois`] (`2026-04-15`)
- [
  [`c8181bd3`](https://github.com/sozu-proxy/sozu/commit/c8181bd3a2edd0e9eb433d7f909be9575fd4c2a6)
  ] test(e2e): cover padded DATA frame body and flow-control credit accounting
  [`Florentin Dubois`] (`2026-04-14`)
- [
  [`bb9ab958`](https://github.com/sozu-proxy/sozu/commit/bb9ab95802d0453cdc51dd4c266d9dcc13ea05ed)
  ] test(e2e): cover idle zombie metric accounting [`Florentin Dubois`]
  (`2026-04-13`)
- [
  [`b7b7f0ee`](https://github.com/sozu-proxy/sozu/commit/b7b7f0ee69a221693ffb9766eb9cbe1e5d58f6a6)
  ] test(fuzz): add e2e test harness for fuzz targets [`Florentin Dubois`]
  (`2026-03-24`)
- [
  [`7ccd8486`](https://github.com/sozu-proxy/sozu/commit/7ccd8486c6e45d7c9432d8d92f72e5061da6191a)
  ] test(e2e): stabilize listener allocation [`Florentin Dubois`] (`2026-03-23`)
- [
  [`b98cf976`](https://github.com/sozu-proxy/sozu/commit/b98cf976534f9dcea8dea50ff71e2e362e7f47f5)
  ] test(h2): join abrupt-close backend thread on stop [`Florentin Dubois`]
  (`2026-03-19`)
- [
  [`8e204189`](https://github.com/sozu-proxy/sozu/commit/8e2041894e7bdb6cff0ad561b990eeb6b98b4b9f)
  ] test(h2): join backend threads before retrying e2e cases [`Florentin
  Dubois`] (`2026-03-19`)
- [
  [`2bdf7aa1`](https://github.com/sozu-proxy/sozu/commit/2bdf7aa15f1f8f544c5a6123120f1d7a4ec5e77a)
  ] test(h2): add 8 MB and 1 GB large response E2E tests [`Florentin Dubois`]
  (`2026-03-17`)
- [
  [`9226feca`](https://github.com/sozu-proxy/sozu/commit/9226fecac06d9a53ce1c33015546641b5dae0e71)
  ] test(h2): add remaining E2E tests for translation, pool, disconnect, reload
  [`Florentin Dubois`] (`2026-03-16`)
- [
  [`c950942a`](https://github.com/sozu-proxy/sozu/commit/c950942a7339cc4fb7b72720e0e159a548955e93)
  ] test(h2): add h2spec conformance test and cargo-fuzz infrastructure
  [`Florentin Dubois`] (`2026-03-16`)
- [
  [`f5671c49`](https://github.com/sozu-proxy/sozu/commit/f5671c4969a3240b8325275c9acae270cada6287)
  ] test(h2): add kawa_h1 editor tests and remaining unit test coverage
  [`Florentin Dubois`] (`2026-03-16`)
- [
  [`6df2e0aa`](https://github.com/sozu-proxy/sozu/commit/6df2e0aa133cad07a4af55c54ac295db9b1bb6be)
  ] test(h2): add E2E tests for concurrent streams, disconnects, mixed traffic
  [`Florentin Dubois`] (`2026-03-16`)
- [
  [`21dc2a4d`](https://github.com/sozu-proxy/sozu/commit/21dc2a4dcede0122a2834d9dc1bffc14dc1a7caf)
  ] test(h2): add P0 security E2E tests for smuggling, desync, and floods
  [`Florentin Dubois`] (`2026-03-16`)
- [
  [`08fd90fd`](https://github.com/sozu-proxy/sozu/commit/08fd90fdad423611a804f411ccfc7589f78cb39a)
  ] test(h2): add E2E tests for protocol translation and connection pool
  [`Florentin Dubois`] (`2026-03-16`)
- [
  [`40d5fec9`](https://github.com/sozu-proxy/sozu/commit/40d5fec92fdcec962cd5b275e61044176024c58f)
  ] test(h2): add unit tests for converter, pkawa, flood detector, prioriser
  [`Florentin Dubois`] (`2026-03-16`)
- [
  [`d715b74d`](https://github.com/sozu-proxy/sozu/commit/d715b74dd80bfeaf058c89e360af9e2385a10f65)
  ] test(h2): add unit tests for serializer, parser edge cases, and pool
  [`Florentin Dubois`] (`2026-03-16`)
- [
  [`03709f81`](https://github.com/sozu-proxy/sozu/commit/03709f815e1fc2d499da4de11314ba703bced78f)
  ] test(e2e): extract H2 test utilities and create reusable helpers [`Florentin
  Dubois`] (`2026-03-16`)
- [
  [`f6041120`](https://github.com/sozu-proxy/sozu/commit/f604112076e7d86d65571537f181b9b34addd4c1)
  ] test(h2): add comprehensive e2e tests for flow control, GoAway, and stream
  independence [`Florentin Dubois`] (`2026-03-12`)
- [
  [`6b3d2ba6`](https://github.com/sozu-proxy/sozu/commit/6b3d2ba67cb1773ccbc9cabda1e8322b7bc59ab2)
  ] test(h2): add comprehensive e2e tests and refine protocol helpers
  [`Florentin Dubois`] (`2026-03-12`)
- [
  [`28a5d844`](https://github.com/sozu-proxy/sozu/commit/28a5d8441c24a881547052bdd1896de017b27e99)
  ] test(mux): add unit and e2e tests for metrics and byte tracking [`Florentin
  Dubois`] (`2026-03-12`)
- [
  [`aff61004`](https://github.com/sozu-proxy/sozu/commit/aff61004ed657828356a9bd86fc401c028d85dc6)
  ] test(h2): add e2e tests for H2 protocol correctness [`Florentin Dubois`]
  (`2026-03-10`)
- [
  [`55b8efda`](https://github.com/sozu-proxy/sozu/commit/55b8efda03374122bea3846941c4c89453bcfa99)
  ] test(e2e): add h2spec HTTP/2 conformance test integration [`Florentin
  Dubois`] (`2026-03-09`)
- [
  [`0100caf1`](https://github.com/sozu-proxy/sozu/commit/0100caf17a1d31028d7a146a32e440249a6deca7)
  ] test(e2e): add comprehensive HTTP/2 e2e tests [`Florentin Dubois`]
  (`2026-03-09`)

#### 🤖 Continuous Integration

- [
  [`7c3d9a4d`](https://github.com/sozu-proxy/sozu/commit/7c3d9a4d436e6d70e87f4c6cce3295c4f736e45f)
  ] ci(workflows): modernise GitHub Actions, add fmt + clippy + MSRV gates
  [`Florentin Dubois`] (`2026-04-26`)
- [
  [`0be2242f`](https://github.com/sozu-proxy/sozu/commit/0be2242f9fb4961790bd45d4bf7307939b2a6c55)
  ] ci: install nightly + cargo-fuzz so fuzz targets run on every matrix job
  [`Florentin Dubois`] (`2026-04-24`)
- [
  [`86c54b0e`](https://github.com/sozu-proxy/sozu/commit/86c54b0e4a6877d71860ec6a04769b7e570c5958)
  ] ci(benchmark): fix rsa-2048 certificate hostname [`Florentin Dubois`]
  (`2026-04-15`)

#### 🚀 Refactoring & style

- [
  [`c36792a6`](https://github.com/sozu-proxy/sozu/commit/c36792a6d160082aa989fd8c41bb1a7797042c18)
  ] style: cargo +nightly fmt --all on consolidated review changes [`Florentin
  Dubois`] (`2026-04-25`)
- [
  [`c5cfe3d7`](https://github.com/sozu-proxy/sozu/commit/c5cfe3d77c065febe60694751934ee90d22b8d76)
  ] style(kawa_h1): wrap remaining 4 log sites and recognise .log_context()
  method form [`Florentin Dubois`] (`2026-04-25`)
- [
  [`73f59cb7`](https://github.com/sozu-proxy/sozu/commit/73f59cb7b7d9a31eea46605f49a3e0982e399c67)
  ] style(tcp): wrap remaining 17 log sites in TCP log_context! envelope
  [`Florentin Dubois`] (`2026-04-25`)
- [
  [`61f60266`](https://github.com/sozu-proxy/sozu/commit/61f6026687fc5e95ba4afdc8a06dc55e56c37718)
  ] style(mux): wrap 4 remaining log sites in MUX/MUX-H2 envelope and make the
  regression guard bidirectional [`Florentin Dubois`] (`2026-04-25`)
- [
  [`0da568ca`](https://github.com/sozu-proxy/sozu/commit/0da568ca7e3819ec35ba089d48abe35d0582d42d)
  ] style(https): introduce HTTPS module + session log macros and wrap callers
  [`Florentin Dubois`] (`2026-04-25`)
- [
  [`7f5b6cb1`](https://github.com/sozu-proxy/sozu/commit/7f5b6cb1b4e8f879fcb8e58c1659c444ce23202e)
  ] style(http): introduce HTTP module + session log macros and wrap callers
  [`Florentin Dubois`] (`2026-04-25`)
- [
  [`ae74d6ca`](https://github.com/sozu-proxy/sozu/commit/ae74d6ca26bc900777e2ab836b2ec6a047686325)
  ] style(tls): introduce TLS-RESOLVER tag and wrap callers [`Florentin Dubois`]
  (`2026-04-25`)
- [
  [`306542ce`](https://github.com/sozu-proxy/sozu/commit/306542ce928e9914fabc78ac28da6773e5056faf)
  ] style(proxy_protocol): introduce PROXY-EXPECT/PROXY-RELAY/PROXY-SEND tags
  and wrap callers [`Florentin Dubois`] (`2026-04-25`)
- [
  [`dcddbc9b`](https://github.com/sozu-proxy/sozu/commit/dcddbc9b52f8e45c244860e5b78c17b2409effda)
  ] style(pipe): prefix three trace! sites with PIPE log_context! [`Florentin
  Dubois`] (`2026-04-25`)
- [
  [`192b2a13`](https://github.com/sozu-proxy/sozu/commit/192b2a1319b491b012610c49f78fd4c555d496d9)
  ] chore(review): address Copilot PR #1209 review comments [`Florentin Dubois`]
  (`2026-04-25`)
- [
  [`c99cd158`](https://github.com/sozu-proxy/sozu/commit/c99cd1584b19699119b1f68ffbe28624db29f234)
  ] style(e2e): rustfmt import layout drift in h2_security tests [`Florentin
  Dubois`] (`2026-04-24`)
- [
  [`54c0d3ff`](https://github.com/sozu-proxy/sozu/commit/54c0d3ff62c19c32b3b1c948db5dd4cb084d2b45)
  ] style(e2e): rustfmt single_read_h1_backend wrap-break [`Florentin Dubois`]
  (`2026-04-24`)
- [
  [`b1f88c40`](https://github.com/sozu-proxy/sozu/commit/b1f88c40a2e2b5c0d0156bdc9944466d4d252c77)
  ] chore(h2): document negotiated max_frame_size caller contract [`Florentin
  Dubois`] (`2026-04-22`)
- [
  [`11cf08b3`](https://github.com/sozu-proxy/sozu/commit/11cf08b39bcf193d19f117fa7b454b7005c14091)
  ] style(e2e): apply nightly fmt to ping-flood diagnostic [`Florentin Dubois`]
  (`2026-04-22`)
- [
  [`e732e560`](https://github.com/sozu-proxy/sozu/commit/e732e560f6e4e5037f400ab4bd256ba44918a02a)
  ] chore(h2): document remove_dead_stream cache-eviction invariant [`Florentin
  Dubois`] (`2026-04-22`)
- [
  [`4f8815bd`](https://github.com/sozu-proxy/sozu/commit/4f8815bdccfbd78074ae8f7256ac2b5f6dad7c04)
  ] chore(https): document rustls-0.23 no-renegotiation assumption in
  strict_sni_binding [`Florentin Dubois`] (`2026-04-22`)
- [
  [`ae314385`](https://github.com/sozu-proxy/sozu/commit/ae3143850b3fd6c46c03a1a1682ceea29dcf4ab8)
  ] chore(h2): document RFC 9218 §7.1 clamp-vs-PROTOCOL_ERROR policy [`Florentin
  Dubois`] (`2026-04-22`)
- [
  [`7ea44d8a`](https://github.com/sozu-proxy/sozu/commit/7ea44d8a20f162dfb23297c180e4df2a5c2abe76)
  ] chore: cargo fmt + clippy clean-up over the second telemetry batch
  [`Florentin Dubois`] (`2026-04-21`)
- [
  [`f056e215`](https://github.com/sozu-proxy/sozu/commit/f056e2156d3bd3dd7ca472543510c58590d1498f)
  ] style: cargo +nightly fmt --all across telemetry commits [`Florentin
  Dubois`] (`2026-04-21`)
- [
  [`f9f21c3f`](https://github.com/sozu-proxy/sozu/commit/f9f21c3f69c4b7dec835ac583a14e4d65ca48d9c)
  ] chore(h2): tier send/receive GOAWAY log severity by H2Error variant
  [`Florentin Dubois`] (`2026-04-21`)
- [
  [`c66d3151`](https://github.com/sozu-proxy/sozu/commit/c66d31514255ca1860b5c5fb1003827afe0867d7)
  ] chore(socket): tier read/write fallthrough log severity by ErrorKind
  [`Florentin Dubois`] (`2026-04-21`)
- [
  [`b089e9dd`](https://github.com/sozu-proxy/sozu/commit/b089e9dd3577d470a864a18149fba54bc20667e6)
  ] chore(rustls): tier TLS handshake errors by severity [`Florentin Dubois`]
  (`2026-04-20`)
- [
  [`7713860a`](https://github.com/sozu-proxy/sozu/commit/7713860abe36b7a1e7179611bde1e23fc49fe02f)
  ] chore(pipe): demote idle-timeout tear-downs from error! to debug!
  [`Florentin Dubois`] (`2026-04-20`)
- [
  [`0d15adfe`](https://github.com/sozu-proxy/sozu/commit/0d15adfe43eeb74d48bb7da5501dda4346d20bbc)
  ] style(logging): align SOCKET prefix column order with MUX/PIPE/RUSTLS
  [`Florentin Dubois`] (`2026-04-20`)
- [
  [`659e1407`](https://github.com/sozu-proxy/sozu/commit/659e140796a115974f3a424ce649e78189fa9fba)
  ] style(logging): align log-context columns and unify tag colors [`Florentin
  Dubois`] (`2026-04-20`)
- [
  [`6cbe82d1`](https://github.com/sozu-proxy/sozu/commit/6cbe82d1dbbbb060438a7af450904a2ec1165717)
  ] style(logging): align log-context columns and unify tag colors [`Florentin
  Dubois`] (`2026-04-17`)
- [
  [`723042bc`](https://github.com/sozu-proxy/sozu/commit/723042bc0989103008ab893230a99b21d9d29585)
  ] chore(deps): bump workspace dependencies to latest + sha2 0.10→0.11
  [`Florentin Dubois`] (`2026-04-17`)
- [
  [`643cce25`](https://github.com/sozu-proxy/sozu/commit/643cce25b001a452ef74daade057a4fedd13a5cc)
  ] chore(bin): downgrade MessageClient response logs from info to debug
  [`Florentin Dubois`] (`2026-04-17`)
- [
  [`e89acc60`](https://github.com/sozu-proxy/sozu/commit/e89acc6036e6d405806680fde6a3bbce7fe27f33)
  ] style(mux): move tests module to bottom of mod.rs [`Florentin Dubois`]
  (`2026-04-15`)
- [
  [`e3b8fb4d`](https://github.com/sozu-proxy/sozu/commit/e3b8fb4dc2db6281d8e379a5a5f946e66ca12c15)
  ] style(mux): apply nightly rustfmt [`Florentin Dubois`] (`2026-04-15`)
- [
  [`fcd24ae1`](https://github.com/sozu-proxy/sozu/commit/fcd24ae1449408a7e278540f18b5b9c0f9ca8aec)
  ] chore(mux): register shared helper module [`Florentin Dubois`]
  (`2026-04-14`)
- [
  [`809a9a0d`](https://github.com/sozu-proxy/sozu/commit/809a9a0d1809c548d397ffa6dbbae3ea11b4e88a)
  ] chore(h2-mux): document branch state and fix quality gates [`Florentin
  Dubois`] (`2026-03-24`)
- [
  [`49ae3465`](https://github.com/sozu-proxy/sozu/commit/49ae346547c0cb40e0bbee40809f7fe9377f2c5c)
  ] style: apply cargo fmt to CLI, config, and HTTPS modules [`Florentin
  Dubois`] (`2026-03-11`)
- [
  [`2dc4b4bd`](https://github.com/sozu-proxy/sozu/commit/2dc4b4bd884ea994221409b1be74eb549bdee2b3)
  ] style(mux): apply nightly rustfmt formatting [`Florentin Dubois`]
  (`2026-03-09`)

#### 📚 Documentation (commit log)

- [
  [`1bd70159`](https://github.com/sozu-proxy/sozu/commit/1bd701591135e8a75c7d1b31f65b3e9f45e11d62)
  ] docs(changelog): record documentation + CI pass [`Florentin Dubois`]
  (`2026-04-26`)
- [
  [`865dae18`](https://github.com/sozu-proxy/sozu/commit/865dae18f3e0a63bd597b916ec1737d9fa2fc12c)
  ] docs(narrative): full rewrite of doc/lifetime_of_a_session.md [`Florentin
  Dubois`] (`2026-04-26`)
- [
  [`93846fcc`](https://github.com/sozu-proxy/sozu/commit/93846fcc9b7b247ea2c75657ac02fe16e0bb7289)
  ] docs(narrative): new LIFECYCLE.md siblings + admin-ops doc + fuzz README
  [`Florentin Dubois`] (`2026-04-26`)
- [
  [`60683f13`](https://github.com/sozu-proxy/sozu/commit/60683f13a67400bea5c60350d481bee018ebae45)
  ] docs(in-source): comment-only sozuctl → sozu rename [`Florentin Dubois`]
  (`2026-04-26`)
- [
  [`e2c643b4`](https://github.com/sozu-proxy/sozu/commit/e2c643b4c1e1a1237c404e99386465930d4db575)
  ] docs(in-source): // SAFETY: annotations on existing unsafe blocks
  [`Florentin Dubois`] (`2026-04-26`)
- [
  [`baf8cf99`](https://github.com/sozu-proxy/sozu/commit/baf8cf990e7feeb6b3203bac8fa2f3c2027e8430)
  ] docs(in-source): module-doc headers + invariant comments [`Florentin
  Dubois`] (`2026-04-26`)
- [
  [`c2528de1`](https://github.com/sozu-proxy/sozu/commit/c2528de1620074398973d31f2233d829a4dca828)
  ] docs(index): refresh doc/README.md [`Florentin Dubois`] (`2026-04-26`)
- [
  [`a1910069`](https://github.com/sozu-proxy/sozu/commit/a1910069f17a1013ccc881dff94b38981d1fcf09)
  ] docs(architecture): refresh mux line counts, retire stale permalinks
  [`Florentin Dubois`] (`2026-04-26`)
- [
  [`97d7beaf`](https://github.com/sozu-proxy/sozu/commit/97d7beaf064faf80ff380e227ba5e47efb12d1d0)
  ] docs(observability): retire sozuctl phrasing in prose AND examples
  [`Florentin Dubois`] (`2026-04-26`)
- [
  [`b7267f8f`](https://github.com/sozu-proxy/sozu/commit/b7267f8fd461173b397fa5d234eaaeb0bf16e0fc)
  ] docs(getting-started): drop fictional crypto-providers section [`Florentin
  Dubois`] (`2026-04-26`)
- [
  [`6d706371`](https://github.com/sozu-proxy/sozu/commit/6d706371b5a2499b8384ee1c007eee936dce1868)
  ] docs(sub-readmes): refresh lib/bin/command/e2e READMEs [`Florentin Dubois`]
  (`2026-04-26`)
- [
  [`a77d0990`](https://github.com/sozu-proxy/sozu/commit/a77d0990b68f837628c11fbfd806260c5c4f1b78)
  ] docs(changelog): regenerate post-1.1.1 unreleased section + footer
  [`Florentin Dubois`] (`2026-04-26`)
- [
  [`d61a25ff`](https://github.com/sozu-proxy/sozu/commit/d61a25ff7f884fe9d10b4468180fe8c4ec91d4a1)
  ] docs(release,contributing): retire sozuctl, master/Travis references
  [`Florentin Dubois`] (`2026-04-26`)
- [
  [`88582b89`](https://github.com/sozu-proxy/sozu/commit/88582b890dac3922b77c31e85cdd196aa0af9701)
  ] docs(readme): drop dead Travis/Gitter badges, refresh source map [`Florentin
  Dubois`] (`2026-04-26`)
- [
  [`93605b4c`](https://github.com/sozu-proxy/sozu/commit/93605b4c343fe9146721eb10fcb5f348e2f878ee)
  ] docs(changelog,configure): document deferred-batch fixes + clippy nit
  [`Florentin Dubois`] (`2026-04-25`)
- [
  [`0a9b59c2`](https://github.com/sozu-proxy/sozu/commit/0a9b59c2b15f28a2d9c4eabd8805c6a83c9daf8a)
  ] docs(changelog): add P2/P3 fix entries from consolidated review [`Florentin
  Dubois`] (`2026-04-25`)
- [
  [`edd42ace`](https://github.com/sozu-proxy/sozu/commit/edd42ace0b54b946063b1691865ac188e7b27632)
  ] docs(mux): refresh LIFECYCLE.md line citations against bcd977f1 HEAD
  [`Florentin Dubois`] (`2026-04-25`)
- [
  [`13f88ae6`](https://github.com/sozu-proxy/sozu/commit/13f88ae65ab7c4a8df530ac966da4bee4b0d0e32)
  ] docs(configure): slab_entries_per_connection knob entry [`Florentin Dubois`]
  (`2026-04-25`)
- [
  [`7b0ec52e`](https://github.com/sozu-proxy/sozu/commit/7b0ec52ef02161afa08d85d012a75ee68f4959b4)
  ] docs: document Shutdown::Both safety on three TCP-only sites [`Florentin
  Dubois`] (`2026-04-25`)
- [
  [`7dc512c1`](https://github.com/sozu-proxy/sozu/commit/7dc512c13351da85fd84656be299e6a52abcec89)
  ] docs: document 4 emitted-but-undocumented metrics + CHANGELOG cleanup
  [`Florentin Dubois`] (`2026-04-25`)
- [
  [`80784d8a`](https://github.com/sozu-proxy/sozu/commit/80784d8af1b1617a1ea86b0668a10e096f7c3d6b)
  ] docs: add close-path log severity tiers section, refresh CLAUDE.md tag list,
  CHANGELOG entries [`Florentin Dubois`] (`2026-04-25`)
- [
  [`5b6bdc91`](https://github.com/sozu-proxy/sozu/commit/5b6bdc914f5d3dc83d26ad195822bebf50e0c780)
  ] docs(h2,ci): refresh LIFECYCLE RST path, CHANGELOG entry, install h2spec in
  CI [`Florentin Dubois`] (`2026-04-24`)
- [
  [`2b88cb9b`](https://github.com/sozu-proxy/sozu/commit/2b88cb9bf6fbf9368219a24aec86be42fef2d357)
  ] docs(configure): document h2.signal.writable.rearmed.\* and
  ready_incremental gauge [`Florentin Dubois`] (`2026-04-24`)
- [
  [`774120d1`](https://github.com/sozu-proxy/sozu/commit/774120d1623cf8a753447fe64eb8d649dbeef293)
  ] docs(e2e): correct stale comments in h2_correctness_tests [`Florentin
  Dubois`] (`2026-04-24`)
- [
  [`20129e76`](https://github.com/sozu-proxy/sozu/commit/20129e7635fae4b1d0abea0c58aab6ffa76008d6)
  ] docs(tasks): B3-p shipped — H1 chunked trailers already worked end-to-end
  [`Florentin Dubois`] (`2026-04-22`)
- [
  [`7a2072a5`](https://github.com/sozu-proxy/sozu/commit/7a2072a55c4b7ab9ec0aad6524cd024c0133a8ef)
  ] docs(tasks): update audit-findings — B3-j shipped, B3-x design, B3-p
  upstream [`Florentin Dubois`] (`2026-04-22`)
- [
  [`21ba7bcb`](https://github.com/sozu-proxy/sozu/commit/21ba7bcbe9309fbd61c54b3d1f5b75fc819a06da)
  ] docs(rate-limit): design for per-tenant 429 + per-source-IP cap (B3-x)
  [`Florentin Dubois`] (`2026-04-22`)
- [
  [`3b98915d`](https://github.com/sozu-proxy/sozu/commit/3b98915d10f74a270e18964f85ab3eb218107c6e)
  ] docs(tasks): record audit findings for B3-d / B3-o / B3-q / B3-f / B3-g
  [`Florentin Dubois`] (`2026-04-22`)
- [
  [`234caee0`](https://github.com/sozu-proxy/sozu/commit/234caee0a03f1d6e93b9ab7aea9cea5597fde08f)
  ] docs(tasks): record post-merge audit findings for B3-c / B3-v / B3-w
  [`Florentin Dubois`] (`2026-04-22`)
- [
  [`10240ab2`](https://github.com/sozu-proxy/sozu/commit/10240ab2de6c66a834276b7fef0585c1019d8d13)
  ] docs(configure): classify TLS handshake log severities [`Florentin Dubois`]
  (`2026-04-22`)
- [
  [`529b188e`](https://github.com/sozu-proxy/sozu/commit/529b188e7bb7cdc9d63aa1b625aa0db4275cc3aa)
  ] docs(ci): document nightly matrix-allowed-to-fail cause [`Florentin Dubois`]
  (`2026-04-22`)
- [
  [`733e241d`](https://github.com/sozu-proxy/sozu/commit/733e241d61d25eb97f17ab59b9085ab8299e70bc)
  ] docs(cluster): clarify http2 flag is a backend-capability hint [`Florentin
  Dubois`] (`2026-04-22`)
- [
  [`83790e3c`](https://github.com/sozu-proxy/sozu/commit/83790e3c22b586ad6d93e51503f0baf7f24211a8)
  ] docs(h2): document h2_max_glitch_count tuning for busy production edges
  [`Florentin Dubois`] (`2026-04-22`)
- [
  [`15012baf`](https://github.com/sozu-proxy/sozu/commit/15012baf7601274d639e05f94c9946578b699685)
  ] docs(observability): add architecture overview + extension-points guide
  [`Florentin Dubois`] (`2026-04-21`)
- [
  [`04610f43`](https://github.com/sozu-proxy/sozu/commit/04610f43ea2402d933b944da73e3215e94d9c8bb)
  ] docs(changelog): record telemetry & observability work [`Florentin Dubois`]
  (`2026-04-21`)
- [
  [`e82142d8`](https://github.com/sozu-proxy/sozu/commit/e82142d82218ebf9bc8050eb012dcd4a93e72f6a)
  ] docs(configure): document 2 emitted-but-undocumented metrics, fix
  OpenTelemetry scope [`Florentin Dubois`] (`2026-04-21`)
- [
  [`ae643218`](https://github.com/sozu-proxy/sozu/commit/ae6432183c237da2cd914242c102a1453ccbb9e0)
  ] docs(socket): dedupe log-prefix slot descriptions, broaden configured_peer
  scope [`Florentin Dubois`] (`2026-04-21`)
- [
  [`39b29837`](https://github.com/sozu-proxy/sozu/commit/39b29837b8cae6b46e50a97c1766395176e20401)
  ] docs(claude): add MUX-ROUTER prefix tag, reference
  HttpContext::log_context() [`Florentin Dubois`] (`2026-04-21`)
- [
  [`2a1c8acf`](https://github.com/sozu-proxy/sozu/commit/2a1c8acf1bd0e164860f59ec783138747cd5fd48)
  ] docs: add CLAUDE.md + AGENTS.md with agent instructions [`Florentin Dubois`]
  (`2026-04-20`)
- [
  [`c9724f61`](https://github.com/sozu-proxy/sozu/commit/c9724f616b874eaaa8c7499e157fd87be1ba921d)
  ] docs(config): document 8 undocumented global keys and add missing examples
  [`Florentin Dubois`] (`2026-04-17`)
- [
  [`e0a2dc01`](https://github.com/sozu-proxy/sozu/commit/e0a2dc0195dedb9759777a4ba7dea0f00b6beb1f)
  ] docs(socket,http): clarify vectored empty-flush invariant and h2c
  non-support [`Florentin Dubois`] (`2026-04-15`)
- [
  [`868fc227`](https://github.com/sozu-proxy/sozu/commit/868fc227bd1459a8bbaf2e63b03176fdd8f880c0)
  ] docs(metrics): comprehensive metrics, OpenTelemetry, and traces reference
  [`Florentin Dubois`] (`2026-03-16`)
- [
  [`cecfa25f`](https://github.com/sozu-proxy/sozu/commit/cecfa25f711bb0276b8872707b65b8658f3cc9df)
  ] docs(changelog): add round 5 entries for 1xx, proxy protocol, and review
  fixes [`Florentin Dubois`] (`2026-03-16`)
- [
  [`b923f9f1`](https://github.com/sozu-proxy/sozu/commit/b923f9f17e93c80f9946706c01701e40533d5875)
  ] docs(h2): comprehensive documentation for H2 Mux internals and round 3-4
  features [`Florentin Dubois`] (`2026-03-14`)
- [
  [`0499d5a4`](https://github.com/sozu-proxy/sozu/commit/0499d5a48113f9bef402acb79c8977a077f74c87)
  ] docs: document ALPN behavior and add ALPN metrics to metrics reference
  [`Florentin Dubois`] (`2026-03-10`)
- [
  [`a6c6c7b7`](https://github.com/sozu-proxy/sozu/commit/a6c6c7b72e65a56daffbb090084f9392529710e0)
  ] docs: add HTTP/2 configuration guide to getting_started and configure
  [`Florentin Dubois`] (`2026-03-10`)
- [
  [`33ad077c`](https://github.com/sozu-proxy/sozu/commit/33ad077c6f52c0aeae44b559242177d6162d30c6)
  ] docs(h2): add architecture diagrams for the Mux/H2 implementation
  [`Florentin Dubois`] (`2026-03-10`)
- [
  [`f05d3265`](https://github.com/sozu-proxy/sozu/commit/f05d326539b04ea6aeafa2b81cafeb0ad0accac6)
  ] docs(h2): add Mux/H2 architecture section and update stale protocol links
  [`Florentin Dubois`] (`2026-03-09`)

**Full Changelog**:
https://github.com/sozu-proxy/sozu/compare/main...feat/h2-mux

## 1.1.1 - 2025-11-03

### ⛑️ Fixed

- We fixed a critical file descriptor handling bug introduced in v1.1.0. The
  Rust Edition 2024 migration incorrectly used `OwnedFd` instead of `BorrowedFd`
  in the `enable_close_on_exec()` and `disable_close_on_exec()` functions,
  causing file descriptors to be closed prematurely and breaking Sōzu's
  operation. This has been corrected by using `BorrowedFd::borrow_raw()` which
  only borrows the file descriptor without taking ownership, see
  [PR #1181](https://github.com/sozu-proxy/sozu/pull/1181),
  [`fd68885a`](https://github.com/sozu-proxy/sozu/commit/fd68885a1df16e739a2cf2e104e6d09c1da23902).

### ✍️ Changed

- We fixed the RPM spec file to enable successful builds on COPR, EPEL 10, and
  Fedora 42+. Changes include: adding missing build dependencies (rust, cargo,
  protobuf-compiler, gcc), fixing changelog date formatting, improving FHS
  compliance with proper use of `_rundir` and `_sharedstatedir` macros,
  simplifying the build mode logic, and resolving SELinux policy installation
  issues, see [PR #1182](https://github.com/sozu-proxy/sozu/pull/1182).
- We applied clippy suggestions for code quality improvements in request
  builder, display modules, and backend error handling, see
  [`28fff561`](https://github.com/sozu-proxy/sozu/commit/28fff561e45a9acbd139e0c0ee3863c91046d4b5).

### Changelog

#### ⛑️ Fixed

- [
  [`fd68885a`](https://github.com/sozu-proxy/sozu/commit/fd68885a1df16e739a2cf2e104e6d09c1da23902)
  ] Change OwnedFd to BorrowedFd (solve file close) [`Eloi DEMOLIS`]
  (`2025-10-31`)

#### ✍️ Changed

- [
  [`28fff561`](https://github.com/sozu-proxy/sozu/commit/28fff561e45a9acbd139e0c0ee3863c91046d4b5)
  ] Apply clippy suggestions [`Eloi DEMOLIS`] (`2025-10-31`)
- [
  [`32efc7dd`](https://github.com/sozu-proxy/sozu/commit/32efc7dd5f2a366062204bf112d9291bd2f0806e)
  ] fix dates in rpm spec [`Ante de Baas`] (`2025-10-31`)
- [
  [`a4c87bd2`](https://github.com/sozu-proxy/sozu/commit/a4c87bd25c1e42814bfda8e76fadb419a10373ba)
  ] Add cargo and update rust build requirements in spec file [`Ante de Baas`]
  (`2025-10-31`)
- [
  [`7c89a137`](https://github.com/sozu-proxy/sozu/commit/7c89a13760a176c1496d55813be02e10a85a7a00)
  ] Add protobuf-compiler and gcc to BuildRequires [`Ante de Baas`]
  (`2025-10-31`)
- [
  [`eae22d4f`](https://github.com/sozu-proxy/sozu/commit/eae22d4f2fd86772a373f80af1899967e3320316)
  ] Refactor sozu.spec for simplified build and improved packaging [`Ante de
  Baas`] (`2025-10-31`)
- [
  [`afc25353`](https://github.com/sozu-proxy/sozu/commit/afc2535396a61145f5e3b6d09c1ee9fbea37ae12)
  ] Update sozu.spec to use \_rundir and \_sharedstatedir paths [`Ante de Baas`]
  (`2025-10-31`)
- [
  [`7f92fff8`](https://github.com/sozu-proxy/sozu/commit/7f92fff82f6a44503a8d20639f22869d5c55e005)
  ] Remove Broken SELinux policy build and install steps from spec file [`Ante
  de Baas`] (`2025-10-31`)
- [
  [`d8183052`](https://github.com/sozu-proxy/sozu/commit/d8183052d7781abd6e34723bcae39c3a052a19c9)
  ] remove postun too [`Ante de Baas`] (`2025-10-31`)
- [
  [`1d46d4fd`](https://github.com/sozu-proxy/sozu/commit/1d46d4fd0cfb4dfc36ced7f74e36da731f799ace)
  ] re-add selinux policy [`Ante de Baas`] (`2025-10-31`)

### 🥹 Contributors

@Wonshtrum, @FlorentinDUBOIS, @antedebaas

**Full Changelog**: https://github.com/sozu-proxy/sozu/compare/1.1.0...1.1.1

## 1.1.0 - 2025-10-29

> **⚠️ BREAKING CHANGE**: This release requires **Rust 1.85.0** or later
> (previously 1.80.0) due to the migration to Rust Edition 2024. Please ensure
> your toolchain is updated before upgrading.

### 🌟 Features

- **OpenTelemetry Support**: We have added comprehensive OpenTelemetry
  integration for distributed tracing and observability. Sōzu now supports
  `traceparent` and `tracestate` headers in access logs, enabling seamless
  integration with OpenTelemetry-compatible monitoring systems. This
  significantly improves the ability to trace requests across distributed
  systems, see [PR #1176](https://github.com/sozu-proxy/sozu/pull/1176),
  [`7035eebb`](https://github.com/sozu-proxy/sozu/commit/7035eebb72d7a6a9ad17540d4a24f8de59dea669),
  [`f72bb977`](https://github.com/sozu-proxy/sozu/commit/f72bb977dfd62403f30a3f4e2bb2e048cd8b6510),
  [`fbce7f65`](https://github.com/sozu-proxy/sozu/commit/fbce7f65c7c404036c1dd90a3a70c41174581669).

- **Sōzu as a Library**: The bin crate has been restructured to allow Sōzu to be
  used as a library in other Rust packages. This enables embedding Sōzu's
  reverse proxy functionality directly into your applications, see
  [PR #1165](https://github.com/sozu-proxy/sozu/pull/1165),
  [`3f5f63fa`](https://github.com/sozu-proxy/sozu/commit/3f5f63fae7de9955589aa3e7cadbf26147f56230).

- **Cross-platform Improvements**: We have improved build support for non-x86_64
  architectures, particularly MacOS ARM. Kawa's SIMD features are now properly
  disabled on non-x86_64 platforms, resolving build issues on ARM architectures,
  see [PR #1136](https://github.com/sozu-proxy/sozu/pull/1136),
  [`2f478d08`](https://github.com/sozu-proxy/sozu/commit/2f478d0862f9f277a8a2fd89cd09bd71abaa5dd3).

### ⛑️ Fixed

- We fixed an issue with incorrect `backend_response_time` metric calculation on
  keep-alive requests, see
  [`5f3371b7`](https://github.com/sozu-proxy/sozu/commit/5f3371b7775dd185f77ed5c3313876acd11290fc).
- We fixed improper quoting in the `forwarded` header field, see
  [PR #1175](https://github.com/sozu-proxy/sozu/pull/1175),
  [`3c660faf`](https://github.com/sozu-proxy/sozu/commit/3c660faf2d955bcb6dd5326770f56991469b11e9).
- We fixed path resolution issues when saving state, ensuring relative paths are
  properly handled, see
  [PR #1171](https://github.com/sozu-proxy/sozu/pull/1171),
  [`a7ecc65a`](https://github.com/sozu-proxy/sozu/commit/a7ecc65a978e9a925d868a66930e82cff2478f99).
- We fixed security vulnerability RUSTSEC-2024-0402, see
  [PR #1168](https://github.com/sozu-proxy/sozu/pull/1168),
  [`9d1fbbe9`](https://github.com/sozu-proxy/sozu/commit/9d1fbbe9fdcfccf399c37ce0feb1052871827198).
- We fixed the Dockerfile build process, see
  [PR #1163](https://github.com/sozu-proxy/sozu/pull/1163),
  [`93a4ea43`](https://github.com/sozu-proxy/sozu/commit/93a4ea4372bf0d4a51e2cbc1eb0fdae9fadb428c).
- We fixed CI issues with bombardier by updating the Go version, see
  [PR #1172](https://github.com/sozu-proxy/sozu/pull/1172),
  [`a1138973`](https://github.com/sozu-proxy/sozu/commit/a1138973538ec80a5bcf890e3cec79eaa65893d6).

### ✍️ Changed

- **Rust Edition 2024 Migration**: The entire workspace has been migrated to
  Rust Edition 2024, requiring Rust 1.85.0 or later. This migration includes
  automatic formatting updates, import reorganization, added unsafe blocks for
  environment variable access, modernized macro syntax, and applied clippy
  suggestions, see [PR #1179](https://github.com/sozu-proxy/sozu/pull/1179),
  [`6650be58`](https://github.com/sozu-proxy/sozu/commit/6650be58c9b6067529599ba54fea866ba52c6575).
- We updated multiple dependencies to their latest versions, including general
  dependency bumps and Cargo.lock updates, see
  [PR #1177](https://github.com/sozu-proxy/sozu/pull/1177),
  [`f96444e9`](https://github.com/sozu-proxy/sozu/commit/f96444e9aa5f2f9fcd58252f98a421feaa65cf59),
  [`703a3df3`](https://github.com/sozu-proxy/sozu/commit/703a3df3f01e2407c644c8be619a2115a328cc2a),
  [`e678ebc2`](https://github.com/sozu-proxy/sozu/commit/e678ebc22851ea1bf809c006fd9582b217ea951f).
- We avoided duplicate crates in the dependency tree to improve build times and
  binary size, see [PR #1170](https://github.com/sozu-proxy/sozu/pull/1170),
  [`e3f81b46`](https://github.com/sozu-proxy/sozu/commit/e3f81b4603cfe860f00a0149a58d17a896920a7d).
- We removed the double definition of the global allocator in the lib crate, see
  [`6e3e7e12`](https://github.com/sozu-proxy/sozu/commit/6e3e7e12343838c9f6c5d2a3c9ee893711446455).
- We improved CI/CD by configuring Docker build and push on every commit, see
  [PR #1166](https://github.com/sozu-proxy/sozu/pull/1166),
  [`a77c928f`](https://github.com/sozu-proxy/sozu/commit/a77c928fc7300cfbed76bf4d07dba9de13ea3dc6).
- We removed coverage upload from CI, see
  [`b9f0e52d`](https://github.com/sozu-proxy/sozu/commit/b9f0e52d2147d6771d85da36de078e5ad55fed4c).

### ➖ Removed

- We removed @keksoj from CODEOWNERS, see
  [`ebdda472`](https://github.com/sozu-proxy/sozu/commit/ebdda472fadf99b1afea797d7859a2ede6505b50).

### 📚 Documentation

- We added missing CHANGELOG entries for versions 1.0.5 and 1.0.6, see
  [`e0017baf`](https://github.com/sozu-proxy/sozu/commit/e0017baf81b94f78cfbd7b0e58fd18e2186b4aa6).
- We fixed documentation for query commands, see
  [PR #1164](https://github.com/sozu-proxy/sozu/pull/1164),
  [`e89e1a40`](https://github.com/sozu-proxy/sozu/commit/e89e1a40ec26de96c41aef7133a220ed988e9e27).

### Changelog

#### ⚠️ BREAKING CHANGE

- [
  [`6650be58`](https://github.com/sozu-proxy/sozu/commit/6650be58c9b6067529599ba54fea866ba52c6575)
  ] chore(toolchain): migrate to Rust edition 2024 and update to 1.85.0
  [`Florentin Dubois`] (`2025-10-29`)

#### 🌟 Features

- [
  [`fbce7f65`](https://github.com/sozu-proxy/sozu/commit/fbce7f65c7c404036c1dd90a3a70c41174581669)
  ] refactor: use headers `traceparent` and `tracestate` [`llenotre`]
  (`2025-10-01`)
- [
  [`7035eebb`](https://github.com/sozu-proxy/sozu/commit/7035eebb72d7a6a9ad17540d4a24f8de59dea669)
  ] feat(accesslogs): opentelemetry [`llenotre`] (`2025-09-26`)
- [
  [`f72bb977`](https://github.com/sozu-proxy/sozu/commit/f72bb977dfd62403f30a3f4e2bb2e048cd8b6510)
  ] feat: opentelemetry (wip) [`llenotre`] (`2025-09-25`)
- [
  [`3f5f63fa`](https://github.com/sozu-proxy/sozu/commit/3f5f63fae7de9955589aa3e7cadbf26147f56230)
  ] feat: allow sozu to be used as lib by another package [`anonkey`]
  (`2024-12-26`)
- [
  [`2f478d08`](https://github.com/sozu-proxy/sozu/commit/2f478d0862f9f277a8a2fd89cd09bd71abaa5dd3)
  ] build: disable kawa default features on non x86_64 [`anonkey`]
  (`2024-09-13`)

#### ⛑️ Fixed

- [
  [`5f3371b7`](https://github.com/sozu-proxy/sozu/commit/5f3371b7775dd185f77ed5c3313876acd11290fc)
  ] Fix backend_reponse_time on keep alive requests [`Eloi Démolis`]
  (`2025-09-30`)
- [
  [`3c660faf`](https://github.com/sozu-proxy/sozu/commit/3c660faf2d955bcb6dd5326770f56991469b11e9)
  ] fix: `forwarded` field quoting [`llenotre`] (`2025-05-28`)
- [
  [`a1138973`](https://github.com/sozu-proxy/sozu/commit/a1138973538ec80a5bcf890e3cec79eaa65893d6)
  ] fix(ci): Update go version for bombardier [`Helian Caumeil`] (`2025-02-25`)
- [
  [`a7ecc65a`](https://github.com/sozu-proxy/sozu/commit/a7ecc65a978e9a925d868a66930e82cff2478f99)
  ] fix: Resolve path when saving state [`hcaumeil`] (`2025-02-25`)
- [
  [`9d1fbbe9`](https://github.com/sozu-proxy/sozu/commit/9d1fbbe9fdcfccf399c37ce0feb1052871827198)
  ] fix: fix RUSTSEC-2024-0402 [`Dimitris Apostolou`] (`2024-12-31`)
- [
  [`93a4ea43`](https://github.com/sozu-proxy/sozu/commit/93a4ea4372bf0d4a51e2cbc1eb0fdae9fadb428c)
  ] build: fix dockerfile [`anonkey`] (`2024-12-26`)

#### ✍️ Changed

- [
  [`b9f0e52d`](https://github.com/sozu-proxy/sozu/commit/b9f0e52d2147d6771d85da36de078e5ad55fed4c)
  ] ci: remove coverage upload [`Florentin Dubois`] (`2025-10-29`)
- [
  [`6e3e7e12`](https://github.com/sozu-proxy/sozu/commit/6e3e7e12343838c9f6c5d2a3c9ee893711446455)
  ] chore(lib): remove double definition of global allocator [`Florentin
  Dubois`] (`2025-10-29`)
- [
  [`e678ebc2`](https://github.com/sozu-proxy/sozu/commit/e678ebc22851ea1bf809c006fd9582b217ea951f)
  ] chore: update nix dependency [`Florentin Dubois`] (`2025-10-29`)
- [
  [`703a3df3`](https://github.com/sozu-proxy/sozu/commit/703a3df3f01e2407c644c8be619a2115a328cc2a)
  ] chore(deps): update Cargo.lock with latest dependency versions [`Florentin
  Dubois`] (`2025-10-29`)
- [
  [`f96444e9`](https://github.com/sozu-proxy/sozu/commit/f96444e9aa5f2f9fcd58252f98a421feaa65cf59)
  ] chore: bump dependencies [`llenotre`] (`2025-09-30`)
- [
  [`e3f81b46`](https://github.com/sozu-proxy/sozu/commit/e3f81b4603cfe860f00a0149a58d17a896920a7d)
  ] deps: avoid duplicate crates [`Dimitris Apostolou`] (`2025-02-24`)
- [
  [`a77c928f`](https://github.com/sozu-proxy/sozu/commit/a77c928fc7300cfbed76bf4d07dba9de13ea3dc6)
  ] ci(docker): build and push on every commits [`Florentin Dubois`]
  (`2024-12-26`)

#### ➖ Removed

- [
  [`ebdda472`](https://github.com/sozu-proxy/sozu/commit/ebdda472fadf99b1afea797d7859a2ede6505b50)
  ] chore: remove @keksoj from code owners [`Florentin Dubois`] (`2024-12-26`)

#### 📚 Documentation

- [
  [`e0017baf`](https://github.com/sozu-proxy/sozu/commit/e0017baf81b94f78cfbd7b0e58fd18e2186b4aa6)
  ] doc(changelog): add missing entries for 1.0.5 and 1.0.6 [`Florentin Dubois`]
  (`2025-01-07`)
- [
  [`e89e1a40`](https://github.com/sozu-proxy/sozu/commit/e89e1a40ec26de96c41aef7133a220ed988e9e27)
  ] docs: fix query commands [`anonkey`] (`2024-12-26`)

### 🥹 Contributors

@FlorentinDUBOIS, @Wonshtrum, @llenotre, @anonkey, @rex4539, @hcaumeil

**Full Changelog**: https://github.com/sozu-proxy/sozu/compare/1.0.6...1.1.0

## 1.0.6 - 2024-12-05

### 🌟 Features

- We add new variables to inject on 400 and 502 errors, see
  [`07c7ec0`](https://github.com/sozu-proxy/sozu/commit/07c7ec0c6f27b4b423372612bed09e1ad12ac0b7).

### ⛑️ Fixed

- We fix the reset of the timeout on tcp backend connection, see
  [`9c432f3`](https://github.com/sozu-proxy/sozu/commit/9c432f3b3b25b10dbe51040b140b7477ba4dba6c).
- We properly executre the close negociation on tls connection when the
  connection is ending by Sōzu, see
  [`2b19805`](https://github.com/sozu-proxy/sozu/commit/2b19805ef7a61b6e94be5f662f126632451902b5).

### Changelog

#### 🌟 Features

- [ [`07c7ec0`](https://github.com/sozu-proxy/sozu/commit/07c7ec0c6f27b4b423372612bed09e1ad12ac0b7)
  ] Refine granularity of 400 and 502 error diagnostics [`Eloi DEMOLIS`]
  (`2024-11-07`)

#### ⛑️ Fixed

- [ [`9c432f3`](https://github.com/sozu-proxy/sozu/commit/9c432f3b3b25b10dbe51040b140b7477ba4dba6c)
  ] Fix TCP back timeout errors on reset [`Eloi DEMOLIS`] (`2024-11-19`)
- [ [`2b19805`](https://github.com/sozu-proxy/sozu/commit/2b19805ef7a61b6e94be5f662f126632451902b5)
  ] Fix TLS close initiated by Sozu, solution is not ideal [`Eloi DEMOLIS`]
  (`2024-11-25`)

#### ✍️ Changed

- [ [`8162e99`](https://github.com/sozu-proxy/sozu/commit/8162e99c2e984cb616b6a9f9ec3cbf2fa69e2e86)
  ] apply clippy suggestions [`Emmanuel Bosquet`] (`2024-11-07`)
- [ [`863dc9b`](https://github.com/sozu-proxy/sozu/commit/863dc9bfcf644d74fcf8f8d21679a23c49883710)
  ] bump dependencies for 1.0.6 [`Emmanuel Bosquet`] (`2024-11-20`)

### 🥹 Contributors

- @keksoj
- @FlorentinDUBOIS
- @Wonshtrum

**Full Changelog**: https://github.com/sozu-proxy/sozu/compare/1.0.5...1.0.6

## 1.0.5 - 2024-11-14

> This changelog entry contains change from the version `1.0.3` to `1.0.5`

### 🚀 Performance

- We have found that compiling Sōzu without `logs-trace` and `logs-debug` flags
  lead to a 30% less performance. In fact with flags, the rust compiler is able
  to track the format! macro to the write_fmt method which allow him to write on
  stdout without allocating memory which is not the case without and the dead
  code eliminator is not able to correctly do its works. The compiler without
  the flags is required to perform the memory allocation that lead to the 30%
  less performance, see
  [`57b8d66`](https://github.com/sozu-proxy/sozu/commit/57b8d6620ce212b12a2cef3c16fa81430dce8110).

### ⛑️ Fixed

- We fixed a corner case that may appear during the exchange of payload between
  Sozu, the kernel and the http client when the payload is fully transfered from
  the backend and sozu, but still in the kernel waiting for the client to
  acknowledge previous frame, see
  [`1e6b38d`](https://github.com/sozu-proxy/sozu/commit/1e6b38d6a795e317ed8d80b0e047a36c1bb40740).

### Changelog

#### 🚀 Performance

- [ [`57b8d66`](https://github.com/sozu-proxy/sozu/commit/57b8d6620ce212b12a2cef3c16fa81430dce8110)
  ] Elide allocation and formating of opt-out logs [`Eloi DEMOLIS`]
  (`2024-10-10`)

#### ✍️ Changed

- [ [`1ad059c`](https://github.com/sozu-proxy/sozu/commit/1ad059c4372f26fbe2291093291bf81173a41205)
  ] bump prost and prost-build to 0.13.1 [`Emmanuel Bosquet`] (`2024-07-25`)
- [ [`c6aad5e`](https://github.com/sozu-proxy/sozu/commit/c6aad5ecbcd1188dca912e07f384358e1204dfd2)
  ] chore: update dependencies [`Florentin Dubois`] (`2024-10-14`)
- [ [`d45227e`](https://github.com/sozu-proxy/sozu/commit/d45227e67bb8d3e572eb912d4da2b804714d0f2a)
  ] change log level of "unknown task" from error to warn [`Emmanuel Bosquet`]
  (`2024-08-07`)
- [ [`daa5c02`](https://github.com/sozu-proxy/sozu/commit/daa5c02424d37e867d16c76913f274352538cba1)
  ] change multiline info and debug statements to inline [`Emmanuel Bosquet`]
  (`2024-08-07`)
- [ [`0849163`](https://github.com/sozu-proxy/sozu/commit/084916353deb4668879212ea60e53f9c14ade6f8)
  ] function main returns exit status [`Emmanuel Bosquet`] (`2024-08-09`)
- [ [`2c85009`](https://github.com/sozu-proxy/sozu/commit/2c85009b7d82d080239556139b60722cfd735724)
  ] add ListFrontends to main process [`Emmanuel Bosquet`] (`2024-08-01`)
- [ [`e48b26d`](https://github.com/sozu-proxy/sozu/commit/e48b26d6269d3e63fa5fcf4aac24fe0b46e0c369)
  ] bump rust-toolchain to 1.80.0 [`Emmanuel Bosquet`] (`2024-08-01`)

#### ➖ Removed

- [ [`2ed27e9`](https://github.com/sozu-proxy/sozu/commit/2ed27e939a8efa65a4267a7485a9e28764749aa0)
  ] remove sozu_lib::router::trie and replace use with pattern_trie [`Emmanuel
  Bosquet`] (`2024-08-14`)

#### ⛑️ Fixed

- [ [`202c0cb`](https://github.com/sozu-proxy/sozu/commit/202c0cb44cf014c3440bf5ab668ba71ef81f50f2)
  ] fix merging of histograms [`Emmanuel Bosquet`] (`2024-08-09`)
- [ [`44f76c3`](https://github.com/sozu-proxy/sozu/commit/44f76c31b6e1c14b8fce43434f86e510fd1829df)
  ] Fix worker softlock by allowing writing on socket when Rustls buffers are
  full [`Eloi DEMOLIS`] (`2024-08-27`)
- [ [`1e6b38d`](https://github.com/sozu-proxy/sozu/commit/1e6b38d6a795e317ed8d80b0e047a36c1bb40740)
  ] Fix Pipe::check_connections: take kernel inflight data into account, to
  prevent early closing of TCP connections with no more backend [`Eloi DEMOLIS`]
  (`2024-10-04`)
- [ [`5ab35c2`](https://github.com/sozu-proxy/sozu/commit/5ab35c2d346b0387a75840bdb568238cddb29ebd)
  ] Properly track the backend bytes out metric in Pipe [`Eloi DEMOLIS`]
  (`2024-08-27`)

#### 📚 Documentation

- [ [`1a611cb`](https://github.com/sozu-proxy/sozu/commit/1a611cb9daef06dbf825d56f2951a4d4660faa8f)
  ] doc(changelog): add 1.0.3 entry [`Emmanuel Bosquet`] (`2024-07-17`)

### 🥹 Contributors

- @keksoj
- @FlorentinDUBOIS
- @Wonshtrum

**Full Changelog**: https://github.com/sozu-proxy/sozu/compare/1.0.3...1.0.5

## 1.0.3 - 2024-07-17

> This changelog entry contains changes from the version `1.0.1` and `1.0.2`

### 🌟 Features

- Metrics are now merged accross workers, to have proxying metrics, and cluster
  metrics, cumulated. This is avoidable in the CLI with the `--workers` flag.
  See
  [`6b4b96b`](https://github.com/sozu-proxy/sozu/commit/6b4b96b3e7e9171c2da546450894b0b4361b658d),
  [`5deac5d`](https://github.com/sozu-proxy/sozu/commit/5deac5d1fbf66a68c61a669c350a0cc0c9147d60)
- Time metrics are now converted to prometheus-compatible histograms, cumulated
  accross workers as explained above. See
  [`64b558e`](https://github.com/sozu-proxy/sozu/commit/64b558eb4e104bff345f51c641a4bbd7593ea35e)
- Additional metrics, "unsent-access-logs", see
  [`ec56678`](https://github.com/sozu-proxy/sozu/commit/ec5667869774becff16ed5a0c9f2581c6aac04c7)
  and "request_time", see
  [`57923de`](https://github.com/sozu-proxy/sozu/commit/57923de5e88276a9fcd2d3a678bf9ef13ccf08b8)
- backend events are displayed in the CLI, see
  [`97c9f30`](https://github.com/sozu-proxy/sozu/commit/97c9f3021c9191ae9d3b132a3c55241f1e611b52)
- Sōzu reconnects with the access log target (for instance, a UNIX socket) if
  the connection is lost. See
  [`18d6649`](https://github.com/sozu-proxy/sozu/commit/18d66498af5d4ac7e976e6e2954bd81a0d0620e2)

### ⛑️ Fixed

- The behaviour of certificate resolver has been entirely reworked to mitigate
  certificate conflicts, and prevent deleting certificates accidently, see
  [`8b3462d`](https://github.com/sozu-proxy/sozu/commit/8b3462daae3989e612877b19e33f5179e59061fc)

### Changelog

#### 🌟 Features

- [
  [`97c9f30`](https://github.com/sozu-proxy/sozu/commit/97c9f3021c9191ae9d3b132a3c55241f1e611b52)
  ] display events in the CLI, document the use [`Emmanuel Bosquet`]
  (`2024-06-25`)
- [
  [`6b4b96b`](https://github.com/sozu-proxy/sozu/commit/6b4b96b3e7e9171c2da546450894b0b4361b658d)
  ] merge proxying metrics accross workers [`Emmanuel Bosquet`] (`2024-07-09`)
- [
  [`5deac5d`](https://github.com/sozu-proxy/sozu/commit/5deac5d1fbf66a68c61a669c350a0cc0c9147d60)
  ] merge cluster metrics across workers [`Emmanuel Bosquet`] (`2024-07-09`)
- [
  [`64b558e`](https://github.com/sozu-proxy/sozu/commit/64b558eb4e104bff345f51c641a4bbd7593ea35e)
  ] metrics: return histograms for time metrics [`Emmanuel Bosquet`]
  (`2024-07-11`)
- [
  [`ec56678`](https://github.com/sozu-proxy/sozu/commit/ec5667869774becff16ed5a0c9f2581c6aac04c7)
  ] add count metric "unsent-access-logs" [`Emmanuel Bosquet`] (`2024-07-11`)
- [
  [`18d6649`](https://github.com/sozu-proxy/sozu/commit/18d66498af5d4ac7e976e6e2954bd81a0d0620e2)
  ] revive backend of access logs when it fails [`Emmanuel Bosquet`]
  (`2024-07-11`)
- [
  [`57923de`](https://github.com/sozu-proxy/sozu/commit/57923de5e88276a9fcd2d3a678bf9ef13ccf08b8)
  ] add request_time metric to access logs, BREAKING [`Emmanuel Bosquet`]
  (`2024-07-11`)

#### 📚 Documentation

- [
  [`e2204ae`](https://github.com/sozu-proxy/sozu/commit/e2204ae2dfffc5c33ab7a4cba0859f179c82afc6)
  ] doc: add @llenotre to codeowners [`Florentin DUBOIS`] (`2024-07-08`)

#### ⛑️ Fixed

- [
  [`8b3462d`](https://github.com/sozu-proxy/sozu/commit/8b3462daae3989e612877b19e33f5179e59061fc)
  ] resolve longer-lived certificates [`Emmanuel Bosquet`] (`2024-06-21`)
- [
  [`67802bf`](https://github.com/sozu-proxy/sozu/commit/67802bf7266c146e51880f3c8027b9b02ad3203b)
  ] CertificateResolver: add two unit tests, and suggestions [`Emmanuel
  Bosquet`] (`2024-06-21`)
- [
  [`7a107c6`](https://github.com/sozu-proxy/sozu/commit/7a107c6f4671b54f0d91e6b0d692f2d70e719733)
  ] better display of backend metrics [`Emmanuel Bosquet`] (`2024-07-08`)
- [
  [`15ac8eb`](https://github.com/sozu-proxy/sozu/commit/15ac8eb9591e64a663d2f5d717d93378ff0b6aac)
  ] fix AggregatedMetrics::merge_metrics [`Emmanuel Bosquet`] (`2024-07-11`)
- [
  [`9f765c1`](https://github.com/sozu-proxy/sozu/commit/9f765c172e7bbc6fd4511ac2f5dc09303da6f3de)
  ] command::logging::logs apply review [`Emmanuel Bosquet`] (`2024-07-17`)

#### ✍️ Changed

- [
  [`e6626c4`](https://github.com/sozu-proxy/sozu/commit/e6626c444d6d3ea61cfd7fb2ceaee98e1f391843)
  ] remove the logs-cache feature [`Emmanuel Bosquet`] (`2024-05-06`)
- [
  [`9e889ce`](https://github.com/sozu-proxy/sozu/commit/9e889ce3f589f2d5ba2532c676bc5df297f0eef8)
  ] bump mio to 1.0.0 [`Emmanuel Bosquet`] (`2024-06-21`)
- [
  [`b74888c`](https://github.com/sozu-proxy/sozu/commit/b74888c19d75d518de32a464f9b990251e6387e3)
  ] Move unsent-access-logs metric increment out of sozu_command_lib [`Eloi
  DEMOLIS`] (`2024-07-11`)
- [
  [`dd54bea`](https://github.com/sozu-proxy/sozu/commit/dd54bea333cd83928d5ade16c347924820574dc0)
  ] propagate logging setup errors [`Emmanuel Bosquet`] (`2024-07-11`)

#### ➕ Added

- [
  [`f30d0b1`](https://github.com/sozu-proxy/sozu/commit/f30d0b10bf184e39c786de775f660a86e7f7ee29)
  ] create command::logging::LogError [`Emmanuel Bosquet`] (`2024-07-11`)

### 🥹 Contributors

- @keksoj
- @FlorentinDUBOIS
- @Wonshtrum
- @llenotre

## 1.0.2 - 2024-06-03

> This changelog entry contains changes from the version `1.0.1` and `1.0.2`

### 🌟 Features

- We format logs from session using the same pattern to harmonize them in the
  console and to provide more information to debug, see
  [`9b1827f`](https://github.com/sozu-proxy/sozu/commit/9b1827f9b464c5fc21eb764b2076f6ed7f3e7b94)
  ].

### 📚 Documentation

- We have updated the readme to correct and be more accurate on the advantages
  section of using Sōzu, see
  [`ed5f4ef`](https://github.com/sozu-proxy/sozu/commit/ed5f4ef7f1f9b580ea4e1a5e19361ca723091f4a).
- We have added the changelog entries in CHANGELOG.md, see
  [`d965e6a`](https://github.com/sozu-proxy/sozu/commit/d965e6a00484c4054baac561a79688d4ad10c15e)
  and
  [`5ce5c5a`](https://github.com/sozu-proxy/sozu/commit/5ce5c5ac0060bff8333920eb8ad45fb8d7e00407).

### ⛑️ Fixed

- We have fixed a few bugs that occurred in production, see
  [`1bf28a2`](https://github.com/sozu-proxy/sozu/commit/1bf28a2c350850b1dfeced9132201ff847bf1fba),
  [`a1dfe01`](https://github.com/sozu-proxy/sozu/commit/a1dfe0164a70ff0b772163bf834863a3d1ab0ea6)
  and
  [`de0da04`](https://github.com/sozu-proxy/sozu/commit/de0da04dd98245213ce470657fe5d604cf283136).

### Changelog

#### 🌟 Features

- [ [`9b1827f`](https://github.com/sozu-proxy/sozu/commit/9b1827f9b464c5fc21eb764b2076f6ed7f3e7b94)
  ] feat: implements a common way to print session log of a request [`Florentin
  Dubois`] (`2024-06-03`)

#### 📚 Documentation

- [ [`ed5f4ef`](https://github.com/sozu-proxy/sozu/commit/ed5f4ef7f1f9b580ea4e1a5e19361ca723091f4a)
  ] README: correct details on the advantages of sozu [`Emmanuel Bosquet`]
  (`2024-05-17`)
- [ [`d965e6a`](https://github.com/sozu-proxy/sozu/commit/d965e6a00484c4054baac561a79688d4ad10c15e)
  ] doc(changelog): add entry for release 1.0.0 [`Florentin Dubois`]
  (`2024-04-16`)
- [ [`5ce5c5a`](https://github.com/sozu-proxy/sozu/commit/5ce5c5ac0060bff8333920eb8ad45fb8d7e00407)
  ] doc(changelog): update typos [`Florentin Dubois`] (`2024-04-16`)
- [ [`2e66285`](https://github.com/sozu-proxy/sozu/commit/2e662856e86aea51dffae2964e0f84b5a0e58f5c)
  ] correct comments in sozu_command_lib::certificate [`Emmanuel Bosquet`]
  (`2024-05-17`)

#### ✍️ Changed

- [ [`f84cdeb`](https://github.com/sozu-proxy/sozu/commit/f84cdeb7dbf9e8506fb24e446bf78e2932e0bc1a)
  ] use std::time instead of crate time [`Emmanuel Bosquet`] (`2024-04-24`)
- [ [`3816cbf`](https://github.com/sozu-proxy/sozu/commit/3816cbf4e03093b82e2cd871ddaf58b9104252c2)
  ] apply clippy suggestions [`Emmanuel Bosquet`] (`2024-04-29`)
- [ [`8f46dad`](https://github.com/sozu-proxy/sozu/commit/8f46daddc5e1470b8293a5d32d8914b0474f0499)
  ] chore: update dependencies [`Florentin Dubois`] (`2024-05-29`)
- [ [`b05fced`](https://github.com/sozu-proxy/sozu/commit/b05fceddd13ca1a5627dade0bbdd745016e388d6)
  ] chore: apply cargo clippy [`Florentin Dubois`] (`2024-06-03`)

#### ⛑️ Fixed

- [ [`de0da04`](https://github.com/sozu-proxy/sozu/commit/de0da04dd98245213ce470657fe5d604cf283136)
  ] fix(command): do not override subject and san of certificate when loading
  configuration from file [`Florentin Dubois`] (`2024-05-29`)
- [ [`a1dfe01`](https://github.com/sozu-proxy/sozu/commit/a1dfe0164a70ff0b772163bf834863a3d1ab0ea6)
  ] fix(kawa_h1): give response stream instead of request stream [`Florentin
  Dubois`] (`2024-06-03`)
- [ [`1bf28a2`](https://github.com/sozu-proxy/sozu/commit/1bf28a2c350850b1dfeced9132201ff847bf1fba)
  ] Fix access logs metrics [`Eloi DEMOLIS`] (`2024-06-03`)

### 🥹 Contributors

- @keksoj
- @FlorentinDUBOIS
- @Wonshtrum

**Full Changelog**: https://github.com/sozu-proxy/sozu/compare/1.0.0..1.0.2

## 1.0.0 - 2024-04-16

> This is the first release of Sōzu which is a huge steps since the beginning of
> the project. We would like to thanks every people involved in the development
> of Sōzu. The next steps is to implements h2(c) on top of kawa with a rework of
> session to be able to use http 1.x clients with h2 backends and h2 clients
> with h2 backends, h2 clients with http 1.x backends.

### 🌟 Features

- This release is the first one that use `protobuf-everywhere`. It means that we
  are moving out from the old exchange model for sockets (which are data
  exchanges between main process and workers processes and also main process and
  control plane process) that use Rust structures that we serialize into json to
  structures described in protobuf that generate Rust source code and then
  communicate with binary format. Besides, this work also have some nice
  side-effects on fork when creating new workers (due to a self-healing or at
  start) which allow to reduce the time to configure a worker by around 50%. It
  also introduces a new way to emit access logs in a binary way, see
  [ [`dfacdb7`](https://github.com/sozu-proxy/sozu/commit/dfacdb7adfaf20876fbccf0b5a21230adc74ec03)
  ],
  [ [`a6ffebe`](https://github.com/sozu-proxy/sozu/commit/a6ffebe627c4647336e4a4cff700b85235dc6bcd)
  ],
  [ [`fb11245`](https://github.com/sozu-proxy/sozu/commit/fb11245d87a0716b0cee7c3ecc5909dc799b0be6)
  ],
  [ [`6daf43c`](https://github.com/sozu-proxy/sozu/commit/6daf43c64d8185c5a691bb638274a85f260cfa46)
  ],
  [ [`8e81a02`](https://github.com/sozu-proxy/sozu/commit/8e81a02875441709579d1825fa9fe7cdd061a608)
  ],
  [ [`24a941b`](https://github.com/sozu-proxy/sozu/commit/24a941b6a79dbdf0e56a91d9d02c823afd101975)
  ],
  [ [`6d160b4`](https://github.com/sozu-proxy/sozu/commit/6d160b40d2f4a7027b99f860b7ec1a79366625a3)
  ],
  [ [`e382a1c`](https://github.com/sozu-proxy/sozu/commit/e382a1c2ab5f7d7560b609dbf2c7bde6963c8866)
  ],
  [ [`4f2d760`](https://github.com/sozu-proxy/sozu/commit/4f2d76042b9a8a7f0f3ef536f0ff298315d47888)
  ],
  [ [`6d43eb1`](https://github.com/sozu-proxy/sozu/commit/6d43eb18b44093790a5d4202b02da38748a634b6)
  ] and
  [ [`5852c6b`](https://github.com/sozu-proxy/sozu/commit/5852c6b1a9778d4a79af947907bea3dcb8e6c9f6)
  ].
- We have reworked emitted logs and access logs format, if you are tools that
  parse them, you have to take a look at the new one. Besides, it will be easier
  to parse. Furthermore, we have added color on logs to improve readability, see
  [ [`0885863`](https://github.com/sozu-proxy/sozu/commit/088586314e2234d8b83613afdeec5542ce5b7d27)
  ],
  [ [`d3eeddc`](https://github.com/sozu-proxy/sozu/commit/d3eeddc4060f73da37b1bd8257ca9562e63e0cec)
  ],
  [ [`3bb3997`](https://github.com/sozu-proxy/sozu/commit/3bb399705bb2a8db6ef81f41236618cc00627b40)
  ] and
  [ [`5f55561`](https://github.com/sozu-proxy/sozu/commit/5f55561ce702083dbdd783456f88caa67dd8db4a)
  ].
- We have bump the minimum Rust supported version to `v1.74.0`, see
  [ [`2c4363d`](https://github.com/sozu-proxy/sozu/commit/2c4363d088376e775e3af7d98113b0e70c6bcca2)
  ],
  [ [`96f5329`](https://github.com/sozu-proxy/sozu/commit/96f5329335410ac8a14bb84b2a3120a63f91e557)
  ] and
  [ [`e6bcfe0`](https://github.com/sozu-proxy/sozu/commit/e6bcfe033975a6aa4696a08bd0591d55ff41e63f)
  ].
- We have introduce a way to define custom http response for a wide range of
  status code, see
  [`3f0bfa1`](https://github.com/sozu-proxy/sozu/commit/3f0bfa12a7fc83ba1487d85ea012e29a83af8aa5),
  [`4923153`](https://github.com/sozu-proxy/sozu/commit/4923153bc76343fa6d135f4e99b60381f2b96cb5),
  [`b9df0c1`](https://github.com/sozu-proxy/sozu/commit/b9df0c1dc4659c2deb25433e3f36dc46998680fa),
  [`ee2430f`](https://github.com/sozu-proxy/sozu/commit/ee2430f9c85ee846314ebb09b8cc981267ba427b),
  [`3030944`](https://github.com/sozu-proxy/sozu/commit/30309447ddd246f9bd3c061ce7b544f7fa40559f),
  [`6023216`](https://github.com/sozu-proxy/sozu/commit/60232166716b656a3f68a5f68326ef3768f763ad),
  [`55242ba`](https://github.com/sozu-proxy/sozu/commit/55242bab7a9a881b3cc5bd3e8b8f569f0ac34a17),
  [`55242ba`](https://github.com/sozu-proxy/sozu/commit/55242bab7a9a881b3cc5bd3e8b8f569f0ac34a17),
  [`8faa16c`](https://github.com/sozu-proxy/sozu/commit/8faa16c999cf47f2c9260c87d6e8b17deca8a269),
  [`a5058c2`](https://github.com/sozu-proxy/sozu/commit/a5058c2981c9bed99e714a98b325320473b9a011),
  [`fb6aad9`](https://github.com/sozu-proxy/sozu/commit/fb6aad93a74b17960af2ed91bb2f4a527594bdf9)
  and
  [`a2236d1`](https://github.com/sozu-proxy/sozu/commit/a2236d11e20a33aeaaf4863d6c6b0a32167dab21).
- We also fix a bug that could occur on some streaming https requests with
  tcp-keepalive when going through multiple Sōzu which does not flush correctly
  the ending chunk of data when calling `rustls::write_vectored`, see
  [`d83f399`](https://github.com/sozu-proxy/sozu/commit/d83f3997953f889a75d3300b10a55021d23f768d).
- We also change how works timeout on frontends as it is now closed by the
  backend, once we have received a first chunk of data, see
  [`6114e17`](https://github.com/sozu-proxy/sozu/commit/6114e17b53aec8740e608c6ddf4e1c17a779b052).

### 🚀 Performance

- Thanks to the work achieved on Rustls
  ([v0.23.0](https://github.com/rustls/rustls/releases/tag/v%2F0.23.0)) with the
  help of maintainers (a huge thanks to them ❤️), we have gain between 10% and
  20% performance on https requests depending of the workload, see
  [ [`7fa680d`](https://github.com/sozu-proxy/sozu/commit/7fa680d8d346031ec50d7bcf40f51e3101cd7cec)
  ].

### ⛑️ Fixed

- We have fixed a bug that occurs during http request when doing streaming and
  tcp keep-alive which prevent to send the close-delimiter on response, see
  [ [`59c0615`](https://github.com/sozu-proxy/sozu/commit/59c06158ad86d664f4e2900b777dc6cfa8fad2f0)
  ].
- We have fix a few bugs on certificate replacement and improve the resolution
  of certificates, see
  [ [`6aa45b9`](https://github.com/sozu-proxy/sozu/commit/6aa45b9bd4ef8cadca308712d82f136e3747b7d5)
  ].
- We have fix a bug that involved timeout on frontend sockets which did not take
  the given value, see
  [ [`d20e759`](https://github.com/sozu-proxy/sozu/commit/d20e759a7e66527d35110e11f7651a5cfee86161)
  ].

### ✍️ Changed

- We have work on errors to have a better context when something happens, see
  [ [`ba8b51a`](https://github.com/sozu-proxy/sozu/commit/ba8b51aa0d6e2e631ec4c2e648ad3fdce67569fa)
  ],
  [ [`629551f`](https://github.com/sozu-proxy/sozu/commit/629551fdf17b943d91acc7543db391eb2ccdd8d3)
  ] and
  [ [`1c90c04`](https://github.com/sozu-proxy/sozu/commit/1c90c0496f315981606f5fae8945ad930369cb50)
  ].

### 📚 Documentation

- We have added some documentation about logs and access logs, see
  [ [`d44b227`](https://github.com/sozu-proxy/sozu/commit/d44b227036c26fa1a70883bfd1682262a02ae39e)
  ] and
  [ [`7241c0e`](https://github.com/sozu-proxy/sozu/commit/7241c0ee6bfa72e53e3317f6b3d13f27de863452)
  ].
- We have reworked or improved examples, see
  [ [`31bc55e`](https://github.com/sozu-proxy/sozu/commit/31bc55e2456f5bfe16d3d7de797d25676107755c)
  ] and
  [ [`95624e6`](https://github.com/sozu-proxy/sozu/commit/95624e6c91e548d2e87ea6df6341d0bc25cff630)
  ].
- We have added some documentation of using Sōzu with firewalld (thanks
  @obreidenich), see
  [ [`4cba552`](https://github.com/sozu-proxy/sozu/commit/4cba552b5d0b3ecead7553fd55bd7febc53ce9d1)
  ].
- We have setup a github continuous integration to provides and document a way
  to benchmark Sōzu (values on the ci are not reliable), see
  [ [`8df9ba5`](https://github.com/sozu-proxy/sozu/commit/8df9ba5618574cc3fd73865e2d5e739557db1bd5)
  ].

### Changelog

#### 🚀 Performance

- [ [`7fa680d`](https://github.com/sozu-proxy/sozu/commit/7fa680d8d346031ec50d7bcf40f51e3101cd7cec)
  ] update dependencies, notably rustls [`Emmanuel Bosquet`] (`2024-03-11`)

#### 🌟 Features

- [ [`3f0bfa1`](https://github.com/sozu-proxy/sozu/commit/3f0bfa12a7fc83ba1487d85ea012e29a83af8aa5)
  ] Dynamic automatic answers system [`Eloi DEMOLIS`] (`2024-04-05`)
- [ [`4923153`](https://github.com/sozu-proxy/sozu/commit/4923153bc76343fa6d135f4e99b60381f2b96cb5)
  ] Unify template creation [`Eloi DEMOLIS`] (`2024-04-05`)
- [ [`b9df0c1`](https://github.com/sozu-proxy/sozu/commit/b9df0c1dc4659c2deb25433e3f36dc46998680fa)
  ] add example 404 and 508 errors [`Emmanuel Bosquet`] (`2024-04-05`)
- [ [`ee2430f`](https://github.com/sozu-proxy/sozu/commit/ee2430f9c85ee846314ebb09b8cc981267ba427b)
  ] make Http[s]ListenerConfig::http_answers optional [`Emmanuel Bosquet`]
  (`2024-04-05`)
- [ [`3030944`](https://github.com/sozu-proxy/sozu/commit/30309447ddd246f9bd3c061ce7b544f7fa40559f)
  ] Minor changes [`Eloi DEMOLIS`] (`2024-04-05`)
- [ [`6023216`](https://github.com/sozu-proxy/sozu/commit/60232166716b656a3f68a5f68326ef3768f763ad)
  ] create protobuf type CustomHttpAnswers [`Emmanuel Bosquet`] (`2024-04-05`)
- [ [`55242ba`](https://github.com/sozu-proxy/sozu/commit/55242bab7a9a881b3cc5bd3e8b8f569f0ac34a17)
  ] rename types [`Emmanuel Bosquet`] (`2024-04-05`)
- [ [`8faa16c`](https://github.com/sozu-proxy/sozu/commit/8faa16c999cf47f2c9260c87d6e8b17deca8a269)
  ] Restore context in logs by moving the fields in HttpContext [`Eloi DEMOLIS`]
  (`2024-04-05`)
- [ [`a5058c2`](https://github.com/sozu-proxy/sozu/commit/a5058c2981c9bed99e714a98b325320473b9a011)
  ] remove 404 and 503 files [`Emmanuel Bosquet`] (`2024-04-05`)
- [ [`fb6aad9`](https://github.com/sozu-proxy/sozu/commit/fb6aad93a74b17960af2ed91bb2f4a527594bdf9)
  ] Move test_https_redirect to e2e, use immutable automatic answers for e2e
  [`Eloi DEMOLIS`] (`2024-04-05`)
- [ [`a2236d1`](https://github.com/sozu-proxy/sozu/commit/a2236d11e20a33aeaaf4863d6c6b0a32167dab21)
  ] More template variables [`Eloi DEMOLIS`] (`2024-04-05`)

#### ➕ Added

- [ [`90ee05c`](https://github.com/sozu-proxy/sozu/commit/90ee05c2f347a0ccb90d65e2cbae5c5e8a2192b6)
  ] benchmark info logs in the CI [`Emmanuel Bosquet`] (`2024-03-15`)
- [ [`a6fde1e`](https://github.com/sozu-proxy/sozu/commit/a6fde1eb44f958b095a6fb8c1208854ab6d8c85f)
  ] distinct PEM and X509 variants for CertificateError [`Emmanuel Bosquet`]
  (`2024-03-15`)
- [ [`e793ec6`](https://github.com/sozu-proxy/sozu/commit/e793ec6667ae83d8be302d549751637315bef0fd)
  ] Edit access logs ASCII format, put logs-cache under feature [`Eloi DEMOLIS`]
  (`2024-03-11`)
- [ [`85e2653`](https://github.com/sozu-proxy/sozu/commit/85e26535a4ec4eafe8c64f186b2f40014b5b1f4e)
  ] implement From<Uint128> for Ulid [`Emmanuel Bosquet`] (`2024-03-11`)
- [ [`756b78b`](https://github.com/sozu-proxy/sozu/commit/756b78bc9c13684b4acb15caf14044f0ad90e623)
  ] create type WebSocketContext [`Emmanuel Bosquet`] (`2024-03-11`)
- [ [`0885863`](https://github.com/sozu-proxy/sozu/commit/088586314e2234d8b83613afdeec5542ce5b7d27)
  ] Logger refactor: better structured logs and colored logs [`Eloi DEMOLIS`]
  (`2024-03-11`)
- [ [`dfacdb7`](https://github.com/sozu-proxy/sozu/commit/dfacdb7adfaf20876fbccf0b5a21230adc74ec03)
  ] protobuf access logs [`Emmanuel Bosquet`] (`2024-03-11`)
- [ [`7c25133`](https://github.com/sozu-proxy/sozu/commit/7c25133e66541b221b151bcbf8f4d7efc87e945e)
  ] create helper function server::worker_response_error [`Emmanuel Bosquet`]
  (`2024-02-23`)
- [ [`a6ffebe`](https://github.com/sozu-proxy/sozu/commit/a6ffebe627c4647336e4a4cff700b85235dc6bcd)
  ] binary and delimited serialization of prost messages in channels [`Emmanuel
  Bosquet`] (`2024-02-02`)
- [ [`62f3db3`](https://github.com/sozu-proxy/sozu/commit/62f3db34ca9d8e209ce9444ab890e1847f3ef093)
  ] add fields and defaults to ServerConfig [`Emmanuel Bosquet`] (`2024-02-02`)
- [ [`fb11245`](https://github.com/sozu-proxy/sozu/commit/fb11245d87a0716b0cee7c3ecc5909dc799b0be6)
  ] create protobuf type SocketAddress, use everywhere [`Emmanuel Bosquet`]
  (`2024-02-02`)
- [ [`f9ac920`](https://github.com/sozu-proxy/sozu/commit/f9ac9207df5edf4bad4e6c1392e829a3e1e080ed)
  ] add size to ChannelError::BufferFull [`Emmanuel Bosquet`] (`2024-02-02`)

#### ➖ Removed

- [ [`c677b10`](https://github.com/sozu-proxy/sozu/commit/c677b102a0694db998ebc3ebf485aeb2d869dc71)
  ] remove duplicate code of HttpsProxy creation [`Emmanuel Bosquet`]
  (`2024-02-26`)
- [ [`39af839`](https://github.com/sozu-proxy/sozu/commit/39af83929154820687bc9c87fc25fc6fa7747b5d)
  ] remove useless Serde error in channel module [`Emmanuel Bosquet`]
  (`2024-02-02`)

### ✍️ Changed

- [ [`6114e17`](https://github.com/sozu-proxy/sozu/commit/6114e17b53aec8740e608c6ddf4e1c17a779b052)
  ] Move timeout responsibility from front to back only when first bytes are
  received from the back [`Eloi DEMOLIS`] (`2024-03-20`)
- [ [`0d7f7d6`](https://github.com/sozu-proxy/sozu/commit/0d7f7d634d964976dfae2f29d737498a6d0d6a7d)
  ] Release v1.0.0-rc.1 [`Florentin Dubois`] (`2024-03-19`)
- [ [`bcef3b8`](https://github.com/sozu-proxy/sozu/commit/bcef3b84328333acb0ef9bb936245328bca60c62)
  ] chore: update dependencies [`Florentin Dubois`] (`2024-03-14`)
- [ [`a806fb6`](https://github.com/sozu-proxy/sozu/commit/a806fb6699aff7662e85d1a8f586d65c149bc3f4)
  ] sort cluster information alphabetically when displaying [`Emmanuel Bosquet`]
  (`2024-03-12`)
- [ [`6daf43c`](https://github.com/sozu-proxy/sozu/commit/6daf43c64d8185c5a691bb638274a85f260cfa46)
  ] write two empty bytes after each protobuf access log [`Emmanuel Bosquet`]
  (`2024-03-11`)
- [ [`8e81a02`](https://github.com/sozu-proxy/sozu/commit/8e81a02875441709579d1825fa9fe7cdd061a608)
  ] rename ProtobufAccessLog::error to 'message' [`Emmanuel Bosquet`]
  (`2024-03-11`)
- [ [`b6ca85b`](https://github.com/sozu-proxy/sozu/commit/b6ca85b03be0d53bd98f98436866bbe44cbc04ff)
  ] apply clippy suggestions for nightly [`Emmanuel Bosquet`] (`2024-03-11`)
- [ [`618bed0`](https://github.com/sozu-proxy/sozu/commit/618bed03e7f7f8535d1fe6ad93a4b1acd30f9404)
  ] apply review: cosmetic changes [`Emmanuel Bosquet`] (`2024-03-11`)
- [ [`182b291`](https://github.com/sozu-proxy/sozu/commit/182b291b347964a02c07e14d7886164e0322fc4e)
  ] Use error_access in lib, add log_access to make it easier [`Eloi DEMOLIS`]
  (`2024-03-11`)
- [ [`5f55561`](https://github.com/sozu-proxy/sozu/commit/5f55561ce702083dbdd783456f88caa67dd8db4a)
  ] rename log*access*_ variables to access*logs*_ [`Emmanuel Bosquet`]
  (`2024-03-11`)
- [ [`d3eeddc`](https://github.com/sozu-proxy/sozu/commit/d3eeddc4060f73da37b1bd8257ca9562e63e0cec)
  ] Small changes mainly relative to logging [`Eloi DEMOLIS`] (`2024-03-11`)
- [ [`3bb3997`](https://github.com/sozu-proxy/sozu/commit/3bb399705bb2a8db6ef81f41236618cc00627b40)
  ] Move all logging primitives in their own module [`Eloi DEMOLIS`]
  (`2024-03-11`)
- [ [`e6bcfe0`](https://github.com/sozu-proxy/sozu/commit/e6bcfe033975a6aa4696a08bd0591d55ff41e63f)
  ] set dependency resolver to 2 [`Emmanuel Bosquet`] (`2024-03-11`)
- [ [`96f5329`](https://github.com/sozu-proxy/sozu/commit/96f5329335410ac8a14bb84b2a3120a63f91e557)
  ] set rust-version to 1.74.0, update Cargo.lock [`Emmanuel Bosquet`]
  (`2024-03-11`)
- [ [`2c4363d`](https://github.com/sozu-proxy/sozu/commit/2c4363d088376e775e3af7d98113b0e70c6bcca2)
  ] bump rust-toolchain to 1.74.0 [`Emmanuel Bosquet`] (`2024-03-11`)
- [ [`c6ee01b`](https://github.com/sozu-proxy/sozu/commit/c6ee01b2a606558bf064688affb8f4f9fb79cd63)
  ] move TCP pool from TcpListener to TcpProxy [`Emmanuel Bosquet`]
  (`2024-02-26`)
- [ [`1c90c04`](https://github.com/sozu-proxy/sozu/commit/1c90c0496f315981606f5fae8945ad930369cb50)
  ] propagate errors in HttpProxy and HttpsProxy [`Emmanuel Bosquet`]
  (`2024-02-23`)
- [ [`629551f`](https://github.com/sozu-proxy/sozu/commit/629551fdf17b943d91acc7543db391eb2ccdd8d3)
  ] propagate errors in TcpProxy [`Emmanuel Bosquet`] (`2024-02-23`)
- [ [`ba8b51a`](https://github.com/sozu-proxy/sozu/commit/ba8b51aa0d6e2e631ec4c2e648ad3fdce67569fa)
  ] HttpsProxy::add_listener returns Result [`Emmanuel Bosquet`] (`2024-02-23`)
- [ [`630b1a7`](https://github.com/sozu-proxy/sozu/commit/630b1a70b20018f8e876307ea6f3872dc4e3c13c)
  ] define buffer sizes in u64 [`Emmanuel Bosquet`] (`2024-02-02`)
- [ [`24a941b`](https://github.com/sozu-proxy/sozu/commit/24a941b6a79dbdf0e56a91d9d02c823afd101975)
  ] pass ServerConfig to new worker [`Emmanuel Bosquet`] (`2024-02-02`)
- [ [`6d160b4`](https://github.com/sozu-proxy/sozu/commit/6d160b40d2f4a7027b99f860b7ec1a79366625a3)
  ] move ServerConfig to sozu_command_lib::config [`Emmanuel Bosquet`]
  (`2024-02-02`)
- [ [`f0ecc54`](https://github.com/sozu-proxy/sozu/commit/f0ecc54451649d4190f34cb8c9dd674321aec2bb)
  ] rewrite CommandServer with MIO [`Eloi DEMOLIS`] (`2024-01-30`)
- [ [`e382a1c`](https://github.com/sozu-proxy/sozu/commit/e382a1c2ab5f7d7560b609dbf2c7bde6963c8866)
  ] translate WorkerRequest and WorkerResponse to protobuf [`Emmanuel Bosquet`]
  (`2024-02-02`)
- [ [`4f2d760`](https://github.com/sozu-proxy/sozu/commit/4f2d76042b9a8a7f0f3ef536f0ff298315d47888)
  ] translate ServerConfig in protobuf [`Emmanuel Bosquet`] (`2024-02-02`)
- [ [`6d43eb1`](https://github.com/sozu-proxy/sozu/commit/6d43eb18b44093790a5d4202b02da38748a634b6)
  ] SCM sockets transmit listeners in binary format [`Emmanuel Bosquet`]
  (`2024-02-02`)
- [ [`5852c6b`](https://github.com/sozu-proxy/sozu/commit/5852c6b1a9778d4a79af947907bea3dcb8e6c9f6)
  ] pass initial state to workers in protobuf [`Emmanuel Bosquet`]
  (`2024-02-02`)
- [ [`cc0a221`](https://github.com/sozu-proxy/sozu/commit/cc0a2214401763862e72d9539cb31e90faf18112)
  ] apply clippy suggestions, remove unwraps [`Emmanuel Bosquet`] (`2024-02-02`)
- [ [`a3fe0d3`](https://github.com/sozu-proxy/sozu/commit/a3fe0d37025fbe29a51f8db126158176564e44a7)
  ] chore: update dependencies [`Florentin Dubois`] (`2024-04-16`)
- [ [`8d89733`](https://github.com/sozu-proxy/sozu/commit/8d89733d9a06b9cb4b0327524fbe44ea69a199bf)
  ] chore: update configuration file [`Florentin Dubois`] (`2024-04-16`)

#### ⛑️ Fixed

- [ [`d20e759`](https://github.com/sozu-proxy/sozu/commit/d20e759a7e66527d35110e11f7651a5cfee86161)
  ] Fix some timeout edge cases [`Eloi DEMOLIS`] (`2024-03-18`)
- [ [`284068d`](https://github.com/sozu-proxy/sozu/commit/284068d73be3624cbe225b06af60278f9a0a72f0)
  ] fix: fix RUSTSEC-2024-0019 [`Dimitris Apostolou`] (`2024-03-09`)
- [ [`59c0615`](https://github.com/sozu-proxy/sozu/commit/59c06158ad86d664f4e2900b777dc6cfa8fad2f0)
  ] Fix close propagation on close-delimited responses with front keep-alive
  [`Eloi DEMOLIS`] (`2024-02-20`)
- [ [`6aa45b9`](https://github.com/sozu-proxy/sozu/commit/6aa45b9bd4ef8cadca308712d82f136e3747b7d5)
  ] fix and rewrite CertificateResolver [`Emmanuel Bosquet`] (`2024-02-14`)
- [ [`e793ec6`](https://github.com/sozu-proxy/sozu/commit/e793ec6667ae83d8be302d549751637315bef0fd)
  ] Edit access logs ASCII format, put logs-cache under feature [`Eloi DEMOLIS`]
  (`2024-03-11`)
- [ [`85e2653`](https://github.com/sozu-proxy/sozu/commit/85e26535a4ec4eafe8c64f186b2f40014b5b1f4e)
  ] implement From<Uint128> for Ulid [`Emmanuel Bosquet`] (`2024-03-11`)
- [ [`756b78b`](https://github.com/sozu-proxy/sozu/commit/756b78bc9c13684b4acb15caf14044f0ad90e623)
  ] create type WebSocketContext [`Emmanuel Bosquet`] (`2024-03-11`)
- [ [`0885863`](https://github.com/sozu-proxy/sozu/commit/088586314e2234d8b83613afdeec5542ce5b7d27)
  ] Logger refactor: better structured logs and colored logs [`Eloi DEMOLIS`]
  (`2024-03-11`)
- [ [`dfacdb7`](https://github.com/sozu-proxy/sozu/commit/dfacdb7adfaf20876fbccf0b5a21230adc74ec03)
  ] protobuf access logs [`Emmanuel Bosquet`] (`2024-03-11`)
- [ [`7c25133`](https://github.com/sozu-proxy/sozu/commit/7c25133e66541b221b151bcbf8f4d7efc87e945e)
  ] create helper function server::worker_response_error [`Emmanuel Bosquet`]
  (`2024-02-23`)
- [ [`a6ffebe`](https://github.com/sozu-proxy/sozu/commit/a6ffebe627c4647336e4a4cff700b85235dc6bcd)
  ] binary and delimited serialization of prost messages in channels [`Emmanuel
  Bosquet`] (`2024-02-02`)
- [ [`62f3db3`](https://github.com/sozu-proxy/sozu/commit/62f3db34ca9d8e209ce9444ab890e1847f3ef093)
  ] add fields and defaults to ServerConfig [`Emmanuel Bosquet`] (`2024-02-02`)
- [ [`fb11245`](https://github.com/sozu-proxy/sozu/commit/fb11245d87a0716b0cee7c3ecc5909dc799b0be6)
  ] create protobuf type SocketAddress, use everywhere [`Emmanuel Bosquet`]
  (`2024-02-02`)
- [ [`f9ac920`](https://github.com/sozu-proxy/sozu/commit/f9ac9207df5edf4bad4e6c1392e829a3e1e080ed)
  ] add size to ChannelError::BufferFull [`Emmanuel Bosquet`] (`2024-02-02`)
- [ [`d44b227`](https://github.com/sozu-proxy/sozu/commit/d44b227036c26fa1a70883bfd1682262a02ae39e)
  ] document the logs-cache feature flag [`Emmanuel Bosquet`] (`2024-03-11`)
- [ [`7241c0e`](https://github.com/sozu-proxy/sozu/commit/7241c0ee6bfa72e53e3317f6b3d13f27de863452)
  ] document the DuplicateOwnership trait [`Emmanuel Bosquet`] (`2024-03-11`)
- [ [`31bc55e`](https://github.com/sozu-proxy/sozu/commit/31bc55e2456f5bfe16d3d7de797d25676107755c)
  ] Add example in command lib to benchmark the logger [`Emmanuel Bosquet`]
  (`2024-03-11`)
- [ [`4cba552`](https://github.com/sozu-proxy/sozu/commit/4cba552b5d0b3ecead7553fd55bd7febc53ce9d1)
  ] A small, descriptive extension to include firewalld. Avoiding a search for
  those not using iptables. [`obreidenich`] (`2024-02-21`)
- [ [`8df9ba5`](https://github.com/sozu-proxy/sozu/commit/8df9ba5618574cc3fd73865e2d5e739557db1bd5)
  ] CI: Adding a benchmark framework [`Guillaume Assier`] (`2024-02-07`)
- [ [`95624e6`](https://github.com/sozu-proxy/sozu/commit/95624e6c91e548d2e87ea6df6341d0bc25cff630)
  ] Refactor HTTP, HTTPS and TCP example code [`Eloi DEMOLIS`] (`2024-02-02`)

### 🥹 Contributors

- @obreidenich made their first contribution in
  https://github.com/sozu-proxy/sozu/pull/1079
- @rex4539 made their first contribution in
  https://github.com/sozu-proxy/sozu/pull/1090
- @keksoj
- @FlorentinDUBOIS
- @Wonshtrum

**Full Changelog**: https://github.com/sozu-proxy/sozu/compare/0.15.19..1.0.0

## 1.0.0-rc.2 - 2024-04-05

> This changelog is the second release candidate before the version `1.0.0`, it
> includes a **breaking change** on the protocol buffer and configuration around
> definition of custom response when Sōzu answers instead of backends defined in
> cluster.

### 🌟 Features

- We have introduce a way to define custom http response for a wide range of
  status code, see
  [`3f0bfa1`](https://github.com/sozu-proxy/sozu/commit/3f0bfa12a7fc83ba1487d85ea012e29a83af8aa5),
  [`4923153`](https://github.com/sozu-proxy/sozu/commit/4923153bc76343fa6d135f4e99b60381f2b96cb5),
  [`b9df0c1`](https://github.com/sozu-proxy/sozu/commit/b9df0c1dc4659c2deb25433e3f36dc46998680fa),
  [`ee2430f`](https://github.com/sozu-proxy/sozu/commit/ee2430f9c85ee846314ebb09b8cc981267ba427b),
  [`3030944`](https://github.com/sozu-proxy/sozu/commit/30309447ddd246f9bd3c061ce7b544f7fa40559f),
  [`6023216`](https://github.com/sozu-proxy/sozu/commit/60232166716b656a3f68a5f68326ef3768f763ad),
  [`55242ba`](https://github.com/sozu-proxy/sozu/commit/55242bab7a9a881b3cc5bd3e8b8f569f0ac34a17),
  [`55242ba`](https://github.com/sozu-proxy/sozu/commit/55242bab7a9a881b3cc5bd3e8b8f569f0ac34a17),
  [`8faa16c`](https://github.com/sozu-proxy/sozu/commit/8faa16c999cf47f2c9260c87d6e8b17deca8a269),
  [`a5058c2`](https://github.com/sozu-proxy/sozu/commit/a5058c2981c9bed99e714a98b325320473b9a011),
  [`fb6aad9`](https://github.com/sozu-proxy/sozu/commit/fb6aad93a74b17960af2ed91bb2f4a527594bdf9)
  and
  [`a2236d1`](https://github.com/sozu-proxy/sozu/commit/a2236d11e20a33aeaaf4863d6c6b0a32167dab21).
- We also fix a bug that could occur on some streaming https requests with
  tcp-keepalive when going through multiple Sōzu which does not flush correctly
  the ending chunk of data when calling `rustls::write_vectored`, see
  [`d83f399`](https://github.com/sozu-proxy/sozu/commit/d83f3997953f889a75d3300b10a55021d23f768d).
- We also change how works timeout on frontends as it is now closed by the
  backend, once we have received a first chunk of data, see
  [`6114e17`](https://github.com/sozu-proxy/sozu/commit/6114e17b53aec8740e608c6ddf4e1c17a779b052).

### Changelog

#### 🌟 Features

- [ [`3f0bfa1`](https://github.com/sozu-proxy/sozu/commit/3f0bfa12a7fc83ba1487d85ea012e29a83af8aa5)
  ] Dynamic automatic answers system [`Eloi DEMOLIS`] (`2024-04-05`)
- [ [`4923153`](https://github.com/sozu-proxy/sozu/commit/4923153bc76343fa6d135f4e99b60381f2b96cb5)
  ] Unify template creation [`Eloi DEMOLIS`] (`2024-04-05`)
- [ [`b9df0c1`](https://github.com/sozu-proxy/sozu/commit/b9df0c1dc4659c2deb25433e3f36dc46998680fa)
  ] add example 404 and 508 errors [`Emmanuel Bosquet`] (`2024-04-05`)
- [ [`ee2430f`](https://github.com/sozu-proxy/sozu/commit/ee2430f9c85ee846314ebb09b8cc981267ba427b)
  ] make Http[s]ListenerConfig::http_answers optional [`Emmanuel Bosquet`]
  (`2024-04-05`)
- [ [`3030944`](https://github.com/sozu-proxy/sozu/commit/30309447ddd246f9bd3c061ce7b544f7fa40559f)
  ] Minor changes [`Eloi DEMOLIS`] (`2024-04-05`)
- [ [`6023216`](https://github.com/sozu-proxy/sozu/commit/60232166716b656a3f68a5f68326ef3768f763ad)
  ] create protobuf type CustomHttpAnswers [`Emmanuel Bosquet`] (`2024-04-05`)
- [ [`55242ba`](https://github.com/sozu-proxy/sozu/commit/55242bab7a9a881b3cc5bd3e8b8f569f0ac34a17)
  ] rename types [`Emmanuel Bosquet`] (`2024-04-05`)
- [ [`8faa16c`](https://github.com/sozu-proxy/sozu/commit/8faa16c999cf47f2c9260c87d6e8b17deca8a269)
  ] Restore context in logs by moving the fields in HttpContext [`Eloi DEMOLIS`]
  (`2024-04-05`)
- [ [`a5058c2`](https://github.com/sozu-proxy/sozu/commit/a5058c2981c9bed99e714a98b325320473b9a011)
  ] remove 404 and 503 files [`Emmanuel Bosquet`] (`2024-04-05`)
- [ [`fb6aad9`](https://github.com/sozu-proxy/sozu/commit/fb6aad93a74b17960af2ed91bb2f4a527594bdf9)
  ] Move test_https_redirect to e2e, use immutable automatic answers for e2e
  [`Eloi DEMOLIS`] (`2024-04-05`)
- [ [`a2236d1`](https://github.com/sozu-proxy/sozu/commit/a2236d11e20a33aeaaf4863d6c6b0a32167dab21)
  ] More template variables [`Eloi DEMOLIS`] (`2024-04-05`)

#### ➕ Added

- [ [`90ee05c`](https://github.com/sozu-proxy/sozu/commit/90ee05c2f347a0ccb90d65e2cbae5c5e8a2192b6)
  ] benchmark info logs in the CI [`Emmanuel Bosquet`] (`2024-03-15`)

### ✍️ Changed

- [ [`6114e17`](https://github.com/sozu-proxy/sozu/commit/6114e17b53aec8740e608c6ddf4e1c17a779b052)
  ] Move timeout responsibility from front to back only when first bytes are
  received from the back [`Eloi DEMOLIS`] (`2024-03-20`)

#### ⛑️ Fixed

- [ [`d83f399`](https://github.com/sozu-proxy/sozu/commit/d83f3997953f889a75d3300b10a55021d23f768d)
  ] Expose internal rustls buffers to ensure they are flushed [`Eloi DEMOLIS`]
  (`2024-04-03`)

### 🥹 Contributors

- @keksoj
- @Wonshtrum

**Full Changelog**:
https://github.com/sozu-proxy/sozu/compare/1.0.0.-rc.1..1.0.0-rc.2

## 1.0.0-rc.1 - 2024-03-19

> This changelog is the first release candidate before the version `1.0.0`, its
> goal is to allow to migrate smoothly other crates as the `sozu-client` to be
> compatible with the protobuf-everywhere part and to be able to make tests on
> production at Clever-Cloud by doing some A/B testing.

### 🌟 Features

- This version is the first one that use `protobuf-everywhere`. It means that we
  are moving out from the old exchange model for sockets (which are data
  exchanges between main process and workers processes and also main process and
  control plane process) that use Rust structures that we serialize into json to
  structures described in protobuf that generate Rust source code and then
  communicate with binary format. Besides, this work also have some nice
  side-effects on fork when creating new workers (due to a self-healing or at
  start) which allow to reduce the time to configure a worker by around 50%. It
  also introduces a new way to emit access logs in a binary way, see
  [ [`dfacdb7`](https://github.com/sozu-proxy/sozu/commit/dfacdb7adfaf20876fbccf0b5a21230adc74ec03)
  ],
  [ [`a6ffebe`](https://github.com/sozu-proxy/sozu/commit/a6ffebe627c4647336e4a4cff700b85235dc6bcd)
  ],
  [ [`fb11245`](https://github.com/sozu-proxy/sozu/commit/fb11245d87a0716b0cee7c3ecc5909dc799b0be6)
  ],
  [ [`6daf43c`](https://github.com/sozu-proxy/sozu/commit/6daf43c64d8185c5a691bb638274a85f260cfa46)
  ],
  [ [`8e81a02`](https://github.com/sozu-proxy/sozu/commit/8e81a02875441709579d1825fa9fe7cdd061a608)
  ],
  [ [`24a941b`](https://github.com/sozu-proxy/sozu/commit/24a941b6a79dbdf0e56a91d9d02c823afd101975)
  ],
  [ [`6d160b4`](https://github.com/sozu-proxy/sozu/commit/6d160b40d2f4a7027b99f860b7ec1a79366625a3)
  ],
  [ [`e382a1c`](https://github.com/sozu-proxy/sozu/commit/e382a1c2ab5f7d7560b609dbf2c7bde6963c8866)
  ],
  [ [`4f2d760`](https://github.com/sozu-proxy/sozu/commit/4f2d76042b9a8a7f0f3ef536f0ff298315d47888)
  ],
  [ [`6d43eb1`](https://github.com/sozu-proxy/sozu/commit/6d43eb18b44093790a5d4202b02da38748a634b6)
  ] and
  [ [`5852c6b`](https://github.com/sozu-proxy/sozu/commit/5852c6b1a9778d4a79af947907bea3dcb8e6c9f6)
  ].
- We have reworked emitted logs and access logs format, if you are tools that
  parse them, you have to take a look at the new one. Besides, it will be easier
  to parse. Furthermore, we have added color on logs to improve readability, see
  [ [`0885863`](https://github.com/sozu-proxy/sozu/commit/088586314e2234d8b83613afdeec5542ce5b7d27)
  ],
  [ [`d3eeddc`](https://github.com/sozu-proxy/sozu/commit/d3eeddc4060f73da37b1bd8257ca9562e63e0cec)
  ],
  [ [`3bb3997`](https://github.com/sozu-proxy/sozu/commit/3bb399705bb2a8db6ef81f41236618cc00627b40)
  ] and
  [ [`5f55561`](https://github.com/sozu-proxy/sozu/commit/5f55561ce702083dbdd783456f88caa67dd8db4a)
  ].
- We have bump the minimum Rust supported version to `v1.74.0`, see
  [ [`2c4363d`](https://github.com/sozu-proxy/sozu/commit/2c4363d088376e775e3af7d98113b0e70c6bcca2)
  ],
  [ [`96f5329`](https://github.com/sozu-proxy/sozu/commit/96f5329335410ac8a14bb84b2a3120a63f91e557)
  ] and
  [ [`e6bcfe0`](https://github.com/sozu-proxy/sozu/commit/e6bcfe033975a6aa4696a08bd0591d55ff41e63f)
  ].

### ⛑️ Fixed

- We have fixed a bug that occurs during http request when doing streaming and
  tcp keep-alive which prevent to send the close-delimiter on response, see
  [ [`59c0615`](https://github.com/sozu-proxy/sozu/commit/59c06158ad86d664f4e2900b777dc6cfa8fad2f0)
  ].
- We have fix a few bugs on certificate replacement and improve the resolution
  of certificates, see
  [ [`6aa45b9`](https://github.com/sozu-proxy/sozu/commit/6aa45b9bd4ef8cadca308712d82f136e3747b7d5)
  ].
- We have fix a bug that involved timeout on frontend sockets which did not take
  the given value, see
  [ [`d20e759`](https://github.com/sozu-proxy/sozu/commit/d20e759a7e66527d35110e11f7651a5cfee86161)
  ].

### 🚀 Performance

- Thanks to the work achieved on Rustls
  ([v0.23.0](https://github.com/rustls/rustls/releases/tag/v%2F0.23.0)) with the
  help of maintainers (a huge thanks to them ❤️), we have gain between 10% and
  20% performance on https requests depending of the workload, see
  [ [`7fa680d`](https://github.com/sozu-proxy/sozu/commit/7fa680d8d346031ec50d7bcf40f51e3101cd7cec)
  ].

### ✍️ Changed

- We have work on errors to have a better context when something happens, see
  [ [`ba8b51a`](https://github.com/sozu-proxy/sozu/commit/ba8b51aa0d6e2e631ec4c2e648ad3fdce67569fa)
  ],
  [ [`629551f`](https://github.com/sozu-proxy/sozu/commit/629551fdf17b943d91acc7543db391eb2ccdd8d3)
  ] and
  [ [`1c90c04`](https://github.com/sozu-proxy/sozu/commit/1c90c0496f315981606f5fae8945ad930369cb50)
  ].

### 📚 Documentation

- We have added some documentation about logs and access logs, see
  [ [`d44b227`](https://github.com/sozu-proxy/sozu/commit/d44b227036c26fa1a70883bfd1682262a02ae39e)
  ] and
  [ [`7241c0e`](https://github.com/sozu-proxy/sozu/commit/7241c0ee6bfa72e53e3317f6b3d13f27de863452)
  ].
- We have reworked or improved examples, see
  [ [`31bc55e`](https://github.com/sozu-proxy/sozu/commit/31bc55e2456f5bfe16d3d7de797d25676107755c)
  ] and
  [ [`95624e6`](https://github.com/sozu-proxy/sozu/commit/95624e6c91e548d2e87ea6df6341d0bc25cff630)
  ].
- We have added some documentation of using Sōzu with firewalld (thanks
  @obreidenich), see
  [ [`4cba552`](https://github.com/sozu-proxy/sozu/commit/4cba552b5d0b3ecead7553fd55bd7febc53ce9d1)
  ].
- We have setup a github continuous integration to provides and document a way
  to benchmark Sōzu (values on the ci are not reliable), see
  [ [`8df9ba5`](https://github.com/sozu-proxy/sozu/commit/8df9ba5618574cc3fd73865e2d5e739557db1bd5)
  ].

### Changelog

#### 🚀 Performance

- [ [`7fa680d`](https://github.com/sozu-proxy/sozu/commit/7fa680d8d346031ec50d7bcf40f51e3101cd7cec)
  ] update dependencies, notably rustls [`Emmanuel Bosquet`] (`2024-03-11`)

#### ⛑️ Fixed

- [ [`d20e759`](https://github.com/sozu-proxy/sozu/commit/d20e759a7e66527d35110e11f7651a5cfee86161)
  ] Fix some timeout edge cases [`Eloi DEMOLIS`] (`2024-03-18`)
- [ [`284068d`](https://github.com/sozu-proxy/sozu/commit/284068d73be3624cbe225b06af60278f9a0a72f0)
  ] fix: fix RUSTSEC-2024-0019 [`Dimitris Apostolou`] (`2024-03-09`)
- [ [`59c0615`](https://github.com/sozu-proxy/sozu/commit/59c06158ad86d664f4e2900b777dc6cfa8fad2f0)
  ] Fix close propagation on close-delimited responses with front keep-alive
  [`Eloi DEMOLIS`] (`2024-02-20`)
- [ [`6aa45b9`](https://github.com/sozu-proxy/sozu/commit/6aa45b9bd4ef8cadca308712d82f136e3747b7d5)
  ] fix and rewrite CertificateResolver [`Emmanuel Bosquet`] (`2024-02-14`)

#### 📚 Documentation

- [ [`d44b227`](https://github.com/sozu-proxy/sozu/commit/d44b227036c26fa1a70883bfd1682262a02ae39e)
  ] document the logs-cache feature flag [`Emmanuel Bosquet`] (`2024-03-11`)
- [ [`7241c0e`](https://github.com/sozu-proxy/sozu/commit/7241c0ee6bfa72e53e3317f6b3d13f27de863452)
  ] document the DuplicateOwnership trait [`Emmanuel Bosquet`] (`2024-03-11`)
- [ [`31bc55e`](https://github.com/sozu-proxy/sozu/commit/31bc55e2456f5bfe16d3d7de797d25676107755c)
  ] Add example in command lib to benchmark the logger [`Emmanuel Bosquet`]
  (`2024-03-11`)
- [ [`4cba552`](https://github.com/sozu-proxy/sozu/commit/4cba552b5d0b3ecead7553fd55bd7febc53ce9d1)
  ] A small, descriptive extension to include firewalld. Avoiding a search for
  those not using iptables. [`obreidenich`] (`2024-02-21`)
- [ [`8df9ba5`](https://github.com/sozu-proxy/sozu/commit/8df9ba5618574cc3fd73865e2d5e739557db1bd5)
  ] CI: Adding a benchmark framework [`Guillaume Assier`] (`2024-02-07`)
- [ [`95624e6`](https://github.com/sozu-proxy/sozu/commit/95624e6c91e548d2e87ea6df6341d0bc25cff630)
  ] Refactor HTTP, HTTPS and TCP example code [`Eloi DEMOLIS`] (`2024-02-02`)

#### ➕ Added

- [ [`a6fde1e`](https://github.com/sozu-proxy/sozu/commit/a6fde1eb44f958b095a6fb8c1208854ab6d8c85f)
  ] distinct PEM and X509 variants for CertificateError [`Emmanuel Bosquet`]
  (`2024-03-15`)
- [ [`e793ec6`](https://github.com/sozu-proxy/sozu/commit/e793ec6667ae83d8be302d549751637315bef0fd)
  ] Edit access logs ASCII format, put logs-cache under feature [`Eloi DEMOLIS`]
  (`2024-03-11`)
- [ [`85e2653`](https://github.com/sozu-proxy/sozu/commit/85e26535a4ec4eafe8c64f186b2f40014b5b1f4e)
  ] implement From<Uint128> for Ulid [`Emmanuel Bosquet`] (`2024-03-11`)
- [ [`756b78b`](https://github.com/sozu-proxy/sozu/commit/756b78bc9c13684b4acb15caf14044f0ad90e623)
  ] create type WebSocketContext [`Emmanuel Bosquet`] (`2024-03-11`)
- [ [`0885863`](https://github.com/sozu-proxy/sozu/commit/088586314e2234d8b83613afdeec5542ce5b7d27)
  ] Logger refactor: better structured logs and colored logs [`Eloi DEMOLIS`]
  (`2024-03-11`)
- [ [`dfacdb7`](https://github.com/sozu-proxy/sozu/commit/dfacdb7adfaf20876fbccf0b5a21230adc74ec03)
  ] protobuf access logs [`Emmanuel Bosquet`] (`2024-03-11`)
- [ [`7c25133`](https://github.com/sozu-proxy/sozu/commit/7c25133e66541b221b151bcbf8f4d7efc87e945e)
  ] create helper function server::worker_response_error [`Emmanuel Bosquet`]
  (`2024-02-23`)
- [ [`a6ffebe`](https://github.com/sozu-proxy/sozu/commit/a6ffebe627c4647336e4a4cff700b85235dc6bcd)
  ] binary and delimited serialization of prost messages in channels [`Emmanuel
  Bosquet`] (`2024-02-02`)
- [ [`62f3db3`](https://github.com/sozu-proxy/sozu/commit/62f3db34ca9d8e209ce9444ab890e1847f3ef093)
  ] add fields and defaults to ServerConfig [`Emmanuel Bosquet`] (`2024-02-02`)
- [ [`fb11245`](https://github.com/sozu-proxy/sozu/commit/fb11245d87a0716b0cee7c3ecc5909dc799b0be6)
  ] create protobuf type SocketAddress, use everywhere [`Emmanuel Bosquet`]
  (`2024-02-02`)
- [ [`f9ac920`](https://github.com/sozu-proxy/sozu/commit/f9ac9207df5edf4bad4e6c1392e829a3e1e080ed)
  ] add size to ChannelError::BufferFull [`Emmanuel Bosquet`] (`2024-02-02`)

#### ✍️ Changed

- [ [`0d7f7d6`](https://github.com/sozu-proxy/sozu/commit/0d7f7d634d964976dfae2f29d737498a6d0d6a7d)
  ] Release v1.0.0-rc.1 [`Florentin Dubois`] (`2024-03-19`)
- [ [`bcef3b8`](https://github.com/sozu-proxy/sozu/commit/bcef3b84328333acb0ef9bb936245328bca60c62)
  ] chore: update dependencies [`Florentin Dubois`] (`2024-03-14`)
- [ [`a806fb6`](https://github.com/sozu-proxy/sozu/commit/a806fb6699aff7662e85d1a8f586d65c149bc3f4)
  ] sort cluster information alphabetically when displaying [`Emmanuel Bosquet`]
  (`2024-03-12`)
- [ [`6daf43c`](https://github.com/sozu-proxy/sozu/commit/6daf43c64d8185c5a691bb638274a85f260cfa46)
  ] write two empty bytes after each protobuf access log [`Emmanuel Bosquet`]
  (`2024-03-11`)
- [ [`8e81a02`](https://github.com/sozu-proxy/sozu/commit/8e81a02875441709579d1825fa9fe7cdd061a608)
  ] rename ProtobufAccessLog::error to 'message' [`Emmanuel Bosquet`]
  (`2024-03-11`)
- [ [`b6ca85b`](https://github.com/sozu-proxy/sozu/commit/b6ca85b03be0d53bd98f98436866bbe44cbc04ff)
  ] apply clippy suggestions for nightly [`Emmanuel Bosquet`] (`2024-03-11`)
- [ [`618bed0`](https://github.com/sozu-proxy/sozu/commit/618bed03e7f7f8535d1fe6ad93a4b1acd30f9404)
  ] apply review: cosmetic changes [`Emmanuel Bosquet`] (`2024-03-11`)
- [ [`182b291`](https://github.com/sozu-proxy/sozu/commit/182b291b347964a02c07e14d7886164e0322fc4e)
  ] Use error_access in lib, add log_access to make it easier [`Eloi DEMOLIS`]
  (`2024-03-11`)
- [ [`5f55561`](https://github.com/sozu-proxy/sozu/commit/5f55561ce702083dbdd783456f88caa67dd8db4a)
  ] rename log*access*_ variables to access*logs*_ [`Emmanuel Bosquet`]
  (`2024-03-11`)
- [ [`d3eeddc`](https://github.com/sozu-proxy/sozu/commit/d3eeddc4060f73da37b1bd8257ca9562e63e0cec)
  ] Small changes mainly relative to logging [`Eloi DEMOLIS`] (`2024-03-11`)
- [ [`3bb3997`](https://github.com/sozu-proxy/sozu/commit/3bb399705bb2a8db6ef81f41236618cc00627b40)
  ] Move all logging primitives in their own module [`Eloi DEMOLIS`]
  (`2024-03-11`)
- [ [`e6bcfe0`](https://github.com/sozu-proxy/sozu/commit/e6bcfe033975a6aa4696a08bd0591d55ff41e63f)
  ] set dependency resolver to 2 [`Emmanuel Bosquet`] (`2024-03-11`)
- [ [`96f5329`](https://github.com/sozu-proxy/sozu/commit/96f5329335410ac8a14bb84b2a3120a63f91e557)
  ] set rust-version to 1.74.0, update Cargo.lock [`Emmanuel Bosquet`]
  (`2024-03-11`)
- [ [`2c4363d`](https://github.com/sozu-proxy/sozu/commit/2c4363d088376e775e3af7d98113b0e70c6bcca2)
  ] bump rust-toolchain to 1.74.0 [`Emmanuel Bosquet`] (`2024-03-11`)
- [ [`c6ee01b`](https://github.com/sozu-proxy/sozu/commit/c6ee01b2a606558bf064688affb8f4f9fb79cd63)
  ] move TCP pool from TcpListener to TcpProxy [`Emmanuel Bosquet`]
  (`2024-02-26`)
- [ [`1c90c04`](https://github.com/sozu-proxy/sozu/commit/1c90c0496f315981606f5fae8945ad930369cb50)
  ] propagate errors in HttpProxy and HttpsProxy [`Emmanuel Bosquet`]
  (`2024-02-23`)
- [ [`629551f`](https://github.com/sozu-proxy/sozu/commit/629551fdf17b943d91acc7543db391eb2ccdd8d3)
  ] propagate errors in TcpProxy [`Emmanuel Bosquet`] (`2024-02-23`)
- [ [`ba8b51a`](https://github.com/sozu-proxy/sozu/commit/ba8b51aa0d6e2e631ec4c2e648ad3fdce67569fa)
  ] HttpsProxy::add_listener returns Result [`Emmanuel Bosquet`] (`2024-02-23`)
- [ [`630b1a7`](https://github.com/sozu-proxy/sozu/commit/630b1a70b20018f8e876307ea6f3872dc4e3c13c)
  ] define buffer sizes in u64 [`Emmanuel Bosquet`] (`2024-02-02`)
- [ [`24a941b`](https://github.com/sozu-proxy/sozu/commit/24a941b6a79dbdf0e56a91d9d02c823afd101975)
  ] pass ServerConfig to new worker [`Emmanuel Bosquet`] (`2024-02-02`)
- [ [`6d160b4`](https://github.com/sozu-proxy/sozu/commit/6d160b40d2f4a7027b99f860b7ec1a79366625a3)
  ] move ServerConfig to sozu_command_lib::config [`Emmanuel Bosquet`]
  (`2024-02-02`)
- [ [`f0ecc54`](https://github.com/sozu-proxy/sozu/commit/f0ecc54451649d4190f34cb8c9dd674321aec2bb)
  ] rewrite CommandServer with MIO [`Eloi DEMOLIS`] (`2024-01-30`)
- [ [`e382a1c`](https://github.com/sozu-proxy/sozu/commit/e382a1c2ab5f7d7560b609dbf2c7bde6963c8866)
  ] translate WorkerRequest and WorkerResponse to protobuf [`Emmanuel Bosquet`]
  (`2024-02-02`)
- [ [`4f2d760`](https://github.com/sozu-proxy/sozu/commit/4f2d76042b9a8a7f0f3ef536f0ff298315d47888)
  ] translate ServerConfig in protobuf [`Emmanuel Bosquet`] (`2024-02-02`)
- [ [`6d43eb1`](https://github.com/sozu-proxy/sozu/commit/6d43eb18b44093790a5d4202b02da38748a634b6)
  ] SCM sockets transmit listeners in binary format [`Emmanuel Bosquet`]
  (`2024-02-02`)
- [ [`5852c6b`](https://github.com/sozu-proxy/sozu/commit/5852c6b1a9778d4a79af947907bea3dcb8e6c9f6)
  ] pass initial state to workers in protobuf [`Emmanuel Bosquet`]
  (`2024-02-02`)
- [ [`cc0a221`](https://github.com/sozu-proxy/sozu/commit/cc0a2214401763862e72d9539cb31e90faf18112)
  ] apply clippy suggestions, remove unwraps [`Emmanuel Bosquet`] (`2024-02-02`)

#### ➖ Removed

- [ [`c677b10`](https://github.com/sozu-proxy/sozu/commit/c677b102a0694db998ebc3ebf485aeb2d869dc71)
  ] remove duplicate code of HttpsProxy creation [`Emmanuel Bosquet`]
  (`2024-02-26`)
- [ [`39af839`](https://github.com/sozu-proxy/sozu/commit/39af83929154820687bc9c87fc25fc6fa7747b5d)
  ] remove useless Serde error in channel module [`Emmanuel Bosquet`]
  (`2024-02-02`)

### 🥹 Contributors

- @obreidenich made their first contribution in
  https://github.com/sozu-proxy/sozu/pull/1079
- @rex4539 made their first contribution in
  https://github.com/sozu-proxy/sozu/pull/1090
- @keksoj
- @FlorentinDUBOIS
- @Wonshtrum

**Full Changelog**:
https://github.com/sozu-proxy/sozu/compare/0.15.19..1.0.0-rc.1

## 0.15.19 - 2024-01-25

> This changelog merges modifications between 0.15.15 to 0.15.19

- We have reduced logging level and enhanced few logs, update the logger to
  better track issues that may occur, see
  [`582ab5b`](https://github.com/sozu-proxy/sozu/commit/582ab5be830684d416e1813d2d84c87456254a5a),
  [`17020fb`](https://github.com/sozu-proxy/sozu/commit/17020fb4032cf5f220075617c9b31a017df02722),
  [`04d3105`](https://github.com/sozu-proxy/sozu/commit/04d3105cfab506fa29467e1365abf31239a88c6d),
  [`730f0c3`](https://github.com/sozu-proxy/sozu/commit/730f0c329917a1da9a09d0dfcbc3799e9a2288d5),
  [`c887666`](https://github.com/sozu-proxy/sozu/commit/c88766694ff10a5ac9f1d7f17e7f7cb0ec919ff6),
  [`ef6e99a`](https://github.com/sozu-proxy/sozu/commit/ef6e99ad46cf302fa6a5fa9b67cc6908f3561d3b),
  [`63e76c7`](https://github.com/sozu-proxy/sozu/commit/63e76c7d1da2aeaa0ab8565772e88a089f0c36da),
  [`3c6ef35`](https://github.com/sozu-proxy/sozu/commit/3c6ef359d16d04e07b59df1878b9998f1f31205e),
  [`4d1500a`](https://github.com/sozu-proxy/sozu/commit/4d1500a0a5b70e4460e09a36aa27d8063dc7937e),
  [`72bfab9`](https://github.com/sozu-proxy/sozu/commit/72bfab997d4df991133d73c0ca1e9b7e70269385),
  [`18ddee3`](https://github.com/sozu-proxy/sozu/commit/18ddee36828e11ad80ca51c4b18fc515a8c7ef2c),
  [`b455bbf`](https://github.com/sozu-proxy/sozu/commit/b455bbfa78c7d1bb33f8f40492e6c331ff16eea2)
  and
  [`d864012`](https://github.com/sozu-proxy/sozu/commit/d864012ad6cdadbf94630c7fd195095968a6b17d).
- We have implemented the flag `--json` on every query command of Sōzu to be
  able to use it with software like `jq`, see
  [`95de156`](https://github.com/sozu-proxy/sozu/commit/95de156c533a67dde6e7135bf0e5bf96b7ea4cb6),
  [`0e62ff3`](https://github.com/sozu-proxy/sozu/commit/0e62ff3ca80bbb4b78c533ef9723b9fc19e8ce66)
  and
  [`822dcb9`](https://github.com/sozu-proxy/sozu/commit/822dcb9530419693c7bc6997a36576520a0e36e1).
- We have fixed behaviors when parsing HTTP 1.1 (mainly pipelining or streaming
  issues), see ,
  [`58a7f03`](https://github.com/sozu-proxy/sozu/commit/58a7f03feac0e8ca23fac6529734f9ded14e724b),
  [`ae8c66d`](https://github.com/sozu-proxy/sozu/commit/ae8c66d9f12bae8d2e6aaa5071d961734ae2d446),
  [`6bd2d85`](https://github.com/sozu-proxy/sozu/commit/6bd2d85833ea9ef35ed14807bffe4afffcd6806d),
  [`707fbf3`](https://github.com/sozu-proxy/sozu/commit/707fbf3f168400a76828a9b348e2bb226609724a),
  [`1cb4d53`](https://github.com/sozu-proxy/sozu/commit/1cb4d53a162f8e5efbdc18937db9d7dddb4a2933)
  and
  [`1710f8a`](https://github.com/sozu-proxy/sozu/commit/1710f8a7f1fa4673676f2045a5cad6d3e89d194b).

### Changelog

#### ➕ Added

- [
  [`72bfab9`](https://github.com/sozu-proxy/sozu/commit/72bfab997d4df991133d73c0ca1e9b7e70269385)
  ] setup logging in accept_clients() [`Emmanuel Bosquet`] (`2023-12-13`)
- [
  [`18ddee3`](https://github.com/sozu-proxy/sozu/commit/18ddee36828e11ad80ca51c4b18fc515a8c7ef2c)
  ] add missing access logs [`Emmanuel Bosquet`] (`2024-01-08`)
- [
  [`822dcb9`](https://github.com/sozu-proxy/sozu/commit/822dcb9530419693c7bc6997a36576520a0e36e1)
  ] CLI: all responses are displayable in JSON [`Emmanuel Bosquet`]
  (`2023-12-06`)
- [
  [`1788fac`](https://github.com/sozu-proxy/sozu/commit/1788faca0193f4311f6e1def45b3cc28ab401bce)
  ] add remove_backend test in state module [`Emmanuel Bosquet`] (`2023-12-06`)
- [
  [`161ca05`](https://github.com/sozu-proxy/sozu/commit/161ca051202f9edff4bdf88ce90c3dc89061d22f)
  ] introduce optional worker_timeout [`Emmanuel Bosquet`] (`2023-11-23`)
- [
  [`b754391`](https://github.com/sozu-proxy/sozu/commit/b754391b6a148202e2622595a55b231103f8730e)
  ] create ConfigState::write_requests_to_file [`Emmanuel Bosquet`]
  (`2023-11-27`)

#### ⛑️ Fixed

- [
  [`1cb4d53`](https://github.com/sozu-proxy/sozu/commit/1cb4d53a162f8e5efbdc18937db9d7dddb4a2933)
  ] handle backend hangup when responses is still transferring [`Emmanuel
  Bosquet`] (`2024-01-08`)
- [
  [`1710f8a`](https://github.com/sozu-proxy/sozu/commit/1710f8a7f1fa4673676f2045a5cad6d3e89d194b)
  ] Fix TCP connection hanging on backend connection error [`Eloi DEMOLIS`]
  (`2024-01-23`)
- [
  [`707fbf3`](https://github.com/sozu-proxy/sozu/commit/707fbf3f168400a76828a9b348e2bb226609724a)
  ] Update TCP states to use SessionResult when possible [`Eloi DEMOLIS`]
  (`2024-01-24`)
- [
  [`6bd2d85`](https://github.com/sozu-proxy/sozu/commit/6bd2d85833ea9ef35ed14807bffe4afffcd6806d)
  ] fix(sozu): reset storage buffers on keep-alive requests [`Florentin Dubois`]
  (`2023-12-07`)
- [
  [`bb1aa11`](https://github.com/sozu-proxy/sozu/commit/bb1aa112789ff7432a9a30eee36f8de0c5585055)
  ] fix(https): panic on failed https upgrade into wss [`Florentin Dubois`]
  (`2023-12-13`)
- [
  [`ae8c66d`](https://github.com/sozu-proxy/sozu/commit/ae8c66d9f12bae8d2e6aaa5071d961734ae2d446)
  ] fix(http): panic on http upgrade into websocket [`Florentin Dubois`]
  (`2023-12-13`)
- [
  [`58a7f03`](https://github.com/sozu-proxy/sozu/commit/58a7f03feac0e8ca23fac6529734f9ded14e724b)
  ] Fix: WouldBlock in SocketHandler::socket_write breaks properly [`Eloi
  DEMOLIS`] (`2023-11-23`)
- [
  [`17020fb`](https://github.com/sozu-proxy/sozu/commit/17020fb4032cf5f220075617c9b31a017df02722)
  ] Fix panic in view [`Eloi DEMOLIS`] (`2023-11-23`)
- [
  [`0d82323`](https://github.com/sozu-proxy/sozu/commit/0d8232317ba6cd84ac053038e47bd6f260107904)
  ] fix worker status command [`Emmanuel Bosquet`] (`2023-11-23`)
- [
  [`582ab5b`](https://github.com/sozu-proxy/sozu/commit/582ab5be830684d416e1813d2d84c87456254a5a)
  ] Do not set RUST_LOG on logger setup [`Eloi DEMOLIS`] (`2023-12-07`)
- [
  [`04d3105`](https://github.com/sozu-proxy/sozu/commit/04d3105cfab506fa29467e1365abf31239a88c6d)
  ] Sanitize user-agent in access logs [`Eloi DEMOLIS`] (`2024-01-24`)
- [
  [`10f5433`](https://github.com/sozu-proxy/sozu/commit/10f54339f6301fa6b3cc365d8b6206513a44ddc9)
  ] fix timeout issue in the CLI [`Emmanuel Bosquet`] (`2024-01-24`)

#### ✍️ Changed

- [
  [`23d8171`](https://github.com/sozu-proxy/sozu/commit/23d81715d0e19b09ef035b46d441cbc59f96382d)
  ] update rustls to 0.22.1 [`Emmanuel Bosquet`] (`2023-12-14`)
- [
  [`b7ef38f`](https://github.com/sozu-proxy/sozu/commit/b7ef38f3784c3321ee13a8faa75f8485a214fc1a)
  ] add no-clusters option on metrics query [`Emmanuel Bosquet`] (`2024-01-24`)
- [
  [`98b5783`](https://github.com/sozu-proxy/sozu/commit/98b5783b096f9f5d244a2add2e294a772b7fd848)
  ] chore: update dependencies [`Florentin Dubois`] (`2024-01-25`)
- [
  [`3a4e4fd`](https://github.com/sozu-proxy/sozu/commit/3a4e4fd93d5451c43f822348e933af1894a2e917)
  ] pass Vec<WorkerRequests> instead of ConfigState to new worker [`Emmanuel
  Bosquet`] (`2023-11-27`)
- [
  [`b455bbf`](https://github.com/sozu-proxy/sozu/commit/b455bbfa78c7d1bb33f8f40492e6c331ff16eea2)
  ] Add SNI and peer address on handshake error logs [`Eloi DEMOLIS`]
  (`2023-11-28`)
- [
  [`d864012`](https://github.com/sozu-proxy/sozu/commit/d864012ad6cdadbf94630c7fd195095968a6b17d)
  ] remove main logger [`Emmanuel Bosquet`] (`2023-12-01`)
- [
  [`505d134`](https://github.com/sozu-proxy/sozu/commit/505d134ce2a1725250b56f24a8649f35694f859b)
  ] remove unused dependencies [`Emmanuel Bosquet`] (`2023-12-04`)
- [
  [`0e62ff3`](https://github.com/sozu-proxy/sozu/commit/0e62ff3ca80bbb4b78c533ef9723b9fc19e8ce66)
  ] refactor cli display by creating Response::display [`Emmanuel Bosquet`]
  (`2023-12-06`)
- [
  [`95de156`](https://github.com/sozu-proxy/sozu/commit/95de156c533a67dde6e7135bf0e5bf96b7ea4cb6)
  ] display no other lines than JSON [`Emmanuel Bosquet`] (`2023-12-06`)
- [
  [`86303a2`](https://github.com/sozu-proxy/sozu/commit/86303a2183a555c5c5c55d08acd3310ceeff0a94)
  ] ConfigState::cluster_state return Option<ClusterInformation> [`Emmanuel
  Bosquet`] (`2023-12-06`)
- [
  [`4d1500a`](https://github.com/sozu-proxy/sozu/commit/4d1500a0a5b70e4460e09a36aa27d8063dc7937e)
  ] chore: reduce verbosity of a few logs [`Florentin Dubois`] (`2023-12-07`)
- [
  [`3c6ef35`](https://github.com/sozu-proxy/sozu/commit/3c6ef359d16d04e07b59df1878b9998f1f31205e)
  ] Better logging for parsing errors [`Eloi DEMOLIS`] (`2023-12-07`)
- [
  [`63e76c7`](https://github.com/sozu-proxy/sozu/commit/63e76c7d1da2aeaa0ab8565772e88a089f0c36da)
  ] chore: reduce logging level [`Florentin Dubois`] (`2023-12-08`)
- [
  [`0f0ed1f`](https://github.com/sozu-proxy/sozu/commit/0f0ed1f80c37eb2ff0c2611e3d0654f9b7e418a9)
  ] workers return only one response when dispatching a request [`Emmanuel
  Bosquet`] (`2023-12-12`)
- [
  [`ef6e99a`](https://github.com/sozu-proxy/sozu/commit/ef6e99ad46cf302fa6a5fa9b67cc6908f3561d3b)
  ] chore(http,https): update warning message with frontend token [`Florentin
  Dubois`] (`2023-12-13`)
- [
  [`32d8e3a`](https://github.com/sozu-proxy/sozu/commit/32d8e3ac91025c764a27e54b461c717ae036bddf)
  ] chore: update rustls to 0.21.10 [`Florentin Dubois`] (`2023-12-13`)
- [
  [`c887666`](https://github.com/sozu-proxy/sozu/commit/c88766694ff10a5ac9f1d7f17e7f7cb0ec919ff6)
  ] better logging of back error [`Emmanuel Bosquet`] (`2023-11-22`)
- [
  [`4a444b1`](https://github.com/sozu-proxy/sozu/commit/4a444b14df4d09d75c6cad37a7c1235a86248d5a)
  ] Mutualize MAX_LOOP_ITERATIONS in config [`Eloi DEMOLIS`] (`2023-11-23`)
- [
  [`730f0c3`](https://github.com/sozu-proxy/sozu/commit/730f0c329917a1da9a09d0dfcbc3799e9a2288d5)
  ] Adjust logging level [`Eloi DEMOLIS`] (`2023-11-23`)

### 🥹 Contributors

- @keksoj
- @FlorentinDUBOIS
- @Wonshtrum

**Full Changelog**: https://github.com/sozu-proxy/sozu/compare/0.15.15...0.15.19

## 0.15.15 - 2023-11-15

> This changelog merges all modifications between versions 0.15.13 and 0.15.15

- Since the deployment of the version 0.15.x at Clever Cloud, we have seen some
  performance issues around tls handshake and we made several efforts to dig in
  and fix them, see
  [`8364454`](https://github.com/sozu-proxy/sozu/commit/8364454da2ac4df3ea8fae517f619431ac0c068e)
  and
  [`92a277c`](https://github.com/sozu-proxy/sozu/commit/92a277c79fa0d319a0f8ad1f192d62b72ffd52a1).
- We have fix a bug when we replace a tls certificate that resolve the old onem
  once replaced, see
  [`50afe7a`](https://github.com/sozu-proxy/sozu/commit/50afe7aa0e33b5d583a301de40f17772eb72c213)
- We also allow to choose the number of ticket tls given to a new tls handshake,
  see
  [`0c3c129`](https://github.com/sozu-proxy/sozu/commit/0c3c129647baae1f0972c7f8af78cbb1200dd78e).
- Update the systemd service to set start interval and burst, see
  [`af5ea00`](https://github.com/sozu-proxy/sozu/commit/af5ea0025eeed64c8ccfafa8387f0a1a4aef8d88).
- We also document a way to benchmark sozu, see
  [`e754a15`](https://github.com/sozu-proxy/sozu/commit/e754a159dc9abf34285c2f33970e6ecbee765e6e).

### Changelog

#### 🚀 Performance

- [
  [`8364454`](https://github.com/sozu-proxy/sozu/commit/8364454da2ac4df3ea8fae517f619431ac0c068e)
  ] Use rustls::Writer::write_vectored to reduce writev syscalls [`Eloi
  DEMOLIS`] (`2023-11-08`)
- [
  [`92a277c`](https://github.com/sozu-proxy/sozu/commit/92a277c79fa0d319a0f8ad1f192d62b72ffd52a1)
  ] store certificates in parsed form in CertificateResolver [`Eloi DEMOLIS`]
  (`2023-11-14`)

#### ⛑️ Fixed

- [
  [`50afe7a`](https://github.com/sozu-proxy/sozu/commit/50afe7aa0e33b5d583a301de40f17772eb72c213)
  ] fix(tls): certificate replacement and remove is still-in-use security
  [`Florentin Dubois`] (`2023-11-14`)

#### ✍️ Changed

- [
  [`0c3c129`](https://github.com/sozu-proxy/sozu/commit/0c3c129647baae1f0972c7f8af78cbb1200dd78e)
  ] make send_tls13_tickets configurable [`Emmanuel Bosquet`] (`2023-11-09`)
- [
  [`1406954`](https://github.com/sozu-proxy/sozu/commit/140695475a38afa6f461a82d46b19fb35778b4e9)
  ] Remove rustls backpressuring flag [`Eloi DEMOLIS`] (`2023-11-08`)
- [
  [`9b29dcf`](https://github.com/sozu-proxy/sozu/commit/9b29dcfa98c95626f641013a0c7615529505e0f2)
  ] proper logging of RouterError::RouteNotFound [`Emmanuel Bosquet`]
  (`2023-11-13`)
- [
  [`af5ea00`](https://github.com/sozu-proxy/sozu/commit/af5ea0025eeed64c8ccfafa8387f0a1a4aef8d88)
  ] distribution(systemd): set start limit interval and burst [`Florentin
  Dubois`] (`2023-11-14`)
- [
  [`cc12789`](https://github.com/sozu-proxy/sozu/commit/cc12789f4516d217fb15a7d8b8dd7b5848fc211d)
  ] comments and renaming in lib::tls [`Emmanuel Bosquet`] (`2023-11-14`)

#### 📚 Documentation

- [
  [`e754a15`](https://github.com/sozu-proxy/sozu/commit/e754a159dc9abf34285c2f33970e6ecbee765e6e)
  ] document benchmarking technique [`Emmanuel Bosquet`] (`2023-11-10`)

### 🥹 Contributors

- @keksoj
- @FlorentinDUBOIS
- @Wonshtrum

**Full Changelog**: https://github.com/sozu-proxy/sozu/compare/0.15.13...0.15.15

## 0.15.13 - 2023-10-27

> This changelog merge all modifications between versions 0.15.6 and 0.15.13

- We have deployed the new release of Sōzu at Clever Cloud on production and
  find out some bugs during the deployment process, see
  [`5d2f3b9`](https://github.com/sozu-proxy/sozu/commit/5d2f3b9de024c538577baf3ef2c6f4ab9b60e236),
  [`7b61c04`](https://github.com/sozu-proxy/sozu/commit/7b61c043fb7d5e2cb63113627376b5eb85bcea1c),
  [`72e9d44`](https://github.com/sozu-proxy/sozu/commit/72e9d4497e9326c3538fed1088cb26b9524c0700),
  [`bf026ee`](https://github.com/sozu-proxy/sozu/commit/bf026ee8ecf9dda2dd75c672baf3b231bc1d3231),
  [`76e0e7d`](https://github.com/sozu-proxy/sozu/commit/76e0e7d6ce2a74e93d4c75ea2ab891aa2d92c45d),
  [`0bdf61d`](https://github.com/sozu-proxy/sozu/commit/0bdf61d235406868c770c0199167d10e59809e53),
  [`89bf73a`](https://github.com/sozu-proxy/sozu/commit/89bf73af57218260e3579a5004eebd61bca18196),
  [`1196a90`](https://github.com/sozu-proxy/sozu/commit/1196a900d3759fbc579bf4434e5f945b45980790),
  [`e562299`](https://github.com/sozu-proxy/sozu/commit/e562299d5e140f9ae133f6692f47eaf0f31ad343),
  [`4c47cfc`](https://github.com/sozu-proxy/sozu/commit/4c47cfc75ee125d942c33849e9107f4b879aec0f),
  [`cda2f01`](https://github.com/sozu-proxy/sozu/commit/cda2f01789b4abde2ef4441d85be636b6e589384),
  [`ea0b8af`](https://github.com/sozu-proxy/sozu/commit/ea0b8afefeaaafb11a9a9fb27fa1e8348378829f)
  and
  [`437eb12`](https://github.com/sozu-proxy/sozu/commit/437eb1252f4f999001dac7d162694dd455dfa057).
- We have added debug logging, see
  [`8854576`](https://github.com/sozu-proxy/sozu/commit/88545767284284c31b8c13dab90d581b39c07b56)
  and
  [`887babe`](https://github.com/sozu-proxy/sozu/commit/887babe4c0ec81d8c73f4054af837222acb2a076).
- We now retrieve subject alternative names for certificate, see
  [`ea6bacd`](https://github.com/sozu-proxy/sozu/commit/ea6bacd463d5fd085fa77e411c73ae9e2e94ebbe).
- We have enable metrics of clusters by default and add some error status code,
  see
  [`9648cf0`](https://github.com/sozu-proxy/sozu/commit/9648cf0433df13bd84efbb14dfd8321b520a91e2)
  and
  [`6b53071`](https://github.com/sozu-proxy/sozu/commit/6b53071303eb56fe45e0242b756fa73bb1fb16d1).
- We have updated sozu to hot reload logging level not only for the main
  processn but also workers, see
  [`641daa3`](https://github.com/sozu-proxy/sozu/commit/641daa3fc86b7883bd794c6dc9f0c601c9289d24)?

### Changelog

#### 📚 Documentation/sozu/commit/be2cfe6da18d7098565b2526b3127651eb8384b9) ] Add 507 default answer [`Eloi DEMOLIS`] (`2023-10-24`)

#### ⛑️ Fixed

- [
  [`5d2f3b9`](https://github.com/sozu-proxy/sozu/commit/5d2f3b9de024c538577baf3ef2c6f4ab9b60e236)
  ] fix misleading CLI line on state saving [`Emmanuel Bosquet`] (`2023-10-27`)
- [
  [`7b61c04`](https://github.com/sozu-proxy/sozu/commit/7b61c043fb7d5e2cb63113627376b5eb85bcea1c)
  ] build: add missing assets [`Florentin Dubois`] (`2023-10-27`)
- [
  [`72e9d44`](https://github.com/sozu-proxy/sozu/commit/72e9d4497e9326c3538fed1088cb26b9524c0700)
  ] Don't override X-Forwarded-Proto and X-Forwarded-Port [`Eloi DEMOLIS`]
  (`2023-10-26`)
- [
  [`bf026ee`](https://github.com/sozu-proxy/sozu/commit/bf026ee8ecf9dda2dd75c672baf3b231bc1d3231)
  ] Add a default certificate when none are found for a host [`Eloi DEMOLIS`]
  (`2023-10-27`)
- [
  [`76e0e7d`](https://github.com/sozu-proxy/sozu/commit/76e0e7d6ce2a74e93d4c75ea2ab891aa2d92c45d)
  ] Fix early connect trials [`Eloi DEMOLIS`] (`2023-10-24`)
- [
  [`0bdf61d`](https://github.com/sozu-proxy/sozu/commit/0bdf61d235406868c770c0199167d10e59809e53)
  ] fix cluster metrics [`Emmanuel Bosquet`] (`2023-10-24`)
- [
  [`89bf73a`](https://github.com/sozu-proxy/sozu/commit/89bf73af57218260e3579a5004eebd61bca18196)
  ] fix(timeout): implements cancel on drop [`Florentin Dubois`] (`2023-10-23`)
- [
  [`1196a90`](https://github.com/sozu-proxy/sozu/commit/1196a900d3759fbc579bf4434e5f945b45980790)
  ] include TCP clusters in command 'cluster list' [`Emmanuel Bosquet`]
  (`2023-09-19`)
- [
  [`e562299`](https://github.com/sozu-proxy/sozu/commit/e562299d5e140f9ae133f6692f47eaf0f31ad343)
  ] Fix TrieNode wildcard and regexp management [`Eloi DEMOLIS`] (`2023-10-17`)
- [
  [`4c47cfc`](https://github.com/sozu-proxy/sozu/commit/4c47cfc75ee125d942c33849e9107f4b879aec0f)
  ] fix the display of non-existing cluster information in cluster list
  [`Emmanuel Bosquet`] (`2023-10-13`)
- [
  [`cda2f01`](https://github.com/sozu-proxy/sozu/commit/cda2f01789b4abde2ef4441d85be636b6e589384)
  ] Fix X-Forwarded-Port when not present [`Eloi DEMOLIS`] (`2023-10-20`)
- [
  [`ea0b8af`](https://github.com/sozu-proxy/sozu/commit/ea0b8afefeaaafb11a9a9fb27fa1e8348378829f)
  ] fix(rustls): read buffer if we received a bufffer full error instead of
  processing new packets [`Florentin Dubois`] (`2023-10-21`)
- [
  [`437eb12`](https://github.com/sozu-proxy/sozu/commit/437eb1252f4f999001dac7d162694dd455dfa057)
  ] fix: allow to read [`Florentin Dubois`] (`2023-10-21`)

#### ✍️ Changed

- [
  [`8854576`](https://github.com/sozu-proxy/sozu/commit/88545767284284c31b8c13dab90d581b39c07b56)
  ] Add log on suspicious X-Forwarded-Proto and Port [`Eloi DEMOLIS`]
  (`2023-10-27`)
- [
  [`ea6bacd`](https://github.com/sozu-proxy/sozu/commit/ea6bacd463d5fd085fa77e411c73ae9e2e94ebbe)
  ] Get Subject Alternative Names from extensions [`Eloi DEMOLIS`]
  (`2023-10-25`)
- [
  [`8595cf9`](https://github.com/sozu-proxy/sozu/commit/8595cf9e8aeae1d4c1fc5f8111f9c27d68dc3613)
  ] Remove early read on TLS upgrade [`Eloi DEMOLIS`] (`2023-10-24`)
- [
  [`9648cf0`](https://github.com/sozu-proxy/sozu/commit/9648cf0433df13bd84efbb14dfd8321b520a91e2)
  ] enable cluster metrics by default [`Emmanuel Bosquet`] (`2023-10-23`)
- [
  [`6b53071`](https://github.com/sozu-proxy/sozu/commit/6b53071303eb56fe45e0242b756fa73bb1fb16d1)
  ] save 4xx and 5xx status codes in cluster metrics [`Emmanuel Bosquet`]
  (`2023-10-23`)
- [
  [`a1d60b2`](https://github.com/sozu-proxy/sozu/commit/a1d60b2fe3203fbf9b2c36c498c1c6f9629b7c20)
  ] more sensible CLI defaults params in config.toml [`Emmanuel Bosquet`]
  (`2023-09-21`)
- [
  [`641daa3`](https://github.com/sozu-proxy/sozu/commit/641daa3fc86b7883bd794c6dc9f0c601c9289d24)
  ] send logging level change requests to workers [`Emmanuel Bosquet`]
  (`2023-10-18`)
- [
  [`887babe`](https://github.com/sozu-proxy/sozu/commit/887babe4c0ec81d8c73f4054af837222acb2a076)
  ] chore: increase logs on access error [`Florentin Dubois`] (`2023-10-21`)

#### 📚 Documentation

- [
  [`9301048`](https://github.com/sozu-proxy/sozu/commit/9301048af9ec64517bf06a7d2f38181fbf1eeae8)
  ] doc(changelog): add 0.15.6 entry [`Florentin Dubois`] (`2023-10-11`)

### 🥹 Contributors

- @keksoj
- @FlorentinDUBOIS
- @Wonshtrum

**Full Changelog**: https://github.com/sozu-proxy/sozu/compare/0.15.6...0.15.13

## 0.15.6 - 2023-10-11

### ⛑️ Fixed

- Fix behaviour on missing `X-Forwarded-Proto` and `X-Forwarded-Port`, we add
  them in that case, see
  [`c09e17a`](https://github.com/sozu-proxy/sozu/commit/c09e17a4bc5d8ff45694402dd7521e50320cb262).
- Fix behaviour on kawa parser when we detect a header `Content-Length` on
  `HEAD` requests, see
  [`7d89372`](https://github.com/sozu-proxy/sozu/commit/7d8937267462be3dea343fba76ec2c6ac1671da3).

### Changelog

#### ⛑️ Fixed

- [
  [`c09e17a`](https://github.com/sozu-proxy/sozu/commit/c09e17a4bc5d8ff45694402dd7521e50320cb262)
  ] Fix X-Forwarded-Proto and X-Forwarded-Port (add them when not present)
  [`Eloi DEMOLIS`] (`2023-10-11`)
- [
  [`7d89372`](https://github.com/sozu-proxy/sozu/commit/7d8937267462be3dea343fba76ec2c6ac1671da3)
  ] Fix responses to head requests (ignore body length) [`Eloi DEMOLIS`]
  (`2023-10-11`)

#### ✍️ Changed

- [
  [`a52e750`](https://github.com/sozu-proxy/sozu/commit/a52e750e3f5e1a95a2d29f13edbb27908a28e3ad)
  ] doc(changelog): add 0.15.5 entry [`Florentin Dubois`] (`2023-09-21`)
- [
  [`4ffaf2b`](https://github.com/sozu-proxy/sozu/commit/4ffaf2b1feea57f2557722a7de9f63e58c673915)
  ] chore: update dependencies [`Florentin Dubois`] (`2023-10-11`)
- [
  [`6de9cf5`](https://github.com/sozu-proxy/sozu/commit/6de9cf541368fca3d874b14df7e068a856d4d183)
  ] chore: update dependencies [`Florentin Dubois`] (`2023-09-21`)

### 🥹 Contributors

- @FlorentinDUBOIS
- @Wonshtrum

**Full Changelog**: https://github.com/sozu-proxy/sozu/compare/0.15.5...0.15.6

## 0.15.5 - 2023-09-21

### ⛑️ Fixed

We fix a bug that can occurs with pki using T.61 charset, see
[`a5412b9`](https://github.com/sozu-proxy/sozu/commit/a5412b9764e860eedc2a206b16e81144946a8d7f).

### Changelog

#### ⛑️ Fixed

- [
  [`a5412b9`](https://github.com/sozu-proxy/sozu/commit/a5412b9764e860eedc2a206b16e81144946a8d7f)
  ] fix(command): retrieve name and san from slice [`Florentin Dubois`]
  (`2023-09-21`)
- [
  [`24c4407`](https://github.com/sozu-proxy/sozu/commit/24c4407d654dfbcd7c490e3a23c46fe8289bce4e)
  ] chore: update changelog to add 0.15.4 [`Florentin Dubois`] (`2023-09-13`)
- [
  [`6de9cf5`](https://github.com/sozu-proxy/sozu/commit/6de9cf541368fca3d874b14df7e068a856d4d183)
  ] chore: update dependencies [`Florentin Dubois`] (`2023-09-21`)

### 🥹 Contributors

- @FlorentinDUBOIS

**Full Changelog**: https://github.com/sozu-proxy/sozu/compare/0.15.4...0.15.5

## 0.15.4 - 2023-09-13

### 🌟 Features

- Expose SIMD as a feature flag and enable it by default, see
  [`9df3f1d`](https://github.com/sozu-proxy/sozu/commit/9df3f1d718e269585aae133fa746362c6cec6a1b).
- Improve documentation, see
  [`d2f1621`](https://github.com/sozu-proxy/sozu/commit/d2f1621e67c214707174ecfb1d03b1c1adbe8455),
  [`e9c185e`](https://github.com/sozu-proxy/sozu/commit/e9c185e6139b7123206513d3a4fee2087977ec2a),
  [`bd5703a`](https://github.com/sozu-proxy/sozu/commit/bd5703ae313a8ab270eba4b1ebe3d9ad911b2793).
- We did some cleaning on unused source code, see
  [`31c26b4`](https://github.com/sozu-proxy/sozu/commit/31c26b46d544259cfb9d8a219ed2cd5fbb9b2875),
  [`c25d483`](https://github.com/sozu-proxy/sozu/commit/c25d483fe6d1065e8e98c9c45379e05205d94ff0),
  [`4aceb86`](https://github.com/sozu-proxy/sozu/commit/4aceb866529979ac113cf17580e04d3637ab362b),
  [`001aa89`](https://github.com/sozu-proxy/sozu/commit/001aa897ee001b30859fa07a414f360f0004edb8).
- We update the minimum supported rust version to `1.70.0`, see
  [`02892b8`](https://github.com/sozu-proxy/sozu/commit/02892b83ccc1aa165cd5dc58637fc4ca6282917b).

### ⛑️ Fixed

- Fix unit and end-to-end (e2e) tests, see
  [`2b84a4b`](https://github.com/sozu-proxy/sozu/commit/2b84a4bf88da6077f3371871634bce4ae6204be0),
  [`4aceb86`](https://github.com/sozu-proxy/sozu/commit/4aceb866529979ac113cf17580e04d3637ab362b),
  [`818bc48`](https://github.com/sozu-proxy/sozu/commit/818bc4822517499e4ae9dc728bb5e15198c4da93),
  [`56dce47`](https://github.com/sozu-proxy/sozu/commit/56dce47e364f2a36d7fb8d1c82bc6d2852eec25d).
- Fix certificate issue at loading, see
  [`daaeb19`](https://github.com/sozu-proxy/sozu/commit/daaeb19f3b87a164dbdd3317444e437ccfc459fc).

### Changelog

#### 🌟 Features

- [
  [`9df3f1d`](https://github.com/sozu-proxy/sozu/commit/9df3f1d718e269585aae133fa746362c6cec6a1b)
  ] introduce SIMD as default feature [`Emmanuel Bosquet`] (`2023-09-13`)

#### ➕ Added

- [
  [`2b84a4b`](https://github.com/sozu-proxy/sozu/commit/2b84a4bf88da6077f3371871634bce4ae6204be0)
  ] create PortProvider in e2e tests [`Emmanuel Bosquet`] (`2023-09-12`)

#### ✍️ Changed

- [
  [`31c26b4`](https://github.com/sozu-proxy/sozu/commit/31c26b46d544259cfb9d8a219ed2cd5fbb9b2875)
  ] remove buffer_queue, useless since introduction of kawa [`Emmanuel Bosquet`]
  (`2023-09-13`)
- [
  [`c25d483`](https://github.com/sozu-proxy/sozu/commit/c25d483fe6d1065e8e98c9c45379e05205d94ff0)
  ] remove unused dependencies [`Emmanuel Bosquet`] (`2023-09-13`)
- [
  [`f9c4ddb`](https://github.com/sozu-proxy/sozu/commit/f9c4ddb703ec943ed94297d4837163d5d88fa9d8)
  ] update dependencies [`Emmanuel Bosquet`] (`2023-09-13`)
- [
  [`90e3bc7`](https://github.com/sozu-proxy/sozu/commit/90e3bc7f7838c9a5cdaa3e2b8df3f171f55fd127)
  ] cargo fmt [`Emmanuel Bosquet`] (`2023-09-13`)
- [
  [`4aceb86`](https://github.com/sozu-proxy/sozu/commit/4aceb866529979ac113cf17580e04d3637ab362b)
  ] remove serial aspect of e2e tests [`Emmanuel Bosquet`] (`2023-09-12`)
- [
  [`f0661a5`](https://github.com/sozu-proxy/sozu/commit/f0661a54bc56b1e4e2b41935e4d0dd6834cde1b1)
  ] update dependencies [`Emmanuel Bosquet`] (`2023-09-11`)
- [
  [`02892b8`](https://github.com/sozu-proxy/sozu/commit/02892b83ccc1aa165cd5dc58637fc4ca6282917b)
  ] set rust-toolchain and rust-version to 1.70.0 [`Emmanuel Bosquet`]
  (`2023-09-11`)

#### 🚀 Refactored

- [
  [`5b14713`](https://github.com/sozu-proxy/sozu/commit/5b1471314d63e0f64bd184f673426664f0e23bda)
  ] merge use statements in kawa_h1::answers [`Emmanuel Bosquet`] (`2023-09-13`)
- [
  [`001aa89`](https://github.com/sozu-proxy/sozu/commit/001aa897ee001b30859fa07a414f360f0004edb8)
  ] remove useless test crate import [`Emmanuel Bosquet`] (`2023-09-13`)
- [
  [`8c164f9`](https://github.com/sozu-proxy/sozu/commit/8c164f9a91b001305d910333eac2ab742feca017)
  ] use TryFrom in prost::Decode [`Emmanuel Bosquet`] (`2023-09-13`)

#### ⛑️ Fixed

- [
  [`818bc48`](https://github.com/sozu-proxy/sozu/commit/818bc4822517499e4ae9dc728bb5e15198c4da93)
  ] fix test for 101 HTTP behavior [`Emmanuel Bosquet`] (`2023-09-13`)
- [
  [`daaeb19`](https://github.com/sozu-proxy/sozu/commit/daaeb19f3b87a164dbdd3317444e437ccfc459fc)
  ] remove skipping of certificate update in GenericCertificateResolver
  [`Emmanuel Bosquet`] (`2023-09-12`)
- [
  [`56dce47`](https://github.com/sozu-proxy/sozu/commit/56dce47e364f2a36d7fb8d1c82bc6d2852eec25d)
  ] fix 103 early hint e2e test [`Emmanuel Bosquet`] (`2023-09-12`)

#### 📚 Documentation

- [
  [`d2f1621`](https://github.com/sozu-proxy/sozu/commit/d2f1621e67c214707174ecfb1d03b1c1adbe8455)
  ] fix(readme): Define covered work interpretation #764 [`Steven LE ROUX`]
  (`2023-08-30`)
- [
  [`e9c185e`](https://github.com/sozu-proxy/sozu/commit/e9c185e6139b7123206513d3a4fee2087977ec2a)
  ] remove doc lines about a removed systemd script [`Emmanuel Bosquet`]
  (`2023-08-24`)
- [
  [`bd5703a`](https://github.com/sozu-proxy/sozu/commit/bd5703ae313a8ab270eba4b1ebe3d9ad911b2793)
  ] fix doc link to systemd unit file [`Emmanuel Bosquet`] (`2023-08-24`)

### 🥹 Contributors

- @keksoj
- @Wonshtrum
- @FlorentinDUBOIS

**Full Changelog**: https://github.com/sozu-proxy/sozu/compare/0.15.3...0.15.4

## 0.15.3 - 2023-08-09

### 🌟 Features

- We have reworked the error management in the command library to remove anyhow
  in favor of thiserror, the idea behind is to give to the binary or library
  that use this crate to better understand the error and be able to
  differentiate, if something is going wrong or not and take actions, see
  [`e7b530d`](https://github.com/sozu-proxy/sozu/commit/e7b530d8a944bbf99ee2d1039be47c22e3bcff3c),
  [`d2e1dcc`](https://github.com/sozu-proxy/sozu/commit/d2e1dcc59b4912ef24898a7ed453836ce6199870),
  [`d0389d8`](https://github.com/sozu-proxy/sozu/commit/d0389d8dcf371c4822cfb0a348374d1f2e612421),
  [`447bed5`](https://github.com/sozu-proxy/sozu/commit/447bed56f5be96a71a551d38efcd78c4add3ff09),
  [`aa55b97`](https://github.com/sozu-proxy/sozu/commit/aa55b97d506a2bab6d72a6262e56ae8b24193cb6),
  [`0a92877`](https://github.com/sozu-proxy/sozu/commit/0a928770352e042d2115990bf68b2874360e1a14),
  [`a04fe34`](https://github.com/sozu-proxy/sozu/commit/a04fe34b326410dc3325a612ceef121a30c5b30f),
  [`e1e7ce2`](https://github.com/sozu-proxy/sozu/commit/e1e7ce29f1bcb6d75d6494069f5efeee3aada16c),
  [`bdde240`](https://github.com/sozu-proxy/sozu/commit/bdde240aebd97d9e4983753f42ef334e49223d03),
  [`d109ccc`](https://github.com/sozu-proxy/sozu/commit/d109ccc78d84f7931e27ff9fc2d17678700f4a1c),
  [`bda6913`](https://github.com/sozu-proxy/sozu/commit/bda69136d27a132f489775a5104cefa84b502399),
  [`cc341e3`](https://github.com/sozu-proxy/sozu/commit/cc341e39beb3c45ba01ddbec99bd6b28d2ce28e3),
  [`5c7a8ef`](https://github.com/sozu-proxy/sozu/commit/5c7a8efb169f9120c8f58b5c0b999545ae51abdd),
  [`b919515`](https://github.com/sozu-proxy/sozu/commit/b9195155fb57853397ef9d4815b888684df7dff2)
  and
  [`f9353d2`](https://github.com/sozu-proxy/sozu/commit/f9353d21b23ca06f9ecdef6db86514e786fa4bd9).
- We have improved the telemetry to expose the cluster id and the backend id if
  available, see
  [`b8e1017`](https://github.com/sozu-proxy/sozu/commit/b8e1017408eaec71127ffc0067b7944475fcf261),
  [`471c46f`](https://github.com/sozu-proxy/sozu/commit/471c46fbf553d7ecb776cb952ea489cbeac666d2),
  [`9aee4da`](https://github.com/sozu-proxy/sozu/commit/9aee4da9e7d837423ab36b8570f93eb60b09ea9f),
  [`11ff5bd`](https://github.com/sozu-proxy/sozu/commit/11ff5bdcfe960880cdb28b5ea05a1ab838ece1c0),
  [`8334bc7`](https://github.com/sozu-proxy/sozu/commit/8334bc71b106c991a9af90dd19eb214a533ec55b),
  [`6df1802`](https://github.com/sozu-proxy/sozu/commit/6df1802275c7ed9206511826c0c738e59f9d4286),
  [`e8caac8`](https://github.com/sozu-proxy/sozu/commit/e8caac8d7ed133ac138f49843cfc0a52d5716ca7),
  [`295c2b6`](https://github.com/sozu-proxy/sozu/commit/295c2b659a718cae271dbc42faefad66f4ebcc18),
  [`0acf06d`](https://github.com/sozu-proxy/sozu/commit/0acf06de26dd425223683d2286c835c69a813890),
  [`3feeca8`](https://github.com/sozu-proxy/sozu/commit/3feeca8f4f9e9754e323cac5901421b5011e7318),
  [`182b579`](https://github.com/sozu-proxy/sozu/commit/182b5791eb6abd5091c3a14d876bf4701d3a0055),
  [`c2b4c9c`](https://github.com/sozu-proxy/sozu/commit/c2b4c9c419b4fed2007a744cfcb80de6b225e23b)
  and
  [`0ecf4cc`](https://github.com/sozu-proxy/sozu/commit/0ecf4cce727723b671327464ab9b76c2f531f517).

### ⛑️ Fixed

- Fix the loading of configuration from a file that was limited to the buffer
  size, see
  [`df69ba6`](https://github.com/sozu-proxy/sozu/commit/df69ba6fa1b99866506c9bde54f18d55f848236a).
- Fix the display of domain names in the command line, see
  [`14868dd`](https://github.com/sozu-proxy/sozu/commit/14868dd65d8a32980c9a57d434bbab032ea516bb)
  and
  [`c738545`](https://github.com/sozu-proxy/sozu/commit/c7385458299bbecce39e90707f3d3808338a02a9).

### Changelog

#### ➕ Added

- [
  [`b8e1017`](https://github.com/sozu-proxy/sozu/commit/b8e1017408eaec71127ffc0067b7944475fcf261)
  ] fix metrics macros import scopes [`hcaumeil`] (`2023-08-02`)
- [
  [`471c46f`](https://github.com/sozu-proxy/sozu/commit/471c46fbf553d7ecb776cb952ea489cbeac666d2)
  ] renaming metrics for consistancy [`hcaumeil`] (`2023-08-02`)
- [
  [`9aee4da`](https://github.com/sozu-proxy/sozu/commit/9aee4da9e7d837423ab36b8570f93eb60b09ea9f)
  ] make http status metrics and access logs metrics cluster related
  [`hcaumeil`] (`2023-08-02`)
- [
  [`11ff5bd`](https://github.com/sozu-proxy/sozu/commit/11ff5bdcfe960880cdb28b5ea05a1ab838ece1c0)
  ] add user-agent in access logs [`hcaumeil`] (`2023-08-02`)
- [
  [`8334bc7`](https://github.com/sozu-proxy/sozu/commit/8334bc71b106c991a9af90dd19eb214a533ec55b)
  ] add path matching time metrics [`hcaumeil`] (`2023-08-02`)
- [
  [`6df1802`](https://github.com/sozu-proxy/sozu/commit/6df1802275c7ed9206511826c0c738e59f9d4286)
  ] rename metric connections.error to backend.connections.error [`hcaumeil`]
  (`2023-08-02`)
- [
  [`e8caac8`](https://github.com/sozu-proxy/sozu/commit/e8caac8d7ed133ac138f49843cfc0a52d5716ca7)
  ] make http.301.redirection metric cluster related [`hcaumeil`] (`2023-08-02`)
- [
  [`295c2b6`](https://github.com/sozu-proxy/sozu/commit/295c2b659a718cae271dbc42faefad66f4ebcc18)
  ] cleaner error handling [`hcaumeil`] (`2023-08-03`)
- [
  [`0acf06d`](https://github.com/sozu-proxy/sozu/commit/0acf06de26dd425223683d2286c835c69a813890)
  ] better formating [`hcaumeil`] (`2023-08-03`)
- [
  [`3feeca8`](https://github.com/sozu-proxy/sozu/commit/3feeca8f4f9e9754e323cac5901421b5011e7318)
  ] variable rename for clarity [`hcaumeil`] (`2023-08-03`)
- [
  [`182b579`](https://github.com/sozu-proxy/sozu/commit/182b5791eb6abd5091c3a14d876bf4701d3a0055)
  ] rename up and down metrics for clarity [`hcaumeil`] (`2023-08-08`)
- [
  [`c2b4c9c`](https://github.com/sozu-proxy/sozu/commit/c2b4c9c419b4fed2007a744cfcb80de6b225e23b)
  ] make some http 4.x.x status metrics clustered (401,408,413) [`hcaumeil`]
  (`2023-08-08`)
- [
  [`0ecf4cc`](https://github.com/sozu-proxy/sozu/commit/0ecf4cce727723b671327464ab9b76c2f531f517)
  ] chore: print user-agent as a tag value in access logs [`Florentin Dubois`]
  (`2023-08-08`)

#### 🚀 Refactored

- [
  [`e7b530d`](https://github.com/sozu-proxy/sozu/commit/e7b530d8a944bbf99ee2d1039be47c22e3bcff3c)
  ] create CertificateError for the certificate module [`Emmanuel Bosquet`]
  (`2023-07-31`)
- [
  [`d2e1dcc`](https://github.com/sozu-proxy/sozu/commit/d2e1dcc59b4912ef24898a7ed453836ce6199870)
  ] remove anyhow from sozu_command_lib dependencies [`Emmanuel Bosquet`]
  (`2023-07-31`)
- [
  [`d0389d8`](https://github.com/sozu-proxy/sozu/commit/d0389d8dcf371c4822cfb0a348374d1f2e612421)
  ] add thiserror to ConfigState::dispatch [`Emmanuel Bosquet`] (`2023-07-31`)
- [
  [`447bed5`](https://github.com/sozu-proxy/sozu/commit/447bed56f5be96a71a551d38efcd78c4add3ff09)
  ] create ScmSocketError for module scm_socket [`Emmanuel Bosquet`]
  (`2023-07-31`)
- [
  [`aa55b97`](https://github.com/sozu-proxy/sozu/commit/aa55b97d506a2bab6d72a6262e56ae8b24193cb6)
  ] create RequestError for the request module [`Emmanuel Bosquet`]
  (`2023-07-31`)
- [
  [`0a92877`](https://github.com/sozu-proxy/sozu/commit/0a928770352e042d2115990bf68b2874360e1a14)
  ] create FrontendFromRequestError [`Emmanuel Bosquet`] (`2023-07-31`)
- [
  [`a04fe34`](https://github.com/sozu-proxy/sozu/commit/a04fe34b326410dc3325a612ceef121a30c5b30f)
  ] create ServerBindError in socket module [`Emmanuel Bosquet`] (`2023-07-31`)
- [
  [`e1e7ce2`](https://github.com/sozu-proxy/sozu/commit/e1e7ce29f1bcb6d75d6494069f5efeee3aada16c)
  ] create RouterError, ProxyError, extend ListenerError [`Emmanuel Bosquet`]
  (`2023-07-31`)
- [
  [`bdde240`](https://github.com/sozu-proxy/sozu/commit/bdde240aebd97d9e4983753f42ef334e49223d03)
  ] create BackendConnectionError and RetrieveClusterError [`Emmanuel Bosquet`]
  (`2023-07-31`)
- [
  [`d109ccc`](https://github.com/sozu-proxy/sozu/commit/d109ccc78d84f7931e27ff9fc2d17678700f4a1c)
  ] create MetricError in metrics module [`Emmanuel Bosquet`] (`2023-07-31`)
- [
  [`bda6913`](https://github.com/sozu-proxy/sozu/commit/bda69136d27a132f489775a5104cefa84b502399)
  ] put struct Backend in backends module, create BackendError [`Emmanuel
  Bosquet`] (`2023-07-31`)
- [
  [`cc341e3`](https://github.com/sozu-proxy/sozu/commit/cc341e39beb3c45ba01ddbec99bd6b28d2ce28e3)
  ] follow review to the error management [`Emmanuel Bosquet`] (`2023-07-31`)
- [
  [`5c7a8ef`](https://github.com/sozu-proxy/sozu/commit/5c7a8efb169f9120c8f58b5c0b999545ae51abdd)
  ] create ChannelError [`Emmanuel Bosquet`] (`2023-07-31`)
- [
  [`b919515`](https://github.com/sozu-proxy/sozu/commit/b9195155fb57853397ef9d4815b888684df7dff2)
  ] create ConfigError for the config module [`Emmanuel Bosquet`] (`2023-07-31`)
- [
  [`f9353d2`](https://github.com/sozu-proxy/sozu/commit/f9353d21b23ca06f9ecdef6db86514e786fa4bd9)
  ] refactor: use `CertificateError` instead of `ParseTlsVersionError`
  [`Florentin Dubois`] (`2023-08-04`)

#### ⛑️ Fixed

- [
  [`df69ba6`](https://github.com/sozu-proxy/sozu/commit/df69ba6fa1b99866506c9bde54f18d55f848236a)
  ] fix parsing in LoadState [`Emmanuel Bosquet`] (`2023-07-28`)
- [
  [`14868dd`](https://github.com/sozu-proxy/sozu/commit/14868dd65d8a32980c9a57d434bbab032ea516bb)
  ] fix(tls): use right method to get cn and san attributes [`Florentin Dubois`]
  (`2023-08-04`)
- [
  [`c738545`](https://github.com/sozu-proxy/sozu/commit/c7385458299bbecce39e90707f3d3808338a02a9)
  ] fix: retrieve cn and san attributes to display them in command line
  [`Florentin Dubois`] (`2023-08-04`)

#### ✍️ Changed

- [
  [`21d3609`](https://github.com/sozu-proxy/sozu/commit/21d3609ffb3ffd669b831aaa741c5d520a38a20f)
  ] chore: update changelog [`Florentin Dubois`] (`2023-07-17`)
- [
  [`e060e2b`](https://github.com/sozu-proxy/sozu/commit/e060e2bf919c11edab7dfab9517acb1198db27a1)
  ] chore: update year of changelog entries [`Florentin DUBOIS`] (`2023-07-18`)
- [
  [`06a214f`](https://github.com/sozu-proxy/sozu/commit/06a214f73d61ac2f7e5d90c94ad28ac426d68aaf)
  ] comment out proxy protocol v1 tests since v1 is not used in sozu [`Emmanuel
  Bosquet`] (`2023-07-20`)
- [
  [`6b89eca`](https://github.com/sozu-proxy/sozu/commit/6b89eca2ab082c9e756db803d39af563d1014931)
  ] rename CustomError to ParseError [`Emmanuel Bosquet`] (`2023-07-27`)
- [
  [`c60f5ee`](https://github.com/sozu-proxy/sozu/commit/c60f5ee57d8dd90831ac6652629adab38c291a7a)
  ] build: increase minimum supported rust version to 1.67.0 [`Florentin
  Dubois`] (`2023-08-04`)
- [
  [`7aab06d`](https://github.com/sozu-proxy/sozu/commit/7aab06d339ceb8cd392ec69ed812bf41564a38a5)
  ] chore: update dependencies [`Florentin Dubois`] (`2023-08-04`)
- [
  [`df5904e`](https://github.com/sozu-proxy/sozu/commit/df5904e44eccad271af4d31f805b4f5a9a12ab0e)
  ] chore: update dependencies [`Florentin Dubois`] (`2023-08-07`)
- [
  [`2539cfb`](https://github.com/sozu-proxy/sozu/commit/2539cfbbaf9e3411385b9b9b0c0d56fae781d90c)
  ] styles(command): remove unused imports [`Florentin Dubois`] (`2023-08-07`)
- [
  [`9c2cbcc`](https://github.com/sozu-proxy/sozu/commit/9c2cbcc5ecbcfea64786124afae488dfabbf5433)
  ] Remove most clippy warnings, remove front_readiness and back_readiness
  getters [`Eloi DEMOLIS`] (`2023-08-08`)
- [
  [`b80c7e8`](https://github.com/sozu-proxy/sozu/commit/b80c7e8e72240a76f7c10a112c1402d7a0136f60)
  ] chore: update clap to 4.3.21 [`Florentin Dubois`] (`2023-08-09`)
- [
  [`8a5fb9a`](https://github.com/sozu-proxy/sozu/commit/8a5fb9a5907fd0ceda80b5d9b8f83249b509908a)
  ] release: v0.15.3 [`Florentin Dubois`] (`2023-08-09`)

#### 📚 Documentation

- [
  [`0b0fbcb`](https://github.com/sozu-proxy/sozu/commit/0b0fbcbd34b7d00893d8feaea4205e5250d5bbe2)
  ] doc: add documentation on tls-related functions [`Florentin Dubois`]
  (`2023-08-04`)

### 🥹 Contributors

- @keksoj
- @hcaumeil
- @Wonshtrum
- @FlorentinDUBOIS

**Full Changelog**: https://github.com/sozu-proxy/sozu/compare/0.15.2...0.15.3

## 0.15.2 - 2023-07-17

### ⛑️ Fixed

- We have found out a bug around the upgrade from proxy-protocol to http, see
  [`211db27`](https://github.com/sozu-proxy/sozu/commit/211db27fb16bede8765487370fb46020aab10c53).

### Changelog

#### ⛑️ Fixed

- [
  [`211db27`](https://github.com/sozu-proxy/sozu/commit/211db27fb16bede8765487370fb46020aab10c53)
  ] Fix empty interest on expect proxy proto upgrade [`Eloi DEMOLIS`]
  (`2023-07-17`)

#### ✍️ Changed

- [
  [`0a31489`](https://github.com/sozu-proxy/sozu/commit/0a314890e58f321ef0f108346b750e6d06c6108e)
  ] chore(http): reduce log verbosity around the http close method [`Florentin
  Dubois`] (`2023-07-13`)
- [
  [`748bf0f`](https://github.com/sozu-proxy/sozu/commit/748bf0f0917a2d0694460234d83d16b0181aeff2)
  ] chore: update dependencies [`Florentin Dubois`] (`2023-07-17`)
- [
  [`c6446e1`](https://github.com/sozu-proxy/sozu/commit/c6446e17e77b2bb603c6933066da40964f1d0c38)
  ] chore: add changelog entry for release v0.15.1 [`Florentin Dubois`]
  (`2023-07-11`)

### 🥹 Contributors

- @Wonshtrum
- @FlorentinDUBOIS

**Full Changelog**: https://github.com/sozu-proxy/sozu/compare/0.15.1...0.15.2

## 0.15.1 - 2023-07-11

### 🌟 Features

- We have reduce the number of noisy logs to focus on what is really important
  on Sōzu, see [
  [`39f4170`](https://github.com/sozu-proxy/sozu/commit/39f4170ccb15c8f6dd0e9f9855275362f2880674)
  ], [
  [`362cd82`](https://github.com/sozu-proxy/sozu/commit/362cd823cedb52f88ddd5669b34be07609d9ff02)
  ] and [
  [`c92d6bd`](https://github.com/sozu-proxy/sozu/commit/c92d6bdda283ed9bbceabdf15df1da061831722d)
  ].
- We have added the `100 Continue` use case in e2e tests to ensure no regression
  on it, see [
  [`c92d6bd`](https://github.com/sozu-proxy/sozu/commit/c92d6bdda283ed9bbceabdf15df1da061831722d)
  ].

### ⛑️ Fixed

- We have identified a bug that create a loop on cluster that have the
  `https redirect` enabled, see [
  [`675c99d`](https://github.com/sozu-proxy/sozu/commit/675c99d803be559ebf90c051501b5bdc322f3775)
  ].

### Changelog

#### ✍️ Changed

- [
  [`5a3b9b2`](https://github.com/sozu-proxy/sozu/commit/5a3b9b20000c871b874d39830c03b637669422bf)
  ] Update changelog to add v0.15.0 [`Florentin Dubois`] (`2023-06-23`)
- [
  [`dfbd4b0`](https://github.com/sozu-proxy/sozu/commit/dfbd4b09de1def171b5575cfe2d697c9aa58ade7)
  ] chore: update dependencies [`Florentin Dubois`] (`2023-06-30`)
- [
  [`1753869`](https://github.com/sozu-proxy/sozu/commit/17538690a61fbedd953a60ecfbb77a2e08391357)
  ] ci: continue ci even if rust nightly build fail [`Florentin Dubois`]
  (`2023-06-30`)
- [
  [`39f4170`](https://github.com/sozu-proxy/sozu/commit/39f4170ccb15c8f6dd0e9f9855275362f2880674)
  ] comments on logging macros [`Emmanuel Bosquet`] (`2023-07-03`)
- [
  [`362cd82`](https://github.com/sozu-proxy/sozu/commit/362cd823cedb52f88ddd5669b34be07609d9ff02)
  ] chore(https): reduce log level for debug logs [`Florentin Dubois`]
  (`2023-07-11`)
- [
  [`0d33f89`](https://github.com/sozu-proxy/sozu/commit/0d33f8993a0c18a7de5e3c72c1ce6a17c4e62d45)
  ] chore: update dependencies [`Florentin Dubois`] (`2023-07-11`)

#### ➕ Added

- [
  [`c92d6bd`](https://github.com/sozu-proxy/sozu/commit/c92d6bdda283ed9bbceabdf15df1da061831722d)
  ] Ignore 107 error on front socket, add 100-continue case in e2e tests [`Eloi
  DEMOLIS`] (`2023-07-07`)

#### 🚀 Refactored

- [
  [`b26d34c`](https://github.com/sozu-proxy/sozu/commit/b26d34c09ca034e37634c5123445d56822ba7ac5)
  ] rename parse_one_command to parse_one_request [`Emmanuel Bosquet`]
  (`2023-07-03`)

#### ⛑️ Fixed

- [
  [`675c99d`](https://github.com/sozu-proxy/sozu/commit/675c99d803be559ebf90c051501b5bdc322f3775)
  ] fix: redirect to https only if the listener is a http [`Florentin Dubois`]
  (`2023-07-11`)
- [
  [`45e97a9`](https://github.com/sozu-proxy/sozu/commit/45e97a9eacf342e49c5fdfce14fd89cd3b969fb7)
  ] fix(os-build): add missing protobuf dependency [`Florentin Dubois`]
  (`2023-06-30`)
- [
  [`fa5a910`](https://github.com/sozu-proxy/sozu/commit/fa5a910662e4c9c8278fadf0acbb79c7583db220)
  ] fix(os-build): add missing protobuf dependency [`Florentin Dubois`]
  (`2023-06-30`)
- [
  [`97034bd`](https://github.com/sozu-proxy/sozu/commit/97034bd671cd71145f7988d43bb0aacaf42e8a48)
  ] fix: update `start_tcp_worker` to use `TCPListen` variant of `Protocol` enum
  [`Florentin Dubois`] (`2023-07-11`)

### 🥹 Contributors

- @Wonshtrum
- @Keksoj
- @FlorentinDUBOIS

**Full Changelog**: https://github.com/sozu-proxy/sozu/compare/0.15.0...0.15.1

## 0.15.0 - 2023-06-23

### 🌟 Features

- We have added on the command line a way to check for certificate validity that
  print the "not after" and "not before" field of the certificate, see
  [`8ea3768`](https://github.com/sozu-proxy/sozu/commit/8ea3768be11e1dcdcb2b951d310618dacec34061)
- This release is also the first one of that use the crate
  [`kawa`](https://github.com/CleverCloud/kawa) to parse HTTP requests and
  translate them into an intermediate representation. It will be a foundation of
  the H2 integration in Sōzu, see
  [`144bdb6`](https://github.com/sozu-proxy/sozu/commit/144bdb6dfb5707418a82221a4d5e594bbabce038),
  [`5ac6fd7`](https://github.com/sozu-proxy/sozu/commit/5ac6fd7dba7494fe26c9fc8f0ff2f244f34a4c0d),
  [`5ac6fd7`](https://github.com/sozu-proxy/sozu/commit/5ac6fd7dba7494fe26c9fc8f0ff2f244f34a4c0d),
  [`1cff096`](https://github.com/sozu-proxy/sozu/commit/1cff096fa281e20467aada9e231f85dde200288e),
  [`c4fbef0`](https://github.com/sozu-proxy/sozu/commit/c4fbef01740ea81b241eb1eb25e218e6b79bc965),
  [`5c17acf`](https://github.com/sozu-proxy/sozu/commit/5c17acf5f529026d0bab2ef3c12caa91ae30cb9e),
  [`b8fb52e`](https://github.com/sozu-proxy/sozu/commit/b8fb52e86721cc3e9b15f9dcfc76c56d461e2a38),
  [`cd55235`](https://github.com/sozu-proxy/sozu/commit/cd552357893334fc42c74a0fe5d467490a04b8db),
  [`3eec22c`](https://github.com/sozu-proxy/sozu/commit/3eec22cf7fbc2b8e030d5b03411d86f1b8583984),
  [`dc78bc0`](https://github.com/sozu-proxy/sozu/commit/dc78bc07f6f2b4667274deae2f7f56550c5056e4),
  [`1b3bbf2`](https://github.com/sozu-proxy/sozu/commit/1b3bbf22795e66b2cf1b379bddc55716da8a1066)
  and
  [`737072f`](https://github.com/sozu-proxy/sozu/commit/737072f8d625e5964a59f8ddb394f445f066a8ad).
- We have updated the packaging for Arch Linux, Docker, Fedora, Exherbo and
  systemd, see
  [`f2176a3`](https://github.com/sozu-proxy/sozu/commit/f2176a333c492f9ca703d3355d88b981f75b0a45),
  [`81f25b6`](https://github.com/sozu-proxy/sozu/commit/81f25b66dd22d1462af781fb6627d762310f2e82)
  and
  [`d088468`](https://github.com/sozu-proxy/sozu/commit/d0884685806b5ef88097d24f36fa80e6b2bd5e79)

### ⛑️ Fixed

- We have fixed some issues that Mac OS users could be met, see
  [`a93f66e`](https://github.com/sozu-proxy/sozu/commit/a93f66e0525759cb7ce0118af08f0505fc4ff903)
  and
  [`0b435b6`](https://github.com/sozu-proxy/sozu/commit/0b435b654f6a998ca5d49101b85d558f8aed8909).
- We have also reduce the number of logs that Sōzu could be create using debug
  logging level as it may slow down it in few cases, see
  [`09b0bf0`](https://github.com/sozu-proxy/sozu/commit/09b0bf02a5fcfe240bf0c00114da2a42e789f51a).

### ⚡ Breaking changes

- We have renamed the command `query cluster` into `cluster list`, see
  [`cf5964b`](https://github.com/sozu-proxy/sozu/commit/cf5964b67ebbbe35f32bb8ae1e2433c988547b1c).
- We have renamed the command `query certifcate` into `certificate list`, see
  [`ec6c58e`](https://github.com/sozu-proxy/sozu/commit/ec6c58ef42ead1e8213fb38db66e2343042be37b)
  and
  [`acdc8c9`](https://github.com/sozu-proxy/sozu/commit/acdc8c9203fbca8f6e5119673de4526afe6006cc).

### Changelog

#### 🌟 Features

- [
  [`8ea3768`](https://github.com/sozu-proxy/sozu/commit/8ea3768be11e1dcdcb2b951d310618dacec34061)
  ] CLI: show certificate validity [`Emmanuel Bosquet`] (`2023-05-22`)
- [
  [`144bdb6`](https://github.com/sozu-proxy/sozu/commit/144bdb6dfb5707418a82221a4d5e594bbabce038)
  ] First integration of HTX in H1 state [`Eloi DEMOLIS`] (`2023-06-05`)
- [
  [`5ac6fd7`](https://github.com/sozu-proxy/sozu/commit/5ac6fd7dba7494fe26c9fc8f0ff2f244f34a4c0d)
  ] Continue HTX integration for H1: [`Eloi DEMOLIS`] (`2023-06-05`)
- [
  [`1cff096`](https://github.com/sozu-proxy/sozu/commit/1cff096fa281e20467aada9e231f85dde200288e)
  ] Remove unused macros [`Eloi DEMOLIS`] (`2023-06-05`)
- [
  [`c4fbef0`](https://github.com/sozu-proxy/sozu/commit/c4fbef01740ea81b241eb1eb25e218e6b79bc965)
  ] Continue HTX (now Kawa) integration: [`Eloi DEMOLIS`] (`2023-06-05`)
- [
  [`5c17acf`](https://github.com/sozu-proxy/sozu/commit/5c17acf5f529026d0bab2ef3c12caa91ae30cb9e)
  ] Continue Kawa integration: [`Eloi DEMOLIS`] (`2023-06-05`)
- [
  [`b8fb52e`](https://github.com/sozu-proxy/sozu/commit/b8fb52e86721cc3e9b15f9dcfc76c56d461e2a38)
  ] Continue Kawa integration: [`Eloi DEMOLIS`] (`2023-06-05`)
- [
  [`cd55235`](https://github.com/sozu-proxy/sozu/commit/cd552357893334fc42c74a0fe5d467490a04b8db)
  ] Add e2e test max_connections, add accept timeout on e2e sync_backend [`Eloi
  DEMOLIS`] (`2023-06-05`)
- [
  [`3eec22c`](https://github.com/sozu-proxy/sozu/commit/3eec22cf7fbc2b8e030d5b03411d86f1b8583984)
  ] Revisit HTTP timeouts, move Checkout synching to also benifit WSS [`Eloi
  DEMOLIS`] (`2023-06-05`)
- [
  [`dc78bc0`](https://github.com/sozu-proxy/sozu/commit/dc78bc07f6f2b4667274deae2f7f56550c5056e4)
  ] Propagate AcceptError if no Checkout could be assigned to new HTTP session
  [`Eloi DEMOLIS`] (`2023-06-05`)
- [
  [`1b3bbf2`](https://github.com/sozu-proxy/sozu/commit/1b3bbf22795e66b2cf1b379bddc55716da8a1066)
  ] Support 103 Responses: [`Eloi DEMOLIS`] (`2023-06-16`)
- [
  [`737072f`](https://github.com/sozu-proxy/sozu/commit/737072f8d625e5964a59f8ddb394f445f066a8ad)
  ] introduce access_logs.count metric [`Emmanuel Bosquet`] (`2023-06-16`)

#### ✍️ Changed

- [
  [`f193370`](https://github.com/sozu-proxy/sozu/commit/f1933702c50632a19b35bf6801550b4fa8688528)
  ] test ConfigState::get_certificate_by_fingerprint [`Emmanuel Bosquet`]
  (`2023-05-22`)
- [
  [`ca18682`](https://github.com/sozu-proxy/sozu/commit/ca18682e5b20f0d5c248fb7d2f12f8fbd2a79308)
  ] query the state for a certificate, by domain [`Emmanuel Bosquet`]
  (`2023-05-22`)
- [
  [`308f22f`](https://github.com/sozu-proxy/sozu/commit/308f22f18e560fae220fd1f0a07d318865ca820c)
  ] remove type CertificateWithNames [`Emmanuel Bosquet`] (`2023-05-22`)
- [
  [`3286204`](https://github.com/sozu-proxy/sozu/commit/32862043141ce3368d5ece8be613aeb8afeb545b)
  ] display certificates from the state in a table [`Emmanuel Bosquet`]
  (`2023-05-22`)
- [
  [`2863c31`](https://github.com/sozu-proxy/sozu/commit/2863c312c406c09a7ff6f63b03dee0fab8b10a65)
  ] CLI: query all certificates in the state [`Emmanuel Bosquet`] (`2023-05-22`)
- [
  [`a71f485`](https://github.com/sozu-proxy/sozu/commit/a71f485477b73b95f3b89bfe2fbb99bd921795d2)
  ] query certificates from the state with fingerprint [`Emmanuel Bosquet`]
  (`2023-05-22`)
- [
  [`294c164`](https://github.com/sozu-proxy/sozu/commit/294c164648b07f2ea261023b77207e6cacf9d866)
  ] Update gitignore [`Florentin Dubois`] (`2023-05-23`)
- [
  [`ffcec1c`](https://github.com/sozu-proxy/sozu/commit/ffcec1c9a0262fbfbb26e73adf69822e9bff778f)
  ] chore: update dependencies [`Florentin Dubois`] (`2023-06-22`)
- [
  [`b73122b`](https://github.com/sozu-proxy/sozu/commit/b73122b53a4d7f19da959a5d7a500a6dffa7d81c)
  ] chore: update dependencies [`Florentin Dubois`] (`2023-06-22`)
- [
  [`cdc4e29`](https://github.com/sozu-proxy/sozu/commit/cdc4e294900057fd76b06b9a09523a7c3f238134)
  ] chore: update dependencies [`Florentin Dubois`] (`2023-06-22`)
- [
  [`3c56fbc`](https://github.com/sozu-proxy/sozu/commit/3c56fbc105b5a93ebd5d912153943f1e55dc9e6c)
  ] chore: update dependencies [`Florentin Dubois`] (`2023-06-22`)

#### ➖ Removed

- [
  [`774d1af`](https://github.com/sozu-proxy/sozu/commit/774d1afd01acd679ecb99502c2b4bba757eb53d3)
  ] Remove legacy folder and script [`Florentin Dubois`] (`2023-05-23`)

#### ⚡ Breaking changes

- [
  [`cf5964b`](https://github.com/sozu-proxy/sozu/commit/cf5964b67ebbbe35f32bb8ae1e2433c988547b1c)
  ] transform CLI command "query clusters" to "cluster get" [`Emmanuel Bosquet`]
  (`2023-05-22`)
- [
  [`acdc8c9`](https://github.com/sozu-proxy/sozu/commit/acdc8c9203fbca8f6e5119673de4526afe6006cc)
  ] transform CLI command "query certificates" to "certificate get" [`Emmanuel
  Bosquet`] (`2023-05-22`)
- [
  [`ec6c58e`](https://github.com/sozu-proxy/sozu/commit/ec6c58ef42ead1e8213fb38db66e2343042be37b)
  ] CLI: rename 'clusters get' to 'clusters list', same for certificates
  [`Emmanuel Bosquet`] (`2023-05-22`)

#### ➕ Added

- [
  [`2f79f3c`](https://github.com/sozu-proxy/sozu/commit/2f79f3cb11b873d6c42579cc2398a06622fffe1c)
  ] create ConfigState::get_certificates_by_domain_name [`Emmanuel Bosquet`]
  (`2023-05-22`)
- [
  [`787ae9e`](https://github.com/sozu-proxy/sozu/commit/787ae9e7317e27824f377339507d80fce25c33d9)
  ] implement Display for CertificateWithNames [`Emmanuel Bosquet`]
  (`2023-05-22`)
- [
  [`1797600`](https://github.com/sozu-proxy/sozu/commit/179760038cc1420893fcdec48b229e20e42da0e2)
  ] implement From<RequestType> for Request [`Emmanuel Bosquet`] (`2023-05-22`)
- [
  [`c19ef26`](https://github.com/sozu-proxy/sozu/commit/c19ef26958488781cbfacc35b2e375b16390215f)
  ] rename order_request to send_request [`Emmanuel Bosquet`] (`2023-05-22`)
- [
  [`538653f`](https://github.com/sozu-proxy/sozu/commit/538653f170d0c040aba167589b6de8b92c4e3aa0)
  ] Create state directory and file if it does not exists [`Florentin Dubois`]
  (`2023-05-23`)
- [
  [`b34b60f`](https://github.com/sozu-proxy/sozu/commit/b34b60f60bc32fb3fc61fe3511cd10d25650457b)
  ] count request types received in ConfigState [`Emmanuel Bosquet`]
  (`2023-06-01`)
- [
  [`3a92069`](https://github.com/sozu-proxy/sozu/commit/3a9206979b3323213b79eec49dff49a3e76049cf)
  ] define defaults in sozu_command_lib::config [`Emmanuel Bosquet`]
  (`2023-06-02`)
- [
  [`f6e011f`](https://github.com/sozu-proxy/sozu/commit/f6e011f0c2626d4b097f6e247d9b50f3f86f62a2)
  ] Add socketstats unittest [`Eloi DEMOLIS`] (`2023-06-05`)

#### 📚 Documentation

- [
  [`de7660d`](https://github.com/sozu-proxy/sozu/commit/de7660d078f2abe48d8dd716099ab10b548e7fc0)
  ] doc: Changed all instances of SSL to TLS. [`Jonathan Davies`] (`2023-05-19`)

#### 🚀 Refactored

- [
  [`9085a20`](https://github.com/sozu-proxy/sozu/commit/9085a207c6b869e0d71dc5ea7fe6df8af011c699)
  ] isolate method ConfigState::list_listeners [`Emmanuel Bosquet`]
  (`2023-05-19`)
- [
  [`c85f65f`](https://github.com/sozu-proxy/sozu/commit/c85f65f607138d1477b7d29b0079928548d1b072)
  ] isolate method ConfigState::list_frontends [`Emmanuel Bosquet`]
  (`2023-05-19`)
- [
  [`e24659d`](https://github.com/sozu-proxy/sozu/commit/e24659d51ce2ecfcd2eb23961bf59e50e680a5d3)
  ] introduce type QueryCertificatesFilters [`Emmanuel Bosquet`] (`2023-05-22`)
- [
  [`9d554a5`](https://github.com/sozu-proxy/sozu/commit/9d554a5e92757636f844b7821b73acc12e92aada)
  ] rename ContentType::Certificates to CertificatesByAddress [`Emmanuel
  Bosquet`] (`2023-05-22`)
- [
  [`4776d5f`](https://github.com/sozu-proxy/sozu/commit/4776d5f7147c3737cdd030d2ba28f7f63ca7ae71)
  ] merge certificate types in CertificatesWithFingerprints [`Emmanuel Bosquet`]
  (`2023-05-22`)
- [
  [`91dc12d`](https://github.com/sozu-proxy/sozu/commit/91dc12d83adab360fd1e27f0aac19e85420c5376)
  ] CertificatesMatchingADomainName contains CertificateAndKey [`Emmanuel
  Bosquet`] (`2023-05-22`)
- [
  [`721a951`](https://github.com/sozu-proxy/sozu/commit/721a951192a03340ace6867cf5e16733a1e17eec)
  ] merge request types into RequestType::QueryCertificatesFromWorkers
  [`Emmanuel Bosquet`] (`2023-05-22`)
- [
  [`fcbe244`](https://github.com/sozu-proxy/sozu/commit/fcbe244a0a5eff32c989197f61a4b65b9aa9aace)
  ] CLI: simplify display::print_cluster_responses [`Emmanuel Bosquet`]
  (`2023-05-22`)
- [
  [`57a0fc6`](https://github.com/sozu-proxy/sozu/commit/57a0fc6c212968eafee827832afef307d3722e16)
  ] Format GitHub Action workflow [`Florentin Dubois`] (`2023-05-23`)
- [
  [`f2176a3`](https://github.com/sozu-proxy/sozu/commit/f2176a333c492f9ca703d3355d88b981f75b0a45)
  ] Update systemd services and configuration [`Florentin Dubois`]
  (`2023-05-23`)
- [
  [`81f25b6`](https://github.com/sozu-proxy/sozu/commit/81f25b66dd22d1462af781fb6627d762310f2e82)
  ] Update Arch Linux packaging [`Florentin Dubois`] (`2023-05-23`)
- [
  [`458bb5a`](https://github.com/sozu-proxy/sozu/commit/458bb5adb49ed1dc5f73b1077ce86e6f3fb33e28)
  ] Update RPM and selinux packaging [`Florentin Dubois`] (`2023-05-23`)
- [
  [`d088468`](https://github.com/sozu-proxy/sozu/commit/d0884685806b5ef88097d24f36fa80e6b2bd5e79)
  ] Update Docker image [`Florentin Dubois`] (`2023-05-23`)
- [
  [`fe9097e`](https://github.com/sozu-proxy/sozu/commit/fe9097ea80fde5b6ec419d989d13954a702c056e)
  ] Apply clippy suggestions [`Eloi DEMOLIS`] (`2023-06-05`)
- [
  [`72f200b`](https://github.com/sozu-proxy/sozu/commit/72f200bcfe3671f04d62b347cc6eba82bd4abb43)
  ] Refactor access logs: [`Eloi DEMOLIS`] (`2023-06-05`)
- [
  [`0ff7b31`](https://github.com/sozu-proxy/sozu/commit/0ff7b31e674b2cf4c1fc24e1c5098f84b8da9003)
  ] rename MetricData to MetricValue [`Emmanuel Bosquet`] (`2023-06-16`)

#### ⛑️ Fixed

- [
  [`584e0bf`](https://github.com/sozu-proxy/sozu/commit/584e0bff812f91e6b6b5aff8930b984a43845fd3)
  ] fix display of hex fingerprint in the CLI [`Emmanuel Bosquet`]
  (`2023-05-22`)
- [
  [`a93f66e`](https://github.com/sozu-proxy/sozu/commit/a93f66e0525759cb7ce0118af08f0505fc4ff903)
  ] Fix some MacOS related issues [`Eloi DEMOLIS`] (`2023-06-14`)
- [
  [`0b435b6`](https://github.com/sozu-proxy/sozu/commit/0b435b654f6a998ca5d49101b85d558f8aed8909)
  ] Fix some MacOS related warnings [`Eloi DEMOLIS`] (`2023-06-14`)
- [
  [`09b0bf0`](https://github.com/sozu-proxy/sozu/commit/09b0bf02a5fcfe240bf0c00114da2a42e789f51a)
  ] chore: decrease logging verbosity [`Florentin Dubois`] (`2023-06-22`)

### 🥹 Contributors

- @Wonshtrum
- @Keksoj
- @FlorentinDUBOIS

**Full Changelog**: https://github.com/sozu-proxy/sozu/compare/0.14.3...0.15.0

## 0.14.3 - 2023-05-17

### 🌟 Features

- We have updated structures that Sōzu use for its communication on the socket.
  It now uses structures that are generated from protobuf. see
  [`b6bc86d`](https://github.com/sozu-proxy/sozu/commit/b6bc86ddf38987430cfd5fbb5c55b977f26ef861),
  [`2f4f769`](https://github.com/sozu-proxy/sozu/commit/2f4f7691d4efb193663e1e61999ca8478207009a),
  [`1732b5d`](https://github.com/sozu-proxy/sozu/commit/1732b5d1c1daf74e31704520a0f0330ca080d327),
  [`e3c8bec`](https://github.com/sozu-proxy/sozu/commit/e3c8beca54be48b1022ebdb5e5f9b9e671a4abbc),
  [`efb9c5d`](https://github.com/sozu-proxy/sozu/commit/efb9c5db6f28a6ecbd85a141fff7f68c6733dda9),
  [`7759984`](https://github.com/sozu-proxy/sozu/commit/77599849ca66897cd68e27b1fa0b66d735ae104c),
  [`1d5c72e`](https://github.com/sozu-proxy/sozu/commit/1d5c72e047fc108120eb9e5b08dd25c4692c9f89),
  [`1b07534`](https://github.com/sozu-proxy/sozu/commit/1b0753485af9ad8c3b7517f563504e3e0cd34bac),
  [`a1a801e`](https://github.com/sozu-proxy/sozu/commit/a1a801e29b9fdd1ad3724a991a24c45825a462f6),
  [`9dc490f`](https://github.com/sozu-proxy/sozu/commit/9dc490f27f3b4052581c8245cff97efd5946caa5),
  [`4bd9c6f`](https://github.com/sozu-proxy/sozu/commit/4bd9c6f8082b26d15f01e65983625cf9c43e14c5),
  [`1421f6c`](https://github.com/sozu-proxy/sozu/commit/1421f6ccf86caf49faefa58bcdf0fcb5b9b3c119),
  [`a39d905`](https://github.com/sozu-proxy/sozu/commit/a39d905ce9a9ca5407c8bae03f95acdb143e6032),
  [`43dfd6e`](https://github.com/sozu-proxy/sozu/commit/43dfd6e2d204fc52f9be232e72da9af60b734a52),
  [`6437a69`](https://github.com/sozu-proxy/sozu/commit/6437a69db97f9d6119754375b82cfcd76badd791),
  [`4ec1b21`](https://github.com/sozu-proxy/sozu/commit/4ec1b21e64a35a9c8f61a10a6d73ab08a71178d8),
  [`c4dbf90`](https://github.com/sozu-proxy/sozu/commit/c4dbf90bb9c372b3a98f611bc7a9b01fb84d3367),
  [`6910aaf`](https://github.com/sozu-proxy/sozu/commit/6910aaf28833584139addc2f8133ffccfa5c4481),
  [`137ae7f`](https://github.com/sozu-proxy/sozu/commit/137ae7fb0403ebff13fb8bbe187e74375d84311d),
  [`aeb2f2e`](https://github.com/sozu-proxy/sozu/commit/aeb2f2ea7601e5891186d41b469269ee90f5a5e7),
  [`7d9b0f5`](https://github.com/sozu-proxy/sozu/commit/7d9b0f5ce4c0c7b6a5c645f0ac2a5968719e58a3),
  [`755716c`](https://github.com/sozu-proxy/sozu/commit/755716c3aae451c47d3dd81b720a1e96abfa52d1),
  [`b2e0a6b`](https://github.com/sozu-proxy/sozu/commit/b2e0a6bc7b5ccb1a4bd0f4d496ca46abd9d4607e),
  [`4509bbe`](https://github.com/sozu-proxy/sozu/commit/4509bbea548ecce56fdf2a8056d6b9d88bd98717),
  [`c1f5b6e`](https://github.com/sozu-proxy/sozu/commit/c1f5b6eaf6d7391bf0db315cedcd83b707859e7d),
  [`2516e3b`](https://github.com/sozu-proxy/sozu/commit/2516e3bbb71b9ab61d5727c353b0c98706e7c0e0),
  [`d909f7d`](https://github.com/sozu-proxy/sozu/commit/d909f7d845d2ac201082efd1313273a58d8169eb),
  [`d2303f9`](https://github.com/sozu-proxy/sozu/commit/d2303f9fad0905c2c69172dc4e551540b24c42a7),
  [`8db54c7`](https://github.com/sozu-proxy/sozu/commit/8db54c725257fe79600f3f6120ff74c19f7f2fea),
  [`cbca836`](https://github.com/sozu-proxy/sozu/commit/cbca836d5331b0fe73521c40e02315f9259a6a96),
  [`35a43ee`](https://github.com/sozu-proxy/sozu/commit/35a43ee824d2c49f6bea40f978ebba124b83763c),
  [`2c6641a`](https://github.com/sozu-proxy/sozu/commit/2c6641a122779ff672889727e98d15d016849e60),
  [`7c6c93f`](https://github.com/sozu-proxy/sozu/commit/7c6c93fedc3f23b0cb37c2ba85eeb2b5e5d8e957),
  [`c53e763`](https://github.com/sozu-proxy/sozu/commit/c53e7639065698ba1d84b426e97205fe153c6832),
  [`e8fdb95`](https://github.com/sozu-proxy/sozu/commit/e8fdb95138035d164b425a13fb7be677f00c4dcc),
  [`c504ffa`](https://github.com/sozu-proxy/sozu/commit/c504ffacee6217af1bf11da896688e64bc8eb5da),
  [`0b74c92`](https://github.com/sozu-proxy/sozu/commit/0b74c9238e2d3340ad2b4e2859e2a94483d86064),
  [`7145d1f`](https://github.com/sozu-proxy/sozu/commit/7145d1f35d4542daecc70081e1774a9bbc167e5a),
  [`d6fba00`](https://github.com/sozu-proxy/sozu/commit/d6fba0087acadfa85c0eba69426414dfa50e52b5),
  [`ee301d9`](https://github.com/sozu-proxy/sozu/commit/ee301d961cbf5bfd8b82d79963a6d4d29c505bc8),
  [`8fcf5e9`](https://github.com/sozu-proxy/sozu/commit/8fcf5e9ede2b804589e1aaece25241e121344d96),
  [`f514bce`](https://github.com/sozu-proxy/sozu/commit/f514bce5b2a654d4f97c25826d15bbb69c4e1301),
  [`145d061`](https://github.com/sozu-proxy/sozu/commit/145d06186f1bc97831c02f698f5e471b69bdc526),
  [`81f4f39`](https://github.com/sozu-proxy/sozu/commit/81f4f39b275afcb6d376844d6f6827968c849661),
  [`7cf64c8`](https://github.com/sozu-proxy/sozu/commit/7cf64c8b2ce05306dab4e19c47946dfec1b04456),
  [`a7e16f8`](https://github.com/sozu-proxy/sozu/commit/a7e16f8eaca129e88ead09386416ed0491b623f6),
  [`4b312d4`](https://github.com/sozu-proxy/sozu/commit/4b312d45f54e8ba0417aa04f98ddae656a035a7b),
  [`fdb8f2e`](https://github.com/sozu-proxy/sozu/commit/fdb8f2ed0828b1da1f04023aa715d1307dfcef03),
  [`0e62f8d`](https://github.com/sozu-proxy/sozu/commit/0e62f8d8e2c38fbe9acfc663f87c775e500ea07a),
  [`e0643f4`](https://github.com/sozu-proxy/sozu/commit/e0643f492bb1b9f6d64db0adbc2ead18befe3c17),
  [`4df5ba9`](https://github.com/sozu-proxy/sozu/commit/4df5ba900802647039bbed6606d5ef4ec7c45c64),
  [`0c94c2b`](https://github.com/sozu-proxy/sozu/commit/0c94c2b34638f3ef558dbae7ad9af702f9c805f6),
  [`ad7f58f`](https://github.com/sozu-proxy/sozu/commit/ad7f58fa137b687923f5f0924c84970af85ad2a7),
  [`a5b0009`](https://github.com/sozu-proxy/sozu/commit/a5b0009cc978662660cb83ba9a743f2d3fd49e88),
  [`5f18b71`](https://github.com/sozu-proxy/sozu/commit/5f18b71fbd04981f8ab31641d00a45c762b77e18),
  [`cb4755a`](https://github.com/sozu-proxy/sozu/commit/cb4755a391ba5f9db74d2edc58d0130f741a7a55),
  [`eb9b18e`](https://github.com/sozu-proxy/sozu/commit/eb9b18e0c479dbfcecd41da2e7c9765a4a82ab76),
  [`3387628`](https://github.com/sozu-proxy/sozu/commit/3387628af71d539a887d3672212aa718492601f5),
  [`30ae223`](https://github.com/sozu-proxy/sozu/commit/30ae2238715a152a1612b5f8bc9485208184f2ba),
  [`72259e7`](https://github.com/sozu-proxy/sozu/commit/72259e76ff2698e4b51ea899c23dd93e059004b8),
  [`476e230`](https://github.com/sozu-proxy/sozu/commit/476e23043f715d32a1403ddad736af7cf087f641),
  [`7fac13a`](https://github.com/sozu-proxy/sozu/commit/7fac13a2a58f1dcfca1b9aed5dd05b878e820426),
  [`31d689a`](https://github.com/sozu-proxy/sozu/commit/31d689a0dbbcba5cc45c222c80a6e5b233809853),
  [`f03aac9`](https://github.com/sozu-proxy/sozu/commit/f03aac9f28dd89f800c2dc329bad97718547eb2b),
  [`5924101`](https://github.com/sozu-proxy/sozu/commit/5924101a12582d3b74dd0c116c13bb416062581c),
  [`8d6eb42`](https://github.com/sozu-proxy/sozu/commit/8d6eb42c50cc09ca35a9fde04a5ca30e673201c3),
  [`8e5e9ce`](https://github.com/sozu-proxy/sozu/commit/8e5e9ceea91459cf1bf856865642f73897ca228d),
  [`65b01e8`](https://github.com/sozu-proxy/sozu/commit/65b01e8893f72161007bf6a0862088c46a5fab17),
  [`1264378`](https://github.com/sozu-proxy/sozu/commit/1264378b5d5360bbdbab3268a317f39a78de5ad5),
  [`565dbd6`](https://github.com/sozu-proxy/sozu/commit/565dbd6ff559894384f508f434604e3ed42c50d4),
  [`87b4d96`](https://github.com/sozu-proxy/sozu/commit/87b4d96a83e6304bb817b32257b59b8396444ab7),
  [`457a9cd`](https://github.com/sozu-proxy/sozu/commit/457a9cd6fb0c3e00177722e65de34dc3b83d0c86),
  [`7ca33c6`](https://github.com/sozu-proxy/sozu/commit/7ca33c674d0d8c58e52ada5ce47476813a9b8433),
  [`2989f49`](https://github.com/sozu-proxy/sozu/commit/2989f49ecbdecbf864b1f0692e36a4f359544288),
  [`8907216`](https://github.com/sozu-proxy/sozu/commit/890721672d51bb464e3e1a6b0c2cad560ba1c060),
  [`36f01dc`](https://github.com/sozu-proxy/sozu/commit/36f01dc1e28facf5bb551180b3a6d2182890edbf),
  [`00f82cf`](https://github.com/sozu-proxy/sozu/commit/00f82cfeca758d921df7d28e82b8a491277048a1),
  [`9a37917`](https://github.com/sozu-proxy/sozu/commit/9a379176b7f4deeab6f7822283a24dba6efe92f6),
  [`5a77faf`](https://github.com/sozu-proxy/sozu/commit/5a77fafde2ca04ea17ddc84eaad05fed4b1c54c5),
  [`f434974`](https://github.com/sozu-proxy/sozu/commit/f4349748adbee1209d8d63189c74dc76285f1185),
  [`6716f8a`](https://github.com/sozu-proxy/sozu/commit/6716f8a06b4ff76e0e399ae7fd5a96d57c615086),
  [`4e1e7d0`](https://github.com/sozu-proxy/sozu/commit/4e1e7d0c0e9fe779cac55c22ba33a82bf204b5cf),
  [`25f6018`](https://github.com/sozu-proxy/sozu/commit/25f6018ffc2a23fbfca09f72d28ae6da7a3c4805),
  [`82bb6c5`](https://github.com/sozu-proxy/sozu/commit/82bb6c5b479640b90fe9fbdcd5727e1dbbc821ba),
  [`685bb16`](https://github.com/sozu-proxy/sozu/commit/685bb1630e39c3fcc59d2a26e5bf5907c169570a),
  [`bdd402f`](https://github.com/sozu-proxy/sozu/commit/bdd402f58ef9536186808fad913142cacb594a27),
  [`c3969d2`](https://github.com/sozu-proxy/sozu/commit/c3969d258d72c474eb7393e9fdb4c3ad9e1ca784),
  [`a87214f`](https://github.com/sozu-proxy/sozu/commit/a87214f8ddea6760cee313108f39ab7d866a2b31),
  [`ba6928c`](https://github.com/sozu-proxy/sozu/commit/ba6928ceb5d98c506418a46fb79e58716d5ef954),
  [`be75673`](https://github.com/sozu-proxy/sozu/commit/be75673c1c101a99ea9b7141429f543aa3db04b3),
  [`a8c87a1`](https://github.com/sozu-proxy/sozu/commit/a8c87a187ded0c1af8e97eb8b85dbea973686cff),
  [`c674b64`](https://github.com/sozu-proxy/sozu/commit/c674b64545546d5384920a8276e1497f13718769),
  [`f6a84e1`](https://github.com/sozu-proxy/sozu/commit/f6a84e1f09ce7595550c84dca8994d98c5cc2cae),
  [`03332c7`](https://github.com/sozu-proxy/sozu/commit/03332c71ae9c23bc9d12a5c76c39b62e1019f164),
  [`332ed3e`](https://github.com/sozu-proxy/sozu/commit/332ed3e6d0062683c5802ce3e0bee597bae7714a),
  [`1edcf68`](https://github.com/sozu-proxy/sozu/commit/1edcf6869e34e440d92234948c2a921ab4f1638e),
  [`3e41887`](https://github.com/sozu-proxy/sozu/commit/3e41887b9cebc6f23ecd1c1d9aecdb81791e4878),
  [`19d2915`](https://github.com/sozu-proxy/sozu/commit/19d29151cdecb1b0534efb5bc42b917c38782fa9),
  [`50b1a2e`](https://github.com/sozu-proxy/sozu/commit/50b1a2e5b62123e6b1864e2fb962b16f1eddd9fd),
  [`a91a576`](https://github.com/sozu-proxy/sozu/commit/a91a5765f9d7e6c48f4269996f4ae04d1e2e5826),
  [`6daea56`](https://github.com/sozu-proxy/sozu/commit/6daea56ab017c065ecdfbc5bdfaa5520d790a1fb),
  [`bd6d535`](https://github.com/sozu-proxy/sozu/commit/bd6d535c9174a34fb7e1714f2f53741788958c2e),
  [`7617b34`](https://github.com/sozu-proxy/sozu/commit/7617b345bf74e4fd7751f53bda99181c6fdd95d4),
  [`7254b98`](https://github.com/sozu-proxy/sozu/commit/7254b989cd859c97d70a13792fcf6e69ff2d10e1),
  [`c2df4b0`](https://github.com/sozu-proxy/sozu/commit/c2df4b06eb1377ea9f8611751333d47f29b938d3),
  [`d12920e`](https://github.com/sozu-proxy/sozu/commit/d12920e5d109c4cf42931a0bf8fdd105d1c2c864),
  [`4472332`](https://github.com/sozu-proxy/sozu/commit/4472332a2854382b492a7874795c5f7bed42c5ac),
  [`dbd1fad`](https://github.com/sozu-proxy/sozu/commit/dbd1fada9a24af882c12b677e4745a7c9b592fd1),
  [`66e36d6`](https://github.com/sozu-proxy/sozu/commit/66e36d6e00c501b3dedff9debdb2ea8d9dc195f0),
  [`582d285`](https://github.com/sozu-proxy/sozu/commit/582d2850679b8e57aed740aafaa6b553ae4993b1),
  [`d817d9d`](https://github.com/sozu-proxy/sozu/commit/d817d9d059b3c44180749c0e9841e414fe9ea874),
  [`7214aa2`](https://github.com/sozu-proxy/sozu/commit/7214aa2ade93f40171b2a4c0117482ab66279325),
  [`7fb08f3`](https://github.com/sozu-proxy/sozu/commit/7fb08f3a800369a08cfa639c740a263c9f5b10e7),
  [`97fb5da`](https://github.com/sozu-proxy/sozu/commit/97fb5dad7240be4125213960b26c79e222877ced),
  [`ae140f2`](https://github.com/sozu-proxy/sozu/commit/ae140f24a6fffd4a3192e5f9b79cfa5a21ed0401),
  [`2e26e76`](https://github.com/sozu-proxy/sozu/commit/2e26e763244214990915b8f7f36fad828338964f),
  [`30e602d`](https://github.com/sozu-proxy/sozu/commit/30e602ded83b11ac127bbe4ecd0e24efb59f99bd),
  [`f005d9a`](https://github.com/sozu-proxy/sozu/commit/f005d9a8ec5b8b1cfb1a9a54116470966d0f96e9),
  [`802c3e9`](https://github.com/sozu-proxy/sozu/commit/802c3e9aa1c55cf5770244fa13d877eb08ddefdf),
  [`75b5ade`](https://github.com/sozu-proxy/sozu/commit/75b5ade95b727d320f7f39ebc97fc09abdbc7743),
  [`537e0e9`](https://github.com/sozu-proxy/sozu/commit/537e0e9910b5641dac85f1aa5ed3dc4093fd2eaf),
  [`dbab4b4`](https://github.com/sozu-proxy/sozu/commit/dbab4b4c93a1a853641e69e035c6a82f6f10893c),
  [`8cebed1`](https://github.com/sozu-proxy/sozu/commit/8cebed1ba391507b3ed635bf13c7cb674ece1e66),
  [`73c496d`](https://github.com/sozu-proxy/sozu/commit/73c496dec0160126b370ba245f51e0b2a6765904),
  [`d51590c`](https://github.com/sozu-proxy/sozu/commit/d51590c139aea2bb784747b091c6a866c518b1ec),
  [`cdc8629`](https://github.com/sozu-proxy/sozu/commit/cdc8629201c9e9df51a72acdec9590e8baa8ec67),
  [`22719bd`](https://github.com/sozu-proxy/sozu/commit/22719bd14a57bbad77d37940ea96ec45c4dc4458),
  [`acb0455`](https://github.com/sozu-proxy/sozu/commit/acb0455235d08d679624cdb41526855f8fad6c61),
  [`f9de7ba`](https://github.com/sozu-proxy/sozu/commit/f9de7bab7f1dcdb30e88b5ffa67d28a86a565891),
  [`b125338`](https://github.com/sozu-proxy/sozu/commit/b125338a160bc8a91169ed10970230c1110b6681),
  [`edb6863`](https://github.com/sozu-proxy/sozu/commit/edb6863c9bdca8976c816b1c273cefb3ec60eab5)
  and
  [`570f8af`](https://github.com/sozu-proxy/sozu/commit/570f8af4486590bb7b3c77a9dd80630c505e5b8a).
- We have improved the command line internals, see
  [`c4c51fa`](https://github.com/sozu-proxy/sozu/commit/c4c51fa670654d3ee0d5918b87d4679f5391b077)
  and
  [`7b568dd`](https://github.com/sozu-proxy/sozu/commit/7b568dd92019216b756b56123dcb51225dc72f6b).
- We now publish a new docker image on each commit on main branch, see
  [`b198efb`](https://github.com/sozu-proxy/sozu/commit/b198efb3f1e774ddf64a345ec746a4d66fc45731)
  and
  [`b260c8c`](https://github.com/sozu-proxy/sozu/commit/b260c8c66b3926b272109cf4b1e8900155d1978c).
- We have set the minimum supported rust version to 1.66.1, see
  [`08504aa`](https://github.com/sozu-proxy/sozu/commit/08504aaf7cbf6045b053882e4ad4220f4d43cd83).

### ✍️ Changed

- We have implemented new tests on the e2e testing framework, see
  [`b5fb1d9`](https://github.com/sozu-proxy/sozu/commit/b5fb1d97dedbe8bbd3469af8f240fb31ccf6ce09).
- We have updated distribution packaging, see
  [`42fa3f2`](https://github.com/sozu-proxy/sozu/commit/42fa3f2afcb041f079bf2d93bc5d46afc5bffbb8),
  [`b7e2f17`](https://github.com/sozu-proxy/sozu/commit/b7e2f17d7213133a188d130b4d380503688f3466)
  and
  [`e5465e4`](https://github.com/sozu-proxy/sozu/commit/e5465e48649fac131ab949a30b0d41513f2d645f).
- We now build the documentation using only the stable version of Rust, see
  [`95f7019`](https://github.com/sozu-proxy/sozu/commit/95f70191e8c053fce34b476f35b08f643640a4ba).
- We also improved the documentation, see
  [`2609746`](https://github.com/sozu-proxy/sozu/commit/2609746f3f8842c9c813afe70693bdf6a41b37f7),
  [`057eac4`](https://github.com/sozu-proxy/sozu/commit/057eac456dc7ca4f59a3d88f990fbe53bcd7da28),
  [`432b22b`](https://github.com/sozu-proxy/sozu/commit/432b22b55dae19e6889e897f8f506ff945586f7e),
  [`91dc44d`](https://github.com/sozu-proxy/sozu/commit/91dc44d4b87be4dc7ac899b4e8e4c35c745ebd7f),
  [`78a6363`](https://github.com/sozu-proxy/sozu/commit/78a636387e36b1d77b9ccecd6a5300bcf39ed235),
  [`3e93455`](https://github.com/sozu-proxy/sozu/commit/3e93455a4281fef82f95fede93194622c7ced6e4),
  [`5032973`](https://github.com/sozu-proxy/sozu/commit/503297330756dae8614d20ba03e99f3d693189d3)
  and
  [`17f7cb6`](https://github.com/sozu-proxy/sozu/commit/17f7cb621fb93fac11968b7fa4062081a1812965).

### ➖ Removed

- We have removed the "acme" sub command of sozu to help us to completely remove
  openssl dependency on OpenSSL, see
  [`d5297dd`](https://github.com/sozu-proxy/sozu/commit/d5297dd0d60b99416df40fb827f47827566ea0d3),
  [`106d3c8`](https://github.com/sozu-proxy/sozu/commit/106d3c8277ae25b6f3f09d3e5a7f81c07a6ae612)
  and the issue [#926](https://github.com/sozu-proxy/sozu/issues/926)

### ⚡ Breaking changes

- As we changed the communication format from json to protobuf, we could not
  keep the compatibility with elder version. However, as we used protobuf now,
  we will be able to support evolutions and changes without creating a breaking
  change.

### Changelog

#### ➕ Added

- [
  [`f0704ef`](https://github.com/sozu-proxy/sozu/commit/f0704ef5cc162e892001010698ba2169e7e3b95c)
  ] Add default variables [`tazou`] (`2021-06-22`)
- [
  [`6616b77`](https://github.com/sozu-proxy/sozu/commit/6616b77c2000e3c5bb9baa2749db3a8fd71b42be)
  ] add context to HTTP and HTTPS listener activation [`Emmanuel Bosquet`]
  (`2022-12-09`)
- [
  [`2f4f769`](https://github.com/sozu-proxy/sozu/commit/2f4f7691d4efb193663e1e61999ca8478207009a)
  ] add protobuf to Dockerfile image [`Emmanuel Bosquet`] (`2023-05-03`)
- [
  [`08504aa`](https://github.com/sozu-proxy/sozu/commit/08504aaf7cbf6045b053882e4ad4220f4d43cd83)
  ] add rust-version and rust-toolchain [`Emmanuel Bosquet`] (`2023-05-05`)
- [
  [`b198efb`](https://github.com/sozu-proxy/sozu/commit/b198efb3f1e774ddf64a345ec746a4d66fc45731)
  ] Docker build and push to Docker Hub in the CI [`Emmanuel Bosquet`]
  (`2022-12-09`)
- [
  [`b260c8c`](https://github.com/sozu-proxy/sozu/commit/b260c8c66b3926b272109cf4b1e8900155d1978c)
  ] push docker build to docker hub only when merging to main [`Emmanuel
  Bosquet`] (`2022-12-09`)
- [
  [`b5fb1d9`](https://github.com/sozu-proxy/sozu/commit/b5fb1d97dedbe8bbd3469af8f240fb31ccf6ce09)
  ] Simple e2e testing framework, passthrough and corner case tests [`Eloi
  DEMOLIS`] (`2023-01-10`)
- [
  [`146dd32`](https://github.com/sozu-proxy/sozu/commit/146dd3205b8eb662e1bdbbf0531a2ae7790ca7ff)
  ] make get_cluster_ids_by_domain a method of ConfigState [`Emmanuel Bosquet`]
  (`2023-05-04`)
- [
  [`ebeabc4`](https://github.com/sozu-proxy/sozu/commit/ebeabc4bf1f852974167bfb506fd309ecd31e570)
  ] make get_certificate a method of ConfigState [`Emmanuel Bosquet`]
  (`2023-05-04`)
- [
  [`c4c51fa`](https://github.com/sozu-proxy/sozu/commit/c4c51fa670654d3ee0d5918b87d4679f5391b077)
  ] list HTTP, HTTPS and TCP listeners in the CLI [`Emmanuel Bosquet`]
  (`2023-01-12`)
- [
  [`7b568dd`](https://github.com/sozu-proxy/sozu/commit/7b568dd92019216b756b56123dcb51225dc72f6b)
  ] cli: simplify request sending, remove boilerplate [`Emmanuel Bosquet`]
  (`2023-01-16`)
- [
  [`7e24d12`](https://github.com/sozu-proxy/sozu/commit/7e24d12de55bbc231101f18f571d48390c7b700b)
  ] PR feedback [`Tim Bart`] (`2022-12-18`)

#### ➖ Removed

- [
  [`106d3c8`](https://github.com/sozu-proxy/sozu/commit/106d3c8277ae25b6f3f09d3e5a7f81c07a6ae612)
  ] chore(acme): remove acme command [`Florentin Dubois`] (`2023-05-04`)
- [
  [`5bf36f5`](https://github.com/sozu-proxy/sozu/commit/5bf36f557ec642c4ff0eda5752277275d56008d8)
  ] remove todo macros in bin/scr/ctl/command.rs [`Emmanuel Bosquet`]
  (`2023-01-16`)

#### ✍️ Changed

- [
  [`42fa3f2`](https://github.com/sozu-proxy/sozu/commit/42fa3f2afcb041f079bf2d93bc5d46afc5bffbb8)
  ] chore(archlinux): update packaging [`Florentin Dubois`] (`2023-01-23`)
- [
  [`b7e2f17`](https://github.com/sozu-proxy/sozu/commit/b7e2f17d7213133a188d130b4d380503688f3466)
  ] chore(docker): update packaging [`Florentin Dubois`] (`2023-01-23`)
- [
  [`e5465e4`](https://github.com/sozu-proxy/sozu/commit/e5465e48649fac131ab949a30b0d41513f2d645f)
  ] chore(rpm): update packaging [`Florentin Dubois`] (`2023-01-23`)
- [
  [`6c87883`](https://github.com/sozu-proxy/sozu/commit/6c87883cc15c1d2e3c21a9a43657a40c7b3783b3)
  ] Update generate.sh [`tazou`] (`2021-06-22`)
- [
  [`1304914`](https://github.com/sozu-proxy/sozu/commit/1304914fc57dbcdf700556447daf73d7fa90d256)
  ] Update command/src/proxy.rs [`Tim Bart`] (`2022-12-20`)
- [
  [`3e48735`](https://github.com/sozu-proxy/sozu/commit/3e487355e648830cc7333e3d0face6e2e25ce97e)
  ] remove unsafe for get_executable_path on macOS [`Tim Bart`] (`2022-12-19`)
- [
  [`95f7019`](https://github.com/sozu-proxy/sozu/commit/95f70191e8c053fce34b476f35b08f643640a4ba)
  ] Github CI: use stable toolchain to build the doc [`Emmanuel Bosquet`]
  (`2023-03-22`)
- [
  [`5698e14`](https://github.com/sozu-proxy/sozu/commit/5698e14997f697ef8e5bc42d92a96b1d54e31ed6)
  ] chore(lib): bump dependencies [`Florentin Dubois`] (`2023-05-03`)
- [
  [`c7fd33d`](https://github.com/sozu-proxy/sozu/commit/c7fd33dfc133adf16ba3e2d60a1ac837374cf8a5)
  ] chore(command): bump dependencies [`Florentin Dubois`] (`2023-05-03`)
- [
  [`9ca3cda`](https://github.com/sozu-proxy/sozu/commit/9ca3cda303acb831cf4e4acb1b8ef8bd35da8723)
  ] chore(e2e): bump dependencies [`Florentin Dubois`] (`2023-05-03`)
- [
  [`4a89060`](https://github.com/sozu-proxy/sozu/commit/4a89060baae9b600590a4435e7b8860ece42554c)
  ] chore(bin): bump dependencies [`Florentin Dubois`] (`2023-05-03`)
- [
  [`78f1647`](https://github.com/sozu-proxy/sozu/commit/78f16476f06ac95d353db292316de4d474b9ef15)
  ] update rustls to 0.21.0 [`Emmanuel Bosquet`] (`2023-04-12`)
- [
  [`6d5215a`](https://github.com/sozu-proxy/sozu/commit/6d5215acafd99dea484b7c896126aa068e3c389b)
  ] Update dependencies [`Florentin Dubois`] (`2023-02-06`)
- [
  [`b6bc86d`](https://github.com/sozu-proxy/sozu/commit/b6bc86ddf38987430cfd5fbb5c55b977f26ef861)
  ] build protobuf types with prost-build, without tonic [`Emmanuel Bosquet`]
  (`2023-04-12`)
- [
  [`d5297dd`](https://github.com/sozu-proxy/sozu/commit/d5297dd0d60b99416df40fb827f47827566ea0d3)
  ] chore(e2e): use rustls instead of openssl [`Florentin Dubois`]
  (`2023-05-04`)
- [
  [`b978a89`](https://github.com/sozu-proxy/sozu/commit/b978a891cffd72e467a071597d7ffc92428e5432)
  ] get TcpFrontends::tags out of its Option<> [`Emmanuel Bosquet`]
  (`2023-05-04`)
- [
  [`9cd613c`](https://github.com/sozu-proxy/sozu/commit/9cd613c199b05e14e6f4525f851392391b38f722)
  ] Update lib/src/protocol/http/mod.rs [`Eloi Démolis`] (`2023-04-13`)
- [
  [`b89f4f7`](https://github.com/sozu-proxy/sozu/commit/b89f4f7d72d6075f5661c2875cfedef43c15280f)
  ] Update lib/src/protocol/http/mod.rs [`Eloi Démolis`] (`2023-04-13`)
- [
  [`a1c02b6`](https://github.com/sozu-proxy/sozu/commit/a1c02b6b76fc5a7254c86788e2d2572777fbeafd)
  ] remove resolved TODOs [`Emmanuel Bosquet`] (`2023-05-04`)

#### 📚 Documentation

- [
  [`2609746`](https://github.com/sozu-proxy/sozu/commit/2609746f3f8842c9c813afe70693bdf6a41b37f7)
  ] add documenting comments [`Emmanuel Bosquet`] (`2022-12-16`)
- [
  [`057eac4`](https://github.com/sozu-proxy/sozu/commit/057eac456dc7ca4f59a3d88f990fbe53bcd7da28)
  ] basic crate documentation on sozu [`Emmanuel Bosquet`] (`2023-05-09`)
- [
  [`432b22b`](https://github.com/sozu-proxy/sozu/commit/432b22b55dae19e6889e897f8f506ff945586f7e)
  ] documenting comments on main process upgrade, remove useless comments
  [`Emmanuel Bosquet`] (`2022-12-16`)
- [
  [`91dc44d`](https://github.com/sozu-proxy/sozu/commit/91dc44d4b87be4dc7ac899b4e8e4c35c745ebd7f)
  ] Remove mentions of sozuctl [`Sykursen`] (`2023-03-01`)
- [
  [`78a6363`](https://github.com/sozu-proxy/sozu/commit/78a636387e36b1d77b9ccecd6a5300bcf39ed235)
  ] document protobuf in the sozu-command-lib README [`Emmanuel Bosquet`]
  (`2023-05-03`)
- [
  [`3e93455`](https://github.com/sozu-proxy/sozu/commit/3e93455a4281fef82f95fede93194622c7ced6e4)
  ] doc: use sozu instead of sozuctl [`Florentin Dubois`] (`2023-01-23`)
- [
  [`5032973`](https://github.com/sozu-proxy/sozu/commit/503297330756dae8614d20ba03e99f3d693189d3)
  ] correct formatting in how_to_use.md [`Emmanuel Bosquet`] (`2023-05-03`)
- [
  [`17f7cb6`](https://github.com/sozu-proxy/sozu/commit/17f7cb621fb93fac11968b7fa4062081a1812965)
  ] rewrite the sozu_lib documentation [`Emmanuel Bosquet`] (`2023-05-09`)

#### 🚀 Refactored

- [
  [`319119a`](https://github.com/sozu-proxy/sozu/commit/319119a46b155fafd6a01a63ce757dfa7d48d3e6)
  ] abstract out HTTP and HTTPS notify methods [`Emmanuel Bosquet`]
  (`2022-12-08`)
- [
  [`03085ea`](https://github.com/sozu-proxy/sozu/commit/03085eab569e6208def948a3b65830c3cd3b9f27)
  ] error propagation on HTTP and HTTPS frontend add and remove [`Emmanuel
  Bosquet`] (`2022-12-08`)
- [
  [`ffb5384`](https://github.com/sozu-proxy/sozu/commit/ffb5384c2454c4077f49456376b7ca68d5deb2c7)
  ] rename ConfigState::handle_order to ConfigState::dispatch [`Emmanuel
  Bosquet`] (`2022-12-16`)
- [
  [`1c9f785`](https://github.com/sozu-proxy/sozu/commit/1c9f785fc5a8ee54be8f99ece08ed3913e415feb)
  ] remove the init_workers function [`Emmanuel Bosquet`] (`2022-12-16`)
- [
  [`1732b5d`](https://github.com/sozu-proxy/sozu/commit/1732b5d1c1daf74e31704520a0f0330ca080d327)
  ] rename command::proxy module to command::worker [`Emmanuel Bosquet`]
  (`2023-03-08`)
- [
  [`e3c8bec`](https://github.com/sozu-proxy/sozu/commit/e3c8beca54be48b1022ebdb5e5f9b9e671a4abbc)
  ] remove optional worker id from CommandRequest [`Emmanuel Bosquet`]
  (`2023-03-08`)
- [
  [`efb9c5d`](https://github.com/sozu-proxy/sozu/commit/efb9c5db6f28a6ecbd85a141fff7f68c6733dda9)
  ] rename CommandRequest to ClientRequest [`Emmanuel Bosquet`] (`2023-03-08`)
- [
  [`7759984`](https://github.com/sozu-proxy/sozu/commit/77599849ca66897cd68e27b1fa0b66d735ae104c)
  ] flatten ProxyRequestOrder variants into RequestContent [`Emmanuel Bosquet`]
  (`2023-03-08`)
- [
  [`1d5c72e`](https://github.com/sozu-proxy/sozu/commit/1d5c72e047fc108120eb9e5b08dd25c4692c9f89)
  ] remove id and version from Requests sent to sozu [`Emmanuel Bosquet`]
  (`2023-03-09`)
- [
  [`1b07534`](https://github.com/sozu-proxy/sozu/commit/1b0753485af9ad8c3b7517f563504e3e0cd34bac)
  ] Remove @BlackYoup from code owners [`Florentin DUBOIS`] (`2023-03-09`)
- [
  [`a1a801e`](https://github.com/sozu-proxy/sozu/commit/a1a801e29b9fdd1ad3724a991a24c45825a462f6)
  ] put Query variants into Order, remove Query [`Emmanuel Bosquet`]
  (`2023-03-09`)
- [
  [`9dc490f`](https://github.com/sozu-proxy/sozu/commit/9dc490f27f3b4052581c8245cff97efd5946caa5)
  ] sozu_command_lib: rename command module to order [`Emmanuel Bosquet`]
  (`2023-03-10`)
- [
  [`4bd9c6f`](https://github.com/sozu-proxy/sozu/commit/4bd9c6f8082b26d15f01e65983625cf9c43e14c5)
  ] segregate types in the order and response modules [`Emmanuel Bosquet`]
  (`2023-03-10`)
- [
  [`1421f6c`](https://github.com/sozu-proxy/sozu/commit/1421f6ccf86caf49faefa58bcdf0fcb5b9b3c119)
  ] rename sozu::Response to sozu::Advancement [`Emmanuel Bosquet`]
  (`2023-03-13`)
- [
  [`a39d905`](https://github.com/sozu-proxy/sozu/commit/a39d905ce9a9ca5407c8bae03f95acdb143e6032)
  ] rename sozu_command_lib::CommandResponse to Response [`Emmanuel Bosquet`]
  (`2023-03-13`)
- [
  [`43dfd6e`](https://github.com/sozu-proxy/sozu/commit/43dfd6e2d204fc52f9be232e72da9af60b734a52)
  ] rename sozu_command_lib::order module to request [`Emmanuel Bosquet`]
  (`2023-03-13`)
- [
  [`6437a69`](https://github.com/sozu-proxy/sozu/commit/6437a69db97f9d6119754375b82cfcd76badd791)
  ] rename sozu::command::orders to sozu::command::requests [`Emmanuel Bosquet`]
  (`2023-03-13`)
- [
  [`4ec1b21`](https://github.com/sozu-proxy/sozu/commit/4ec1b21e64a35a9c8f61a10a6d73ab08a71178d8)
  ] sozu::Worker::is_not_stopped_or_stopping() method [`Emmanuel Bosquet`]
  (`2023-03-13`)
- [
  [`c4dbf90`](https://github.com/sozu-proxy/sozu/commit/c4dbf90bb9c372b3a98f611bc7a9b01fb84d3367)
  ] method Request::is_a_stop() [`Emmanuel Bosquet`] (`2023-03-13`)
- [
  [`6910aaf`](https://github.com/sozu-proxy/sozu/commit/6910aaf28833584139addc2f8133ffccfa5c4481)
  ] remove worker.rs [`Emmanuel Bosquet`] (`2023-03-13`)
- [
  [`137ae7f`](https://github.com/sozu-proxy/sozu/commit/137ae7fb0403ebff13fb8bbe187e74375d84311d)
  ] return error if no worker is found when reloading configuration [`Emmanuel
  Bosquet`] (`2023-03-13`)
- [
  [`aeb2f2e`](https://github.com/sozu-proxy/sozu/commit/aeb2f2ea7601e5891186d41b469269ee90f5a5e7)
  ] remove useless ProxyEvent, redundant with Event [`Emmanuel Bosquet`]
  (`2023-03-13`)
- [
  [`7d9b0f5`](https://github.com/sozu-proxy/sozu/commit/7d9b0f5ce4c0c7b6a5c645f0ac2a5968719e58a3)
  ] use type ResponseStatus for ProxyResponse [`Emmanuel Bosquet`]
  (`2023-03-13`)
- [
  [`755716c`](https://github.com/sozu-proxy/sozu/commit/755716c3aae451c47d3dd81b720a1e96abfa52d1)
  ] rename sozu::Worker::is_not_stopped_or_stopping to is_active [`Emmanuel
  Bosquet`] (`2023-03-16`)
- [
  [`b2e0a6b`](https://github.com/sozu-proxy/sozu/commit/b2e0a6bc7b5ccb1a4bd0f4d496ca46abd9d4607e)
  ] build Config using a ConfigBuilder [`Emmanuel Bosquet`] (`2023-03-16`)
- [
  [`4509bbe`](https://github.com/sozu-proxy/sozu/commit/4509bbea548ecce56fdf2a8056d6b9d88bd98717)
  ] sozu_command_lib::config::FileConfig::load_from_path returns anyhow::Result
  [`Emmanuel Bosquet`] (`2023-03-16`)
- [
  [`c1f5b6e`](https://github.com/sozu-proxy/sozu/commit/c1f5b6eaf6d7391bf0db315cedcd83b707859e7d)
  ] parse String to SocketAddr in config::Listener [`Emmanuel Bosquet`]
  (`2023-03-16`)
- [
  [`2516e3b`](https://github.com/sozu-proxy/sozu/commit/2516e3bbb71b9ab61d5727c353b0c98706e7c0e0)
  ] replace SocketAddr with String in certificate requests [`Emmanuel Bosquet`]
  (`2023-03-16`)
- [
  [`d909f7d`](https://github.com/sozu-proxy/sozu/commit/d909f7d845d2ac201082efd1313273a58d8169eb)
  ] create struct AddBackend where SocketAddr is a String [`Emmanuel Bosquet`]
  (`2023-03-16`)
- [
  [`d2303f9`](https://github.com/sozu-proxy/sozu/commit/d2303f9fad0905c2c69172dc4e551540b24c42a7)
  ] create struct RequestHttpFrontend where SocketAddr is a String [`Emmanuel
  Bosquet`] (`2023-03-16`)
- [
  [`8db54c7`](https://github.com/sozu-proxy/sozu/commit/8db54c725257fe79600f3f6120ff74c19f7f2fea)
  ] documenting comments and defaults on Config [`Emmanuel Bosquet`]
  (`2023-03-16`)
- [
  [`cbca836`](https://github.com/sozu-proxy/sozu/commit/cbca836d5331b0fe73521c40e02315f9259a6a96)
  ] builder pattern for listeners [`Emmanuel Bosquet`] (`2023-03-16`)
- [
  [`35a43ee`](https://github.com/sozu-proxy/sozu/commit/35a43ee824d2c49f6bea40f978ebba124b83763c)
  ] default values for timeouts in Config serialization [`Emmanuel Bosquet`]
  (`2023-03-16`)
- [
  [`2c6641a`](https://github.com/sozu-proxy/sozu/commit/2c6641a122779ff672889727e98d15d016849e60)
  ] const DEFAULT_STICKY_NAME with value SOZUBALANCEID [`Emmanuel Bosquet`]
  (`2023-03-16`)
- [
  [`7c6c93f`](https://github.com/sozu-proxy/sozu/commit/7c6c93fedc3f23b0cb37c2ba85eeb2b5e5d8e957)
  ] protocol checks when building Listener in config.rs [`Emmanuel Bosquet`]
  (`2023-03-16`)
- [
  [`c53e763`](https://github.com/sozu-proxy/sozu/commit/c53e7639065698ba1d84b426e97205fe153c6832)
  ] documenting comments on listener builders [`Emmanuel Bosquet`]
  (`2023-03-16`)
- [
  [`e8fdb95`](https://github.com/sozu-proxy/sozu/commit/e8fdb95138035d164b425a13fb7be677f00c4dcc)
  ] implement Into<RequestHttpFrontend> for HttpFrontend [`Emmanuel Bosquet`]
  (`2023-03-16`)
- [
  [`c504ffa`](https://github.com/sozu-proxy/sozu/commit/c504ffacee6217af1bf11da896688e64bc8eb5da)
  ] remove impl Default for HttpListenerConfig [`Emmanuel Bosquet`]
  (`2023-03-16`)
- [
  [`0b74c92`](https://github.com/sozu-proxy/sozu/commit/0b74c9238e2d3340ad2b4e2859e2a94483d86064)
  ] remove useless field http_addresses on ConfigState [`Emmanuel Bosquet`]
  (`2023-03-16`)
- [
  [`7145d1f`](https://github.com/sozu-proxy/sozu/commit/7145d1f35d4542daecc70081e1774a9bbc167e5a)
  ] parse socket addresses in the CLI before sending requests [`Emmanuel
  Bosquet`] (`2023-03-20`)
- [
  [`d6fba00`](https://github.com/sozu-proxy/sozu/commit/d6fba0087acadfa85c0eba69426414dfa50e52b5)
  ] rename QueryAnswerCluster to ClusterInformation [`Emmanuel Bosquet`]
  (`2023-03-22`)
- [
  [`ee301d9`](https://github.com/sozu-proxy/sozu/commit/ee301d961cbf5bfd8b82d79963a6d4d29c505bc8)
  ] rename CertificateFingerprint to Fingerprint [`Emmanuel Bosquet`]
  (`2023-03-22`)
- [
  [`8fcf5e9`](https://github.com/sozu-proxy/sozu/commit/8fcf5e9ede2b804589e1aaece25241e121344d96)
  ] introduce response type CertificateSummary [`Emmanuel Bosquet`]
  (`2023-03-22`)
- [
  [`f514bce`](https://github.com/sozu-proxy/sozu/commit/f514bce5b2a654d4f97c25826d15bbb69c4e1301)
  ] create Request::QueryAllCertificates [`Emmanuel Bosquet`] (`2023-03-22`)
- [
  [`145d061`](https://github.com/sozu-proxy/sozu/commit/145d06186f1bc97831c02f698f5e471b69bdc526)
  ] create Request::QueryCertificatesByDomain [`Emmanuel Bosquet`]
  (`2023-03-22`)
- [
  [`81f4f39`](https://github.com/sozu-proxy/sozu/commit/81f4f39b275afcb6d376844d6f6827968c849661)
  ] create Request::QueryCertificateByFingerprint [`Emmanuel Bosquet`]
  (`2023-03-22`)
- [
  [`7cf64c8`](https://github.com/sozu-proxy/sozu/commit/7cf64c8b2ce05306dab4e19c47946dfec1b04456)
  ] make Request::QueryCertificateByFingerprint contain Fingerprint [`Emmanuel
  Bosquet`] (`2023-03-22`)
- [
  [`a7e16f8`](https://github.com/sozu-proxy/sozu/commit/a7e16f8eaca129e88ead09386416ed0491b623f6)
  ] remove ResponseContent::CertificatesByDomain [`Emmanuel Bosquet`]
  (`2023-03-22`)
- [
  [`4b312d4`](https://github.com/sozu-proxy/sozu/commit/4b312d45f54e8ba0417aa04f98ddae656a035a7b)
  ] remove QueryAnswerCertificate [`Emmanuel Bosquet`] (`2023-03-22`)
- [
  [`fdb8f2e`](https://github.com/sozu-proxy/sozu/commit/fdb8f2ed0828b1da1f04023aa715d1307dfcef03)
  ] rename ClusterMetricsData to ClusterMetrics [`Emmanuel Bosquet`]
  (`2023-03-22`)
- [
  [`0e62f8d`](https://github.com/sozu-proxy/sozu/commit/0e62f8d8e2c38fbe9acfc663f87c775e500ea07a)
  ] remove ProxyResponseContent, put its variant in ResponseContent [`Emmanuel
  Bosquet`] (`2023-03-22`)
- [
  [`e0643f4`](https://github.com/sozu-proxy/sozu/commit/e0643f492bb1b9f6d64db0adbc2ead18befe3c17)
  ] rename ProxyResponse to WorkerResponse [`Emmanuel Bosquet`] (`2023-03-22`)
- [
  [`4df5ba9`](https://github.com/sozu-proxy/sozu/commit/4df5ba900802647039bbed6606d5ef4ec7c45c64)
  ] put QueryAnswer variants in ProxyResponse, remove QueryAnswer [`Emmanuel
  Bosquet`] (`2023-03-22`)
- [
  [`0c94c2b`](https://github.com/sozu-proxy/sozu/commit/0c94c2b34638f3ef558dbae7ad9af702f9c805f6)
  ] make PathRule a struct, with embedded enum PathRuleKind [`Emmanuel Bosquet`]
  (`2023-03-22`)
- [
  [`ad7f58f`](https://github.com/sozu-proxy/sozu/commit/ad7f58fa137b687923f5f0924c84970af85ad2a7)
  ] rename AggregatedMetricsData to AggregatedMetrics [`Emmanuel Bosquet`]
  (`2023-03-22`)
- [
  [`a5b0009`](https://github.com/sozu-proxy/sozu/commit/a5b0009cc978662660cb83ba9a743f2d3fd49e88)
  ] rename FilteredData to FilteredMetrics [`Emmanuel Bosquet`] (`2023-03-22`)
- [
  [`5f18b71`](https://github.com/sozu-proxy/sozu/commit/5f18b71fbd04981f8ab31641d00a45c762b77e18)
  ] create and use response::BackendMetrics [`Emmanuel Bosquet`] (`2023-03-22`)
- [
  [`cb4755a`](https://github.com/sozu-proxy/sozu/commit/cb4755a391ba5f9db74d2edc58d0130f741a7a55)
  ] make AggregatedMetrics contain WorkerMetrics [`Emmanuel Bosquet`]
  (`2023-03-22`)
- [
  [`eb9b18e`](https://github.com/sozu-proxy/sozu/commit/eb9b18e0c479dbfcecd41da2e7c9765a4a82ab76)
  ] create AvailableMetrics, remove QueryAnswerMetrics [`Emmanuel Bosquet`]
  (`2023-03-22`)
- [
  [`3387628`](https://github.com/sozu-proxy/sozu/commit/3387628af71d539a887d3672212aa718492601f5)
  ] move QueryAnswerCertificate::All to ResponseContent::AllCertificates
  [`Emmanuel Bosquet`] (`2023-03-22`)
- [
  [`30ae223`](https://github.com/sozu-proxy/sozu/commit/30ae2238715a152a1612b5f8bc9485208184f2ba)
  ] move QueryAnswerCertificate::Domain to ResponseContent::CertificatesByDomain
  [`Emmanuel Bosquet`] (`2023-03-22`)
- [
  [`72259e7`](https://github.com/sozu-proxy/sozu/commit/72259e76ff2698e4b51ea899c23dd93e059004b8)
  ] move QueryAnswerCertificate::Fingerprint to
  ResponseContent::CertificateByFingerprint [`Emmanuel Bosquet`] (`2023-03-22`)
- [
  [`476e230`](https://github.com/sozu-proxy/sozu/commit/476e23043f715d32a1403ddad736af7cf087f641)
  ] Http refactor: move backend logic from Session to State [`Eloi DEMOLIS`]
  (`2023-02-13`)
- [
  [`7fac13a`](https://github.com/sozu-proxy/sozu/commit/7fac13a2a58f1dcfca1b9aed5dd05b878e820426)
  ] create Request::QueryClusterById [`Emmanuel Bosquet`] (`2023-03-23`)
- [
  [`31d689a`](https://github.com/sozu-proxy/sozu/commit/31d689a0dbbcba5cc45c222c80a6e5b233809853)
  ] create Request::QueryClusterByDomain [`Emmanuel Bosquet`] (`2023-03-23`)
- [
  [`f03aac9`](https://github.com/sozu-proxy/sozu/commit/f03aac9f28dd89f800c2dc329bad97718547eb2b)
  ] HttpFrontend::route has type Option<ClusterId> [`Emmanuel Bosquet`]
  (`2023-03-27`)
- [
  [`5924101`](https://github.com/sozu-proxy/sozu/commit/5924101a12582d3b74dd0c116c13bb416062581c)
  ] rename HttpFrontend::route to cluster_id [`Emmanuel Bosquet`] (`2023-03-27`)
- [
  [`8d6eb42`](https://github.com/sozu-proxy/sozu/commit/8d6eb42c50cc09ca35a9fde04a5ca30e673201c3)
  ] replace SocketAddr type with String in Listeners [`Emmanuel Bosquet`]
  (`2023-04-03`)
- [
  [`8e5e9ce`](https://github.com/sozu-proxy/sozu/commit/8e5e9ceea91459cf1bf856865642f73897ca228d)
  ] field active on TcpListenerConfig [`Emmanuel Bosquet`] (`2023-04-03`)
- [
  [`65b01e8`](https://github.com/sozu-proxy/sozu/commit/65b01e8893f72161007bf6a0862088c46a5fab17)
  ] field active on HttpsListenerConfig [`Emmanuel Bosquet`] (`2023-04-03`)
- [
  [`1264378`](https://github.com/sozu-proxy/sozu/commit/1264378b5d5360bbdbab3268a317f39a78de5ad5)
  ] field active on HttpListenerConfig [`Emmanuel Bosquet`] (`2023-04-03`)
- [
  [`565dbd6`](https://github.com/sozu-proxy/sozu/commit/565dbd6ff559894384f508f434604e3ed42c50d4)
  ] implement fmt::Display for RequestHttpFrontend [`Emmanuel Bosquet`]
  (`2023-04-04`)
- [
  [`87b4d96`](https://github.com/sozu-proxy/sozu/commit/87b4d96a83e6304bb817b32257b59b8396444ab7)
  ] populate https_frontends in ConfigState [`Emmanuel Bosquet`] (`2023-04-04`)
- [
  [`457a9cd`](https://github.com/sozu-proxy/sozu/commit/457a9cd6fb0c3e00177722e65de34dc3b83d0c86)
  ] protobuf scaffold [`Emmanuel Bosquet`] (`2023-04-04`)
- [
  [`7ca33c6`](https://github.com/sozu-proxy/sozu/commit/7ca33c674d0d8c58e52ada5ce47476813a9b8433)
  ] write RequestHttpFrontend in protobuf [`Emmanuel Bosquet`] (`2023-04-04`)
- [
  [`2989f49`](https://github.com/sozu-proxy/sozu/commit/2989f49ecbdecbf864b1f0692e36a4f359544288)
  ] write CertificateSummary in protobuf [`Emmanuel Bosquet`] (`2023-04-04`)
- [
  [`8907216`](https://github.com/sozu-proxy/sozu/commit/890721672d51bb464e3e1a6b0c2cad560ba1c060)
  ] write TlsVersion in protobuf [`Emmanuel Bosquet`] (`2023-04-04`)
- [
  [`36f01dc`](https://github.com/sozu-proxy/sozu/commit/36f01dc1e28facf5bb551180b3a6d2182890edbf)
  ] write CertificateAndKey in protobuf [`Emmanuel Bosquet`] (`2023-04-04`)
- [
  [`00f82cf`](https://github.com/sozu-proxy/sozu/commit/00f82cfeca758d921df7d28e82b8a491277048a1)
  ] write AddCertificate in protobuf [`Emmanuel Bosquet`] (`2023-04-04`)
- [
  [`9a37917`](https://github.com/sozu-proxy/sozu/commit/9a379176b7f4deeab6f7822283a24dba6efe92f6)
  ] write RemoveCertificate in protobuf [`Emmanuel Bosquet`] (`2023-04-04`)
- [
  [`5a77faf`](https://github.com/sozu-proxy/sozu/commit/5a77fafde2ca04ea17ddc84eaad05fed4b1c54c5)
  ] write ReplaceCertificate in protobuf [`Emmanuel Bosquet`] (`2023-04-04`)
- [
  [`f434974`](https://github.com/sozu-proxy/sozu/commit/f4349748adbee1209d8d63189c74dc76285f1185)
  ] write LoadBalancingAlgorithms in protobuf [`Emmanuel Bosquet`]
  (`2023-04-05`)
- [
  [`6716f8a`](https://github.com/sozu-proxy/sozu/commit/6716f8a06b4ff76e0e399ae7fd5a96d57c615086)
  ] write ProxyProtocolConfig in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [
  [`4e1e7d0`](https://github.com/sozu-proxy/sozu/commit/4e1e7d0c0e9fe779cac55c22ba33a82bf204b5cf)
  ] write LoadMetric in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [
  [`25f6018`](https://github.com/sozu-proxy/sozu/commit/25f6018ffc2a23fbfca09f72d28ae6da7a3c4805)
  ] write Cluster in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [
  [`82bb6c5`](https://github.com/sozu-proxy/sozu/commit/82bb6c5b479640b90fe9fbdcd5727e1dbbc821ba)
  ] write RequestTcpFrontend in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [
  [`685bb16`](https://github.com/sozu-proxy/sozu/commit/685bb1630e39c3fcc59d2a26e5bf5907c169570a)
  ] write RemoveBackend in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [
  [`bdd402f`](https://github.com/sozu-proxy/sozu/commit/bdd402f58ef9536186808fad913142cacb594a27)
  ] write AddBackend and LoadBalancingParams in protobuf [`Emmanuel Bosquet`]
  (`2023-04-05`)
- [
  [`c3969d2`](https://github.com/sozu-proxy/sozu/commit/c3969d258d72c474eb7393e9fdb4c3ad9e1ca784)
  ] write QueryClusterByDomain in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [
  [`a87214f`](https://github.com/sozu-proxy/sozu/commit/a87214f8ddea6760cee313108f39ab7d866a2b31)
  ] write QueryMetricsOptions in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [
  [`ba6928c`](https://github.com/sozu-proxy/sozu/commit/ba6928ceb5d98c506418a46fb79e58716d5ef954)
  ] write MetricsConfiguration in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [
  [`be75673`](https://github.com/sozu-proxy/sozu/commit/be75673c1c101a99ea9b7141429f543aa3db04b3)
  ] write RunState in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [
  [`a8c87a1`](https://github.com/sozu-proxy/sozu/commit/a8c87a187ded0c1af8e97eb8b85dbea973686cff)
  ] write WorkerInfo in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [
  [`c674b64`](https://github.com/sozu-proxy/sozu/commit/c674b64545546d5384920a8276e1497f13718769)
  ] write Percentiles in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [
  [`f6a84e1`](https://github.com/sozu-proxy/sozu/commit/f6a84e1f09ce7595550c84dca8994d98c5cc2cae)
  ] write FilteredTimeSerie in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [
  [`03332c7`](https://github.com/sozu-proxy/sozu/commit/03332c71ae9c23bc9d12a5c76c39b62e1019f164)
  ] write FilteredMetrics in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [
  [`332ed3e`](https://github.com/sozu-proxy/sozu/commit/332ed3e6d0062683c5802ce3e0bee597bae7714a)
  ] write BackendMetrics and ClusterMetrics in protobuf [`Emmanuel Bosquet`]
  (`2023-04-05`)
- [
  [`1edcf68`](https://github.com/sozu-proxy/sozu/commit/1edcf6869e34e440d92234948c2a921ab4f1638e)
  ] write WorkerMetrics in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [
  [`3e41887`](https://github.com/sozu-proxy/sozu/commit/3e41887b9cebc6f23ecd1c1d9aecdb81791e4878)
  ] write AggregatedMetrics in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [
  [`19d2915`](https://github.com/sozu-proxy/sozu/commit/19d29151cdecb1b0534efb5bc42b917c38782fa9)
  ] put field names to CertificateAndKey [`Emmanuel Bosquet`] (`2023-04-05`)
- [
  [`50b1a2e`](https://github.com/sozu-proxy/sozu/commit/50b1a2e5b62123e6b1864e2fb962b16f1eddd9fd)
  ] write HttpFrontendConfig in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [
  [`a91a576`](https://github.com/sozu-proxy/sozu/commit/a91a5765f9d7e6c48f4269996f4ae04d1e2e5826)
  ] write HttpsFrontendConfig in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [
  [`6daea56`](https://github.com/sozu-proxy/sozu/commit/6daea56ab017c065ecdfbc5bdfaa5520d790a1fb)
  ] write TcpListenerConfig in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [
  [`bd6d535`](https://github.com/sozu-proxy/sozu/commit/bd6d535c9174a34fb7e1714f2f53741788958c2e)
  ] write ListenersList in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [
  [`7617b34`](https://github.com/sozu-proxy/sozu/commit/7617b345bf74e4fd7751f53bda99181c6fdd95d4)
  ] write ListenerType in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [
  [`7254b98`](https://github.com/sozu-proxy/sozu/commit/7254b989cd859c97d70a13792fcf6e69ff2d10e1)
  ] write RemoveListener in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [
  [`c2df4b0`](https://github.com/sozu-proxy/sozu/commit/c2df4b06eb1377ea9f8611751333d47f29b938d3)
  ] write ActivateListener in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [
  [`d12920e`](https://github.com/sozu-proxy/sozu/commit/d12920e5d109c4cf42931a0bf8fdd105d1c2c864)
  ] write DeactivateListener in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [
  [`4472332`](https://github.com/sozu-proxy/sozu/commit/4472332a2854382b492a7874795c5f7bed42c5ac)
  ] remove useless HttpProxy, HttpsProxy, add TODOs [`Emmanuel Bosquet`]
  (`2023-04-05`)
- [
  [`dbd1fad`](https://github.com/sozu-proxy/sozu/commit/dbd1fada9a24af882c12b677e4745a7c9b592fd1)
  ] write AvailableMetrics in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [
  [`66e36d6`](https://github.com/sozu-proxy/sozu/commit/66e36d6e00c501b3dedff9debdb2ea8d9dc195f0)
  ] write Request in protobuf [`Emmanuel Bosquet`] (`2023-04-26`)
- [
  [`582d285`](https://github.com/sozu-proxy/sozu/commit/582d2850679b8e57aed740aafaa6b553ae4993b1)
  ] write ResponseStatus in protobuf [`Emmanuel Bosquet`] (`2023-04-28`)
- [
  [`d817d9d`](https://github.com/sozu-proxy/sozu/commit/d817d9d059b3c44180749c0e9841e414fe9ea874)
  ] create type WorkerInfos [`Emmanuel Bosquet`] (`2023-04-28`)
- [
  [`7214aa2`](https://github.com/sozu-proxy/sozu/commit/7214aa2ade93f40171b2a4c0117482ab66279325)
  ] replace ResponseContent::Status with ResponseContent::Workers [`Emmanuel
  Bosquet`] (`2023-04-28`)
- [
  [`7fb08f3`](https://github.com/sozu-proxy/sozu/commit/7fb08f3a800369a08cfa639c740a263c9f5b10e7)
  ] make response::Event a struct, create EventKind [`Emmanuel Bosquet`]
  (`2023-04-28`)
- [
  [`97fb5da`](https://github.com/sozu-proxy/sozu/commit/97fb5dad7240be4125213960b26c79e222877ced)
  ] write Event and EventKind in protobuf [`Emmanuel Bosquet`] (`2023-04-28`)
- [
  [`ae140f2`](https://github.com/sozu-proxy/sozu/commit/ae140f24a6fffd4a3192e5f9b79cfa5a21ed0401)
  ] remove the DumpState command [`Emmanuel Bosquet`] (`2023-04-28`)
- [
  [`2e26e76`](https://github.com/sozu-proxy/sozu/commit/2e26e763244214990915b8f7f36fad828338964f)
  ] create type ClusterHashes [`Emmanuel Bosquet`] (`2023-04-28`)
- [
  [`30e602d`](https://github.com/sozu-proxy/sozu/commit/30e602ded83b11ac127bbe4ecd0e24efb59f99bd)
  ] write ClusterInformation in protobuf [`Emmanuel Bosquet`] (`2023-04-28`)
- [
  [`f005d9a`](https://github.com/sozu-proxy/sozu/commit/f005d9a8ec5b8b1cfb1a9a54116470966d0f96e9)
  ] create type ClusterInformations [`Emmanuel Bosquet`] (`2023-04-28`)
- [
  [`802c3e9`](https://github.com/sozu-proxy/sozu/commit/802c3e9aa1c55cf5770244fa13d877eb08ddefdf)
  ] create type CertificateWithNames [`Emmanuel Bosquet`] (`2023-04-28`)
- [
  [`75b5ade`](https://github.com/sozu-proxy/sozu/commit/75b5ade95b727d320f7f39ebc97fc09abdbc7743)
  ] create types ListOfCertificatesByAddress and CertificatesByAddress
  [`Emmanuel Bosquet`] (`2023-04-28`)
- [
  [`537e0e9`](https://github.com/sozu-proxy/sozu/commit/537e0e9910b5641dac85f1aa5ed3dc4093fd2eaf)
  ] write ListedFrontends in protobuf [`Emmanuel Bosquet`] (`2023-04-28`)
- [
  [`dbab4b4`](https://github.com/sozu-proxy/sozu/commit/dbab4b4c93a1a853641e69e035c6a82f6f10893c)
  ] write ResponseContent in protobuf [`Emmanuel Bosquet`] (`2023-04-28`)
- [
  [`8cebed1`](https://github.com/sozu-proxy/sozu/commit/8cebed1ba391507b3ed635bf13c7cb674ece1e66)
  ] remove id from Response [`Emmanuel Bosquet`] (`2023-04-28`)
- [
  [`73c496d`](https://github.com/sozu-proxy/sozu/commit/73c496dec0160126b370ba245f51e0b2a6765904)
  ] remove protocol version from response [`Emmanuel Bosquet`] (`2023-04-28`)
- [
  [`d51590c`](https://github.com/sozu-proxy/sozu/commit/d51590c139aea2bb784747b091c6a866c518b1ec)
  ] write Response in protobuf [`Emmanuel Bosquet`] (`2023-04-28`)
- [
  [`cdc8629`](https://github.com/sozu-proxy/sozu/commit/cdc8629201c9e9df51a72acdec9590e8baa8ec67)
  ] remove the DumpState command from the protobuf Request [`Emmanuel Bosquet`]
  (`2023-05-03`)
- [
  [`22719bd`](https://github.com/sozu-proxy/sozu/commit/22719bd14a57bbad77d37940ea96ec45c4dc4458)
  ] remove JSON serialization tests [`Emmanuel Bosquet`] (`2023-05-03`)
- [
  [`acb0455`](https://github.com/sozu-proxy/sozu/commit/acb0455235d08d679624cdb41526855f8fad6c61)
  ] isolate method SessionManager::at_capacity() [`Emmanuel Bosquet`]
  (`2023-05-03`)
- [
  [`f9de7ba`](https://github.com/sozu-proxy/sozu/commit/f9de7bab7f1dcdb30e88b5ffa67d28a86a565891)
  ] create AcceptError::BufferCapacityReached, use it [`Emmanuel Bosquet`]
  (`2023-05-03`)
- [
  [`b125338`](https://github.com/sozu-proxy/sozu/commit/b125338a160bc8a91169ed10970230c1110b6681)
  ] use default values wherever possible [`Emmanuel Bosquet`] (`2023-04-12`)
- [
  [`edb6863`](https://github.com/sozu-proxy/sozu/commit/edb6863c9bdca8976c816b1c273cefb3ec60eab5)
  ] rewrite use statements [`Emmanuel Bosquet`] (`2023-04-13`)
- [
  [`570f8af`](https://github.com/sozu-proxy/sozu/commit/570f8af4486590bb7b3c77a9dd80630c505e5b8a)
  ] implement From<ContentType> for ResponseContent [`Emmanuel Bosquet`]
  (`2023-05-04`)

#### ⛑️ Fixed

- [
  [`4901718`](https://github.com/sozu-proxy/sozu/commit/490171851965727735e46c3efbacd564efd1bec1)
  ] fix nightly warning in network drain [`Emmanuel Bosquet`] (`2023-05-15`)
- [
  [`564177c`](https://github.com/sozu-proxy/sozu/commit/564177c5ae09b73a470df702c88ec65ed1cf6745)
  ] fix examples: use statements, bugs [`Emmanuel Bosquet`] (`2023-05-09`)
- [
  [`6fa15e5`](https://github.com/sozu-proxy/sozu/commit/6fa15e5617a077dd3fc0ea13a665038027efdd60)
  ] apply clippy fixes [`Emmanuel Bosquet`] (`2023-05-05`)
- [
  [`ea0db5d`](https://github.com/sozu-proxy/sozu/commit/ea0db5dcf93ed7ff9d4069bd264b3a7b8a9b615c)
  ] Fix a "blue green" issue: [`Eloi DEMOLIS`] (`2023-04-13`)
- [
  [`7555d38`](https://github.com/sozu-proxy/sozu/commit/7555d383acdee72e0224685d07a827faeb94217a)
  ] fix(proxy): Add power_of_two and least_loaded to FromStr trait [`Tim Bart`]
  (`2022-12-18`)
- [
  [`11407b8`](https://github.com/sozu-proxy/sozu/commit/11407b8800e87caea635f365d40d4dadfb28745a)
  ] fix(macos): minor tweaks to for cargo build to run successfully [`Tim Bart`]
  (`2022-12-18`)
- [
  [`384489c`](https://github.com/sozu-proxy/sozu/commit/384489c7e0ede18b2caedc20024fbe4d3e5f6e06)
  ] Use clippy with Rust 1.67.0 and format source code [`Florentin Dubois`]
  (`2023-02-06`)
- [
  [`fbad528`](https://github.com/sozu-proxy/sozu/commit/fbad528ff72113072c98ffad24213917a7b9644e)
  ] Update return for get_executable_path (freebsd) [`3boll`] (`2023-03-01`)

### 🥹 Contributors

- @alkavan made their first contribution in
  https://github.com/sozu-proxy/sozu/pull/693
- @kianmeng made their first contribution in
  https://github.com/sozu-proxy/sozu/pull/830
- @pims made their first contribution in
  https://github.com/sozu-proxy/sozu/pull/868
- @Sykursen made their first contribution in
  https://github.com/sozu-proxy/sozu/pull/893
- @jmingov made their first contribution in
  https://github.com/sozu-proxy/sozu/pull/894
- @Wonshtrum
- @Keksoj
- @FlorentinDUBOIS

**Full Changelog**: https://github.com/sozu-proxy/sozu/compare/0.14.2...0.14.3

## 0.14.2 - 2022-12-08

### 🌟 Features

- Added support of `brotli` (passthrough), see [
  [`7d9b560`](https://github.com/sozu-proxy/sozu/commit/7d9b560f36cb64b3c0d06d69a944445254bc3104)
  ]
- Remove `OpenSSL` in favor of `RusTLS`, see [
  [`22bf673`](https://github.com/sozu-proxy/sozu/commit/22bf673ea67df24fcf55bf18ddb5fece4444a2a9)
  ], [
  [`d8f6b30`](https://github.com/sozu-proxy/sozu/commit/d8f6b302072c85a82bb30ebfe7d5eaed4178727f)
  ], [
  [`79755c8`](https://github.com/sozu-proxy/sozu/commit/79755c83081b650401d7047ed700d54826e0e485)
  ] and [
  [`e202d1c`](https://github.com/sozu-proxy/sozu/commit/e202d1c2c0236f0aeccd4b2fc7596640f064d95c)
  ]
- Merge Sozu ACME implementation, see [
  [`339277d`](https://github.com/sozu-proxy/sozu/commit/339277d6f79fa34af4fd5ddc305af5934a82f2cd)
  ] and [
  [`7118e64`](https://github.com/sozu-proxy/sozu/commit/7118e640b321dc72b0f521af4d9fbc53c3610870)
  ]

### ✍️ Changed

- Update `RPM` packaging, see [
  [`0776217`](https://github.com/sozu-proxy/sozu/commit/07762173f210e2fbba2faa7ef48d581e12a4ce20)
  ], [
  [`37e90c2`](https://github.com/sozu-proxy/sozu/commit/37e90c28aa1e75a9476134f5b0fd685dcdfea562)
  ],
- Update dependencies and refactor a bunch of source code to prepare h2, see [ a
  lot of commits below 😛 ]

### ⚡ Breaking changes

We remove the support of `OpenSSL` in favor of `RusTLS`, so the tls provider
configurations associated to the selection of a tls provider has been dropped as
well, see [
[`e202d1c`](https://github.com/sozu-proxy/sozu/commit/e202d1c2c0236f0aeccd4b2fc7596640f064d95c)
]

### Changelog

#### ➕ Added

- [
  [`7d9b560`](https://github.com/sozu-proxy/sozu/commit/7d9b560f36cb64b3c0d06d69a944445254bc3104)
  ] add brotli to encoding header values [`Emmanuel Bosquet`] (`2022-11-29`)
- [
  [`22bf673`](https://github.com/sozu-proxy/sozu/commit/22bf673ea67df24fcf55bf18ddb5fece4444a2a9)
  ] Add support of OpenSSL 3.0.x [`Florentin Dubois`] (`2022-10-17`)
- [
  [`d8f6b30`](https://github.com/sozu-proxy/sozu/commit/d8f6b302072c85a82bb30ebfe7d5eaed4178727f)
  ] Add configuration options for OpenSSL TLS provider [`Florentin Dubois`]
  (`2022-10-20`)
- [
  [`79755c8`](https://github.com/sozu-proxy/sozu/commit/79755c83081b650401d7047ed700d54826e0e485)
  ] Merged https_openssl and https_rustls [`Eloi DEMOLIS`] (`2022-11-17`)
- [
  [`e202d1c`](https://github.com/sozu-proxy/sozu/commit/e202d1c2c0236f0aeccd4b2fc7596640f064d95c)
  ] Remove OpenSSL [`Eloi DEMOLIS`] (`2022-11-17`)
- [
  [`7118e64`](https://github.com/sozu-proxy/sozu/commit/7118e640b321dc72b0f521af4d9fbc53c3610870)
  ] import https://github.com/sozu-proxy/sozu-acme into the sozu command line
  [`Emmanuel Bosquet`] (`2022-12-07`)
- [
  [`339277d`](https://github.com/sozu-proxy/sozu/commit/339277d6f79fa34af4fd5ddc305af5934a82f2cd)
  ] update acme-lib and tiny_http dependencies [`Emmanuel Bosquet`]
  (`2022-12-07`)
- [
  [`da0f667`](https://github.com/sozu-proxy/sozu/commit/da0f667d70755ff89dc3f8871d6684e2c898e661)
  ] Add codeowners file [`Florentin Dubois`] (`2022-11-30`)

#### ✍️ Changed

- [
  [`0776217`](https://github.com/sozu-proxy/sozu/commit/07762173f210e2fbba2faa7ef48d581e12a4ce20)
  ] Making some updates to RPM build spec and script. [`Igal Alkon`]
  (`2021-07-31`)
- [
  [`37e90c2`](https://github.com/sozu-proxy/sozu/commit/37e90c28aa1e75a9476134f5b0fd685dcdfea562)
  ] Updating the rpm build script to have two modes of build the packages.
  [`Igal Alkon`] (`2021-07-31`)
- [
  [`8997ec2`](https://github.com/sozu-proxy/sozu/commit/8997ec2e31474019e5bdd71ecdc541ccd3308a85)
  ] Update changelog to add entry for 0.14.1 [`Florentin Dubois`] (`2022-10-13`)
- [
  [`073375f`](https://github.com/sozu-proxy/sozu/commit/073375fd4cc67a4d79ae5dfa5627e494b1a4fcca)
  ] Unit tests, comments and refactoring of command/src/channel.rs [`Emmanuel
  Bosquet`] (`2022-10-17`)
- [
  [`e6a4615`](https://github.com/sozu-proxy/sozu/commit/e6a4615dcbe797ed8c608d1554c3aeb2b2922c45)
  ] Update README.md to remove `ctl` crate and add `command` one [`Florentin
  Dubois`] (`2022-10-17`)
- [
  [`a930847`](https://github.com/sozu-proxy/sozu/commit/a930847c7b05e831d3680a2e1e7cb461d4b742dd)
  ] Fix typos [`Kian-Meng Ang`] (`2022-11-10`)
- [
  [`6fdfc18`](https://github.com/sozu-proxy/sozu/commit/6fdfc183014bd40f5185a333ba15dd0d0086d210)
  ] update build scripts with the --locked flag [`Emmanuel Bosquet`]
  (`2022-12-01`)
- [
  [`6f38476`](https://github.com/sozu-proxy/sozu/commit/6f38476a2b036f4143298ca8f2907134d08aa806)
  ] Update dependencies [`Florentin Dubois`] (`2022-12-01`)
- [
  [`ae8ffe7`](https://github.com/sozu-proxy/sozu/commit/ae8ffe78a4100e6c5ec0ea67e7fa12b0f55f0a47)
  ] Update behaviour of add_certificate to skip already loaded certificate
  [`Florentin Dubois`] (`2022-12-02`)
- [
  [`aef7baa`](https://github.com/sozu-proxy/sozu/commit/aef7baadb427118cb605b5fff379ea1b3283d226)
  ] Rename variables l to listener on command/src/config [`Florentin Dubois`]
  (`2022-12-02`)
- [
  [`19c4f70`](https://github.com/sozu-proxy/sozu/commit/19c4f7075cd7708f9ca1e3a9b412b1f6e732b957)
  ] Use rustfmt to format project [`Florentin Dubois`] (`2022-12-02`)
- [
  [`72bb233`](https://github.com/sozu-proxy/sozu/commit/72bb2333a62750f5ca59575fd0c299cd186fbbcd)
  ] fullfill syntax TODOs [`Emmanuel Bosquet`] (`2022-11-17`)
- [
  [`dceea6f`](https://github.com/sozu-proxy/sozu/commit/dceea6facf43476661cc9f9c17329069ee6845be)
  ] log ConnectionError with thiserror [`Emmanuel Bosquet`] (`2022-11-22`)
- [
  [`a7697ff`](https://github.com/sozu-proxy/sozu/commit/a7697ff5e358038f0692f6a211763ba86c1aba11)
  ] Use listener cert when https front lacks cert [`Ion Agorria`] (`2022-11-25`)
- [
  [`42c548b`](https://github.com/sozu-proxy/sozu/commit/42c548ba16d3ae1f7e5e49a7b5b333723632ddf6)
  ] set a TODO to handle EINPROGRESS error when connecting to backend [`Emmanuel
  Bosquet`] (`2022-11-25`)

#### 🚀 Refactored

- [
  [`c882946`](https://github.com/sozu-proxy/sozu/commit/c882946c4816df71b203b1585217cbc446b29d37)
  ] processing messages between main process and CLI [`Emmanuel Bosquet`]
  (`2022-10-20`)
- [
  [`e90cc52`](https://github.com/sozu-proxy/sozu/commit/e90cc52319343cd43db8d82ec5b3f6dfc89b698c)
  ] ctl: display response message for successful ProxyRequestOrder [`Emmanuel
  Bosquet`] (`2022-10-25`)
- [
  [`69aac18`](https://github.com/sozu-proxy/sozu/commit/69aac18ca8bfcd2d4a59fdcaa2e5961b6d888f86)
  ] http session: clean-up session creation [`Emmanuel Bosquet`] (`2022-11-08`)
- [
  [`f31c894`](https://github.com/sozu-proxy/sozu/commit/f31c89489524193b17580fafcd2de8972c5f6d92)
  ] tcp session: clean-up session creation [`Emmanuel Bosquet`] (`2022-11-08`)
- [
  [`94994a3`](https://github.com/sozu-proxy/sozu/commit/94994a3652eeafcbec5552cd4ec1086b95c84dda)
  ] htts_openssl session: clean-up session creation [`Emmanuel Bosquet`]
  (`2022-11-08`)
- [
  [`f5c6cf5`](https://github.com/sozu-proxy/sozu/commit/f5c6cf5dfaa5929ea3f2190ed3951205ebc4653c)
  ] https_rustls session: clean-up session creation [`Emmanuel Bosquet`]
  (`2022-11-08`)
- [
  [`a4dc22e`](https://github.com/sozu-proxy/sozu/commit/a4dc22ee4b962a4862696d48b296f09ba4a1d8f3)
  ] remove useless front_socket_mut functions on tcp and http Session [`Emmanuel
  Bosquet`] (`2022-11-08`)
- [
  [`3e2f445`](https://github.com/sozu-proxy/sozu/commit/3e2f4450ac38547d3593f64af60a269a7fc7b4d1)
  ] full error propagation on ConfigState::handle_order() [`Emmanuel Bosquet`]
  (`2022-11-16`)
- [
  [`42986fd`](https://github.com/sozu-proxy/sozu/commit/42986fd868be27f5244b04c9552d3891c0cacc8e)
  ] Error propagation on ScmSocket and Channel [`Emmanuel Bosquet`]
  (`2022-11-17`)
- [
  [`edfa1b4`](https://github.com/sozu-proxy/sozu/commit/edfa1b4d7262c4358c5a5ddf4d4ff50f57ee7f53)
  ] remove obsolete comment about main process crashing [`Emmanuel Bosquet`]
  (`2022-11-17`)
- [
  [`bfc4884`](https://github.com/sozu-proxy/sozu/commit/bfc4884b33c4ad621a053edb016051ef22909e6e)
  ] replace ConnectionError with anyhow::Result [`Emmanuel Bosquet`]
  (`2022-12-05`)
- [
  [`59bb989`](https://github.com/sozu-proxy/sozu/commit/59bb989ce0ce88da2dd99a7e8ab232314d70e321)
  ] merge client loop creation logic in the accept_clients function [`Emmanuel
  Bosquet`] (`2022-12-05`)
- [
  [`a6c7c9f`](https://github.com/sozu-proxy/sozu/commit/a6c7c9f077ef9fb4b9cdcd97ed506dabc348f589)
  ] error propagation on getting 404 and 503 answers from file system [`Emmanuel
  Bosquet`] (`2022-11-17`)
- [
  [`5d83eef`](https://github.com/sozu-proxy/sozu/commit/5d83eefd26ac6e560b999561e3901afb832ac586)
  ] define the ClusterId type only once [`Emmanuel Bosquet`] (`2022-11-17`)
- [
  [`605b95e`](https://github.com/sozu-proxy/sozu/commit/605b95ec593de8efd18696772b0513874f4634fb)
  ] abstract out the function reset_loop_time_and_get_timeout [`Emmanuel
  Bosquet`] (`2022-11-25`)
- [
  [`2959876`](https://github.com/sozu-proxy/sozu/commit/2959876d11d95d1e22e946e09cd6c54caf7c19d1)
  ] abstract out the function read_channel_messages_and_notify [`Emmanuel
  Bosquet`] (`2022-11-25`)
- [
  [`7743eba`](https://github.com/sozu-proxy/sozu/commit/7743eba56ddf86ce2f3eeee09bea1d9565e5fa08)
  ] abstract out the functions zombie_check and shutting_down_complete
  [`Emmanuel Bosquet`] (`2022-11-25`)
- [
  [`c5e2262`](https://github.com/sozu-proxy/sozu/commit/c5e2262ab7d048765ea99a56c94e155660f432ba)
  ] add fields to Server for a clearer loop flow [`Emmanuel Bosquet`]
  (`2022-11-25`)
- [
  [`96b90c9`](https://github.com/sozu-proxy/sozu/commit/96b90c99df179dd8597908f0d88c4d6234533b01)
  ] clearer syntax on read_channel_messages_and_notify, comments [`Emmanuel
  Bosquet`] (`2022-11-28`)
- [
  [`1aee08d`](https://github.com/sozu-proxy/sozu/commit/1aee08d034f872bea8a04808841476ea4da74ba5)
  ] create ConnectionError::MioConnectError [`Emmanuel Bosquet`] (`2022-11-28`)
- [
  [`33b3e5f`](https://github.com/sozu-proxy/sozu/commit/33b3e5fcbafbad94daca2520857830a55082663b)
  ] Abstract out notify proxys (#842) [`Emmanuel Bosquet`] (`2022-11-30`)

#### 💪 First works on H2

- [
  [`6bdb937`](https://github.com/sozu-proxy/sozu/commit/6bdb937e60abf63e92633602f1fdec71d930003a)
  ] Rename HTTPS states, remove Option around State, rename https.rs [`Eloi
  DEMOLIS`] (`2022-11-17`)
- [
  [`a819fa3`](https://github.com/sozu-proxy/sozu/commit/a819fa37edac3fef6b722b6dc51242cd47b6a6c5)
  ] Add Invalid State in HTTPS, add ALPN to Rustls ServerConfig [`Eloi DEMOLIS`]
  (`2022-11-17`)
- [
  [`26f7d15`](https://github.com/sozu-proxy/sozu/commit/26f7d15dc0613d004ebc78185deabe5c12bfa295)
  ] Make way for H2 with Alpn switch [`Eloi DEMOLIS`] (`2022-11-17`)
- [
  [`ec31f82`](https://github.com/sozu-proxy/sozu/commit/ec31f8235e69e3033cf89a8817664ee6f4ffca27)
  ] make RouteKey a common struct, make use of its methods [`Emmanuel Bosquet`]
  (`2022-11-17`)

#### ⛑️ Fixed

- [
  [`790f96b`](https://github.com/sozu-proxy/sozu/commit/790f96b49443c85d191bbe3bd83555c485eabda6)
  ] prevent concurrent testing in the github CI [`Emmanuel Bosquet`]
  (`2022-12-01`)
- [
  [`719b637`](https://github.com/sozu-proxy/sozu/commit/719b637a44f23a63eb09d6a865f0ad95ed76641e)
  ] ScmSocket::set_blocking updates its blocking field, takes &mut self
  [`Emmanuel Bosquet`] (`2022-12-01`)
- [
  [`2624a43`](https://github.com/sozu-proxy/sozu/commit/2624a430c5d1c02e257a3bf9ddb545f46e29c8fd)
  ] Fix Sessions close: [`Eloi DEMOLIS`] (`2022-12-02`)

### 🥹 Contributors

- @alkavan made their first contribution in
  https://github.com/sozu-proxy/sozu/pull/693
- @kianmeng made their first contribution in
  https://github.com/sozu-proxy/sozu/pull/830
- @Wonshtrum
- @Keksoj
- @FlorentinDUBOIS

**Full Changelog**: https://github.com/sozu-proxy/sozu/compare/v0.14.1...0.14.2

## 0.14.1 - 2022-10-13

### 🔖 Documentation

- 💪 Improve documentation about the lifecycle of a session, see
  [`7e223ee`](https://github.com/sozu-proxy/sozu/commit/7e223eee09b848f599b560f5a6d0dc918e94d20e),
  [`50a940a`](https://github.com/sozu-proxy/sozu/commit/50a940acad497358c9e60e28d0fed6310d26d290),
  [`a13eb35`](https://github.com/sozu-proxy/sozu/commit/a13eb3571853f9d1468e6d6a385df2a8d8cb7235)
  and
  [`2b6706c`](https://github.com/sozu-proxy/sozu/commit/2b6706c8e3d8ab4c94eefe8d7653cdbd918fc772)
- 🔑 Update TLS cipher suites, see
  [`3af39a6`](https://github.com/sozu-proxy/sozu/commit/3af39a6fff29d95e0baeccd3a6abfa63354607ce)

### ✍️ Changed

- 🔧 Use Rust edition 2021 and update dependencies, see
  [`693bc84`](https://github.com/sozu-proxy/sozu/commit/693bc84f0130a7fd6b6f5de3a294ac587bdb26d7),
  [`8f4449c`](https://github.com/sozu-proxy/sozu/commit/8f4449cd66b85cfa7c7684817115ba13a8762f61),
  [`e14109b`](https://github.com/sozu-proxy/sozu/commit/e14109bf208e912bfcd8f344b1538cfb02b02c4b),
  [`339ed21`](https://github.com/sozu-proxy/sozu/commit/339ed2126cf61d79fcb328675a93f14fed9b4b03),
  [`f064d8b`](https://github.com/sozu-proxy/sozu/commit/f064d8babdab23bb3f64ab7bc400abefa0015358)
  and
  [`0e3fffe`](https://github.com/sozu-proxy/sozu/commit/0e3fffeefe2a201c85255eda25b2b85babd65252)

### 📖 Changelog

- [
  [`07ccff3`](https://github.com/sozu-proxy/sozu/commit/07ccff35a2176171c3f6d218bdd1622ef2554146)
  ] Update changelog to add version v0.14.0 [`Florentin Dubois`] (`2022-10-06`)
- [
  [`7e223ee`](https://github.com/sozu-proxy/sozu/commit/7e223eee09b848f599b560f5a6d0dc918e94d20e)
  ] Rewrite session creation in lifetime_of_a_session.md [`Eloi DEMOLIS`]
  (`2022-10-11`)
- [
  [`50a940a`](https://github.com/sozu-proxy/sozu/commit/50a940acad497358c9e60e28d0fed6310d26d290)
  ] Continue work on the documentation, rename listen_token to listener_token
  [`Eloi DEMOLIS`] (`2022-10-11`)
- [
  [`a13eb35`](https://github.com/sozu-proxy/sozu/commit/a13eb3571853f9d1468e6d6a385df2a8d8cb7235)
  ] Finish rewriting existing parts [`Eloi DEMOLIS`] (`2022-10-11`)
- [
  [`2b6706c`](https://github.com/sozu-proxy/sozu/commit/2b6706c8e3d8ab4c94eefe8d7653cdbd918fc772)
  ] Finish the lifetime of a session documentation [`Eloi DEMOLIS`]
  (`2022-10-12`)
- [
  [`3af39a6`](https://github.com/sozu-proxy/sozu/commit/3af39a6fff29d95e0baeccd3a6abfa63354607ce)
  ] Update TLS cipher suites [`Florentin Dubois`] (`2022-10-12`)
- [
  [`693bc84`](https://github.com/sozu-proxy/sozu/commit/693bc84f0130a7fd6b6f5de3a294ac587bdb26d7)
  ] Update rust edition to 2021 [`Florentin Dubois`] (`2022-10-13`)
- [
  [`8f4449c`](https://github.com/sozu-proxy/sozu/commit/8f4449cd66b85cfa7c7684817115ba13a8762f61)
  ] Update command dependencies [`Florentin Dubois`] (`2022-10-13`)
- [
  [`e14109b`](https://github.com/sozu-proxy/sozu/commit/e14109bf208e912bfcd8f344b1538cfb02b02c4b)
  ] Update rust edition to 2021 [`Florentin Dubois`] (`2022-10-13`)
- [
  [`339ed21`](https://github.com/sozu-proxy/sozu/commit/339ed2126cf61d79fcb328675a93f14fed9b4b03)
  ] Update rust edition to 2021 [`Florentin Dubois`] (`2022-10-13`)
- [
  [`f064d8b`](https://github.com/sozu-proxy/sozu/commit/f064d8babdab23bb3f64ab7bc400abefa0015358)
  ] Update binary dependencies [`Florentin Dubois`] (`2022-10-13`)
- [
  [`0e3fffe`](https://github.com/sozu-proxy/sozu/commit/0e3fffeefe2a201c85255eda25b2b85babd65252)
  ] Fix a blocking clippy suggestion [`Florentin Dubois`] (`2022-10-13`)

### 🥹 Contributors

- @Wonshtrum
- @Keksoj
- @FlorentinDUBOIS

**Full Changelog**: https://github.com/sozu-proxy/sozu/compare/v0.14.0...0.14.1

## 0.14.0 - 2022-10-06

### 🌟 Features

- 🚀 A new HTTP/1 router, see
  [`5b58b91`](https://github.com/sozu-proxy/sozu/commit/5b58b91f14b1f98299911b5a3bac4cd0c11a39d2),
  [`016d89c`](https://github.com/sozu-proxy/sozu/commit/016d89ca72e309499d388464b211aa501e0cf135),
  [`a9dd46e`](https://github.com/sozu-proxy/sozu/commit/a9dd46e95d9c691e5266e5d476469829e4e1c93a),
  [`043b928`](https://github.com/sozu-proxy/sozu/commit/043b928e73cf36c11f4a750165bd61ad4284f812),
  [`c8504d4`](https://github.com/sozu-proxy/sozu/commit/c8504d4d9dab6053b31462c8fa14f1fe32a37e5a),
  [`5dc5c00`](https://github.com/sozu-proxy/sozu/commit/5dc5c0058192457f540b7614029c4d7279f9dc5b),
  [`c5b9bfa`](https://github.com/sozu-proxy/sozu/commit/c5b9bfa9d6702e0bc09c92f70f7fe5ed150c92e8),
  [`f96d1ac`](https://github.com/sozu-proxy/sozu/commit/f96d1ac7a632ee318bb2398b9c891d11eb8058cc),
  [`af38431`](https://github.com/sozu-proxy/sozu/commit/af38431236243c3183d5b1d75c483e33b8e07c81)
- 🤩 Bootstrap HTTP/2 work, see
  [`f247ce3`](https://github.com/sozu-proxy/sozu/commit/f247ce3783b912084a330bdf43d861c9c748dfed),
  [`65d5043`](https://github.com/sozu-proxy/sozu/commit/65d5043f8629744c166994d7e4c71d78983eccf9),
  [`6da9038`](https://github.com/sozu-proxy/sozu/commit/6da9038355f6c27060ef89a87e6110a33ba9a995),
- 1️⃣ One command line to rules them all, see
  [`8555e44`](https://github.com/sozu-proxy/sozu/commit/8555e44987fe3b0939403c24482fb65cbac93bb1)
- ✨ Improve metrics, see
  [`b7fa649`](https://github.com/sozu-proxy/sozu/commit/b7fa649a5778a498bf096a03b33274fbdf783557),
  [`8ffca7f`](https://github.com/sozu-proxy/sozu/commit/8ffca7f7e3e812a9bdacd3a08ba19ea564a2d740),
  [`9e6bd1d`](https://github.com/sozu-proxy/sozu/commit/9e6bd1df2dce563c831fcc71d51d7d0980128aec),
  [`7515a7e`](https://github.com/sozu-proxy/sozu/commit/7515a7e9d40e17090a9d6d63a23e5a75f3669b55),
  [`809a481`](https://github.com/sozu-proxy/sozu/commit/809a48130f5408e008d85c04f50a154b9ec5174e),
  [`67b98d0`](https://github.com/sozu-proxy/sozu/commit/67b98d0dca4814a3476e71b48f5e44db3a2e3c66),
  [`086fdca`](https://github.com/sozu-proxy/sozu/commit/086fdca72a5d244f44f78b778bd4c20545a4a872),
  [`3b956fb`](https://github.com/sozu-proxy/sozu/commit/3b956fb15993316de9432762f1704e9ea3cff1cf),
  [`5691fb9`](https://github.com/sozu-proxy/sozu/commit/5691fb9161182aeb6cd8dd391f54494725ae6881),
  [`effc0a9`](https://github.com/sozu-proxy/sozu/commit/effc0a91c96f0cd52d1586a3c5c6788271c2a9f9),
  [`829ad4b`](https://github.com/sozu-proxy/sozu/commit/829ad4b6ad4e294ad4957d7201208384c4676bfa),
  [`3de4cb7`](https://github.com/sozu-proxy/sozu/commit/3de4cb7981525077524f71a6d2b316abe0ec33b4),
  [`6dd7f2f`](https://github.com/sozu-proxy/sozu/commit/6dd7f2fddabc668e5f8cf7aeeaf76c8550021316),
  [`5427107`](https://github.com/sozu-proxy/sozu/commit/54271078eed9220217cfaad73d6344e807f3d1f1),
  [`c271481`](https://github.com/sozu-proxy/sozu/commit/c2714818685f899bc9883620c67f7f51794aac45),
  [`7af7a98`](https://github.com/sozu-proxy/sozu/commit/7af7a98a98c918aca5ebfd59ee8fcf190c2f6723),
  [`d0f853a`](https://github.com/sozu-proxy/sozu/commit/d0f853a6fd4fab9f016f7d4e3ce752ec1908762f),
  [`aa1176a`](https://github.com/sozu-proxy/sozu/commit/aa1176ac6735187c15154507e55b9e818f542a7c),
  [`1c2c87a`](https://github.com/sozu-proxy/sozu/commit/1c2c87a9f4001f50163c7ff9c6d558d51d9da0aa),
  [`5309acb`](https://github.com/sozu-proxy/sozu/commit/5309acb7b385d7ba341eefda110abf7e6d44cf50),
  [`751c2e8`](https://github.com/sozu-proxy/sozu/commit/751c2e8deaff0e7b92d5963159006e071cc13e98),
  [`daacf30`](https://github.com/sozu-proxy/sozu/commit/daacf30c735853c41de0e6c1636ebb6ffd02d3d6),
  [`4f6a47f`](https://github.com/sozu-proxy/sozu/commit/4f6a47fb79c95fc9ca200822956846609213b01f),
  [`985afc0`](https://github.com/sozu-proxy/sozu/commit/985afc0e3f8ad5fcd0a041d331c903b8760dce06),
  [`3418b6f`](https://github.com/sozu-proxy/sozu/commit/3418b6f7847c7ca9c9c928c74a77da28b94e0d7f),
  [`f2d07eb`](https://github.com/sozu-proxy/sozu/commit/f2d07ebd3e32d75e830e86f024a872f7917cf3cf),
  [`ab34222`](https://github.com/sozu-proxy/sozu/commit/ab342224840a9b3b12edb7cccf06d5f7ec76c80a),
  [`aaf2224`](https://github.com/sozu-proxy/sozu/commit/aaf2224ed5f9aec4bceee116e6a5626271d9613c),
  [`f1dec19`](https://github.com/sozu-proxy/sozu/commit/f1dec19515ef286e9d617b45b46a05a75f6a16b3),
  [`67445c4`](https://github.com/sozu-proxy/sozu/commit/67445c4531b1cbf2316d30d4650a9448e076db7d),
  [`ae8d9c1`](https://github.com/sozu-proxy/sozu/commit/ae8d9c14e77a9d1e416f90396e01389f567a1066),
  [`b0844e3`](https://github.com/sozu-proxy/sozu/commit/b0844e37e7ac5f4046e5d592d7d92eaad4d6ffac),
  [`9eb2c01`](https://github.com/sozu-proxy/sozu/commit/9eb2c016713b69f3a09f0df78f21d363c2bd4f2f),
  [`e774cff`](https://github.com/sozu-proxy/sozu/commit/e774cff0208bfcb7f862bbfcc2fdc2163fd14a5d),
  [`81f24c4`](https://github.com/sozu-proxy/sozu/commit/81f24c4a9311d42238abb645c8ffcd763bd3cb48),
  [`c02b130`](https://github.com/sozu-proxy/sozu/commit/c02b130f45a925fcf316cd5540aeb49e320fd935),
  [`ce2c764`](https://github.com/sozu-proxy/sozu/commit/ce2c764101fc16d5eb5c6837c5cb6c9cee06999f)
- 🙈 Return a HTTP 503 status code when we reach too many connection, see
  [`a82a83f`](https://github.com/sozu-proxy/sozu/commit/a82a83f68d6929d6f69b1b355a66581efbffb1f8)
- Unified TLS certificate lifecycle and management, see
  [`a7952a1`](https://github.com/sozu-proxy/sozu/commit/a7952a10ea7e8e536148d1385026e9f2b28b7a7f),
  [`bbd4f5f`](https://github.com/sozu-proxy/sozu/commit/bbd4f5fd5af401c8c4d6d20db717297a54037d9b),
  [`7276e17`](https://github.com/sozu-proxy/sozu/commit/7276e17d27eaf9a70b5da5aff80050ecbfc3ab89)
- 1️⃣ Set event-based command handling, see
  [`5f62687`](https://github.com/sozu-proxy/sozu/commit/5f626872f9305b92ec7f9302cdc24785b49d012f),
  [`3c7ab65`](https://github.com/sozu-proxy/sozu/commit/3c7ab654e3282f8163530cfbbfae3114930593a5),
  [`d322bc2`](https://github.com/sozu-proxy/sozu/commit/d322bc2e6228e00a2960ea7d2f6f953a50d02249),
  [`33b6e11`](https://github.com/sozu-proxy/sozu/commit/33b6e11744c256419325427f40a1d625a7588f54),
  [`5bbde99`](https://github.com/sozu-proxy/sozu/commit/5bbde99bca1e31fe13ac6b78dffbacc05c5d73fe),
  [`3c24088`](https://github.com/sozu-proxy/sozu/commit/3c24088a2a5670876ef31850db13c6995dc2c73b)
- 💪 Allow to set custom tags on logs, see
  [`115b9b1`](https://github.com/sozu-proxy/sozu/commit/115b9b1eee9fc94ceedd1e016eda8c2f588155cb),
  [`a9959ba`](https://github.com/sozu-proxy/sozu/commit/a9959ba34dbe07130170f40204035a1bf14f97ac)

### ⚡ Breaking changes

Between the `v0.13.6` and `v0.14.0` structures present in the `command` folder
has changed which led to an incomptibility between those two versions.

### Changelog

#### ➕ Added

- [
  [`22f1a72`](https://github.com/sozu-proxy/sozu/commit/22f1a72f71c61a2eb6bd32c3e610b07caddd77f0)
  ] docs(ctl): add use cases [`Gaël Reyrol`] (`2021-04-19`)
- [
  [`c007794`](https://github.com/sozu-proxy/sozu/commit/c0077947b8b0ea1bd2d8d95b0d0f1a39ad770f9a)
  ] docs: update references to sozuctl [`Gaël Reyrol`] (`2021-04-19`)
- [
  [`9e5a2c6`](https://github.com/sozu-proxy/sozu/commit/9e5a2c61f7f0e09074a03a0d2d60a33bc6007264)
  ] docs(ctl): typo [`Gaël Reyrol`] (`2021-04-19`)
- [
  [`06a8e4b`](https://github.com/sozu-proxy/sozu/commit/06a8e4bec0874df165cfcfc115bbe7c9bb64b7af)
  ] start describing how client sessions are handled [`Geoffroy Couprie`]
  (`2021-07-30`)
- [
  [`f247ce3`](https://github.com/sozu-proxy/sozu/commit/f247ce3783b912084a330bdf43d861c9c748dfed)
  ] import WIP HTTP/2 implementation [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`65d5043`](https://github.com/sozu-proxy/sozu/commit/65d5043f8629744c166994d7e4c71d78983eccf9)
  ] change the default buffer size to accomodate HTTP/2 [`Geoffroy Couprie`]
  (`2022-07-13`)
- [
  [`6da9038`](https://github.com/sozu-proxy/sozu/commit/6da9038355f6c27060ef89a87e6110a33ba9a995)
  ] start integrating HTTP/2 in the HTTPS-OpenSSL proxy [`Geoffroy Couprie`]
  (`2022-07-13`)
- [
  [`c5b9bfa`](https://github.com/sozu-proxy/sozu/commit/c5b9bfa9d6702e0bc09c92f70f7fe5ed150c92e8)
  ] add an Equals path rule [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`b7fa649`](https://github.com/sozu-proxy/sozu/commit/b7fa649a5778a498bf096a03b33274fbdf783557)
  ] start integrating a KV store for metrics [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`8ffca7f`](https://github.com/sozu-proxy/sozu/commit/8ffca7f7e3e812a9bdacd3a08ba19ea564a2d740)
  ] merge the Count and Gauge columns in metrics dump [`Geoffroy Couprie`]
  (`2022-07-13`)
- [
  [`9e6bd1d`](https://github.com/sozu-proxy/sozu/commit/9e6bd1df2dce563c831fcc71d51d7d0980128aec)
  ] aggregate stored metrics [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`855c35e`](https://github.com/sozu-proxy/sozu/commit/855c35eaf4d27975fc8dbba5bec50d4de79f6d03)
  ] simplify the metrics printer [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`2a2c3b9`](https://github.com/sozu-proxy/sozu/commit/2a2c3b9c6d9f9ec51cb90784ca40273b8b0bd532)
  ] store an app level metric aggregating backend metrics [`Geoffroy Couprie`]
  (`2022-07-13`)
- [
  [`7515a7e`](https://github.com/sozu-proxy/sozu/commit/7515a7e9d40e17090a9d6d63a23e5a75f3669b55)
  ] add a separating character for app metrics [`Geoffroy Couprie`]
  (`2022-07-13`)
- [
  [`809a481`](https://github.com/sozu-proxy/sozu/commit/809a48130f5408e008d85c04f50a154b9ec5174e)
  ] sort cluster ids, backend ids and metric names to keep a consistent view
  [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`67b98d0`](https://github.com/sozu-proxy/sozu/commit/67b98d0dca4814a3476e71b48f5e44db3a2e3c66)
  ] add a request counter per cluster and backend [`Geoffroy Couprie`]
  (`2022-07-13`)
- [
  [`086fdca`](https://github.com/sozu-proxy/sozu/commit/086fdca72a5d244f44f78b778bd4c20545a4a872)
  ] metrics query in command line [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`912508a`](https://github.com/sozu-proxy/sozu/commit/912508a1eeb99546f30eb18deb7116a2fe518cab)
  ] store cluster and backnd level metrics in different trees [`Geoffroy
  Couprie`] (`2022-07-13`)
- [
  [`3b956fb`](https://github.com/sozu-proxy/sozu/commit/3b956fb15993316de9432762f1704e9ea3cff1cf)
  ] more structured answers for metrics queries [`Geoffroy Couprie`]
  (`2022-07-13`)
- [
  [`5691fb9`](https://github.com/sozu-proxy/sozu/commit/5691fb9161182aeb6cd8dd391f54494725ae6881)
  ] add the metrics list command [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`effc0a9`](https://github.com/sozu-proxy/sozu/commit/effc0a91c96f0cd52d1586a3c5c6788271c2a9f9)
  ] add a flag to refresh metrics output [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`829ad4b`](https://github.com/sozu-proxy/sozu/commit/829ad4b6ad4e294ad4957d7201208384c4676bfa)
  ] store and query time metrics [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`3de4cb7`](https://github.com/sozu-proxy/sozu/commit/3de4cb7981525077524f71a6d2b316abe0ec33b4)
  ] start aggregation for time metrics [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`6dd7f2f`](https://github.com/sozu-proxy/sozu/commit/6dd7f2fddabc668e5f8cf7aeeaf76c8550021316)
  ] reorder time metric key components [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`5427107`](https://github.com/sozu-proxy/sozu/commit/54271078eed9220217cfaad73d6344e807f3d1f1)
  ] allow queries with timestamps [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`c271481`](https://github.com/sozu-proxy/sozu/commit/c2714818685f899bc9883620c67f7f51794aac45)
  ] fix count and gauge aggregation [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`7af7a98`](https://github.com/sozu-proxy/sozu/commit/7af7a98a98c918aca5ebfd59ee8fcf190c2f6723)
  ] no need to aggregate data when clearing [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`d0f853a`](https://github.com/sozu-proxy/sozu/commit/d0f853a6fd4fab9f016f7d4e3ce752ec1908762f)
  ] deactivate metrics dump for now [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`aa1176a`](https://github.com/sozu-proxy/sozu/commit/aa1176ac6735187c15154507e55b9e818f542a7c)
  ] do not store time metrics every second [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`1c2c87a`](https://github.com/sozu-proxy/sozu/commit/1c2c87a9f4001f50163c7ff9c6d558d51d9da0aa)
  ] send the date field [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`5309acb`](https://github.com/sozu-proxy/sozu/commit/5309acb7b385d7ba341eefda110abf7e6d44cf50)
  ] metrics collection can now be disabled at runtime [`Geoffroy Couprie`]
  (`2022-07-13`)
- [
  [`751c2e8`](https://github.com/sozu-proxy/sozu/commit/751c2e8deaff0e7b92d5963159006e071cc13e98)
  ] clear time metrics like other metrics [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`daacf30`](https://github.com/sozu-proxy/sozu/commit/daacf30c735853c41de0e6c1636ebb6ffd02d3d6)
  ] reduce logging in metrics [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`a673a3b`](https://github.com/sozu-proxy/sozu/commit/a673a3b6a002be82d3795e29fbe5aee148bf3bfb)
  ] count config messages loaded at startup [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`4f6a47f`](https://github.com/sozu-proxy/sozu/commit/4f6a47fb79c95fc9ca200822956846609213b01f)
  ] time metrics can be deactivated independently [`Geoffroy Couprie`]
  (`2022-07-13`)
- [
  [`985afc0`](https://github.com/sozu-proxy/sozu/commit/985afc0e3f8ad5fcd0a041d331c903b8760dce06)
  ] no need to clear metrics if they are disabled [`Geoffroy Couprie`]
  (`2022-07-13`)
- [
  [`3418b6f`](https://github.com/sozu-proxy/sozu/commit/3418b6f7847c7ca9c9c928c74a77da28b94e0d7f)
  ] use batching for time metrics insertion [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`f2d07eb`](https://github.com/sozu-proxy/sozu/commit/f2d07ebd3e32d75e830e86f024a872f7917cf3cf)
  ] batch insertion of gauges and counts [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`acb0b06`](https://github.com/sozu-proxy/sozu/commit/acb0b06b469a956c1a6e32c27a7c65c87d01cd55)
  ] reduce allocations in time metrics handling [`Geoffroy Couprie`]
  (`2022-07-13`)
- [
  [`ab34222`](https://github.com/sozu-proxy/sozu/commit/ab342224840a9b3b12edb7cccf06d5f7ec76c80a)
  ] move back to in memory storage for metrics [`Geoffroy Couprie`]
  (`2022-07-13`)
- [
  [`aaf2224`](https://github.com/sozu-proxy/sozu/commit/aaf2224ed5f9aec4bceee116e6a5626271d9613c)
  ] simpify store_metric_at [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`a82a83f`](https://github.com/sozu-proxy/sozu/commit/a82a83f68d6929d6f69b1b355a66581efbffb1f8)
  ] return a HTTP 503 error when there are too many connections [`Geoffroy
  Couprie`] (`2022-07-13`)
- [
  [`a7952a1`](https://github.com/sozu-proxy/sozu/commit/a7952a10ea7e8e536148d1385026e9f2b28b7a7f)
  ] Create an unified certificate resolver for both https with openssl and
  rustls [`Florentin Dubois`] (`2022-07-13`)
- [
  [`53f6911`](https://github.com/sozu-proxy/sozu/commit/53f6911deb5f2423fa51372279b214071a9189cf)
  ] added error management in the connection to the socket [`Emmanuel Bosquet`]
  (`2022-07-13`)
- [
  [`bbd4f5f`](https://github.com/sozu-proxy/sozu/commit/bbd4f5fd5af401c8c4d6d20db717297a54037d9b)
  ] Use already parsed certificate and chains in rustls hello method [`Florentin
  Dubois`] (`2022-07-13`)
- [
  [`5f62687`](https://github.com/sozu-proxy/sozu/commit/5f626872f9305b92ec7f9302cdc24785b49d012f)
  ] make the command server entirely event-based [`Emmanuel Bosquet`]
  (`2022-07-13`)
- [
  [`3c7ab65`](https://github.com/sozu-proxy/sozu/commit/3c7ab654e3282f8163530cfbbfae3114930593a5)
  ] additional error logs and context [`Emmanuel Bosquet`] (`2022-07-13`)
- [
  [`d322bc2`](https://github.com/sozu-proxy/sozu/commit/d322bc2e6228e00a2960ea7d2f6f953a50d02249)
  ] edited comments in Command Server, a few bails [`Emmanuel Bosquet`]
  (`2022-07-13`)
- [
  [`33b6e11`](https://github.com/sozu-proxy/sozu/commit/33b6e11744c256419325427f40a1d625a7588f54)
  ] reset the CommandManager channel as nonblocking [`Emmanuel Bosquet`]
  (`2022-07-13`)
- [
  [`5bbde99`](https://github.com/sozu-proxy/sozu/commit/5bbde99bca1e31fe13ac6b78dffbacc05c5d73fe)
  ] Implements command parser with serde error propagation [`Emmanuel Bosquet`]
  (`2022-07-13`)
- [
  [`3c24088`](https://github.com/sozu-proxy/sozu/commit/3c24088a2a5670876ef31850db13c6995dc2c73b)
  ] Split ctl/command.rs into modules [`Emmanuel Bosquet`] (`2022-07-13`)
- [
  [`7276e17`](https://github.com/sozu-proxy/sozu/commit/7276e17d27eaf9a70b5da5aff80050ecbfc3ab89)
  ] Update certificate replacement behaviour (#762) [`Florentin DUBOIS`]
  (`2022-07-13`)
- [
  [`9b14a65`](https://github.com/sozu-proxy/sozu/commit/9b14a65ea0d05e811f106fc8ff42ea1226842e4d)
  ] Add support of openssl 1.1.1 [`Florentin Dubois`] (`2022-07-13`)
- [
  [`115b9b1`](https://github.com/sozu-proxy/sozu/commit/115b9b1eee9fc94ceedd1e016eda8c2f588155cb)
  ] Implements custom tags on access logs of protocols TCP, HTTP and HTTPS
  [`Emmanuel Bosquet`] (`2022-07-29`)
- [
  [`a9959ba`](https://github.com/sozu-proxy/sozu/commit/a9959ba34dbe07130170f40204035a1bf14f97ac)
  ] Add reference counting on for listeners on proxy [`Florentin Dubois`]
  (`2022-08-03`)
- [
  [`f1dec19`](https://github.com/sozu-proxy/sozu/commit/f1dec19515ef286e9d617b45b46a05a75f6a16b3)
  ] fix metrics enabling, disabling and clear on the CLI and Command Server
  [`Emmanuel Bosquet`] (`2022-08-09`)
- [
  [`67445c4`](https://github.com/sozu-proxy/sozu/commit/67445c4531b1cbf2316d30d4650a9448e076db7d)
  ] store cluster metrics in a simpler way, query them all [`Emmanuel Bosquet`]
  (`2022-08-19`)
- [
  [`ae8d9c1`](https://github.com/sozu-proxy/sozu/commit/ae8d9c14e77a9d1e416f90396e01389f567a1066)
  ] retrieve cluster and backend metrics by ids [`Emmanuel Bosquet`]
  (`2022-08-22`)
- [
  [`b0844e3`](https://github.com/sozu-proxy/sozu/commit/b0844e37e7ac5f4046e5d592d7d92eaad4d6ffac)
  ] filter metrics by metric name [`Emmanuel Bosquet`] (`2022-08-23`)
- [
  [`9eb2c01`](https://github.com/sozu-proxy/sozu/commit/9eb2c016713b69f3a09f0df78f21d363c2bd4f2f)
  ] clear the metrics LocalDrain every plain hour [`Emmanuel Bosquet`]
  (`2022-08-24`)
- [
  [`e774cff`](https://github.com/sozu-proxy/sozu/commit/e774cff0208bfcb7f862bbfcc2fdc2163fd14a5d)
  ] error management in metrics recording and retrieving [`Emmanuel Bosquet`]
  (`2022-08-24`)
- [
  [`81f24c4`](https://github.com/sozu-proxy/sozu/commit/81f24c4a9311d42238abb645c8ffcd763bd3cb48)
  ] gather and display main process metrics [`Emmanuel Bosquet`] (`2022-08-24`)
- [
  [`c02b130`](https://github.com/sozu-proxy/sozu/commit/c02b130f45a925fcf316cd5540aeb49e320fd935)
  ] refactor metrics query format and CLI metric command [`Emmanuel Bosquet`]
  (`2022-08-24`)
- [
  [`ce2c764`](https://github.com/sozu-proxy/sozu/commit/ce2c764101fc16d5eb5c6837c5cb6c9cee06999f)
  ] cli table, nice formatting [`Emmanuel Bosquet`] (`2022-08-29`)

#### ✍️ Changed

- [
  [`6382efd`](https://github.com/sozu-proxy/sozu/commit/6382efdca78c1cd644c3725876a35fee14d885b0)
  ] refactor: reorganize docs and typo [`Gaël Reyrol`] (`2021-04-16`)
- [
  [`803b482`](https://github.com/sozu-proxy/sozu/commit/803b48296ed45289f5197e9f2a57ceeafe0599f5)
  ] refactor: typo and move some blocks [`Gaël Reyrol`] (`2021-04-16`)
- [
  [`8a530c2`](https://github.com/sozu-proxy/sozu/commit/8a530c278e3984fb03bd05091f1cf07b952c43a6)
  ] initialize the logger before writing the pid file [`Emmanuel Bosquet`]
  (`2021-07-30`)
- [
  [`3f98117`](https://github.com/sozu-proxy/sozu/commit/3f98117ab322b57cb7d10badf89dd2fd0885bdfc)
  ] rewrite start function with beautiful error handling [`Emmanuel Bosquet`]
  (`2021-07-30`)
- [
  [`7628b46`](https://github.com/sozu-proxy/sozu/commit/7628b46f4b89b64c10a288995b86aeb60476d0ab)
  ] more readable error handling on write_pid_file() [`Emmanuel Bosquet`]
  (`2021-07-30`)
- [
  [`4ce87cb`](https://github.com/sozu-proxy/sozu/commit/4ce87cb567dd384d0d71c2f5e6ac0d6fc136f636)
  ] take review into account, return errors instead of Ok(()) [`Emmanuel
  Bosquet`] (`2021-08-18`)
- [
  [`4cd98ff`](https://github.com/sozu-proxy/sozu/commit/4cd98ff575dde071dd5c4cc3ca48b611689af27e)
  ] minor fixes to sozuctl, command.rs [`Emmanuel Bosquet`] (`2021-08-18`)
- [
  [`3b38893`](https://github.com/sozu-proxy/sozu/commit/3b3889314d1db6189aab5a4da27ccdf6aeb4a02b)
  ] added with_context() and map_err() to error management of sozuctl [`Emmanuel
  Bosquet`] (`2021-08-18`)
- [
  [`387fe2b`](https://github.com/sozu-proxy/sozu/commit/387fe2b6daea0eab810cc1ae6e168478046d8468)
  ] more harmonious, systematic error handling [`Emmanuel Bosquet`]
  (`2021-08-18`)
- [
  [`7979942`](https://github.com/sozu-proxy/sozu/commit/79799422b8c24c541e1dcf4eb7080dd9cf19281f)
  ] better follow-up of worker RunState [`Emmanuel Bosquet`] (`2021-08-18`)
- [
  [`5cb34ac`](https://github.com/sozu-proxy/sozu/commit/5cb34ac8ed473f4219a84bcffe43bd7a2268b873)
  ] comments about what is to change [`Emmanuel Bosquet`] (`2021-08-18`)
- [
  [`863ee59`](https://github.com/sozu-proxy/sozu/commit/863ee59e405d181d729974a45cd1f737b27fe2cb)
  ] added frustrated comments about code making no sense [`Emmanuel Bosquet`]
  (`2021-08-18`)
- [
  [`6b24a24`](https://github.com/sozu-proxy/sozu/commit/6b24a24b2541d5944e3862cfd00bd006da27224a)
  ] proper handling of WorkerClose by the CommandServer [`Emmanuel Bosquet`]
  (`2021-08-18`)
- [
  [`7cddf2c`](https://github.com/sozu-proxy/sozu/commit/7cddf2ca9c1e28860f7f2500d2b5d0b76613d5ce)
  ] add a test for protocol upgrades [`Geoffroy Couprie`] (`2021-08-20`)
- [
  [`78bf601`](https://github.com/sozu-proxy/sozu/commit/78bf601e72b09684c39af9f69e3fd7d8caf97f5d)
  ] update to nom 7.0, remove the last macros [`Geoffroy Couprie`]
  (`2021-08-23`)
- [
  [`9c551d6`](https://github.com/sozu-proxy/sozu/commit/9c551d6e6a3cd0bd090f43f84724eeff52dd5721)
  ] describe what happens when reading and parsing the request [`Geoffroy
  Couprie`] (`2021-08-25`)
- [
  [`1c0f75a`](https://github.com/sozu-proxy/sozu/commit/1c0f75af2474e84133b8ca37a08e7e19f395b8de)
  ] Replace the TODO "Why you should use Sōzu?" [`Arnaud Lemercier`]
  (`2021-11-08`)
- [
  [`7fb636f`](https://github.com/sozu-proxy/sozu/commit/7fb636fe52596cb3897f1cedcaa79d1a9861ec77)
  ] allow Dockerfile to choose Alpine base version (#755) [`Mickaël Wolff`]
  (`2022-02-21`)
- [
  [`5b58b91`](https://github.com/sozu-proxy/sozu/commit/5b58b91f14b1f98299911b5a3bac4cd0c11a39d2)
  ] replace the trie with a tree of hashmaps [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`96c0c5c`](https://github.com/sozu-proxy/sozu/commit/96c0c5c0f57ceaa2a1c8196738b738a949bcf7cd)
  ] create the router module [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`016d89c`](https://github.com/sozu-proxy/sozu/commit/016d89ca72e309499d388464b211aa501e0cf135)
  ] add a variant of the trie that supports regexp matches [`Geoffroy Couprie`]
  (`2022-07-13`)
- [
  [`4fa109c`](https://github.com/sozu-proxy/sozu/commit/4fa109cbb93d4dee8526ea5dc1bf803b1965d689)
  ] remove debug logs [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`a9dd46e`](https://github.com/sozu-proxy/sozu/commit/a9dd46e95d9c691e5266e5d476469829e4e1c93a)
  ] implement the new Router [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`043b928`](https://github.com/sozu-proxy/sozu/commit/043b928e73cf36c11f4a750165bd61ad4284f812)
  ] add new routing rules to configuration messages [`Geoffroy Couprie`]
  (`2022-07-13`)
- [
  [`c8504d4`](https://github.com/sozu-proxy/sozu/commit/c8504d4d9dab6053b31462c8fa14f1fe32a37e5a)
  ] use the new router and integrate in sozuctl [`Geoffroy Couprie`]
  (`2022-07-13`)
- [
  [`8a2a17c`](https://github.com/sozu-proxy/sozu/commit/8a2a17ca6a541f916f592a8191fc292a2e3e4455)
  ] ignore wildcard case in quickcheck tests [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`3822c5a`](https://github.com/sozu-proxy/sozu/commit/3822c5aea55b61d6b760fe186f4ab3b83beb0f92)
  ] update to cookie-factory 0.3 [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`8555e44`](https://github.com/sozu-proxy/sozu/commit/8555e44987fe3b0939403c24482fb65cbac93bb1)
  ] merge sozuctl in sozu [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`5dc5c00`](https://github.com/sozu-proxy/sozu/commit/5dc5c0058192457f540b7614029c4d7279f9dc5b)
  ] add deny rules [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`270bfee`](https://github.com/sozu-proxy/sozu/commit/270bfeefb43303f092383dfdc982c669670e8e48)
  ] rename HttpFront to HttpFrontend [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`cf59369`](https://github.com/sozu-proxy/sozu/commit/cf593699ce2ccabe4cf6a1aa63d909056d8f4ecb)
  ] rename CertFingerprint to CertificateFingerprint [`Geoffroy Couprie`]
  (`2022-07-13`)
- [
  [`f4f05b4`](https://github.com/sozu-proxy/sozu/commit/f4f05b46267b30bcb634426f2935a24c78091182)
  ] rename Application to Cluster [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`d2cf976`](https://github.com/sozu-proxy/sozu/commit/d2cf9764b0a6fe89398f0bd96e79ab4fb73caf7a)
  ] the HTTP frontends hashmap key should serialize to a string [`Geoffroy
  Couprie`] (`2022-07-13`)
- [
  [`95909ea`](https://github.com/sozu-proxy/sozu/commit/95909eaeabde905af11ded7ea27144687cefeb00)
  ] rename front to address [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`9d22c12`](https://github.com/sozu-proxy/sozu/commit/9d22c12dd4c777d662472a48004be3fcdf85c9e5)
  ] custom PartialOrd and Ord implementations for SocketAddr [`Geoffroy
  Couprie`] (`2022-07-13`)
- [
  [`1fff30d`](https://github.com/sozu-proxy/sozu/commit/1fff30de4484477d5347fbaa578bfe333602efeb)
  ] update dependencies [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`f96d1ac`](https://github.com/sozu-proxy/sozu/commit/f96d1ac7a632ee318bb2398b9c891d11eb8058cc)
  ] add routing based on the HTTP method [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`af38431`](https://github.com/sozu-proxy/sozu/commit/af38431236243c3183d5b1d75c483e33b8e07c81)
  ] make the method configurable through the cli [`Geoffroy Couprie`]
  (`2022-07-13`)
- [
  [`a6e7c72`](https://github.com/sozu-proxy/sozu/commit/a6e7c7210fbc6042639bdcd6d41798ed28ec2ed7)
  ] move target_to_backend to sozu-command-lib [`Geoffroy Couprie`]
  (`2022-07-13`)
- [
  [`b66b0c8`](https://github.com/sozu-proxy/sozu/commit/b66b0c805e9cd462339473da9bf827e17348b06e)
  ] update dependencies [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`f12bacb`](https://github.com/sozu-proxy/sozu/commit/f12bacbc340087c7da11dfe15487961bc96ec8e2)
  ] sled can create directly in a temporary file [`Geoffroy Couprie`]
  (`2022-07-13`)
- [
  [`f079127`](https://github.com/sozu-proxy/sozu/commit/f079127410b78c0a0bec54c76d01ee3921d7ade8)
  ] change key format [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`194908f`](https://github.com/sozu-proxy/sozu/commit/194908fbc86d5758210d21580effd743e07c2248)
  ] cosmetics [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`6a853ce`](https://github.com/sozu-proxy/sozu/commit/6a853ce0673d6f9608cce63fc6627c4f1c28ce4c)
  ] more logs [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`627dd0e`](https://github.com/sozu-proxy/sozu/commit/627dd0ead5e83bae362e4dfbe0e5958f55262e44)
  ] rusfmt pass [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`2e723bc`](https://github.com/sozu-proxy/sozu/commit/2e723bcc4ff7775159f49a0df4aa2a7b8f880d44)
  ] edition 2021 [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`2cfeff7`](https://github.com/sozu-proxy/sozu/commit/2cfeff7ad754de21de8f26fba10c941359cc047f)
  ] update dependencies [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`0328f0e`](https://github.com/sozu-proxy/sozu/commit/0328f0e8b50834fe4833140060de54d7204da963)
  ] update dependencies [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`92ded4a`](https://github.com/sozu-proxy/sozu/commit/92ded4a542b260cd5bca6eca9e9cb335c9e21594)
  ] store a mio::Registry in ProxyConfiguration implementations [`Geoffroy
  Couprie`] (`2022-07-13`)
- [
  [`538741c`](https://github.com/sozu-proxy/sozu/commit/538741ccfe65570952e3eb4de534046e6a177d91)
  ] remove the poll argument from ProxyConfiguration methods [`Geoffroy
  Couprie`] (`2022-07-13`)
- [
  [`e217e03`](https://github.com/sozu-proxy/sozu/commit/e217e037538d172561d044c170822c344c0df73b)
  ] store the sessions slab in a Rc<RefCell<>> [`Geoffroy Couprie`]
  (`2022-07-13`)
- [
  [`3e6ee85`](https://github.com/sozu-proxy/sozu/commit/3e6ee858bc02f9b5824cdffae5a00a1f3ebd579e)
  ] store the sessions slab in ProxyConfiguration implementations [`Geoffroy
  Couprie`] (`2022-07-13`)
- [
  [`72ccfe9`](https://github.com/sozu-proxy/sozu/commit/72ccfe9ee28f92cd6388b034a3180aed943d5700)
  ] now create_session uses the internal slab instance [`Geoffroy Couprie`]
  (`2022-07-13`)
- [
  [`7eef4f0`](https://github.com/sozu-proxy/sozu/commit/7eef4f0e1fc0bc75bccdebc61657d209de7b4946)
  ] use the internal slab instance in connect_to_backend [`Geoffroy Couprie`]
  (`2022-07-13`)
- [
  [`ad26bb8`](https://github.com/sozu-proxy/sozu/commit/ad26bb895e28d984f6225108645529db7b4bfa49)
  ] move slab capacity check in connect_to_backend [`Geoffroy Couprie`]
  (`2022-07-13`)
- [
  [`0716c42`](https://github.com/sozu-proxy/sozu/commit/0716c42cf6feeb5004f364ae16890cdad871d488)
  ] factor data in the new SessionManager object [`Geoffroy Couprie`]
  (`2022-07-13`)
- [
  [`f5b8290`](https://github.com/sozu-proxy/sozu/commit/f5b8290d664875b3ce1f7e3947809f7b8912a231)
  ] refactor session management [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`56e6579`](https://github.com/sozu-proxy/sozu/commit/56e6579dd32e3bb476a496f36b7dcb680fff17a5)
  ] simplify session creation [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`a152063`](https://github.com/sozu-proxy/sozu/commit/a15206320d748055c912ee0a859768b699e0a923)
  ] move close_session() to the session manager [`Geoffroy Couprie`]
  (`2022-07-13`)
- [
  [`8a9bbc9`](https://github.com/sozu-proxy/sozu/commit/8a9bbc9a3d93063a9c9ef3d7af77691e14af51b2)
  ] pass a Rc<Refcell<Proxy>> as argument to create_session [`Geoffroy Couprie`]
  (`2022-07-13`)
- [
  [`4ef0c60`](https://github.com/sozu-proxy/sozu/commit/4ef0c607d304b2283d1b21810e3320cef294725f)
  ] store an instance of Proxy in sessions, handle close() in ready() [`Geoffroy
  Couprie`] (`2022-07-13`)
- [
  [`77e36b8`](https://github.com/sozu-proxy/sozu/commit/77e36b87f453d77ca20678a12b189829328c1527)
  ] implement CloseBackend in sessions [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`11bf1c8`](https://github.com/sozu-proxy/sozu/commit/11bf1c88491636be50acc319b2b3ae205810c966)
  ] the circuit breaker check should be in the session [`Geoffroy Couprie`]
  (`2022-07-13`)
- [
  [`8f1b0cc`](https://github.com/sozu-proxy/sozu/commit/8f1b0cc6a24021145c9e28e48c8d0a7925392975)
  ] move some checks to the session object [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`e2f40a5`](https://github.com/sozu-proxy/sozu/commit/e2f40a5fb76f5fec99ab29a25bbb8e3cc3ac0ad1)
  ] simplify backend_from_request [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`1685d8f`](https://github.com/sozu-proxy/sozu/commit/1685d8fd48d5d7def53fcbc7f19ee81910a8b031)
  ] pass the session to the ready() method [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`6b7681b`](https://github.com/sozu-proxy/sozu/commit/6b7681b28145b055155a72ff6342a790604def00)
  ] move connect_to_backend to the session, call it from ready() [`Geoffroy
  Couprie`] (`2022-07-13`)
- [
  [`6310f19`](https://github.com/sozu-proxy/sozu/commit/6310f19affc4645df0dcf5c4afbd46686c2a4711)
  ] implement reconnect_to_backend in sessions [`Geoffroy Couprie`]
  (`2022-07-13`)
- [
  [`c6ce2fd`](https://github.com/sozu-proxy/sozu/commit/c6ce2fd21c699e8239d49c29bf16015129ddafd0)
  ] remove connect_to_backend from ProxyConfiguration [`Geoffroy Couprie`]
  (`2022-07-13`)
- [
  [`59c0548`](https://github.com/sozu-proxy/sozu/commit/59c0548432dabacb57d57aa629d5bf105cb30d0b)
  ] remove the ProxySessionCast trait [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`8a801cf`](https://github.com/sozu-proxy/sozu/commit/8a801cf752d1bc0956b60e17404a7710b6b9d2bc)
  ] deregister the back socket inside close_backend_inner() [`Geoffroy Couprie`]
  (`2022-07-13`)
- [
  [`fc8daa4`](https://github.com/sozu-proxy/sozu/commit/fc8daa4a2b552ffbd46873a7060552c408194441)
  ] deregister the front socket inside close_inner [`Geoffroy Couprie`]
  (`2022-07-13`)
- [
  [`3151dea`](https://github.com/sozu-proxy/sozu/commit/3151dea7be521fd57ae384cef5cfb9b96f6fd1c9)
  ] replace ProxySession::close() with close_inner(), remove close_session()
  [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`6c2da60`](https://github.com/sozu-proxy/sozu/commit/6c2da60f3720dd40e2aeeae7343d7b63a9e51f34)
  ] remove ProxySession::close_backend() [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`c573f40`](https://github.com/sozu-proxy/sozu/commit/c573f409cb37e32543d55c4e6b0dce1861340851)
  ] handle session close in ProxySession::timeout() [`Geoffroy Couprie`]
  (`2022-07-13`)
- [
  [`6dd1f30`](https://github.com/sozu-proxy/sozu/commit/6dd1f30cb02241b94441d7d188889008a0d489ea)
  ] remove interpret_session_order() [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`c74fb59`](https://github.com/sozu-proxy/sozu/commit/c74fb59534bd9d126e0c04920c2872cc22e89f82)
  ] handle session close in ProxySession::shutting_down() [`Geoffroy Couprie`]
  (`2022-07-13`)
- [
  [`61cb878`](https://github.com/sozu-proxy/sozu/commit/61cb87884ab54f9f7f1467c757a3a642adafa54f)
  ] clean up some warnings [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`f995e45`](https://github.com/sozu-proxy/sozu/commit/f995e45886c3f4c9ae3a0e9b9ce36f887e5fd4dd)
  ] handle ConnectBackend and ReconnectBackend in ready_inner [`Geoffroy
  Couprie`] (`2022-07-13`)
- [
  [`f78c9c6`](https://github.com/sozu-proxy/sozu/commit/f78c9c63da6d5ffc003e7f0e0c2b3519f7ad550f)
  ] remove a warning and a debug log [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`09a75ba`](https://github.com/sozu-proxy/sozu/commit/09a75bae61f4c8a39e0a2e2b08e74292e2c7fa31)
  ] anyhow almost everywhere [`Emmanuel Bosquet`] (`2022-07-13`)
- [
  [`c7fe82d`](https://github.com/sozu-proxy/sozu/commit/c7fe82da818c5933c0945de11f39ab72538fe692)
  ] remove returned anyhow::Result from CommandServer::run() [`Emmanuel
  Bosquet`] (`2022-07-13`)
- [
  [`b7e34cf`](https://github.com/sozu-proxy/sozu/commit/b7e34cf0e51a33c72566002dadc060f9889de71a)
  ] anyhow error management in main.rs [`Emmanuel Bosquet`] (`2022-07-13`)
- [
  [`047c03a`](https://github.com/sozu-proxy/sozu/commit/047c03a367fae0b749527ef417f00ff45124bfdc)
  ] clearer WorkerClose syntax [`Emmanuel Bosquet`] (`2022-07-13`)
- [
  [`8512878`](https://github.com/sozu-proxy/sozu/commit/8512878664ae4e24fcf22d8092033600f4a5a5b8)
  ] better syntax and error management in launch_worker() [`Emmanuel Bosquet`]
  (`2022-07-13`)
- [
  [`af29ec6`](https://github.com/sozu-proxy/sozu/commit/af29ec6889c0b30869e660104c0fbde79cb34d2a)
  ] add a loop in bin/src/ctl::upgrade_main() [`Emmanuel Bosquet`]
  (`2022-07-13`)
- [
  [`c18a540`](https://github.com/sozu-proxy/sozu/commit/c18a540ed6a88988500084e2efdff7ea76f93ea3)
  ] initialize logging in main.rs so that ctl() benefits from it [`Emmanuel
  Bosquet`] (`2022-07-13`)
- [
  [`da3cdc0`](https://github.com/sozu-proxy/sozu/commit/da3cdc0281ddc78c54c080a54f780393c91eb4d9)
  ] initialize logging in start() and in ctl() but not in main [`Emmanuel
  Bosquet`] (`2022-07-13`)
- [
  [`d74ff84`](https://github.com/sozu-proxy/sozu/commit/d74ff84b48b55f29758de9420301d132a22a1289)
  ] handle_worker_close() method [`Emmanuel Bosquet`] (`2022-07-13`)
- [
  [`22ae844`](https://github.com/sozu-proxy/sozu/commit/22ae84490d6c9299269aebb0f26e4016c771d329)
  ] some logging in ctl/command::upgrade_worker() [`Emmanuel Bosquet`]
  (`2022-07-13`)
- [
  [`a854c77`](https://github.com/sozu-proxy/sozu/commit/a854c775437ce2903cd913b7c3f72c430b56cee8)
  ] small corrections for review, adding error handling and removing useless
  code [`Emmanuel Bosquet`] (`2022-07-13`)
- [
  [`92fc1bf`](https://github.com/sozu-proxy/sozu/commit/92fc1bf3954dab921b62fd260306ce1fbe12f2d8)
  ] do not borrow the sessions slab while calling a session timeout [`Geoffroy
  Couprie`] (`2022-07-13`)
- [
  [`ec9fdf8`](https://github.com/sozu-proxy/sozu/commit/ec9fdf8324ad34f1ac46c1c383a6a3d111db1c6f)
  ] minor config file change [`Emmanuel Bosquet`] (`2022-07-13`)
- [
  [`3036749`](https://github.com/sozu-proxy/sozu/commit/30367493f8007c2e62f8febb0e7be671999e2b06)
  ] more verbose cli logging command [`Emmanuel Bosquet`] (`2022-07-13`)
- [
  [`6b141e0`](https://github.com/sozu-proxy/sozu/commit/6b141e008720a69cae94feeb83fd0a8da033581f)
  ] Add basic frontend list subcommand and hello world response [`Emmanuel
  Bosquet`] (`2022-07-13`)
- [
  [`e0b94bf`](https://github.com/sozu-proxy/sozu/commit/e0b94bf2454fc46229a9b07af5ae92d42ed411b2)
  ] added filtering of frontends by domain name [`Emmanuel Bosquet`]
  (`2022-07-13`)
- [
  [`ee64329`](https://github.com/sozu-proxy/sozu/commit/ee643293776cbd1a4530cfc749be92c006e80a90)
  ] better syntax on panick safeguard [`Emmanuel Bosquet`] (`2022-07-13`)
- [
  [`7f799d7`](https://github.com/sozu-proxy/sozu/commit/7f799d7002384b77161b9876d646ae12b7e5351e)
  ] update most dependencies [`Marc-Antoine Perennou`] (`2022-07-13`)
- [
  [`730d901`](https://github.com/sozu-proxy/sozu/commit/730d901a649fe10d8daf13f7a12158bae5f80d1a)
  ] comment out randomly failing test for now [`Marc-Antoine Perennou`]
  (`2022-07-13`)
- [
  [`57b07a4`](https://github.com/sozu-proxy/sozu/commit/57b07a44855affea0150f9d25087b4afeef4bf7b)
  ] don't use deprecated mio features [`Marc-Antoine Perennou`] (`2022-07-13`)
- [
  [`64715c7`](https://github.com/sozu-proxy/sozu/commit/64715c70a1c9912548dd142ad97b0718ba4d6268)
  ] switch to socket2 [`Marc-Antoine Perennou`] (`2022-07-13`)
- [
  [`b1fb359`](https://github.com/sozu-proxy/sozu/commit/b1fb35967db8813a22d46b8e3f64fab878aa07cf)
  ] silence warnings [`Marc-Antoine Perennou`] (`2022-07-13`)
- [
  [`13f153c`](https://github.com/sozu-proxy/sozu/commit/13f153cc7fca5ab03c22e50d65a86443d8969055)
  ] update to rustls 0.20 [`Marc-Antoine Perennou`] (`2022-07-13`)
- [
  [`d56536a`](https://github.com/sozu-proxy/sozu/commit/d56536a6b6a01b21add90242ad1a9bd8e23b4b1c)
  ] wrappring channel.read_message() with a timeout function [`Emmanuel
  Bosquet`] (`2022-07-13`)
- [
  [`0ca805e`](https://github.com/sozu-proxy/sozu/commit/0ca805eef5d4b1a9ff6d58eecdca0b58d2af8355)
  ] Created read_message_blocking_timeout() method on Channel [`Emmanuel
  Bosquet`] (`2022-07-13`)
- [
  [`48e9493`](https://github.com/sozu-proxy/sozu/commit/48e9493bf6b86c60bbbb03c47c9bfb738fcb6564)
  ] more verbose worker upgrade error [`Emmanuel Bosquet`] (`2022-07-13`)
- [
  [`5629cca`](https://github.com/sozu-proxy/sozu/commit/5629ccabe350be4b7051f8f49ab9c1d0675f7d8f)
  ] added proper timeout to upgrade_worker() call in upgrade_main() [`Emmanuel
  Bosquet`] (`2022-07-13`)
- [
  [`8f73aa1`](https://github.com/sozu-proxy/sozu/commit/8f73aa1d6555decdc3d52dcfd2f388dc290642c9)
  ] update mio to 0.8 [`Marc-Antoine Perennou`] (`2022-07-13`)
- [
  [`78272fc`](https://github.com/sozu-proxy/sozu/commit/78272fcc9702a517b55672dc0b127aa44034a631)
  ] error logging on getting saved_state from the config [`Emmanuel Bosquet`]
  (`2022-07-13`)
- [
  [`d9e008f`](https://github.com/sozu-proxy/sozu/commit/d9e008f2db9b4062dd209737609e267af8530b3a)
  ] comment in config.toml that the path of saved_state should be relative
  [`Emmanuel Bosquet`] (`2022-07-13`)
- [
  [`fba2f8e`](https://github.com/sozu-proxy/sozu/commit/fba2f8e6fdcd2e47368d0488895dba21eedf689d)
  ] adapt unit test of Config::load_from_path() [`Emmanuel Bosquet`]
  (`2022-07-13`)
- [
  [`c4ec939`](https://github.com/sozu-proxy/sozu/commit/c4ec939f2b51b2584f42768b5eeb5d56310a6ccf)
  ] handle_client_message returns Result<OrderSuccess> [`Emmanuel Bosquet`]
  (`2022-07-13`)
- [
  [`c9d0c6c`](https://github.com/sozu-proxy/sozu/commit/c9d0c6c73181a40037712e621c19adbb8c599df8)
  ] beautify use statements and CommandServer impl blocks [`Emmanuel Bosquet`]
  (`2022-07-13`)
- [
  [`9d60cd8`](https://github.com/sozu-proxy/sozu/commit/9d60cd8aa624bde7d86640f81c28f67555059c3d)
  ] Apply clippy suggestions using rust edition 2021 [`Florentin Dubois`]
  (`2022-07-13`)
- [
  [`e05b80e`](https://github.com/sozu-proxy/sozu/commit/e05b80eba26693748ac2c32327ff44b7638043cb)
  ] segregate the state parsing logic into parse_state_data() [`Emmanuel
  Bosquet`] (`2022-07-13`)
- [
  [`9d90d45`](https://github.com/sozu-proxy/sozu/commit/9d90d45e4679f658e5e94966d06756067c07bfe7)
  ] Revert "segregate the state parsing logic into parse_state_data()"
  [`Emmanuel Bosquet`] (`2022-07-13`)
- [
  [`11f916d`](https://github.com/sozu-proxy/sozu/commit/11f916ddb6c5e571044faf005e04d90db49e37e6)
  ] Format all use statements (#749) [`Emmanuel Bosquet`] (`2022-07-13`)
- [
  [`5aa0ee1`](https://github.com/sozu-proxy/sozu/commit/5aa0ee1325ce89943afc95e569790add31c990ad)
  ] sort use statements in files of main process (#750) [`Emmanuel Bosquet`]
  (`2022-07-13`)
- [
  [`fdcbacc`](https://github.com/sozu-proxy/sozu/commit/fdcbacc7548d5029f9c30b34a7ed20ed3eacf21b)
  ] commented the worker and client loops, renamed variables (#752) [`Emmanuel
  Bosquet`] (`2022-07-13`)
- [
  [`adf2d3a`](https://github.com/sozu-proxy/sozu/commit/adf2d3a72ff4b57a4a122c3bdad48ede09424a74)
  ] segregate the log level changing logic into its own function [`Emmanuel
  Bosquet`] (`2022-07-13`)
- [
  [`ce80563`](https://github.com/sozu-proxy/sozu/commit/ce805638462ebf018b8ff48b5a53a086d70ab1f4)
  ] better variable naming and comments in CommandServer::worker_order()
  [`Emmanuel Bosquet`] (`2022-07-13`)
- [
  [`1b3cd3c`](https://github.com/sozu-proxy/sozu/commit/1b3cd3c79490d6807e90583f54dfc081e771d513)
  ] Update workspace dependencies [`Florentin Dubois`] (`2022-07-13`)
- [
  [`da2adcf`](https://github.com/sozu-proxy/sozu/commit/da2adcf67b9aa026b8bd2b544ad2f952afed13b4)
  ] Update command, lib and binaries dependencies [`Florentin Dubois`]
  (`2022-07-13`)
- [
  [`697af1d`](https://github.com/sozu-proxy/sozu/commit/697af1d38f93e40fef55eee0a6bcb9ab5e8f2559)
  ] respond with ProxyResponseStatus::Error instead of panic when no listener is
  found [`Emmanuel Bosquet`] (`2022-07-13`)
- [
  [`15bd0fd`](https://github.com/sozu-proxy/sozu/commit/15bd0fd19b76ff57cd5baba6fae465fd0f3071cd)
  ] constructor functions for ProxyResponse [`Emmanuel Bosquet`] (`2022-07-13`)
- [
  [`a4e7dec`](https://github.com/sozu-proxy/sozu/commit/a4e7dec4d23dbcaa462c09a28ef4a5b6f13cbbaf)
  ] remove if let statements from server::run and some session logic [`Emmanuel
  Bosquet`] (`2022-07-20`)
- [
  [`17c376a`](https://github.com/sozu-proxy/sozu/commit/17c376afcecc47bde78dc926ce60670267e9e10b)
  ] refactor certificate logic in ctl, with Results instead of Options
  [`Emmanuel Bosquet`] (`2022-07-22`)
- [
  [`11bda07`](https://github.com/sozu-proxy/sozu/commit/11bda07680c090b4e898e92e28580cb44969b3f9)
  ] Remove all nonbreakable spaces [`Emmanuel Bosquet`] (`2022-07-29`)
- [
  [`9054d9c`](https://github.com/sozu-proxy/sozu/commit/9054d9c6ca31cf70fbf8aa437fbd8b44eee4f400)
  ] Use matching pattern and Entry enum to add listener [`Florentin Dubois`]
  (`2022-08-03`)
- [
  [`a8dde73`](https://github.com/sozu-proxy/sozu/commit/a8dde73027dc5dcf15e620e1acf6322d23f310dc)
  ] Use std::collections::HashMap instead of hashbrown::HashMap [`Florentin
  Dubois`] (`2022-08-03`)
- [
  [`55b6c58`](https://github.com/sozu-proxy/sozu/commit/55b6c580cecde379d171cbdae88d8278f037015c)
  ] Use clap 3 with derive instead of StructOpt [`Florentin Dubois`]
  (`2022-08-03`)
- [
  [`53a47ae`](https://github.com/sozu-proxy/sozu/commit/53a47ae426648fc9f0d42781fb8360c093f60e8b)
  ] Fix command line arguments conflicts [`Florentin Dubois`] (`2022-08-04`)
- [
  [`8004c76`](https://github.com/sozu-proxy/sozu/commit/8004c76da17f664d35444ab1dfa10e2fa20b34b3)
  ] correct command line tutorial in doc/configure_cli [`Emmanuel Bosquet`]
  (`2022-08-04`)
- [
  [`3403cc4`](https://github.com/sozu-proxy/sozu/commit/3403cc4792ec30068bcbc08d3ee84f88cbb1417e)
  ] Update dependencies [`Florentin Dubois`] (`2022-08-08`)
- [
  [`4f8eb4b`](https://github.com/sozu-proxy/sozu/commit/4f8eb4ba1f527314a44a4d9fb5176ef86b052dfd)
  ] Add convenient method as helpers and use `PartialOrd` and `Ord` derive
  instructions [`Florentin Dubois`] (`2022-08-08`)
- [
  [`4ccd277`](https://github.com/sozu-proxy/sozu/commit/4ccd27711368db4e163834bdf5424ab0c4a3eb02)
  ] rename "application" to "cluster" for consistency [`Emmanuel Bosquet`]
  (`2022-08-08`)
- [
  [`8860c94`](https://github.com/sozu-proxy/sozu/commit/8860c945fdffba2fd2040f29fd5a1cd5460e1424)
  ] rudimentary lexicon [`Emmanuel Bosquet`] (`2022-08-09`)
- [
  [`431b63f`](https://github.com/sozu-proxy/sozu/commit/431b63fa6b29b79a2f516eb626c80a2aa23bc439)
  ] debug a few things [`Emmanuel Bosquet`] (`2022-08-09`)
- [
  [`9f029ac`](https://github.com/sozu-proxy/sozu/commit/9f029acfefac321db2febe5f7a1faeedc7370132)
  ] restore anyhow to 1.0.59 [`Emmanuel Bosquet`] (`2022-08-09`)
- [
  [`0b9b92f`](https://github.com/sozu-proxy/sozu/commit/0b9b92f2ab2af6b6a70c527f506ccb3e24747a58)
  ] add an all-metrics command line option [`Emmanuel Bosquet`] (`2022-08-12`)
- [
  [`9b7fdc9`](https://github.com/sozu-proxy/sozu/commit/9b7fdc9eed1f15572521e6d380dc442069141c10)
  ] tree_mut getter function, comments on struct fields, variable renaming
  [`Emmanuel Bosquet`] (`2022-08-16`)
- [
  [`0cac89c`](https://github.com/sozu-proxy/sozu/commit/0cac89ce9fe9d912851c39e27407bd272d9049b7)
  ] refactor metrics printing with segreggated functions [`Emmanuel Bosquet`]
  (`2022-08-16`)
- [
  [`4962f38`](https://github.com/sozu-proxy/sozu/commit/4962f3818033a1c53b931873d6cbf11a82383b26)
  ] list both proxy metric names and cluster metric names, refactoring, variable
  renaming [`Emmanuel Bosquet`] (`2022-08-17`)
- [
  [`c8a9918`](https://github.com/sozu-proxy/sozu/commit/c8a9918801b518c4faafdfae0e8c6a0da206049b)
  ] metrics table formatting in the cli [`Emmanuel Bosquet`] (`2022-08-17`)
- [
  [`1d6722e`](https://github.com/sozu-proxy/sozu/commit/1d6722e420c7e8de7f8063e1379db7e279b1a7b0)
  ] format metrics in nice boxes, suggestions in comments [`Emmanuel Bosquet`]
  (`2022-08-18`)
- [
  [`ecf0353`](https://github.com/sozu-proxy/sozu/commit/ecf0353aec995bdbb024408ca75e13f13be73110)
  ] anyhow version to 1.0.62 [`Emmanuel Bosquet`] (`2022-08-18`)
- [
  [`7b213e5`](https://github.com/sozu-proxy/sozu/commit/7b213e5338507b4292f4a15452ba4235c1a76dbf)
  ] trickle up errors if no metric for a backend or cluster [`Emmanuel Bosquet`]
  (`2022-08-23`)
- [
  [`c44db1f`](https://github.com/sozu-proxy/sozu/commit/c44db1f9e62a914facebea743c94f0e3c43b79ba)
  ] Documenting comments and minor refactor [`Eloi DEMOLIS`] (`2022-08-31`)
- [
  [`9b19aae`](https://github.com/sozu-proxy/sozu/commit/9b19aae8abd8b56e42a2b8dc1fcce86416116db1)
  ] Renamed RequestLine and StatusLine and their raw versions [`Eloi DEMOLIS`]
  (`2022-08-31`)
- [
  [`8f2f1c0`](https://github.com/sozu-proxy/sozu/commit/8f2f1c08a6c1d45b8d669d63768584d6a6056de5)
  ] comments in bin imports, functions and struct fields, refactoring [`Emmanuel
  Bosquet`] (`2022-09-02`)
- [
  [`1a22d09`](https://github.com/sozu-proxy/sozu/commit/1a22d09f7be892cf6c3d5f9cdf8f38585284292d)
  ] proper error management on receive_listeners method of scm sockets
  [`Emmanuel Bosquet`] (`2022-09-02`)
- [
  [`a30eb4e`](https://github.com/sozu-proxy/sozu/commit/a30eb4ee4401d8ee6c65f8786c44a8b5b04978ac)
  ] variable renaming, documenting comments, light refactoring [`Emmanuel
  Bosquet`] (`2022-09-02`)
- [
  [`0955dcd`](https://github.com/sozu-proxy/sozu/commit/0955dcd97c6115484095c3f31be7ab39fe51676b)
  ] rename fields to better detailed names [`Emmanuel Bosquet`] (`2022-09-02`)
- [
  [`0d88159`](https://github.com/sozu-proxy/sozu/commit/0d88159980e9e80f350e3be1ff7100ee65af2bc7)
  ] rename start_worker_process into fork_main_into_worker [`Emmanuel Bosquet`]
  (`2022-09-05`)
- [
  [`5190f55`](https://github.com/sozu-proxy/sozu/commit/5190f555d5dce33f44241111b3b8a38471434b00)
  ] rename IPC sockets explicitly [`Emmanuel Bosquet`] (`2022-09-05`)
- [
  [`74b82ce`](https://github.com/sozu-proxy/sozu/commit/74b82ce7c4a4193d929605749046ce278fd9b649)
  ] better naming for worker variables [`Emmanuel Bosquet`] (`2022-09-05`)
- [
  [`5e77738`](https://github.com/sozu-proxy/sozu/commit/5e77738d723bd798ec96e3231698929a45c871a3)
  ] Renaming variables for clarity, light refactoring [`Eloi DEMOLIS`]
  (`2022-09-05`)
- [
  [`7b30066`](https://github.com/sozu-proxy/sozu/commit/7b30066108fa8d63f02878a4656448844021c253)
  ] Potential place for 499 integration [`Eloi DEMOLIS`] (`2022-09-07`)
- [
  [`3607dea`](https://github.com/sozu-proxy/sozu/commit/3607dea20f0be79da06bc594a00eaa867f0a2987)
  ] status() function in the Command Server (not finished) [`Emmanuel Bosquet`]
  (`2022-09-09`)
- [
  [`6d293c5`](https://github.com/sozu-proxy/sozu/commit/6d293c5b29aad2a535123d2b37adc64a69ec5add)
  ] finished implementing displaying of worker statuses [`Emmanuel Bosquet`]
  (`2022-09-09`)
- [
  [`b3c4f90`](https://github.com/sozu-proxy/sozu/commit/b3c4f90dc69b11368072b20ada9971c1d0a8214e)
  ] debugging [`Emmanuel Bosquet`] (`2022-09-12`)
- [
  [`c79af23`](https://github.com/sozu-proxy/sozu/commit/c79af2399356ed48b9c2f7785070184976dc603e)
  ] delete legacy status function in ctl [`Emmanuel Bosquet`] (`2022-09-12`)
- [
  [`8c1a0f4`](https://github.com/sozu-proxy/sozu/commit/8c1a0f41195405945b59bdf711ea15cc34d2bfff)
  ] anyhow error management on FileClusterConfig::to_cluster_config() and
  downstream [`Emmanuel Bosquet`] (`2022-09-19`)
- [
  [`766f56c`](https://github.com/sozu-proxy/sozu/commit/766f56ca9304a3603fd70164eaa32c50d6cef41e)
  ] anyhow error management on FileConfig::into() and downstream [`Emmanuel
  Bosquet`] (`2022-09-21`)
- [
  [`05a62f4`](https://github.com/sozu-proxy/sozu/commit/05a62f4a1d4c2c0f8656c0110bef17eda83b4d46)
  ] trickle errors on metrics setup [`Emmanuel Bosquet`] (`2022-09-21`)
- [
  [`3ec6eb1`](https://github.com/sozu-proxy/sozu/commit/3ec6eb111801144b077d36c218008ac9f0d830e1)
  ] http::start() returns anyhow::Result, trickle errors [`Emmanuel Bosquet`]
  (`2022-09-22`)
- [
  [`2563427`](https://github.com/sozu-proxy/sozu/commit/2563427e0b799a287af8db9c7cc94810a53a9e09)
  ] https_openssl::start() returns anyhow::Result, trickle errors [`Emmanuel
  Bosquet`] (`2022-09-22`)
- [
  [`863b3cf`](https://github.com/sozu-proxy/sozu/commit/863b3cf2af4ff3685417c5428e70983fd1f4e7ce)
  ] error management in all server starts and Server::new() [`Emmanuel Bosquet`]
  (`2022-09-23`)
- [
  [`abb4a7a`](https://github.com/sozu-proxy/sozu/commit/abb4a7a26502c87e97bf3f9c072dae6111c8e77d)
  ] Addresses part of #808 and #810 [`Eloi DEMOLIS`] (`2022-10-03`)
- [
  [`53be70e`](https://github.com/sozu-proxy/sozu/commit/53be70e04228c904c8b18c611e19b8affb6c44e9)
  ] Fix HTTP crash for missing listeners on accept while shutdown. [`Eloi
  DEMOLIS`] (`2022-10-04`)
- [
  [`1610b33`](https://github.com/sozu-proxy/sozu/commit/1610b3392e588d4b4ab560f0030b081e2851d0e9)
  ] Update dependencies and apply linter suggestions [`Florentin Dubois`]
  (`2022-10-04`)

#### ➖ Removed

- [
  [`e82588a`](https://github.com/sozu-proxy/sozu/commit/e82588a15e2e207ecd705faea31aa8179de0d9f6)
  ] remove the end keys [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`fcc5d17`](https://github.com/sozu-proxy/sozu/commit/fcc5d17b133afa9fa86fbefd076a90e5aa744342)
  ] unused file [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`ce0b23c`](https://github.com/sozu-proxy/sozu/commit/ce0b23c657a49e3daf96a921f68ab245201ce4fc)
  ] remove log message [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`5c808eb`](https://github.com/sozu-proxy/sozu/commit/5c808eb963658db8d285769a963b9b8cd1ca9a26)
  ] remove unused dependencies [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`5ef9338`](https://github.com/sozu-proxy/sozu/commit/5ef9338a18bd9a80cd4a9ba57d3842e1c20740e6)
  ] Remove unused futures member [`Florentin Dubois`] (`2022-07-13`)
- [
  [`4c7e8cb`](https://github.com/sozu-proxy/sozu/commit/4c7e8cbe734ebfab0594ee6c0d710229b1b81a5f)
  ] remove unwrap() and expect() statements [`Emmanuel Bosquet`] (`2022-09-19`)

#### ⛑️ Fixed

- [
  [`05341d7`](https://github.com/sozu-proxy/sozu/commit/05341d76911aa6101447fe5fc683c3967390bcbf)
  ] fix: when targeting musl, construct msghdr differently [`Nathaniel`]
  (`2021-02-22`)
- [
  [`f9af65e`](https://github.com/sozu-proxy/sozu/commit/f9af65eba32bdec5fb9d7f26484e3c31a9c05078)
  ] fix: link [`Gaël Reyrol`] (`2021-04-16`)
- [
  [`931e198`](https://github.com/sozu-proxy/sozu/commit/931e19877a0494d2a0c898600b7ccaa4d49bfc97)
  ] fix: typo [`Gaël Reyrol`] (`2021-04-16`)
- [
  [`6d2fcd0`](https://github.com/sozu-proxy/sozu/commit/6d2fcd045a5d93a531cf42b3de95d6c445ae6ce4)
  ] fix: typo [`Gaël Reyrol`] (`2021-04-16`)
- [
  [`6981f09`](https://github.com/sozu-proxy/sozu/commit/6981f097ef423212469c8c8f63ba1d8e00d7e906)
  ] test (and fix) close delimited responses [`Geoffroy Couprie`] (`2021-08-20`)
- [
  [`532a658`](https://github.com/sozu-proxy/sozu/commit/532a6589ae0b956aa4716a0cc0eba91aed719037)
  ] fix warnings [`Geoffroy Couprie`] (`2021-08-20`)
- [
  [`bab0156`](https://github.com/sozu-proxy/sozu/commit/bab01562dd872876e7d181fbcb0900ee01b87c1a)
  ] fix missing slash [`Alexey Pozdnyakov`] (`2021-12-18`)
- [
  [`d935da5`](https://github.com/sozu-proxy/sozu/commit/d935da5241b4ea243d5a8da8bc4b68b877579dc9)
  ] Allow passing domain names from the command line (#757) [`Sojan James`]
  (`2022-02-05`)
- [
  [`e0700fb`](https://github.com/sozu-proxy/sozu/commit/e0700fbc4fa859b9ba00a3b803403a729f99cddf)
  ] Fix the config.toml file (#754) [`Hubert Bonisseur`] (`2022-02-10`)
- [
  [`cbca349`](https://github.com/sozu-proxy/sozu/commit/cbca349441fa9aeb360a33fbcdc8c8a15815b838)
  ] fix build on stable [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`108876b`](https://github.com/sozu-proxy/sozu/commit/108876be561b3dbba4c2f1abf4e6c492b6f0572b)
  ] handle empty prefixes [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`ba6019e`](https://github.com/sozu-proxy/sozu/commit/ba6019edf213a5fe220a8a76c44f38ac083c3179)
  ] clean some warnings [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`f5f6700`](https://github.com/sozu-proxy/sozu/commit/f5f6700f27e7c8a77393a236db6a67620d2c32fe)
  ] fix dependencies and compilation [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`5729d5f`](https://github.com/sozu-proxy/sozu/commit/5729d5f43e120806499296c9dc9d48e761bc04d4)
  ] fix state hashing [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`9b345af`](https://github.com/sozu-proxy/sozu/commit/9b345afc36dd1b4df57ea91994a984079373adcc)
  ] fix performance of state hashing [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`99b1d29`](https://github.com/sozu-proxy/sozu/commit/99b1d2961de6ac757dc35a7c1c0b4eb0ed28e04a)
  ] remove some warnings [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`397620b`](https://github.com/sozu-proxy/sozu/commit/397620b9a41c3f7503b166cf82b67fb98269d490)
  ] missing RemoveCluster implementation [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`fce54ca`](https://github.com/sozu-proxy/sozu/commit/fce54ca083131538116a5defe45d513eaf181b92)
  ] fix unit tests [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`4b50212`](https://github.com/sozu-proxy/sozu/commit/4b50212a1b8aa0128c878e79802cd0cde9bfec2a)
  ] do not panic on sled errors [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`97e2fdd`](https://github.com/sozu-proxy/sozu/commit/97e2fdd38f973e75b49ea2d721255bf3c11830d0)
  ] fix answer message counting [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`d66ff8f`](https://github.com/sozu-proxy/sozu/commit/d66ff8fd201e02cd6f797f98065e0671a137de5e)
  ] fix some debug logs [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`478c6bb`](https://github.com/sozu-proxy/sozu/commit/478c6bbf2864d74ec428a72db4d3aac7da6d48ec)
  ] fix test compilation [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`e25013f`](https://github.com/sozu-proxy/sozu/commit/e25013fce55328c5da8c6866d65faa28cd597c88)
  ] fix warnings [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`4e17f86`](https://github.com/sozu-proxy/sozu/commit/4e17f86bffc58914d28cdf1a3fd39e6c0c51d742)
  ] fix doc build [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`4806fa1`](https://github.com/sozu-proxy/sozu/commit/4806fa1ea1ff93a3d338c1722b9cfd24b36e5ba6)
  ] fix the rustls proxy [`Geoffroy Couprie`] (`2022-07-13`)
- [
  [`6c70039`](https://github.com/sozu-proxy/sozu/commit/6c7003942845c4ad0afe8208a86a49eff6db934d)
  ] fix a warning [`Emmanuel Bosquet`] (`2022-07-13`)
- [
  [`01ec16c`](https://github.com/sozu-proxy/sozu/commit/01ec16c12101349b32b1caa6ec714ff6f2fd6545)
  ] fix socket removal on start [`Emmanuel Bosquet`] (`2022-07-13`)
- [
  [`5a62a49`](https://github.com/sozu-proxy/sozu/commit/5a62a4969c9a761e14d8677dc4292494a2a17de9)
  ] safeguard against thread panick in edge case scenario [`Emmanuel Bosquet`]
  (`2022-07-13`)
- [
  [`dc3edac`](https://github.com/sozu-proxy/sozu/commit/dc3edac8225ec72365989e5119b0b27ff894a829)
  ] check if a worker exists before trying to upgrade it [`Emmanuel Bosquet`]
  (`2022-07-13`)
- [
  [`e5f9b1d`](https://github.com/sozu-proxy/sozu/commit/e5f9b1d85affc4861b7908ae42a19710bfe49c62)
  ] Fix main process upgrade and shutdown [`Eloi DEMOLIS`] (`2022-09-05`)
- [
  [`f423a3e`](https://github.com/sozu-proxy/sozu/commit/f423a3e16e1a4dac613eaa3eea3fc08ea3e11c7c)
  ] Update parser of header value to trim linear white space [`Florentin
  Dubois`] (`2022-07-13`)

### 🥹 Contributors

- @arnolem made their first contribution in
  https://github.com/sozu-proxy/sozu/pull/727
- @av-elier made their first contribution in
  https://github.com/sozu-proxy/sozu/pull/746
- @sjames made their first contribution in
  https://github.com/sozu-proxy/sozu/pull/757
- @DeLaBatth made their first contribution in
  https://github.com/sozu-proxy/sozu/pull/754
- @LupusMichaelis made their first contribution in
  https://github.com/sozu-proxy/sozu/pull/755
- @Wonshtrum made their first contribution in
  https://github.com/sozu-proxy/sozu/pull/797
- @Geal
- @Keksoj
- @FlorentinDUBOIS

**Full Changelog**: https://github.com/sozu-proxy/sozu/compare/0.13.6...v0.14.0

## 0.11.17 - 2019-07-24

debug release

### Fixed

- TLS 1.3 metric

### Changed

- removed domain fronting check (temporary)

## 0.11.16 - 2019-07-23

### Fixed

- detect application level configuration changes in state diff
- TLS 1.3 is now working properly with OpenSSL

## 0.11.15 - 2019-07-19

### Fixed

- pass the app id from HTTP protocol to Pipe protocol when in websocket

## 0.11.14 - 2019-07-18

### Added

- more info in logs about socket errors

## 0.11.13 - 2019-07-12

### Added

- more logs and metrics around socket errors

### Fixed

- do not clear the metric update flag too soon

## 0.11.12 - 2019-07-10

### Fixed

- add logs-debug and logs-trace options to sozuctl to fix build on Exherbo

## 0.11.11 - 2019-07-09

### Added

- send 408 or 504 HTTP errors in case of timeouts
- backend connection time and response time metrics

### Fixed

- test back socket connections before reusing them

### Changed

- a metric is not sent again if its value did not change
- the backend id is added as matedata to backend metrics

## 0.11.10 - 2019-07-04

### Fixed

- test if the backend socket is still valid before reusing it

## 0.11.9 - 2019-06-28

debug release

## 0.11.8 - 2019-06-28

### Fixed

- do not duplicate backend if we modified a backend's parameters

## 0.11.7 - 2019-06-26

### Fixed

- fix infinite loop with front socket

## 0.11.6 - 2019-06-19

### Fixed

- check for existence of the unix logging socket

### Changed

- access log format: indicate if we got the log from HTTP or HTTPS sessions

## 0.11.5 - 2019-06-13

### Added

- will print the session's state if handling it resulted in an infinite loop

### Fixed

- websocket protocol upgrade

## 0.11.4 - 2019-06-07

### Fixed

- wildcard matching

## 0.11.3 - 2019-06-06

### Added

- sozuctl commands to query certificates
- more logs and metrics aroundSNI in OpenSSL

## 0.11.2 - 2019-05-21

### Added

- ActivateListener message for TCP proxies

### Fixed

- wildcard certificate mtching with multiple subdomains in configuration
- diff of TCP listener configuration

## 0.11.1 - 2019-05-06

### Changed

- activate jemallocator and link time optimization
- sozuctl now uses the buffer size defined in the configuration file

### Removed

- procinfo dependency

## 0.11 - 2018-11-15

breaking changes:

- the `public_address` field for listeners is now an `Option<SocketAddr>`, so
  it's configuration file format is `IP:port` instead of just an IP address
- the `AddHttpsFront` message does not use a certificate fingerprint anymore, so
  HTTPS frontends do not depend on certificate anymore

### Added

- unit tests checking for structure size changes
- more error handling in sozuctl
- new `automatic_state_save` option to store the configuration state
  automatically after changes
- event notification system: by sending the `SUBSCRIBE_EVENTS` message,
  configuration clients can get regular notifications, like messages indicating
  backend servers are down

### Fixed

- 100 continue behaviour was broken in 0.10 and fixed in 0.10.1
- sticky session cookies are now sent again
- Forwarded headers now indicates correct adresses

## 0.10.0 - 2018-10-25

breaking change: modules have been moved around in sozu-lib

### Added

- sozuctl has a "config check" command
- sozuctl shows the backup flag for backends
- sozuctl shows more info for TCP proxys

### Removed

- sozuctl displays an address column for backends, instead of IP and port

### Changed

- new code organization for sozu-lib, with everything related to protocol
  implementations in src/protocol
- refactoring of the HTTP protocol implementation
- in preparation for HTTP/2, the pool now handles instances of Buffer, not
  BufferQueue

### Fixed

- work on TCP proxy stability
- reduce allocations in the HTTP parser
- integer underflow when counting backends in the master state
- get the correct client IP in the HTTPS proxys logs
- do not panic when the client disconnects while we're in the Send proxy
  protocol implementation

## 0.9.0 - 2018-09-27

### Added

- a futures executor for asynchronous tasks in the master process
- custom 503 page per application

### Changed

- HTTP parser optimizations
- renamed various parts of the code and configuration protocol for more
  consistency

### Fixed

- upgrade process
- event loop behaviour around abckend connections
- openssl cipher configuration
- circuit breaker

## 0.8.0 - 2018-08-21

- metrics writing fixes
- event loop fixes
- front socket timeout verification
- configuration state verification optimizations
- rustls and openssl configuration fixes
- separate listeners as a specific configuration option
- configuration file refactoring and simplification
- zombie session check

## 0.7.0 - 2018-06-07

- more metrics
- circuit breaking in the TCP proxy

## 0.6.0 - 2018-04-11

- disable debug and trace logs in release builds
- rustls based HTTPS proxy
- ProxyClient trait refactoring
- proxy protocol implementation
- option to send metrics in InfluxDB's tagged format
- PID file

## 0.5.0 - 2018-01-29

- TCP proxy refactoring
- sozuctl UX
- HTTP -> HTTPS redirection
- documentation
- ReplaceCertifacte message

## 0.4.0 - 2017-11-29

- remove mio timeouts
- upgrade fixes
- optimizations

## 0.3.0 - 2017-11-21

- process affinity
- clean system shutdown
- implement 100 continue
- sticky sessions
- build scripts for Fedora, Atchlinux, RPM
- systemd unit file
- metrics
- load balancing algorithms
- retry policy algorithms

### Added

### Changed

### Removed

### Fixed

## 0.2.0 - 2017-04-20

- Event loop refactoring
- contribution guidelines

## 0.1.0 - 2017-04-04

Started implementation:

- TCP proxy
- HTTP proxy
- HTTPS proxy with SNI
- mio based event loop
- configuration diff messages support
- buffer based streaming
- Docker image
- HTTP keep alive
- tested getting configuration events directly from AMQP, was removed
- getting configuration events from a Unix socket
- configuration bootstrap from a TOML file
- logger implementation
- architecture based around master process and worker processes
- control with command line app sozuctl
- command library

[Unreleased]: https://github.com/sozu-proxy/sozu/compare/1.1.1...HEAD
[0.10.0]: https://github.com/sozu-proxy/sozu/compare/0.9.0...0.10.0
[0.9.0]: https://github.com/sozu-proxy/sozu/compare/0.8.0...0.9.0
[0.8.0]: https://github.com/sozu-proxy/sozu/compare/0.7.0...0.8.0
[0.7.0]: https://github.com/sozu-proxy/sozu/compare/0.6.0...0.7.0
[0.6.0]: https://github.com/sozu-proxy/sozu/compare/0.5.0...0.6.0
[0.5.0]: https://github.com/sozu-proxy/sozu/compare/0.4.0...0.5.0
[0.4.0]: https://github.com/sozu-proxy/sozu/compare/0.3.0...0.4.0
[0.3.0]: https://github.com/sozu-proxy/sozu/compare/0.2.0...0.3.0
[0.2.0]: https://github.com/sozu-proxy/sozu/compare/0.1.0...0.2.0
