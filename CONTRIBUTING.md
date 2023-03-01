# Contributing to sōzu

Thank you for your interest in sōzu, you're very welcome here!

This is a young project, and things are still a bit in flux, so there are a lot
of opportunities to learn and have a great impact on the project. We marked some
issues as [easy](https://github.com/sozu-proxy/sozu/issues?q=is%3Aissue+is%3Aopen+label%3Aeasy)
for first contributors, please check them out!

## Communication

Most of the discussion around the project happens in the [issues](https://github.com/sozu-proxy/sozu/issues),
so please check the list there. You can also reach out on [Gitter](https://gitter.im/sozu-proxy/sozu).

## Navigating the source

The proxy is designed to have a minimal core that you drive through external tooling.
There are 4 Rust crates in the repository:

- `lib` is the event loop handling network communication and parsing. Workers are small wrappers around this
- `bin` is the main executable, providing a master/workers multiprocess architecture and exposing a unix socket to receive configuration changes. It also contains the command line.
- `command` is a library wrapping the communication protocol of the unix socket. You'd typically use it to make tools to drive the proxy

The easiest way to contribute would be to work on the command line (`cli.rs`) or on external tools
using the `command` library. `lib` can be complex since you need familiarity with
the event loop handling, but the parsers and protocol are well separated. `bin` is
mostly plumbing to handle command line options, handle workers and dispatching
orders from the socket.

## Debugging

If the bug or feature you are exploring is linked to the HTTP or TLS implementation,
you can reuse directly the code of one of the [examples](https://github.com/sozu-proxy/sozu/tree/master/lib/examples),
they allow you to use the proxy without the overhead of the worker system.

Otherwise, the proxy uses the `log` crate and follows the `RUST_LOG` environment
variable convention, so you can precisely trace what happens.

## Copyright assignment

We ([Clever Cloud](https://www.clever-cloud.com)) will ask you to sign a
[copyright assignment agreement](https://gist.github.com/Geal/c61fc84f0a32a9b76ff606274848370d)
before accepting your code into the project. In short, the agreement allows us
to change the license of the project (currently in
[AGPL 3](https://github.com/sozu-proxy/sozu/blob/master/LICENSE)) if needed.
You can of course keep using the published version of sōzu with your contribution
as AGPL 3.

We ask this because we might run an internal fork of sōzu on our infrastructure
with changes that make no sense for public use.

Copyright assignment is handled through the [CLA assistant bot](https://cla-assistant.io/),
which will ask you in a pull request to accept the agreement.
