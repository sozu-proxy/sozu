# How to use Sōzu

> If you didn't take a look at the [configure documentation](./configure.md), we advise you to do so because you will need to know what to put in your configuration file.

## Run it

If you used the `cargo install` way, `sozu` is already in your `$PATH`.

    sozu start -c <path/to/your/config.toml>

However, if you built the project from source, `sozu` is placed in the `target` directory.

    ./target/release/sozu start -c <path/to/your/config.toml>

> `cargo build --release --locked` puts the resulting binary in `target/release` instead of `target/debug`.

You can find a working `config.toml` example [here][cfg].

To start the reverse proxy:

```bash
sozu start -c config.toml
```

You can edit the reverse proxy's configuration with the `config.toml` file. You can declare new clusters, their frontends and backends through that file.

**But** for more flexibility, you should use the command socket (you can find one end of that unix socket at the path designed by `command_socket` in the configuration file).

You can use the `sozu` binary as a CLI to interact with the reverse proxy.

Check out the command line [documentation](./configure_cli.md) for more information.

## Run it with Docker

The repository provides a multi-stage [Dockerfile][df] image based on `alpine:edge`.

You can build the image by doing:

    docker build -t sozu .

There's also the [clevercloud/sozu](https://hub.docker.com/r/clevercloud/sozu/) image
following the master branch (outdated).

Run it with the command:

```bash
docker run \
  --ulimit nofile=262144:262144 \
  --name sozu-proxy \
  -v /run/sozu:/run/sozu \
  -v /path/to/config/file:/etc/sozu \
  -v /my/state/:/var/lib/sozu \
  -p 8080:80 \
  -p 8443:443 \
  sozu
```

To build an image with a specific version of Alpine:

    docker build --build-arg ALPINE_VERSION=3.14 -t sozu:main-alpine-3.14 .

### Using a custom `config.toml` configuration file

The default configuration for sozu can be found in `../os-build/docker/config.toml`.
If `/my/custom/config.toml` is the path and name of your custom configuration file, you can start your sozu container with this in a volume to override the default configuration (note that only the directory path of the custom config file is used in this command):

    docker run -v /my/custom:/etc/sozu sozu

### Using sozu command line with the docker container

To use `sozu` CLI from the host with the docker container you have to bind `/run/sozu` with the host by using a docker volume:

    docker run -v /run/sozu:/run/sozu sozu

To change the path of the configuration socket, modify the `command_socket` option in the configuration file (default value is `/var/lib/sozu/sock`).

### Provide an initial configuration state

Sōzu can use a JSON file to load an initial configuration state for its routing. You can mount it by using a volume, you can start your sozu container with this in a volume (note that only the directory path of the custom config file is used in this command):

    docker run -v /my/state:/var/lib/sozu sozu

To change the path of the saved state file, modify the `saved_state` option in the configuration file (default value is `/var/lib/sozu/state.json`).

[cfg]: ../bin/config.toml
[df]: ../Dockerfile

## Systemd integration

The repository provides a unit file [here][unit-file]. You can copy it to `/etc/systemd/system/` and invoke `systemctl daemon-reload`.

This will make systemd take notice of it, and now you can start the service with `systemctl start sozu.service`. Furthermore, you can enable it, so that it is activated by default on future boots with `systemctl enable sozu.service`.

[unit-file]: ../os-build/systemd/sozu.service
