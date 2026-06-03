# Contributing to sōzu

Thank you for your interest in sōzu, you're very welcome here!

This is a young project, and things are still a bit in flux, so there are a lot
of opportunities to learn and have a great impact on the project. We marked some
issues as [easy](https://github.com/sozu-proxy/sozu/issues?q=is%3Aissue+is%3Aopen+label%3Aeasy)
for first contributors, please check them out!

## Communication

Most of the discussion around the project happens in the [issues](https://github.com/sozu-proxy/sozu/issues),
so please check the list there.

## Navigating the source

The proxy is designed to have a minimal core that you drive through external tooling.
The Cargo workspace ships four crates:

- `lib` is the event loop handling network communication and parsing. Workers are small wrappers around this.
- `bin` is the main executable, providing a master/workers multiprocess architecture and exposing a unix socket to receive configuration changes. It also contains the command-line interface.
- `command` is a library wrapping the communication protocol of the unix socket. You'd typically use it to make tools to drive the proxy.
- `e2e` is the integration test harness that spawns real workers plus mock clients and backends.

The `fuzz/` crate (cargo-fuzz targets `fuzz_frame_parser`, `fuzz_hpack_decoder`) lives outside the Cargo workspace and is built with `cargo +nightly fuzz` from inside `fuzz/`.

The easiest way to contribute would be to work on the command-line interface (`bin/src/ctl/`) or on external tools
using the `command` library. `lib` can be complex since you need familiarity with
the event loop handling, but the parsers and protocols are well separated. `bin` is
mostly plumbing to handle command-line options, supervise workers, and dispatch
orders from the socket.

## Debugging

If the bug or feature you are exploring is linked to the HTTP or TLS implementation,
you can reuse directly the code of one of the [examples](https://github.com/sozu-proxy/sozu/tree/main/lib/examples),
they allow you to use the proxy without the overhead of the worker system.

Otherwise, the proxy uses the `log` crate and follows the `RUST_LOG` environment
variable convention, so you can precisely trace what happens.

## Testing

Read [`doc/testing.md`](./doc/testing.md) — it is the authoritative guide and describes Sōzu's testing
doctrine (assertion-first, in the style of TigerBeetle's TigerStyle, plus deterministic simulation in the
style of FoundationDB's simulator) and the five test categories (unit, integration/e2e, fuzz, deterministic
simulation, regression guards).

Before opening a pull request, the local validation chain must be green:

```bash
cargo build --all-features --locked
cargo clippy --all-targets --all-features -- -D warnings
cargo +nightly fmt --all -- --check
cargo test --workspace --locked
```

Rules every change is expected to follow:

- **Land tests in the same changeset.** A protocol, parser, or otherwise security-sensitive change ships its
  unit + property/simulation + fuzz + e2e coverage in the same pull request — not as a follow-up.
- **Assertion density for state machines.** New sans-io cores, parsers, and state machines carry dense
  `debug_assert!` pre/post-conditions and pair assertions (assert what you expect *and* what you don't), and —
  where applicable — a deterministic simulation harness on the pattern of `lib/tests/udp_simulation.rs`.
- **Never panic on network-controlled input** on the release path: turn invalid traffic into a drop + metric +
  contextual log (or the protocol's error response), never an `unwrap`/`panic!`. `debug_assert!` is for
  invariant violations only and compiles out in release.
- **No hardcoded ports in e2e** (allocate via `e2e/src/port_registry.rs`); drain responses with the
  `loop_read_*` / `receive_until_eof` helpers; prefer deadlines / `repeat_until_error_or` over `sleep`; and
  assert *provable* invariants rather than statistical fractions when an input varies between runs.
- **`#[ignore]` must carry a reason string** and only gate an environment dependency or a tracked follow-up —
  it must never hide a failing test. CI rejects a bare `#[ignore]`.
- **Docs are code:** a change to a public metric, config key, or CLI flag updates its documentation in the same
  changeset.

## Copyright assignment

We ([Clever Cloud](https://www.clever-cloud.com)) will ask you to sign a
[copyright assignment agreement](https://gist.github.com/Geal/c61fc84f0a32a9b76ff606274848370d)
before accepting your code into the project. In short, the agreement allows us
to change the license of the project (currently in
[AGPL 3](https://github.com/sozu-proxy/sozu/blob/main/LICENSE)) if needed.
You can of course keep using the published version of sōzu with your contribution
as AGPL 3.

We ask this because we might run an internal fork of sōzu on our infrastructure
with changes that make no sense for public use.

Copyright assignment is handled through the [CLA assistant bot](https://cla-assistant.io/),
which will ask you in a pull request to accept the agreement.
