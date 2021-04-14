# Getting started

## Setting up Rust

Make sure to have the latest stable version of `Rust` installed.
We recommend using [rustup][ru] for that.

After you did that, `Rust` should be fully installed.

## Setting up Sōzu

### Required dependencies

* openssl 1.0.1 or above
* libssl-dev

### Install

`sozu` and `sozuctl` are published on [crates.io][cr].

To install them, you only have to do `cargo install sozu sozuctl`.

They will be built and available in the `~/.cargo/bin` folder.

### Build from source

* Build the sozu executable

`cd bin && cargo build --release`

> The `--release` parameter inform cargo to compile sozu with optimizations turned on.
> Only use `--release` to make a production version.

* Build the sozuctl executable to manage the reverse proxy

`cd ctl && cargo build --release;`

[ru]: https://rustup.rs
[cr]: https://crates.io/
