# End to end tests

We want to check Sōzu's behavior in all corner cases for the CI.

## Principles

This crate contains thin wrappers around the Sōzu lib, that create:

- a Sōzu worker with a very simple config, able to run a detached thread
- mocked clients that send simple HTTP requests
- mocked backends, sync and async, that reply dummy 200 OK responses (for instance)

This crate provides the `Aggregator` trait, that allows to create simple aggregators.
The aggregators keep track, for instance, of how many requests were received,
and how many were sent, by a backend (just an example).

These elements are instantiated in test functions. The current suite covers
normal traffic-passthrough behaviour plus the protocol corner cases that have
shown up on `feat/h2-mux`:

- HTTP/2 multiplexing: `e2e/src/tests/h2_tests.rs`,
  `h2_correctness_tests.rs`, `h2_priority_rearm_tests.rs`,
  `h2_security_tests.rs`, and the focused
  `h2_security_{parser,session,sni,header_injection}.rs` modules
  (~130 tests covering desync vectors, HPACK / pseudo-header
  validation, RFC 9218 priorities, RFC 9113 §6.8 drain, GOAWAY
  attribution, the H2FloodDetector chokepoint, and the
  CVE-2023-44487 / CVE-2024-27316 / CVE-2025-8671 mitigations).
- HTTP/1.1, PROXY-protocol, keepalive, and HUP edge cases:
  `mux_tests.rs`, `h1_security_tests.rs`.
- TLS handshake, ALPN, SNI binding: `tls_tests.rs`.
- Raw TCP proxy: `tcp_tests.rs`.
- Listener live-update: `listener_update_tests.rs`.
- Hot upgrade discipline: `test_upgrade*` cases (run with
  `cargo test -p sozu-e2e test_upgrade`).
- Fuzz wrappers: `fuzz_tests.rs` defines `#[ignore]` shims around the
  `cargo-fuzz` targets — exercise with
  `cargo test -p sozu-e2e -- --ignored fuzz`.
- h2spec acceptance: `test_h2spec_conformance` (currently 145/145/0/0).

Mock backends live in `e2e/src/mock/`: `sync_backend.rs`,
`async_backend.rs`, `h2_backend.rs`, `raw_h2_response_backend.rs`,
`client.rs`, `https_client.rs`, `aggregator.rs`. The shared H2 helpers in
`e2e/src/tests/h2_utils.rs` (notably `loop_read_*`) absorb TCP segmentation
in assertions so single-shot `read()` race conditions cannot mask a real
truncation regression.

Tests that toggle the process-wide `e2e-hooks` `AtomicBool` injection
points (the canonical group is in
`e2e/src/tests/h2_security_session.rs:239, 622`) MUST be marked
`#[serial_test::serial(force_h2_client_failure)]` so they do not race
against each other through the shared atomic. This serialisation is
narrow — generic `AtomicBool` helpers that do not gate the shared
injection state do not need it.

# How to run

The tests are flagged with the usual macros, so they will run with all other tests when you do:

    cargo test

You can run just one test using

    cargo test test_issue_810_timeout

If you want to run all e2e tests at once, do:

    cd e2e
    cargo test
