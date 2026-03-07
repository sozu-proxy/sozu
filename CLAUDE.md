# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Sōzu is a lightweight, fast, always-up reverse proxy server written in Rust. It supports hot reconfiguration without reloading and upgrades itself while still processing requests. The project is structured as a Cargo workspace with four main crates.

## Build Commands

### Standard Build
```bash
# Build all workspace members (uses default crypto provider: crypto-ring)
cargo build --workspace

# Release build (includes LTO optimization)
cargo build --release
```

**Note:** `cargo build --all-features` does NOT work because crypto providers are mutually exclusive
(`crypto-ring`, `crypto-aws-lc-rs`, `crypto-openssl`). Use `--workspace` or select a provider explicitly:
```bash
cargo build -p sozu --no-default-features --features crypto-aws-lc-rs
cargo build -p sozu --no-default-features --features crypto-openssl
```

### Architecture-Specific Builds

SIMD is enabled by default using SSE instructions (Intel-specific). To build on non-Intel architectures (e.g., MacOS ARM):
```bash
cargo build --workspace --no-default-features
```

### Validation Commands

After making changes, run these commands to ensure code quality:
```bash
# Build workspace (default provider)
cargo build --workspace

# Run clippy
cargo clippy --workspace

# Format code with nightly rustfmt
cargo +nightly fmt
```

### Testing

```bash
# Run all tests in workspace
cargo test --workspace

# Run end-to-end tests specifically
cd e2e && cargo test

# Run a specific test
cargo test test_name

# Run tests with logging (if bugs need debugging)
RUST_LOG=debug cargo test test_name
```

### Feature Flags

Available features in `sozu-lib`:
- `crypto-ring`: Use `ring` as the rustls crypto provider (default in `sozu` binary)
- `crypto-aws-lc-rs`: Use `aws-lc-rs` as the rustls crypto provider (FIPS-capable, faster bulk transfers)
- `crypto-openssl`: Use OpenSSL as the rustls crypto provider (via `rustls-openssl`)
- `fips`: Enable FIPS 140-3 mode (implies `crypto-aws-lc-rs`, requires cmake + Go at build time)
- `simd`: Enable SIMD optimizations via Kawa (SSE-based, x86_64 only)
- `logs-debug`: Enable DEBUG level logs in release mode
- `logs-trace`: Enable TRACE level logs in release mode
- `splice`: Enable splice system call support
- `tolerant-http1-parser`: Use tolerant HTTP/1 parsing
- `unstable`: Enable unstable features

**Crypto provider selection**: Exactly one of `crypto-ring`, `crypto-aws-lc-rs`, or `crypto-openssl` must be enabled.
The `sozu` binary defaults to `crypto-ring`. To build with an alternative provider:
```bash
cargo build -p sozu --no-default-features --features crypto-aws-lc-rs
cargo build -p sozu --no-default-features --features crypto-openssl
```

## Workspace Structure

The repository is a Cargo workspace with four crates:

### `lib/` - sozu-lib (AGPL-3.0)
The core reverse proxy library containing:
- **Event loop management**: Single-threaded epoll-based event loop using `mio`
- **Protocol implementations**: Located in `lib/src/protocol/`
  - `kawa_h1/`: HTTP/1 implementation using the Kawa parser (zero-copy HTTP parsing)
  - `h2/`: HTTP/2 implementation
  - `rustls.rs`: TLS handshake using rustls
  - `pipe.rs`: TCP piping for raw TCP proxying
- **Session management**: `server.rs` contains the `SessionManager` and main event loop
- **Load balancing**: `backends.rs` and `load_balancing.rs` implement backend selection (round-robin, random, least_loaded, power of two)
- **Routing**: `router/` directory contains pattern matching and routing logic
- **Buffer management**: `pool.rs` manages fixed-size reusable buffers (typically 16kB)
- **Socket abstraction**: `socket.rs` provides socket wrapper utilities
- **Protocol-specific proxies**: `http.rs`, `https.rs`, `tcp.rs`, `tls.rs`

### `command/` - sozu-command-lib (LGPL-3.0)
Communication protocol library:
- **Protobuf definitions**: `command.proto` defines Request/Response message format
- **Channel abstraction**: Tools to communicate with Sōzu's unix socket
- Commands are binary protobuf messages separated by `\0` byte
- Responses have three status types: `Ok`, `Processing`, `Failure`

### `bin/` - sozu executable (AGPL-3.0)
Main process managing workers:
- **Main process**: Manages worker lifecycle and configuration distribution
- **Worker processes**: Wrap `sozu-lib` in isolated processes
- **Configuration**: TOML-based (`config.toml`)
- **Command socket**: Unix socket for runtime configuration changes
- **CLI**: Command-line interface in `cli.rs`

### `e2e/` - End-to-end tests
Integration tests using mocked clients and backends to test:
- Traffic passthrough
- Soft/hard stop behavior
- Zombie checker
- Backend reconnection
- Timeout handling

## Architecture Deep Dive

### Main/Worker Model

- One main process + multiple worker processes
- Each worker runs a single-threaded event loop with epoll/kqueue (via mio)
- Workers are shared-nothing: each has a complete copy of routing configuration
- Configuration changes propagated as "diffs" (e.g., "add backend", "remove frontend")
- Listening sockets use `SO_REUSEPORT` so multiple workers can listen on the same address

### Session Lifecycle

1. **Accept connection**: `TcpListener` accepts connection, stores `TcpStream` in accept queue
2. **Create session**: Session created from accept queue, registered with mio using a Token
3. **Parse request**: HTTP headers parsed to extract hostname, path, HTTP verb
4. **Find backend**: Router determines cluster, load balancer selects backend
5. **Connect backend**: Open TCP connection to backend, register in mio with back_token
6. **Forward data**: Proxy data between frontend and backend sockets
7. **Close/Reuse**: Session closed or reset for keep-alive connections

Sessions are tracked in the `SessionManager` using a `Slab` data structure mapping mio Tokens to session references.

### Protocol State Machines

Sessions can upgrade between protocols:
- ExpectProxyProtocol → TLS Handshake → HTTP → WebSocket
- Each protocol implements `readable()` and `writable()` methods
- State machines in `lib/src/protocol/` handle protocol-specific logic

### Zero-Copy Design

Sōzu minimizes memory allocation and copying:
- **Kawa**: HTTP parser with zero-copy design
- **Rustls**: TLS library optimized for minimal memory usage
- **Buffer pool**: Fixed-size reusable buffers reduce allocations
- **Edge-triggered epoll**: Events received once, stored in `Readiness` struct

## Development Workflow

### Debugging

For protocol-specific issues (HTTP/TLS), use examples directly:
```bash
# Run examples without worker overhead
cargo run --example http
```

#### Logging

Set log level via environment variable or config:
```bash
RUST_LOG=debug cargo run
# Or trace level (must build with logs-trace feature)
RUST_LOG=trace cargo run --features logs-trace
```

#### Development Config

When debugging, modify `bin/config.toml`:
- Set `worker_count = 1` to avoid log duplication
- Set `worker_automatic_restart = false` to stop on panic
- Disable unused protocols (HTTP/HTTPS/TCP) to simplify debugging
- Start with `RUST_BACKTRACE=1` for panic traces

#### State Inspection

```bash
# Dump current state
sozu -c /etc/config.toml status
sozu -c /etc/config.toml metrics get
sozu -c /etc/config.toml clusters list
sozu -c /etc/config.toml state save -f "state-$(date -Iseconds).txt"
```

### Configuration

- **Main config**: `bin/config.toml` sets worker count, socket paths, log settings
- **Runtime config**: Send commands via unix socket (path in `command_socket`)
- **Protobuf format**: Commands are binary protobuf, not JSON/text
- **TLS key exchange groups**: `groups_list` on HTTPS listeners controls KX algorithm negotiation (see `doc/configure.md`)
- **Crypto provider**: Compile-time selection via feature flags (`lib/src/crypto.rs`)

### Key Metrics to Monitor

- `sozu.http.requests`: Total request count
- `sozu.http.errors`: Failed requests (sum of parse errors + 4xx/5xx)
- `sozu.client.connections`: Frontend connection gauge
- `sozu.backend.connections`: Backend connection gauge
- `sozu.buffer.number`: Active buffer count
- `sozu.slab.entries`: Slab allocator usage
- `sozu.zombies`: Zombie sessions detected (indicates bugs)

## Important Files

- `lib/src/server.rs`: Main event loop, SessionManager, zombie checker
- `lib/src/lib.rs`: Core types and traits
- `lib/src/protocol/kawa_h1/`: HTTP/1 protocol implementation
- `lib/src/protocol/h2/`: HTTP/2 protocol implementation
- `lib/src/protocol/rustls.rs`: TLS handshake implementation
- `lib/src/backends.rs`: Backend server management
- `lib/src/load_balancing.rs`: Load balancing algorithms
- `lib/src/retry.rs`: Retry and circuit breaker logic
- `lib/src/crypto.rs`: Crypto provider abstraction and feature-gated imports
- `lib/src/router/`: URL pattern matching and routing
- `bin/src/cli.rs`: Command-line interface
- `command/command.proto`: Protobuf message definitions

## Contributing Notes

- Copyright assignment required (Clever Cloud)
- License: AGPL-3.0 for lib/bin, LGPL-3.0 for command
- Communication: GitHub issues, Gitter
- Easy issues tagged with "easy" label
- Prefer working on CLI (`cli.rs`) or tools using `command` library for first contributions

## References

- Documentation: `doc/` directory
- Architecture: `doc/architecture.md`
- Session lifecycle: `doc/lifetime_of_a_session.md`
- Debugging: `doc/debugging_strategies.md`
- Getting started: `doc/getting_started.md`
