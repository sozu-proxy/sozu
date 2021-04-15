# [Sōzu](https://www.sozu.io/) &middot; [![Join the chat at https://gitter.im/sozu-proxy/sozu](https://badges.gitter.im/sozu-proxy/sozu.svg)](https://gitter.im/sozu-proxy/sozu?utm_source=badge&utm_medium=badge&utm_campaign=pr-badge&utm_content=badge) [![Build Status](https://travis-ci.org/sozu-proxy/sozu.svg?branch=master)](https://travis-ci.org/sozu-proxy/sozu)

**Sōzu** is a lightweight, fast, always-up reverse proxy server.

## Why use Sōzu?

- **Hot configurable:** Sozu can receive configuration changes at runtime through secure unix sockets.
- **Upgrades without restarting:** Sozu is always-up, meaning it upgrades itself *while still processing requests*.
- **Handles SSL:** Sozu handles SSL, so your backend servers can focus on what they do best.
- **Protects your network:** Sozu protect backends by shielding them behind the reverse proxy, limiting direct network access. Sozu uses Rust, a language primed for memory safety. And even if a worker is exploited, sozu workers are sandboxed.

To get started check out our [documentation](./doc/README.md) !

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
