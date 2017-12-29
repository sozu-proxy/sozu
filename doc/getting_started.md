# Getting started

## Setting up Rust

Make sure to have the latest stable version of `Rust` installed.
We recommend using [rustup][ru] for that.


After you did that, `Rust` should be fully installed.

## Setting up Sozu

### Required dependencies

* openssl 1.0.1 or above
* libssl-dev

### Install

`sozu` and `sozuctl` are published on [crates.io][cr]. To install them, you only have to do `cargo install sozu sozuctl`. They will be build and available in the `~/.cargo/bin` folder.

### Build from source

* Build the sozu executable 

`cd bin && cargo build --release` 

> The `--release` parameter inform cargo to compile sozu with optimizations turned on.
> Only use `--release` to make a production version.

* Build the sozuctl executable to manage the reverse proxy

`cd ctl && cargo build --release;`

### Run it

If you used the `cargo install` way, `sozu` and `sozuctl` are already in your `$PATH`.
`$ sozu -c <path/to/your/config.toml>`

However, if you built the project from source, `sozu` and `sozuctl` are placed in the `target` directory.

`$ ./target/release/sozu start -c <path/to/your/config.toml>`

> `cargo build --release` puts the resulting binary in `target/release` instead of `target/debug`.

You can find a working `config.toml` exemple [here][cfg]. For a proper install, you can move it to your `/etc/sozu/config.toml`.


### Run it with Docker

The repository provides a multi-stage [Dockerfile][df] image based on `alpine:edge`.

You can build the image by doing:

`docker build -t sozu .`

And run it with the command

`docker run -p 8000:80 -p 8443:443 sozu`

[cr]: https://crates.io/
[cfg]: https://github.com/sozu-proxy/sozu/blob/master/bin/config.toml
[ru]: https://rustup.rs
[tr]: https://github.com/sozu-proxy/sozu/blob/master/.travis.yml
[df]: https://github.com/sozu-proxy/sozu/blob/master/Dockerfile