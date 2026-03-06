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

> The `--release` parameter inform cargo to compile sozu with optimizations turned on.
> Only use `--release` to make a production version.
>
> The `--locked` flag tells cargo to stick to dependencies versions as specified in `Cargo.lock`
> and thus prevent dependencie breaks.

### Choosing a crypto provider

Sōzu uses [Rustls](https://github.com/rustls/rustls) for TLS and supports multiple cryptographic backends, selected at compile time via feature flags. Only one crypto provider can be enabled at a time.

| Feature | Backend | Notes |
|---------|---------|-------|
| `crypto-ring` (default) | [ring](https://github.com/briansmith/ring) | No extra system dependencies |
| `crypto-aws-lc-rs` | [AWS-LC](https://github.com/aws/aws-lc-rs) | Post-quantum key exchange support. Requires `cmake` |
| `crypto-openssl` | [OpenSSL](https://www.openssl.org/) (via rustls-openssl) | Uses the system OpenSSL. Requires `cmake` and OpenSSL dev headers |
| `fips` | AWS-LC in FIPS mode | FIPS 140-3 compliance. Requires `cmake` |

```bash
# Default build (ring)
cd bin && cargo build --release --locked

# AWS-LC with post-quantum support
cd bin && cargo build --release --locked --no-default-features --features crypto-aws-lc-rs

# OpenSSL backend
cd bin && cargo build --release --locked --no-default-features --features crypto-openssl

# FIPS 140-3 compliance (aws-lc-rs in FIPS mode)
cd bin && cargo build --release --locked --no-default-features --features fips
```

> When using `--no-default-features`, the `jemallocator` allocator is also disabled.
> Add it back explicitly if desired: `--features jemallocator,crypto-aws-lc-rs`.

[ru]: https://rustup.rs
[cr]: https://crates.io/
