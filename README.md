# [Sōzu](https://www.sozu.io/) &middot; [![Join the chat at https://gitter.im/sozu-proxy/sozu](https://badges.gitter.im/sozu-proxy/sozu.svg)](https://gitter.im/sozu-proxy/sozu?utm_source=badge&utm_medium=badge&utm_campaign=pr-badge&utm_content=badge) [![Build Status](https://travis-ci.org/sozu-proxy/sozu.svg?branch=master)](https://travis-ci.org/sozu-proxy/sozu)

**Sōzu** is a lightweight, fast, always-up reverse proxy server.

## Why use Sōzu?

- **Hot configurable:** Sozu can receive configuration changes at runtime through secure unix sockets without having to reload.
- **Upgrades without restarting:** Sozu is always-up, meaning it upgrades itself *while still processing requests*.
- **Handles SSL:** Sozu handles SSL, so your backend servers can focus on what they do best.
- **Protects your network:** Sozu protect backends by shielding them behind the reverse proxy, limiting direct network access. Sozu uses Rust, a language primed for memory safety. And even if a worker is exploited, sozu workers are sandboxed.
- **Optimize performance:** Sozu can expose your web service over the Internet with HTTP/2 protocol even if your backend only supports HTTP/1.*. Your Web apps benefit from connection multiplexing using transparent HTTP/2 streams. 

To get started check out our [documentation](./doc/README.md) !

## Exploring the source

- `lib/`: the `sozu-lib` reverse proxy library contains the event loop management, the parsers and protocols
- `bin/`: the `sozu` executable wraps the library in worker processes, and handle dynamic configuration
- `command`: the `sozu-command-lib` contains all structures to interact with Sōzu

## License

Sozu itself is covered by the GNU Affero General Public License (AGPL) version 3.0 and above. Traffic going through Sozu doesn't consider Clients and Servers as "covered work" hence don't have to be placed under the same license. A "covered work" in the Licence terms, will consider a service using Sozu's code, methods or specific algorithms. This service can be a self managed software or an online service. The "covered work" will not consider a specific control plane you could have develop to control or use Sozu. In simple terms, Sozu is a Free and Open Source software you can use for both infrastructure and business but in case of a business based on Sozu (e.g. a Load Balancer product), you should either give back your contributions to the project, or contact Clever Cloud for a specific Business Agreement.

### sozu-lib, sozu

This program is free software: you can redistribute it and/or modify it under
the terms of the GNU Affero General Public License as published by the Free
Software Foundation, version 3.

This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY;
without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
See the GNU Affero General Public License for more details.

### sozu-command-lib

sozu-command-lib is released under LGPL version 3



Copyright (C) 2015-2023 Clever Cloud
