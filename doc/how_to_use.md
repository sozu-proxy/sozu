# How to use Sōzu

> If you didn't take a look at the [configure documentation](./configure.md), we advise you to do so because you will need to know what to put in your configuration file.

## Run it

If you used the `cargo install` way, `sozu` and `sozuctl` are already in your `$PATH`.

`sozu start -c <path/to/your/config.toml>`

However, if you built the project from source, `sozu` and `sozuctl` are placed in the `target` directory.

`./target/release/sozu start -c <path/to/your/config.toml>`

> `cargo build --release` puts the resulting binary in `target/release` instead of `target/debug`.

You can find a working `config.toml` exemple [here][cfg].

To start the reverse proxy:

```bash
sozu start -c config.toml
```

You can edit the reverse proxy's configuration with the `config.toml` file. You can declare new applications, their frontends and backends through that file.

**But** for more flexibility, you should use the command socket (you can find one end of that unix socket at the path designed by `command_socket` in the configuration file).

You can use `sozuctl` to interact with the reverse proxy.

Checkout sozuctl [documentation](../ctl/README.md) for more informations.

## Logging

The reverse proxy uses `env_logger`. You can select which module displays logs at which level with an environment variable. Here is an example to display most logs at `info` level, but use `trace` level for the HTTP parser module:

```bash
RUST_LOG=info,sozu_lib::parser::http11=trace ./target/debug/sozu
```

## Run it with Docker

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

### Using a custom `config.toml` configuration file

The default configuration for sozu can be found in `../os-build/docker/config.toml`.
If `/my/custom/config.toml` is the path and name of your custom configuration file, you can start your sozu container with this in a volume to override the default configuration (note that only the directory path of the custom config file is used in this command):

`docker run -v /my/custom:/etc/sozu sozu`

### Using sozuctl with the docker container

To use `sozuctl` CLI from the host with the docker container you have to bind `/run/sozu` with the host by using a docker volume:

`docker run -v /run/sozu:/run/sozu sozu`

To change the path of the configuration socket, modify the `command_socket` option in the configuration file (default value is `/var/lib/sozu/sock`).

### Provide an initial configuration state

Sozu can use a JSON file to load an initial configuration state for its routing. You can mount it by using a volume, you can start your sozu container with this in a volume (note that only the directory path of the custom config file is used in this command):
`docker run -v /my/state:/var/lib/sozu sozu`

To change the path of the saved state file, modify the `saved_state` option in the configuration file (default value is `/var/lib/sozu/state.json`).

[cfg]: ../bin/config.toml
[df]: ../Dockerfile

## Systemd integration

The repository provides a unit file [here][un]. You can copy it to `/etc/systemd/system/` and invoke `systemctl daemon-reload`.

This will make systemd take notice of it, and now you can start the service with `systemctl start sozu.service`. Furthermore, you can enable it, so that it is activated by default on future boots with `systemctl enable sozu.service`.

You can use a `bash` script and call `sed` to automate this part. e.g.: [generate.sh][gen].

This script will generate `sozu.service`, `sozu.conf` and `config.toml` files into a `generated` folder at the root of `os-build` directory. You will have to set your own `__BINDIR__`, `__SYSCONFDIR__`, `__DATADIR__` and `__RUNDIR__` variables.

Here is an example of those variables:

- `__BINDIR__` : `/usr/local/bin`
- `__SYSCONFDIR__` : `/etc`
- `__DATADIR__` : `/var/lib/sozu`
- `__RUNDIR__` : `/run`

[un]: ../os-build/systemd/sozu.service.in
[gen]: ../os-build/exherbo/generate.sh
