# Getting started

## Setting up Rust

Make sure to have the latest stable version of `Rust` installed.
We recommend using [rustup][ru] for that.

After you did that, `Rust` should be fully installed.

## Setting up Sōzu

### Required dependencies

- libssl-dev

### Install

`sozu` is published on [crates.io][cr].

To install them, you only have to do `cargo install sozu sozuctl`.

They will be built and available in the `~/.cargo/bin` folder.

### Build from source

Build the sozu executable and command line:

`cd bin && cargo build --release --locked`

> The `--release` parameter inform cargo to compile sozu with optimizations turned on.
> Only use `--release` to make a production version.
>
> The `--locked` flag tells cargo to stick to dependencies versions as specified in `Cargo.lock`
> and thus prevent dependencie breaks.

[ru]: https://rustup.rs
[cr]: https://crates.io/
