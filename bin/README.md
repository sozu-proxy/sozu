# sozu, a HTTP proxy

This project wraps the `sozu_lib` library to make it scalable and dynamically
configured. Each single threaded event loop is started in a worker process that
receives configuration commands through anonymous unix sockets.

This executable requires a configuration file in the TOML format, that describes
the worker types and numbers, along with global information. This file can
describe the applications handled by the proxy, but it is more recommended to
use the command unix socket, through which the proxy listens for orders or
configuration changes. The path of that unix socket is set in the configuration
file.

## Command socket message format

The proxy receives orders through a unix socket. The path to this unix socket can
be defined by the `command_socket` option in the TOML configuration file.

The messages are sent as JSON messages, separated by the 0 byte.

Their format is defined in `../command/README.md`. Additionally, the
`sozu_command_lib` crate in `../command` already provides the necessary
structures, serialization and deserialization code to communicate with
the command socket.
