# Getting started

## Setting up Rust

Make sure to have the latest stable version of `Rust` installed.
We recommend using [rustup][ru] for that.

After you did that, `Rust` should be fully installed.

## Setting up Sōzu

### Install

`sozu` is published on [crates.io][cr].

To install them, you only have to do `cargo install sozu`.

They will be built and available in the `~/.cargo/bin` folder.

### Build from source

Build the sozu executable and command line:

`cd bin && cargo build --release --locked`

> The `--release` parameter informs cargo to compile sozu with optimizations turned on.
> Only use `--release` to make a production version.
>
> The `--locked` flag tells cargo to stick to dependency versions as specified in `Cargo.lock`
> and thus prevent dependency breaks.

### Cargo features

`rustls` is wired to the `ring` crypto provider via the workspace `Cargo.toml`,
and there is no provider-pluggable surface today — the build links exactly one
backend. The Cargo features published by the workspace tune behaviour, not
crypto:

| Feature | Crate(s) | Effect |
|---------|----------|--------|
| `tolerant-http1-parser` | `lib`, `bin` | Relaxes H1 parsing via `kawa/tolerant-parsing`. |
| `simd` | `lib`, `bin` | Enables `kawa/simd` SIMD acceleration. |
| `splice` | `lib`, `bin` | Linux-only zero-copy TCP forwarding via `splice(2)`. Default 64 KiB kernel-pipe per session per direction; tunable via `splice_pipe_capacity_bytes` (clamped at `/proc/sys/fs/pipe-max-size`). Applies to `Protocol::TCP` listeners only. |
| `opentelemetry` | `lib`, `bin` | Compiles in OpenTelemetry export. |
| `logs-debug` | all | Compiles in `DEBUG` logs (release strips them otherwise). |
| `logs-trace` | all | Compiles in `TRACE` logs (release strips them otherwise). |
| `e2e-hooks` | `lib` | Test-injection APIs — never enable in production builds. |

The authoritative list per crate lives in the per-crate `Cargo.toml`
(`lib/Cargo.toml`, `bin/Cargo.toml`, `command/Cargo.toml`); `cargo build
--all-features --locked` succeeds across the workspace today.

## HTTP/2 support

Sōzu supports HTTP/2 out of the box, with no additional configuration needed for most
use cases.

**Frontend (client → Sōzu):** HTTP/2 is automatically available on all HTTPS listeners.
Clients negotiate the protocol during the TLS handshake via ALPN. To disable HTTP/2
on a specific listener, set `alpn_protocols = ["http/1.1"]`.

**Backend (Sōzu → server):** By default, Sōzu speaks HTTP/1.1 to backends. To use
cleartext HTTP/2 (h2c) for backend connections, set `http2 = true` on the cluster:

```toml
[clusters.MyCluster]
protocol = "http"
http2 = true
frontends = [
  { address = "0.0.0.0:8443", hostname = "app.example.com", certificate = "cert.pem", key = "key.pem", certificate_chain = "chain.pem" }
]
backends = [
  { address = "127.0.0.1:8080" }
]
```

Make sure `buffer_size` is at least **16393** (16384 max H2 frame + 9 byte header) in
the global configuration section.

You can also toggle HTTP/2 at runtime using the CLI:

```bash
sozu cluster h2 enable --id MyCluster
sozu cluster h2 disable --id MyCluster
```

**Security:** Sōzu includes built-in flood detection for HTTP/2 connections, protecting
against Rapid Reset (CVE-2023-44487), CONTINUATION flood (CVE-2024-27316), and other
protocol abuse vectors. Thresholds are configurable per-listener — see the
[configuration reference](./configure.md#h2-flood-detection-thresholds) for details.

**Priorities:** Sōzu implements RFC 9218 Extensible Priorities. Streams with the
`priority` header (`u=N, i` format) are scheduled by urgency level, ensuring
higher-priority responses are sent first.

See the [configuration reference](./configure.md) for all HTTP/2 options, or the
[H2 Mux Internals](./h2_mux_internals.md) for developer-facing implementation details.

[ru]: https://rustup.rs
[cr]: https://crates.io/
