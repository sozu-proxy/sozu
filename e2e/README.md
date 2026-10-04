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
- Real gRPC over both supported routes: `grpc_tests.rs` runs a Tonic
  server/client through HTTPS with ALPN `h2` and an h2c backend, then through
  the raw TCP proxy. It covers unary Put/Get, ordered bidirectional streaming,
  terminal status metadata, a real Tonic deadline propagating as `Cancelled`,
  deadline and cancellation resource cleanup, reuse of the surviving channel,
  and a new frontend connection without replay. Enable it explicitly with the
  `grpc-e2e` feature.
  The reconnect oracle counts client connector calls at the Sōzu frontend.
  It deliberately requires only that the h2c backend accepts a connection:
  Sōzu may validly reuse or pool an upstream HTTP/2 transport.
- Listener live-update: `listener_update_tests.rs`.
- Hot upgrade discipline: `test_upgrade*` cases (run with
  `cargo test -p sozu-e2e test_upgrade`).
- Fuzz wrappers: `fuzz_tests.rs` defines `#[ignore]` shims around the
  `cargo-fuzz` targets — exercise with
  `cargo test -p sozu-e2e -- --ignored fuzz`.
- h2spec acceptance: `test_h2spec_conformance` (currently 146/146/0/0 with h2spec 2.6.0).

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

Run the two real gRPC routes with:

    cargo test -p sozu-e2e --features grpc-e2e tests::grpc_tests::

If you want to run all e2e tests at once, do:

    cd e2e
    cargo test

## Real protocol services

The real-service tests start one pinned container, put its published endpoint
behind a Sōzu TCP or UDP listener, and use a native Rust protocol client only
against the Sōzu frontend. They are opt-in because they download service images
and compile client libraries that are irrelevant to the regular protocol
matrix. An enabled feature fails when neither Docker nor Podman can reach a
container server; it never turns a missing service into a skipped test.

| Feature | Service and application operations |
| --- | --- |
| `service-postgres` | PostgreSQL transactions, update, savepoint rollback, exact reconnect read |
| `service-mysql` | MySQL transactions, update, rollback, exact reconnect read |
| `service-redis` | Redis pipeline, exact `MGET`, `DEL`, and absence check |
| `service-mongodb` | MongoDB `insert_many`, update, sorted reconnect read, database removal |
| `service-kafka` | One Kafka KRaft broker, keyed batch produce/fetch and exact offsets; no consumer-group coverage |
| `service-rabbitmq` | RabbitMQ publisher confirms, fresh consumer connection, exact deliveries and acknowledgements |
| `service-pulsar` | Pulsar standalone, Magnetar 1.7.2 producer/consumer acknowledgements and exact reconnect delivery |
| `service-coredns` | CoreDNS authoritative A, AAAA, TXT and NXDOMAIN over raw UDP, flow expiry and listener reactivation |

Pulsar uses Magnetar with its `tokio` and `crypto-ring` features. This focused
job validates that client/provider combination and does not claim coverage for
the other Sōzu crypto-provider cells. Its container becomes ready only after the
broker health endpoint returns the exact `ok` body and the standalone
`public/default` namespace exists. Kafka uses the pure-Rust `rskafka` client;
the standalone stream test deliberately does not exercise consumer groups.
The DNS client uses `hickory-proto` only to encode and decode datagrams on a
connected `UdpSocket`; there is no system resolver and no hidden TCP fallback.

The fixture takes ownership of each container ID before readiness polling. If
startup fails, it saves the container logs, inspect data, and exit state before
removing only an object whose engine and run labels still match, together with
that object's anonymous volumes. When the
engine returns no usable ID and no uniquely named object with those exact
labels can be resolved, the fixture reports the failure without deleting an
unverified object; the workflow finalizer removes only resources bearing the
current run label.

RabbitMQ health checks run `rabbitmq-diagnostics` through the image's
`su-exec rabbitmq` path. Docker otherwise executes the health command as the
image's root user, which can race broker startup by creating the shared Erlang
cookie under `/var/lib/rabbitmq` with permissions that exclude the broker user.
The failing historical cookie's ownership was not retained after its exact
container cleanup; the preserved exit log and the pinned image's user and
entrypoint behavior establish this startup boundary.

For example:

```sh
SOZU_CONTAINER_ENGINE=docker \
SOZU_PROTOCOL_SERVICE_ARTIFACT_DIR=/tmp/sozu-protocol-services \
cargo test -p sozu-e2e -j4 --features service-postgres \
  tests::real_services_tcp::postgres::round_trip_and_reconnect_via_sozu \
  -- --exact --nocapture --test-threads=1
```

Each fixture assigns a unique owner label, removes only its exact container ID,
and verifies disappearance after normal completion. On failure it writes the
owned container log beneath `SOZU_PROTOCOL_SERVICE_ARTIFACT_DIR`. CI also sets
a run-specific label so its `always()` cleanup can remove and archive only
resources from that matrix cell. The fixtures create no named container volume;
the CoreDNS zone is an invocation-owned temporary bind mount. Every test
disables the Sōzu listener while the backend remains healthy and proves that a
fresh application client cannot bypass the proxy before reactivation.
