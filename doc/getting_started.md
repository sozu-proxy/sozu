# Getting started
> This getting started is focus on peoples who have never been involved in a `rust` project.


## Setting up Rust

Make sure to have the latest stable of `Rust` installed.
We recommend using [rustup][ru] for that.


After you did that, `Rust` should be fully installed.

> Our continuous integration use the latest toolchains version of:

``` yaml
rust:
  - nightly
  - beta
  - stable
```
See the [travis.yaml][tr]




## Setting up Sozu

### Required dependencies

* openssl 1.0.1 or above
* libssl-dev

### Install

`sozu` and `sozuctl` are publishs on [crates.io][cr]. To install it, you only have to do `cargo install sozu sozuctl`. There will be build and place in the folder `~/.cargo/bin`.

### Build from source

* Create the executable sozu

`cd bin && cargo build --release` 

> Only use `--release` to make a prod version.

* Create the executable sozuctl to manage the reverse proxy

`cd ctl && cargo build;`

### Run it

If you used the `cargo install` way. The sozu and ̀sozuctl are already in your `$PATH`.
`$ sozu -c <path/to/your/config.toml>`

However, if you have built the project from the source. The executables are placed in the `target` directory.

`$ ./target/release/sozu start -c <path/to/your/config.toml>`

> Change `/release/` to `/debug/` if you didn't use the `--release` option during the installation step.

You can find a working `config.toml` exemple [here][cfg]. To a proper install, you can move it to your `etc/sozu/config.toml`.


### Run it with Docker

The repository provide a multi-stage [Dockerfile][df] image based on `alpine:edge`.

You can build the image by doing:

`docker build -t sozu .`

And run it by the command

`docker run -p 8000:80 -p 8443:443 sozu`


### Systemd integration

The repository provide a unit file [here][un]. You can copy it to `/etc/systemd/system/` and invoke `systemctl daemon-reload`.  This will make systemd take notice of it, and now we can start the service with `systemctl start sozu.service`.
Furthermore, we can enable it, so that it is activated by default on future boots with 
`systemctl enable sozu.service`.

> You have to set your own `__BINDIR__` and `__SYSCONFDIR__`.
> You can use a `bash` script and calling `sed` to automted this part. e.g.: [generate.sh][gen]

[cr]: https://crates.io/
[cfg]: https://github.com/sozu-proxy/sozu/blob/master/bin/config.toml
[un]: https://github.com/sozu-proxy/sozu/blob/master/os-build/systemd/sozu.service.in
[ru]: https://rustup.rs
[tr]: https://github.com/sozu-proxy/sozu/blob/master/.travis.yml
[df]: https://github.com/sozu-proxy/sozu/blob/master/Dockerfile
[gen]: https://github.com/sozu-proxy/sozu/blob/master/os-build/exherbo/generate.sh