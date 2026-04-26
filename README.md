# [Sōzu](https://www.sozu.io/)

**Sōzu** is a lightweight, fast, always-up reverse proxy server.

## Why use Sōzu?

- **Hot configurable:** Sōzu can receive configuration changes at runtime, through secure unix sockets, without having to reload.
- **Upgrades without restarting:** Sōzu is always-up, meaning it upgrades itself *while still processing requests*.
- **Handles SSL:** Sōzu works as a TLS endpoint, so your backend servers can focus on what they do best.
- **Protects your network:** Sōzu protect backends by shielding them behind the reverse proxy, limiting direct network access. Sōzu uses Rust, a language primed for memory safety. And even if a worker is exploited, Sōzu workers are sandboxed.
- **Optimize performance:** Sōzu makes the most of Rust's capacity to avoid useless copying and memory usage.
   Two key dependencies have been optimized in this way:
   - [Kawa](https://github.com/CleverCloud/kawa) is a generic HTTP representation library that parses and translates HTTP messages with zero copy
   - [Rustls](https://github.com/rustls/rustls) is a TLS library that encrypts/decrypts TLS traffic with as little intermediate memory usage as it gets

To get started check out our [documentation](./doc/README.md) !

## Exploring the source

The Cargo workspace ships four crates (Rust 2024 edition, MSRV 1.85):

- `lib/`: the `sozu-lib` reverse proxy library hosts the single-threaded mio event loop, the HTTP/1.1, HTTP/2 multiplexer, TCP and TLS protocols, routing, sockets, metrics, and the buffer pool.
- `bin/`: the `sozu` binary wraps the library in a master/worker supervisor, exposes the unix command socket, and orchestrates hot reconfiguration and zero-downtime upgrades.
- `command/`: the `sozu-command-lib` ships the protobuf IPC schema, configuration parser, replicated state, channels, and FD-passing helpers used by both `lib` and `bin`.
- `e2e/`: the `sozu-e2e` integration harness spawns real workers plus mock clients and backends to exercise the H1, H2, TLS, and PROXY-protocol paths.

The `fuzz/` crate (`fuzz_frame_parser`, `fuzz_hpack_decoder`) lives outside the Cargo workspace and is built with `cargo +nightly fuzz` from inside `fuzz/`.

HTTP/2 support (frontend and backend) is driven through the multiplexer in `lib/src/protocol/mux/` and TLS ALPN negotiation in `lib/src/protocol/rustls.rs`.

## License

Sōzu itself is covered by the GNU Affero General Public License (AGPL) version 3.0 and above. Traffic going through Sōzu doesn't consider Clients and Servers as "covered work" hence don't have to be placed under the same license. A "covered work" in the Licence terms, will consider a service using Sōzu's code, methods or specific algorithms. This service can be a self managed software or an online service. The "covered work" will not consider a specific control plane you could have develop to control or use Sōzu. In simple terms, Sōzu is a Free and Open Source software you can use for both infrastructure and business but in case of a business based on Sōzu (e.g. a Load Balancer product), you should either give back your contributions to the project, or contact Clever Cloud for a specific Business Agreement.

### sozu-lib, sozu

This program is free software: you can redistribute it and/or modify it under
the terms of the GNU Affero General Public License as published by the Free
Software Foundation, version 3.

This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY;
without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
See the GNU Affero General Public License for more details.

### sozu-command-lib

sozu-command-lib is released under LGPL version 3



Copyright (C) 2015-2026 Clever Cloud
