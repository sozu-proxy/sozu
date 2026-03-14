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

### Crypto providers

Sōzu supports multiple TLS crypto providers. Exactly one must be enabled at compile time:

| Feature | Provider | Notes |
|---------|----------|-------|
| `crypto-ring` | ring | Default, pure Rust |
| `crypto-aws-lc-rs` | aws-lc-rs | Faster bulk transfers, post-quantum key exchange |
| `crypto-openssl` | rustls-openssl | Uses system OpenSSL |

To build with an alternative provider:

```bash
cd bin && cargo build --release --locked --no-default-features --features crypto-aws-lc-rs
```

> **Important:** `--all-features` does not work. Crypto providers are mutually exclusive.

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
