# sōzu HTTP reverse proxy

[![Join the chat at https://gitter.im/sozu-proxy/sozu](https://badges.gitter.im/sozu-proxy/sozu.svg)](https://gitter.im/sozu-proxy/sozu?utm_source=badge&utm_medium=badge&utm_campaign=pr-badge&utm_content=badge)

it will be awesome when it will be ready

## Goals

Build the most reliable reverse proxy ever:

- it should never crash (currently fixing the remaining panics)
- you should not need to restart it
  - it can receive configuration changes from a unix socket at runtime
  - it should be able to upgrade without any downtime
- it should not have exploitable memory errors
  - even if it has one, workers will be sandboxed
- you set up a limit on the number of concurrent connections to a worker
  - the reverse proxy will refuse new connections over that limit, instead of requesting unavailable resources like memory


## Building and starting

Requirements:
- openssl 1.0.1 or above (including libraries/includes, ie apt install libssl-dev / rpm install openssl-devel

You can create the required executables like this:

```
cd ctl && cargo build; cd ../bin && cargo build
```

This will create the `sozu` executable for the reverse proxy, and `sozuctl` to command it.

To start the reverse proxy:

```
cd bin;
../target/debug/sozu start -c config.toml
```

You can edit the reverse proxy's configuration with the `config.toml` file. You can declare
new applications, their frontends and backends through that file, but for more flexibility,
you should use the command socket (you can find one end of that unix socket at the path
designed by `command_socket` in the configuration file).

`sozuctl` has a few commands you can use to interact with the reverse proxy:


- soft shutdown (wait for active connections to stop): `sozuctl -c config.toml shutdown`
- hard shutdown: `sozuctl -c config.toml shutdown --hard`
- display the list of current configuration messages: `sozuctl -c config.toml state dump`
- save the configuration state to a file: `sozuctl -c config.toml state save -f state.json`

### For OSX build

Mac OS uses an old version of openssl, so we need to use one from Homebrew:

```
brew install openssl
brew link --force openssl
```

If it does not work, set the following environment variables before building:

```
export OPENSSL_LIB_DIR=/usr/local/opt/openssl/lib/
export OPENSSL_INCLUDE_DIR=/usr/local/opt/openssl/include/
```

## Logging

The reverse proxy uses `env_logger`. You can select which module displays logs at which level with an environment variable. Here is an example to display most logs at `info` level, but use `trace` level for the HTTP parser module:

```
RUST_LOG=info,sozu_lib::parser::http11=trace ./target/debug/sozu
```

## Exploring the source

- `lib/`: the `sozu_lib` reverse proxy library contains the event loop management, the parsers and protocols
- `bin/`: the `sozu` executable wraps the library in worker processes, and handle dynamic configuration
- `ctl/`: the `sozuctl` executable can send commands to the reverse proxy

## License

Copyright (C) 2015-2016 Geoffroy Couprie, Clément Delafargue

This program is free software: you can redistribute it and/or modify it under
the terms of the GNU Affero General Public License as published by the Free
Software Foundation, version 3.

This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY;
without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
See the GNU Affero General Public License for more details.
