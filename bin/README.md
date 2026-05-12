# sozu, a HTTP proxy

This project wraps the `sozu_lib` library to make it scalable and dynamically
configured. Each single threaded event loop is started in a worker process that
receives configuration commands through anonymous unix sockets.

This executable requires a configuration file in the TOML format, that describes
the worker types and numbers, along with global information. This file can
describe clusters handled by the proxy, but it is more recommended to
use the command unix socket, through which the proxy listens for orders or
configuration changes. The path of that unix socket is set in the configuration
file.

## Command socket message format

The proxy receives orders through a unix socket. The path to this unix socket can
be defined by the `command_socket` option in the TOML configuration file.

Messages on the live command channel are length-delimited binary protobuf: each
frame is preceded by a native-`usize` length prefix that the parser reads via
`Channel::read_message_*` (`command/src/channel.rs:611`). This is **not** a
0-byte separator — the only place the legacy `\n\0` separator is still in use is
the on-disk state-file format produced by `save_state` and consumed by
`load_state`, which writes one JSON record per line followed by a NUL byte
(`command/src/state.rs:1613, 1630`). Treat the two delimiters as distinct
schemes; the live channel is binary-only.

Message types are defined in `../command/src/command.proto`. The
`sozu-command-lib` crate (the `command/` workspace member) provides the
`Channel` and `Request`/`Response` Rust bindings used to talk to the socket from
Sōzu and from external tools such as the `sozu` CLI subcommands under
`bin/src/ctl/`.

A dedicated supervisor-lifecycle write-up — accept loop, audit log, hot
reconfig fan-out, FD-passing for hot upgrades — is planned alongside the
in-tree code in `bin/src/command/`. Until that lands the canonical sources are
the module-level comments in `bin/src/command/{mod,server,sessions}.rs` and the
control-plane audit-log section of `doc/observability.md`.

## `sozu top` — live operator TUI

Build with `--features tui` to get a btop/htop-style live console. `sozu top`
talks to the same command socket as the rest of the CLI and renders clusters,
backends, listeners, certificates, H2 flood counters, and a `SubscribeEvents`
tail in a single screen. The TUI is read-only and self-clears its cardinality
lease on exit. See [`../doc/sozu-top.md`](../doc/sozu-top.md) and the flag
reference in [`../doc/configure_cli.md`](../doc/configure_cli.md).
