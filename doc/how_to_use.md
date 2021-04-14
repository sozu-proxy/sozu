# How to use Sōzu

> Before a deep dive in the configuration part of the proxy, you should take a look at the [getting started documentation](./getting_started.md) if you haven't done it yet.

## Run it

If you used the `cargo install` way, `sozu` and `sozuctl` are already in your `$PATH`.

`sozu -c <path/to/your/config.toml>`

However, if you built the project from source, `sozu` and `sozuctl` are placed in the `target` directory.

`./target/release/sozu start -c <path/to/your/config.toml>`

> `cargo build --release` puts the resulting binary in `target/release` instead of `target/debug`.

You can find a working `config.toml` exemple [here][cfg]. For a proper install, you can move it to your `/etc/sozu/config.toml`.

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

[cfg]: https://github.com/sozu-proxy/sozu/blob/master/bin/config.toml
[df]: https://github.com/sozu-proxy/sozu/blob/master/Dockerfile

## Systemd integration

The repository provides a unit file [here][un].

You can copy it to `/etc/systemd/system/` and invoke `systemctl daemon-reload`.

This will make systemd take notice of it, and now you can start the service with `systemctl start sozu.service`.

Furthermore, we can enable it, so that it is activated by default on future boots with `systemctl enable sozu.service`.

> You can use a `bash` script and call `sed` to automte this part. e.g.: [generate.sh][gen]
> You have to set your own `__BINDIR__` and `__SYSCONFDIR__`.

[un]: https://github.com/sozu-proxy/sozu/blob/master/os-build/systemd/sozu.service.in
[gen]: https://github.com/sozu-proxy/sozu/blob/master/os-build/exherbo/generate.sh
