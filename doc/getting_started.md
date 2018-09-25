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

`sozu` and `sozuctl` are published on [crates.io][cr]. To install them, you only have to do `cargo install sozu sozuctl`. They will be built and available in the `~/.cargo/bin` folder.

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

There's also the [clevercloud/sozu](https://hub.docker.com/r/clevercloud/sozu/) image
following the master branch.

Run it with the command:

```bash
docker run \
  --name sozu-proxy
  -v /run/sozu:/run/sozu \
  -v /path/to/config/file:/etc/sozu \
  -v /my/state/:/var/lib/sozu \
  -p 8080:80 \
  -p 8443:443 \
  sozu
```

#### Using a custom `config.toml` configuration file

The default configuration for sozu can be found in `../os-build/docker/config.toml`.
If `/my/custom/config.toml` is the path and name of your custom configuration file, you can start your sozu container with this in a volume to override the default configuration (note that only the directory path of the custom config file is used in this command):
`docker run -v /my/custom:/etc/sozu sozu`

#### Using sozuctl with the docker container

To use `sozuctl` CLI from the host with the docker container you have to bind `/run/sozu` with the host by using a docker volume:
`docker run -v /run/sozu:/run/sozu sozu`

To change the path of the configuration socket, modify the `command_socket` option in the configuration file (default value is `/var/lib/sozu/sock`).

#### Provide an initial configuration state
Sozu can use a JSON file to load an initial configuration state for its routing.
you can mount it by using a volume, you can start your sozu container with this in a volume (note that only the directory path of the custom config file is used in this command):
`docker run -v /my/state:/var/lib/sozu sozu`

To change the path of the saved state file, modify the `saved_state` option in the configuration file (default value is `/var/lib/sozu/state.json`).

[cr]: https://crates.io/
[cfg]: https://github.com/sozu-proxy/sozu/blob/master/bin/config.toml
[ru]: https://rustup.rs
[tr]: https://github.com/sozu-proxy/sozu/blob/master/.travis.yml
[df]: https://github.com/sozu-proxy/sozu/blob/master/Dockerfile
