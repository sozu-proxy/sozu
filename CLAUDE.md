# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Sozu is a lightweight, fast, always-up reverse proxy server written in Rust. It supports hot reconfiguration without reloading and upgrades itself while still processing requests.

## Build Commands

```bash
# Build all workspace members (default crypto provider: crypto-ring)
cargo build --workspace

# Release build (LTO + single codegen unit)
cargo build --release

# Run clippy
cargo clippy --workspace

# Format with nightly rustfmt
cargo +nightly fmt

# Run all tests
cargo test --workspace

# Run a specific test
cargo test test_name

# Run e2e tests
cd e2e && cargo test

# Run benchmarks (must specify a crypto provider)
cargo bench --features crypto-ring -p sozu-lib

# Debug with logging
RUST_LOG=debug cargo test test_name
```

**`--all-features` does NOT work.** Crypto providers are mutually exclusive — enabling more than one triggers `compile_error!`. Build with a specific provider:
```bash
cargo build -p sozu --no-default-features --features crypto-aws-lc-rs
cargo build -p sozu --no-default-features --features crypto-openssl
```

On non-x86_64 (e.g., MacOS ARM), disable SIMD: `cargo build --workspace --no-default-features`

## Feature Flags

Exactly one crypto provider must be enabled in `sozu-lib`:

| Feature | Provider | Notes |
|---------|----------|-------|
| `crypto-ring` | ring | Default in `sozu` binary, pure Rust |
| `crypto-aws-lc-rs` | aws-lc-rs | Faster bulk transfers, PQ key exchange (X25519MLKEM768) |
| `crypto-openssl` | rustls-openssl | OpenSSL-backed, PQ support with OpenSSL 3.5+ |
| `fips` | aws-lc-rs (FIPS mode) | Implies `crypto-aws-lc-rs`, requires cmake + Go at build time. Restricts to NIST curves + AES-GCM only (no X25519, no CHACHA20) |

Other features: `simd` (SSE, x86_64 only), `splice` (splice syscall), `logs-debug`, `logs-trace`, `tolerant-http1-parser`, `unstable`

The `sozu` binary (`bin/Cargo.toml`) forwards all features to `sozu-lib` and defaults to `["jemallocator", "crypto-ring"]`.

## Workspace Structure

Four crates in one Cargo workspace:

| Crate | Path | License | Purpose |
|-------|------|---------|---------|
| `sozu-lib` | `lib/` | AGPL-3.0 | Core reverse proxy library: event loop, protocols, routing, TLS |
| `sozu-command-lib` | `command/` | LGPL-3.0 | Configuration types, protobuf definitions, channel abstraction |
| `sozu` | `bin/` | AGPL-3.0 | Binary: main process, worker management, CLI |
| `sozu-e2e` | `e2e/` | — | End-to-end integration tests |

The `bin/` crate has both a `[lib]` and `[[bin]]` target sharing the same name `sozu`. Functions used only by the binary target appear as dead code in the library target — this is expected.

## Architecture

### Main/Worker Model

One main process forks multiple worker processes. Each worker runs a **single-threaded** epoll-based event loop (via `mio`). Workers are shared-nothing: each holds a complete copy of the routing configuration. Configuration changes are propagated as diffs over unix socket channels. Listening sockets use `SO_REUSEPORT` so all workers bind the same address.

Key flow: `bin/src/command/` (main process) → `Channel<WorkerRequest, WorkerResponse>` → `lib/src/server.rs` (worker event loop)

### Session Lifecycle

1. `TcpListener` accepts → accept queue
2. Session created, registered with mio `Token`
3. HTTP headers parsed → hostname + path extracted
4. Router finds cluster → load balancer selects backend
5. Backend TCP connection opened, registered with back_token
6. Data proxied between frontend/backend sockets
7. Session closed or reset for keep-alive

Sessions tracked in `SessionManager` via `Slab<Token, Session>` in `lib/src/server.rs`.

### Protocol State Machines

Sessions can upgrade: ExpectProxyProtocol → TLS Handshake → HTTP/1 or HTTP/2 → WebSocket. Each protocol implements `readable()` and `writable()`. Protocol implementations live in `lib/src/protocol/`:
- `kawa_h1/`: HTTP/1 via Kawa (zero-copy parser)
- `h2/`: HTTP/2
- `rustls.rs`: TLS handshake
- `pipe.rs`: raw TCP piping

### Crypto Provider Abstraction

`lib/src/crypto.rs` is the central abstraction. It re-exports `default_provider()`, cipher suites, and `kx_group_by_name()` from whichever provider is feature-gated at compile time. The `compile_error!` guards enforce mutual exclusivity. `lib/src/https.rs` uses `kx_group_by_name()` to resolve the `groups_list` config into actual KX groups, falling back to `default_provider().kx_groups` when the list is empty.

### Communication Protocol

Commands to Sozu use binary protobuf messages (`command/command.proto`) over unix sockets. Messages are separated by `\0` bytes. Responses have three statuses: `Ok`, `Processing`, `Failure`. The `Channel` abstraction in `command/src/channel.rs` handles serialization.

## Key Files

| File | What it does |
|------|-------------|
| `lib/src/server.rs` | Main event loop, `SessionManager`, zombie checker |
| `lib/src/crypto.rs` | Crypto provider abstraction, feature-gated re-exports |
| `lib/src/https.rs` | HTTPS proxy, TLS `ServerConfig` construction with cipher/KX group selection |
| `lib/src/protocol/kawa_h1/` | HTTP/1 protocol implementation |
| `lib/src/protocol/h2/` | HTTP/2 protocol implementation |
| `lib/src/backends.rs` | Backend server management |
| `lib/src/router/` | URL pattern matching and routing |
| `bin/src/cli.rs` | CLI argument parsing |
| `bin/src/command/` | Main process command hub |
| `bin/src/worker.rs` | Worker fork + exec logic |
| `command/command.proto` | Protobuf message definitions |
| `command/src/config.rs` | Configuration types, defaults (`DEFAULT_GROUPS_LIST`, `DEFAULT_RUSTLS_CIPHER_LIST`) |
| `bin/config.toml` | Reference configuration file |

## Configuration

- **Main config**: `bin/config.toml` (TOML) — worker count, socket paths, logging, listeners
- **Runtime changes**: Send protobuf commands via unix socket (`command_socket` path)
- **TLS tuning**: `cipher_list` and `groups_list` on HTTPS listeners control cipher suites and KX groups (see `doc/configure.md`)
- **Crypto provider**: Compile-time only, via feature flags

## Debugging

```bash
# Minimal debugging config in bin/config.toml:
#   worker_count = 1
#   worker_automatic_restart = false
RUST_BACKTRACE=1 RUST_LOG=debug cargo run

# State inspection
sozu -c /etc/config.toml status
sozu -c /etc/config.toml metrics get
sozu -c /etc/config.toml clusters list
```

## References

- `doc/architecture.md` — System architecture
- `doc/lifetime_of_a_session.md` — Session lifecycle details
- `doc/configure.md` — Configuration reference (KX groups, certificates, cipher suites)
- `doc/debugging_strategies.md` — Debugging guide
- `doc/getting_started.md` — Getting started
