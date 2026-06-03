# [Sōzu](https://www.sozu.io/)

**Sōzu** is a lightweight, fast, always-up reverse proxy server.

## Why use Sōzu?

- **Hot configurable:** Sōzu can receive configuration changes at runtime, through secure unix sockets, without having to reload.
- **Upgrades without restarting:** Sōzu is always-up, meaning it upgrades itself *while still processing requests*.
- **Handles SSL:** Sōzu works as a TLS endpoint, so your backend servers can focus on what they do best. Multiple cryptographic backends are supported at compile time: [ring](https://github.com/briansmith/ring) (default), [AWS-LC](https://github.com/aws/aws-lc-rs) (with post-quantum and FIPS 140-3 options), and [OpenSSL](https://www.openssl.org/).
- **Protects your network:** Sōzu protect backends by shielding them behind the reverse proxy, limiting direct network access. Sōzu uses Rust, a language primed for memory safety. And even if a worker is exploited, Sōzu workers are sandboxed.
- **Optimize performance:** Sōzu makes the most of Rust's capacity to avoid useless copying and memory usage.
   Two key dependencies have been optimized in this way:
   - [Kawa](https://github.com/CleverCloud/kawa) is a generic HTTP representation library that parses and translates HTTP messages with zero copy
   - [Rustls](https://github.com/rustls/rustls) is a TLS library that encrypts/decrypts TLS traffic with as little intermediate memory usage as it gets
- **Live operator TUI:** `sozu top` (build with `--features tui`) is a btop/htop-style live dashboard over the existing command socket — sparklines, sortable cluster + backend tables, H2 flood-mitigation counters, and a colour-coded event tail in a single screen. See [`doc/sozu-top.md`](doc/sozu-top.md).

To get started check out our [documentation](./doc/README.md) !

## Installation

- **Pre-built binaries** for Linux are attached to every tagged release on the [GitHub Releases page](https://github.com/sozu-proxy/sozu/releases). 10 tarballs cover the cross-product of:

  - **target** (3 published, 4 supported from source): `x86_64-unknown-linux-{gnu,musl}` and `aarch64-unknown-linux-gnu`. `aarch64-unknown-linux-musl` builds from source but is **not** in the prebuilt matrix — `jemalloc-sys 0.5.4`'s cross-compile probe cannot link its atomics tests against the `musl-tools` wrapper on the arm64 runner; build from source against a real `aarch64-linux-musl-gcc` toolchain (Alpine, musl-cross-make, etc.) if you need that combination.
  - **crypto provider feature** (2-4 per target):
    - `crypto-ring` (default — ring backend, all 3 published targets)
    - `crypto-aws-lc-rs` (post-quantum capable, AWS-LC backend, all 3 published targets)
    - `crypto-openssl` (system OpenSSL backend, gnu targets only)
    - `fips` (aws-lc-rs in FIPS-validated mode, no SLA per project policy, gnu targets only)

  Tarball names follow `sozu-<VERSION>-<TARGET>-<PROVIDER>.tar.gz`. Each release also carries a `SHA256SUMS` file covering all 10 tarballs, a sigstore keyless signature pair (`SHA256SUMS.sig` + `SHA256SUMS.pem`), and SLSA build-provenance attestations.

  ```sh
  # Pick the (target, provider) combination you need. The default for
  # operators who don't have a specific compliance constraint is
  # crypto-ring on linux-gnu.
  TARGET=x86_64-unknown-linux-gnu
  PROVIDER=crypto-ring

  curl -LO https://github.com/sozu-proxy/sozu/releases/download/<VERSION>/sozu-<VERSION>-${TARGET}-${PROVIDER}.tar.gz
  curl -LO https://github.com/sozu-proxy/sozu/releases/download/<VERSION>/SHA256SUMS
  curl -LO https://github.com/sozu-proxy/sozu/releases/download/<VERSION>/SHA256SUMS.sig
  curl -LO https://github.com/sozu-proxy/sozu/releases/download/<VERSION>/SHA256SUMS.pem

  # Verify the signature was produced by this repository's release workflow.
  # Requires cosign >= 2.4.1 (GHSA-whqx-f9j3-ch6m).
  cosign verify-blob \
    --certificate SHA256SUMS.pem \
    --signature SHA256SUMS.sig \
    --certificate-identity-regexp '^https://github\.com/sozu-proxy/sozu/\.github/workflows/release\.yml@refs/tags/[0-9]+\.[0-9]+\.[0-9]+(-rc\.[0-9]+)?$' \
    --certificate-oidc-issuer https://token.actions.githubusercontent.com \
    --certificate-github-workflow-repository sozu-proxy/sozu \
    --certificate-github-workflow-ref refs/tags/<VERSION> \
    SHA256SUMS

  # Verify the tarball matches the signed sums:
  sha256sum -c SHA256SUMS --ignore-missing
  tar -xzf sozu-<VERSION>-${TARGET}-${PROVIDER}.tar.gz
  ```

  Build provenance can be inspected with `gh attestation verify sozu-<VERSION>-<TARGET>-<PROVIDER>.tar.gz --owner sozu-proxy`. Pre-release tags (`X.Y.Z-rc.N`) are published as GitHub pre-releases.

- **Docker** images are published to [Docker Hub](https://hub.docker.com/r/clevercloud/sozu/) for every tagged release as `clevercloud/sozu:<VERSION>` and `clevercloud/sozu:latest` (stable tags only).

  **Note on Docker images vs. signed tarballs.** `clevercloud/sozu:<VERSION>` is built from source by the same release workflow (`.github/workflows/release.yml`) but inside an Alpine builder stage that uses the distro's `cargo` and `rust` packages, not the `dtolnay/rust-toolchain@1.88.0` pin used for the Linux tarballs. The image therefore links musl libc and is **not** byte-equivalent to either the `gnu` or `musl` tarballs verified by the cosign-signed `SHA256SUMS`. SLSA build provenance currently covers the tarballs only. If you need the cosign-attested binary, extract `sozu-<VERSION>-x86_64-unknown-linux-musl.tar.gz` and run `sozu` directly. A from-tarball Docker image is tracked as a follow-up.

- **From source.** See [`doc/getting_started.md`](./doc/getting_started.md) for compilation prerequisites (`protoc`, Rust toolchain pinned via `rust-toolchain`).

## Quickstart

Once the prerequisites (`protoc`, Rust 1.88 via the pinned `rust-toolchain`) are installed, building and starting Sōzu takes two commands:

```sh
cargo build -p sozu --release --locked
target/release/sozu start -c bin/config.toml
```

`bin/config.toml` is the in-tree sample config — copy it next to your TLS certs and edit listener/cluster blocks before going to production. The live operator dashboard is available with `cargo build -p sozu --release --locked --features tui` then `target/release/sozu top`. Configuration reference: [`doc/configure.md`](./doc/configure.md).

## Exploring the source

The Cargo workspace ships four crates (Rust 2024 edition, MSRV 1.88):

- `lib/`: the `sozu-lib` reverse proxy library hosts the single-threaded mio event loop, the HTTP/1.1 (Kawa) and HTTP/2 multiplexer, TCP, UDP, and TLS protocols, routing, per-IP rate limiting, sockets, metrics, and the buffer pool.
- `bin/`: the `sozu` binary wraps the library in a master/worker supervisor, exposes the unix command socket, orchestrates hot reconfiguration and zero-downtime upgrades, and ships the optional `sozu top` TUI behind the `tui` feature.
- `command/`: the `sozu-command-lib` ships the protobuf IPC schema, configuration parser, replicated state, length-prefixed channels, and SCM FD-passing helpers used by both `lib` and `bin`.
- `e2e/`: the `sozu-e2e` integration harness spawns real workers plus mock clients and backends to exercise the H1, H2, TLS, PROXY-protocol, command-channel hardening, and listener/upgrade hot-reconfig paths.

The `fuzz/` crate (`fuzz_frame_parser`, `fuzz_hpack_decoder`) lives outside the Cargo workspace and is built with `cargo +nightly fuzz` from inside `fuzz/`.

HTTP/2 support (frontend and backend) is driven through the multiplexer in `lib/src/protocol/mux/` and TLS ALPN negotiation in `lib/src/protocol/rustls.rs`; the multiplexer includes flood mitigations for CVE-2023-44487 (Rapid Reset), CVE-2024-27316 (CONTINUATION flood), and CVE-2025-8671 (MadeYouReset), plus PING/SETTINGS/priority floods.

## License

Sōzu itself is covered by the GNU Affero General Public License (AGPL) version 3.0 and above. Traffic going through Sōzu doesn't consider Clients and Servers as "covered work" hence don't have to be placed under the same license. A "covered work" in the Licence terms, will consider a service using Sōzu's code, methods or specific algorithms. This service can be a self managed software or an online service. The "covered work" will not consider a specific control plane you could have develop to control or use Sōzu. In simple terms, Sōzu is a Free and Open Source software you can use for both infrastructure and business but in case of a business based on Sōzu (e.g. a Load Balancer product), you should either give back your contributions to the project, or contact Clever Cloud for a specific Business Agreement.

### sozu-lib, sozu

This program is free software: you can redistribute it and/or modify it under
the terms of the GNU Affero General Public License as published by the Free
Software Foundation, version 3.

This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY;
without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
See the GNU Affero General Public License for more details.

### sozu-command-lib

sozu-command-lib is released under LGPL version 3



Copyright (C) 2015-2026 Clever Cloud
