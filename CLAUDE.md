Sōzu — a hot-reconfigurable HTTP/1.x + HTTP/2 reverse proxy (AGPL-3.0). Rust 2024 (MSRV 1.88.0, matching the `1.88.0` toolchain pinned in `rust-toolchain`). Upstream: `github.com/sozu-proxy/sozu`. The `feat/h2-mux` branch carries the H2 multiplexer rewrite; PR #1209 is open against `main`.

This file is the primary agent instruction for the repo. It is symlinked to `AGENTS.md` so OpenAI Codex picks up the same content. Anthropic's global `~/.claude/CLAUDE.md` conventions (worktree-first, GPG sign-off, commitizen style) still apply and are not duplicated here.

# Workspace

Cargo workspace with four members (`resolver = "2"`):

- `command/` — `sozu-command-lib` (LGPL-3.0). Protobuf IPC schema, config parser, state, logging macros, channel + SCM FD passing. Control-plane library.
- `lib/` — `sozu-lib` (AGPL-3.0). Event loop, protocols (H1, H2, TCP, TLS, proxy-protocol), routing, sockets, metrics, buffer pool. Single-threaded mio runtime.
- `bin/` — `sozu` binary + internal lib. Master/worker multiprocess supervisor, CLI (clap), config loader, hot-upgrade orchestrator, unix-socket command server.
- `e2e/` — `sozu-e2e`. Integration tests that spawn real workers + mock clients/backends. Enables `sozu-lib`'s `e2e-hooks` feature.

`fuzz/` is an out-of-workspace cargo-fuzz crate with two targets: `fuzz_frame_parser`, `fuzz_hpack_decoder`.

Dependency graph: `lib → command`; `bin → lib + command`; `e2e → lib + command` (`e2e-hooks`).

# Build

`protoc` (protobuf-compiler) is a hard prerequisite — `command/build.rs` runs `prost-build` and no Rust file compiles before `protoc` succeeds. There is no `.envrc` or `mise.toml` at the repo root; the toolchain is selected by `rust-toolchain`.

```bash
cargo build --locked                            # debug, all workspace members
cargo build --all-features --locked             # currently builds (verified 2026-04 on feat/h2-mux)
cargo build -p sozu --release --locked          # production binary (release = lto + codegen-units=1)
cargo +nightly fmt --all -- --check             # nightly REQUIRED: rustfmt.toml uses `ignore = [...]` which stable treats as nightly-only
cargo clippy --all-targets --locked             # currently emits one `clippy::if_same_then_else` in e2e/src/tests/h2_correctness_tests.rs — `-D warnings` fails until fixed
cargo test --workspace --locked                 # unit + e2e
cargo test -p sozu-e2e -- h2_                   # filter to H2 e2e tests (~149 tests)
cargo +nightly fuzz run fuzz_frame_parser       # from fuzz/, requires cargo-fuzz + nightly
```

No CI clippy/fmt job runs automatically — run both locally before pushing. CI (`.github/workflows/ci.yml`) matrixes stable/beta/nightly build + test; nightly is allowed to fail. The benchmark workflow builds `bombardier` and `lagging_server` from upstream repos — don't expect it to work in a fresh clone.

# Feature flags

| Flag | Crate | Effect |
|---|---|---|
| `default = ["jemallocator", "crypto-ring"]` | `bin` | jemalloc as global allocator + ring crypto provider |
| `crypto-ring` (default) | bin, lib | rustls + ring crypto provider |
| `crypto-aws-lc-rs` | bin, lib | rustls + aws-lc-rs crypto provider |
| `crypto-openssl` | bin, lib | rustls + openssl crypto provider (`rustls-openssl`) |
| `fips` | bin, lib | implies `crypto-aws-lc-rs` + activates `rustls/fips`. Precedence chain `fips > ring > aws-lc-rs > openssl` when several are enabled together |
| `e2e-hooks` | `lib` | Exposes test-injection APIs. **Never enable in production builds.** |
| `logs-debug`, `logs-trace` | all | Compile in `DEBUG`/`TRACE` levels (release strips them otherwise) |
| `tolerant-http1-parser` | lib, bin | Relaxes H1 parsing via `kawa/tolerant-parsing` |
| `simd` | lib, bin | Enables `kawa/simd` |
| `splice` | lib | Linux `splice(2)` fast path |
| `opentelemetry` | lib, bin | OTel export |

Crypto-provider features live in `bin/Cargo.toml` + `lib/Cargo.toml`; CI exercises all four cells (`crypto-ring`, `crypto-aws-lc-rs`, `crypto-openssl`, `fips`) plus the bare default-features baseline. Provider precedence at runtime is resolved in `lib/src/crypto.rs::default_provider()`.

# Code style

- **Edition 2024**, MSRV 1.88. Use 2024-only idioms freely.
- **Errors**: `thiserror` for library error enums, `anyhow` at binary boundaries. Follow the nearest existing `thiserror::Error` enum; don't introduce new error crates.
- **No panic on network-facing input.** In parser, socket, mux, TLS, command-channel, and config paths, convert invalid traffic into `SessionResult` / H2 `GOAWAY` / `RST_STREAM` / default HTTP answer + metric + contextual log. `unwrap`/`expect`/`panic!`/`unreachable!` are acceptable in tests and in hard internal invariants with useful messages.
- **Ownership**: prefer `ToOwned::to_owned()` over `Clone::clone()` when going `&str → String` or `&[u8] → Vec<u8>` (clearer intent). Stick with `.clone()` when the type is already `Clone + !ToOwned`.
- **Logging**: every protocol module defines its own `macro_rules! log_context!` / `log_context_lite!` / `log_module_context!` (see `protocol/mux/mod.rs:49/87/118`, `protocol/mux/router.rs:30`, `protocol/mux/connection.rs:40`, `protocol/mux/parser.rs:19`, `protocol/mux/pkawa.rs:25`, `protocol/mux/stream.rs:27`, `protocol/mux/converter.rs:24`, `protocol/rustls.rs:23`, `protocol/pipe.rs:26`, `protocol/kawa_h1/mod.rs:64`, `protocol/proxy_protocol/{expect,relay,send}.rs`, `tcp.rs:68`, `socket.rs:86/133`, `tls.rs`, `http.rs`, `https.rs`). Prefix tags `MUX`, `MUX-H1`, `MUX-H2`, `MUX-CONN`, `MUX-ROUTER`, `MUX-PARSER`, `MUX-PKAWA`, `MUX-STREAM`, `MUX-CONV`, `RUSTLS`, `SOCKET`, `PIPE`, `KAWA-H1`, `TCP`, `HTTP`, `HTTPS`, `TLS-RESOLVER`, `PROXY-EXPECT`, `PROXY-RELAY`, `PROXY-SEND` are load-bearing for log-search. Use the macros — do NOT call `log::info!`/`log::error!` directly from protocol code. When a macro has an `HttpContext` in scope, prefer `$http_ctx.log_context()` (`kawa_h1/editor.rs:587`) over hand-rolling a `LogContext { ... }` struct literal — the helper is canonical (see `rustls.rs:44`, `router.rs:67`) and renders the same `[session req cluster backend]` bracket as RUSTLS/PIPE/TCP. A regression guard at `lib/tests/log_layout.rs` (with a non-fatal `cargo:warning=` echo from `lib/build.rs`) catches drift; new sites must use the canonical envelope or join the `KNOWN_PREEXISTING_VIOLATIONS` allowlist if the legacy site is out of scope.
- **Log levels**: `debug!`/`trace!` for expected idle closes, timeouts, noisy state. `warn!`/`error!` for real protocol errors or invariant breaks.
- **Worker runtime is single-threaded per worker** — no `Arc<Mutex>` inside the event loop. Session state is slab-allocated.
- **No `async fn` in `lib/`.** `lib/` is pure mio + edge-triggered epoll; introducing tokio/futures there is a design violation. `e2e/` may use tokio because it hosts hyper-based mock clients.
- **Metrics macros** live in `lib/src/metrics/mod.rs`: `incr!`, `count!`, `gauge!`, `gauge_add!`, `time!`. Update `doc/configure.md` when adding or renaming a public metric. Gauge underflow is a correctness bug, not a rounding issue.
- **Don't hand-edit `command/src/proto/command.rs`.** It is regenerated by `prost-build` at build time and ignored by `rustfmt.toml`. Edit `command/src/command.proto` and let `build.rs` regenerate.
- **New `unsafe` in hot paths needs an invariant comment + a test.** Existing `unsafe` in socket and H2 code is gated by explicit local invariants; new usage follows suit.

# Testing

Unit tests live beside their modules (`#[cfg(test)] mod tests` in `lib/src/**`). Integration tests are in `e2e/src/tests/` and registered via `e2e/src/tests/mod.rs`:

- `h2_tests.rs`, `h2_correctness_tests.rs`, `h2_security_tests.rs`, `h2_security_{parser,session,sni,header_injection}.rs` — H2 mux coverage (~130 tests on this branch).
- `mux_tests.rs` — H1 + proxy-protocol + keepalive/hup edge cases.
- `h1_security_tests.rs`, `tls_tests.rs`, `tcp_tests.rs`.
- `fuzz_tests.rs` — `#[ignore]` wrappers around the cargo-fuzz targets. Run with `cargo test -p sozu-e2e -- --ignored fuzz`.
- `h2_utils.rs` — shared helpers, including `loop_read_*` (absorbs TCP segmentation in assertions).
- `tests.rs` — `setup_sync_test`, `setup_async_test`, worker harness, port-registry integration.

Mock backends live in `e2e/src/mock/`: `sync_backend.rs`, `async_backend.rs`, `h2_backend.rs`, `raw_h2_response_backend.rs`, `client.rs`, `https_client.rs`, `aggregator.rs`.

**E2E conventions:**

- **Never hardcode ports.** Allocate via `e2e/src/port_registry.rs`.
- **Always `loop_read_*` when asserting on TCP responses.** A single `read()` sees one segment under load. Commits `7a7e87d9`, `6bd85a3f`, `73341f4f` exist only to paper over this being skipped.
- **Prefer `repeat_until_error_or` / explicit deadlines over `sleep`** for timing-sensitive tests.
- **Upgrade work**: read `doc/upgrade_e2e_tests.md` and run `cargo test -p sozu-e2e test_upgrade`.
- **H2 parser / HPACK changes**: run the focused e2e tests plus both cargo-fuzz targets.

# H2 architecture (what reading code won't tell you)

- **Two-crate session split.** `ConnectionH2<Front>` in `mux/h2.rs` holds wire state (HPACK coders, flow window, flood counters). `Context<L>` in `mux/mod.rs` holds the buffer-owning `Vec<Stream>`. Streams are referenced across the two by a `GlobalStreamId = usize`. Relevant files: `mux/mod.rs`, `mux/connection.rs`, `mux/h1.rs`, `mux/h2.rs`, `mux/parser.rs` (nom-based frame parser), `mux/pkawa.rs` (HPACK + pseudo-header validation + RFC 9218 priorities), `mux/stream.rs`, `mux/router.rs`, `mux/converter.rs`, `mux/serializer.rs`, `mux/answers.rs`, `mux/shared.rs`, `mux/debug.rs`.
- **Canonical on-branch reference: `lib/src/protocol/mux/LIFECYCLE.md`** (~28 KB, actively maintained) covers the stream/slot lifecycle with `file.rs:LINE` citations. Read this first for any mux change. `doc/h2_mux_internals.md` is the second source. Older `doc/lifetime_of_a_session.md` still links to `https_openssl.rs` (removed); cross-check against current `lib/src/https.rs` and `protocol/rustls.rs` before trusting it.
- **Edge-triggered epoll via mio.** When you queue bytes from the read path, you MUST call `signal_pending_write` on the readiness tracker — there is no wake-up for free. Forgetting this produces "stuck session" bugs that only manifest at exact byte boundaries.
- **Never `shutdown(Shutdown::Both)` on a TLS frontend socket.** It emits a TCP RST which truncates the already-queued response. Use write-only shutdown; the peer close arrives via the normal read path.
- **`socket_write` and `socket_write_vectored` must stay structurally symmetric.** Past divergence (one retry-looped, the other didn't) caused the 4.5 MB truncation bug on this branch.
- **Buffer-pool sizing**: `buffer_size` must be >= 16393 (16384 max H2 frame + 9-byte header) globally when H2 is enabled. Smaller values deadlock the mux on large frames.
- **H2 config knobs** agents typically touch: `alpn_protocols`, `strict_sni_binding`, `disable_http11`, `h2_stream_idle_timeout_seconds`, `h2_max_header_table_size`, plus the flood thresholds in `doc/configure.md`. Frontend H2 is TLS ALPN-driven; `cluster.http2 = true` is a backend-capability hint (does NOT gate frontend H2).
- **Flood detection**: `H2FloodDetector` in `mux/h2.rs` mitigates CVE-2023-44487 (Rapid Reset), CVE-2024-27316 (CONTINUATION flood), CVE-2025-8671 (MadeYouReset), plus PING/SETTINGS/priority floods. Thresholds are listener-config knobs.
- **Master/worker model**: one supervisor forks `N` workers, each a single-threaded event loop over shared listener fds. Hot reconfig: unix socket → validate in master → fan out to workers via anonymous unix socket pairs. Upgrades re-exec and hand off fds (`bin/src/upgrade.rs`).

# Security-sensitive areas

Conservative changes + tests required in:

- `lib/src/tls.rs`, `lib/src/https.rs`, `lib/src/protocol/rustls.rs` — rustls config, ALPN, SNI binding, cert loading, close-notify.
- `lib/src/protocol/mux/` — parser, HPACK, pseudo-header ordering, request smuggling, content-length reconciliation, flow control, GOAWAY/RST_STREAM semantics, edge-triggered readiness.
- `lib/src/protocol/proxy_protocol/` — partial headers, oversized headers, TCP health checks.
- `command/src/channel.rs`, `command/src/scm_socket.rs`, `command/src/command.proto`, `bin/src/command/` — message framing, buffer sizing, FD passing, state replay.
- Metrics / logs — never leak sensitive values; counters/gauges must stay correct on error paths and shutdown.

Crypto-provider features (`crypto-ring`, `crypto-aws-lc-rs`, `crypto-openssl`, `fips`) live in `bin/Cargo.toml` + `lib/Cargo.toml`; CI exercises all four cells.

# Branching & commits

- **Branch naming**: `feat/<scope>`, `fix/<scope>`, `refactor/<scope>`, `chore/<scope>`, `docs/<scope>`, `test/<scope>`, `perf/<scope>`, `style/<scope>`, `ci/<scope>`, `bench/<scope>`.
- **Conventional commits** with scope: `feat(h2): ...`, `fix(mux): ...`, `test(e2e): ...`, etc. No commitizen config — follow the existing `git log` style manually.
- **Sign + GPG**: `git commit -s -S` for new commits. History on this branch has a minority of unsigned/unsignedoff commits; keep new ones signed.
- **PR base is `main`.** For sub-worktrees forked off `feat/h2-mux` (most in-flight work), manually target `feat/h2-mux` when merging back. `wt merge -y` defaults to the repo default (`main`) — confirm the target before using it.
- **After committing: push.** On this repo, `git push` is part of the commit task; don't stop at `git commit`. Post the resulting `github.com/sozu-proxy/sozu/actions` run URL.
- **PRs** are reviewed by CODEOWNERS (`@FlorentinDUBOIS @Wonshtrum @llenotre`) and gated by the CLA in `CONTRIBUTING.md`. PR descriptions should call out protocol/security impact, commands run, tests added, and doc updates.

# Gotchas

- **`protoc` missing** = `command/build.rs` fails before any Rust compiles. CI installs `protobuf-compiler` explicitly.
- **`rust-toolchain` pins `1.88.0`** — local builds must match. CI still exercises stable/beta/nightly.
- **Release strips `DEBUG`/`TRACE` logs** unless built with `--features logs-debug,logs-trace`.
- **Repro for event-loop / zombie bugs**: `worker_count = 1`, `worker_automatic_restart = false`, `RUST_BACKTRACE=1`.
- **`CHANGELOG.md` follows Keep-a-Changelog.** Update it when a change ships user-visible behavior (config keys, metrics, CLI flags).
- **Cross-check non-trivial changes with Codex** (`/codex` skill): the user habitually validates analyses, fixes, and PR descriptions with a second model before shipping.

# Deployment

```bash
cargo build -p sozu --release --locked
target/release/sozu start -c /path/to/config.toml
```

Container: `docker build -t sozu:local .` (Dockerfile installs `protobuf`, `protobuf-dev`, `pkgconfig`, `llvm-libunwind`; uses `cargo build --release --frozen`). Release images push as `clevercloud/sozu:${GITHUB_SHA}`. OS packaging in `os-build/` (systemd, RPM, Arch). Upgrade + release guidance: `RELEASE.md` + `doc/upgrade_e2e_tests.md`.
