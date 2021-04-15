# Getting started

## Setting up Rust

Make sure to have the latest stable version of `Rust` installed.
We recommend using [rustup][ru] for that.

After you did that, `Rust` should be fully installed.

## Setting up Sōzu

### Required dependencies

- openssl 1.0.1 or above
- libssl-dev

### Install

`sozu` and `sozuctl` are published on [crates.io][cr].

To install them, you only have to do `cargo install sozu sozuctl`.

They will be built and available in the `~/.cargo/bin` folder.

### Build from source

Build the sozu executable:

`cd bin && cargo build --release`

> The `--release` parameter inform cargo to compile sozu with optimizations turned on.
> Only use `--release` to make a production version.

Build the sozuctl executable to manage the reverse proxy:

`cd ctl && cargo build --release;`

This will create the `sozu` executable for the reverse proxy, and `sozuctl` to command it.

[ru]: https://rustup.rs
[cr]: https://crates.io/

#### For OSX build

Mac OS uses an old version of openssl, so we need to use one from Homebrew:

```bash
brew install openssl
brew link --force openssl
```

If it does not work, set the following environment variables before building:

```bash
export OPENSSL_LIB_DIR=/usr/local/opt/openssl/lib/
export OPENSSL_INCLUDE_DIR=/usr/local/opt/openssl/include/
```