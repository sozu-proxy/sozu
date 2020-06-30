# [Sōzu](https://www.sozu.io/) &middot; [![Join the chat at https://gitter.im/sozu-proxy/sozu](https://badges.gitter.im/sozu-proxy/sozu.svg)](https://gitter.im/sozu-proxy/sozu?utm_source=badge&utm_medium=badge&utm_campaign=pr-badge&utm_content=badge) [![Build Status](https://travis-ci.org/sozu-proxy/sozu.svg?branch=master)](https://travis-ci.org/sozu-proxy/sozu)

**Sōzu** is a lightweight, fast, always-up reverse proxy server.

## Why use Sōzu?

- **Hot configurable:** Sozu can receive configuration changes at runtime through secure unix sockets.
- **Upgrades without restarting:** Sozu is always-up, meaning it upgrades itself *while still processing requests*.
- **Handles SSL:** Sozu handles SSL, so your backend servers can focus on what they do best.
- **Protects your network:** Sozu protect backends by shielding them behind the reverse proxy, limiting direct network access. Sozu uses Rust, a language primed for memory safety. And even if there a worker is exploited, sozu workers are sandboxed.

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

Copyright (C) 2015-2018 Geoffroy Couprie, Clément Delafargue and Clever Cloud

This program is free software: you can redistribute it and/or modify it under
the terms of the GNU Affero General Public License as published by the Free
Software Foundation, version 3.

This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY;
without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
See the GNU Affero General Public License for more details.
