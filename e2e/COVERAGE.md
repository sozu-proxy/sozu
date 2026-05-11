# E2E protocol-pair matrix — coverage spec

This document is the index for the four-cell `(frontend × backend)`
protocol-pair matrix that v2.x e2e tests opt into. The harness already
exists at `e2e/src/tests/tests.rs::try_tls_cardinality_cell`; this
document captures **how to use it**, **where to apply it**, and the
**applicable-cells-only** rule.

## The matrix

| Cell     | Frontend        | Backend       | Mock backend                 | Notes                                                                         |
| -------- | --------------- | ------------- | ---------------------------- | ----------------------------------------------------------------------------- |
| `h1-h1`  | h1 + TLS        | h1 cleartext  | `AsyncBackend::http_handler` | Baseline; covered by `try_tls_endpoint`, `try_tls_rsa_2048`, `try_tls_ecdsa`. |
| `h1-h2c` | h1 + TLS        | h2c cleartext | `H2Backend::start`           | Covered by `try_tls_cardinality_h1_h2_*`.                                     |
| `h2-h1`  | h2 + TLS (ALPN) | h1 cleartext  | `AsyncBackend::http_handler` | Covered by `try_tls_cardinality_h2_h1_*`.                                     |
| `h2-h2c` | h2 + TLS (ALPN) | h2c cleartext | `H2Backend::start`           | Covered by `try_tls_cardinality_h2_h2_*`.                                     |

Backend **TLS** (h1-over-TLS, h2-over-TLS) is **not** in the current
release; the follow-up enhancement is tracked at
[#1218](https://github.com/sozu-proxy/sozu/issues/1218). At that point
the matrix grows to 8 cells (each backend axis gains a TLS variant).

## How the harness works

```rust
// Existing API in e2e/src/tests/tests.rs (around line 660):
pub fn try_tls_cardinality_cell(
    test_name: &str,
    cert_pem: &str,
    key_pem: &str,
    tls_versions: Option<Vec<TlsVersion>>,
    frontend_h2: bool,   // false = H1+TLS, true = H2+TLS
    backend_h2: bool,    // false = h1 cleartext, true = h2c cleartext
) -> State { ... }
```

For each cell:

- Frontend ALPN preference is set indirectly via the chosen Hyper client
  (`build_https_client` for H1, `build_h2_client` for H2). rustls
  negotiates the cell's protocol.
- Backend protocol is selected by `cluster.http2 = Some(true|false)` plus
  the appropriate mock (`H2Backend` for h2c, `AsyncBackend` for H1).

Existing 4-cell wrappers (RSA + ECDSA cert kinds × 2 cardinality cells
that aren't trivially covered by the H1-baseline tests):

- `try_tls_cardinality_h1_h2_rsa` / `…_ecdsa` — H1 frontend + h2c backend
- `try_tls_cardinality_h2_h1_rsa` / `…_ecdsa` — H2 frontend + H1 backend
- `try_tls_cardinality_h2_h2_rsa` / `…_ecdsa` — H2 frontend + h2c backend

The H1+TLS / H1-backend cell is the legacy baseline — `try_tls_endpoint`
covers it.

## "Applicable cells only" rule

Not every test makes sense in every cell. Tests opt into the cells they
actually exercise. The decision matrix:

| Feature under test               | h1-h1                | h1-h2c | h2-h1 | h2-h2c | Notes                                                               |
| -------------------------------- | -------------------- | ------ | ----- | ------ | ------------------------------------------------------------------- |
| HTTP Basic auth                  | ✓                    | ✓      | ✓     | ✓      | All cells; auth gate is router-layer.                               |
| 301/302/308 redirect             | ✓                    | ✓      | ✓     | ✓      | All cells; redirect renders same answer-template.                   |
| X-Real-IP injection              | (✓)                  | ✓      | (✓)   | ✓      | H1-backend cells (h1-h1, h2-h1) assert frontend forwarding only — `AsyncBackend` does not currently expose request bytes for the strip/inject assertion (tracked as a future `AsyncBackend` enhancement). H2 trailer-elision needs H2 frontend cells specifically. |
| Per-IP `429` limit               | ✓                    | ✓      | ✓     | ✓      | All cells; one of each cert kind is enough.                         |
| `evict_on_queue_full`            | ✓                    | ✓      | ✓     | ✓      | All cells.                                                          |
| Custom answer template           | ✓                    | ✓      | ✓     | ✓      | All cells.                                                          |
| RFC 9218 priority                | ✗                    | ✗      | ✓     | ✓      | H2 frontend only — RFC 9218 is HTTP/2 priorities.                   |
| HPACK rejection counters         | ✗                    | ✗      | ✓     | ✓      | H2 frontend only — HPACK is H2 wire.                                |
| H2 flood detector                | ✗                    | ✗      | ✓     | ✓      | H2 frontend only.                                                   |
| HTTP/1 pipelining                | ✓                    | ✓      | ✗     | ✗      | H1 frontend only — H2 multiplexing replaces pipelining.             |
| H2 trailer header elision        | ✗                    | ✗      | ✓     | ✓      | H2 frontend only — trailers are H2 frames.                          |
| RFC 7239 `Forwarded` (planned)   | ✓                    | ✓      | ✓     | ✓      | All cells; header semantic is protocol-agnostic.                    |
| Backend TLS / mTLS-up (planned)  | (new TLS-axis cells) |        |       |        | Adds 4 TLS-backend cells; matrix grows to 8.                        |
| ACME HTTP-01 challenge (planned) | ✓                    | ✓      | ✓     | ✓      | Challenge route is HTTP/1.1 GET on port 80; ALPN-agnostic.          |
| OCSP stapling (planned)          | ✓                    | ✓      | ✓     | ✓      | Stapling is TLS-handshake, before HTTP.                             |

When adding a test, declare the applicable cells in a comment and use
the relevant `try_tls_cardinality_*` wrapper(s).

## Convenience pattern

`e2e/src/tests/protocol_pair_matrix.rs` exposes a `protocol_pair_matrix!`
macro that emits the four wrapper functions plus their `#[test]`
harnesses for a single feature cell function:

```rust
// Cell function: takes (frontend_h2, backend_h2), returns State.
fn try_basic_auth_cell(frontend_h2: bool, backend_h2: bool) -> State { ... }

// Macro emits `pub mod basic_auth { ... }` containing
// try_h1_h1 / try_h1_h2 / try_h2_h1 / try_h2_h2 wrappers plus
// matching test_h1_h1 / test_h1_h2 / test_h2_h1 / test_h2_h2
// `#[test]` harnesses that wrap the cell in `repeat_until_error_or`.
// `cargo test basic_auth` filters all four cells.
protocol_pair_matrix!(basic_auth, try_basic_auth_cell, "basic auth");
```

Hand-authored wrappers (e.g. the cardinality smoke tests in `tests.rs`)
remain valid for cases that need a custom `repeat` count or a non-cell
shape. The macro covers the common case where each cell is one
`(frontend_h2, backend_h2)` boolean pair.

## Priority-1 backfill

Landed in `e2e/src/tests/protocol_pair_matrix.rs` (16 tests, four cells
each):

- **Basic auth** (`required_auth = true` + `authorized_hashes`) — three
  arms per cell: missing header → 401, wrong creds → 401, correct
  creds → 200 from the backend.
- **301 redirect** (`RedirectPolicy::Permanent`) — backend never
  contacted; the response carries `:status 301` (H2) /
  `HTTP/1.1 301` (H1) plus a `Location` header.
- **Custom answer template** (`Cluster.answers."503"`) — operator's
  503 template renders verbatim (verified via load-bearing
  `X-Sozu-Stamp` header that the listener default does not emit).
- **X-Real-IP elide + send** (`with_elide_x_real_ip` +
  `with_send_x_real_ip` on the HTTPS listener) — client-supplied
  spoof stripped, proxy-generated header reaches the backend.
  Initial-HEADERS only; H2 trailer-frame elision is a separate
  targeted test scaffolded at `tests::test_x_real_ip_elide_h2_trailer`.

Deferred from matrix coverage (existing single-cell tests stay
authoritative until connection-pinning helpers land):

- **Per-IP `429` limit** — `cluster_ip_limit_tests.rs` exercises the
  H1 cleartext path. Matrix coverage needs sustained TLS + HTTP/2
  connection holds while a second connection races; the current
  Hyper client pool reuses connections opaquely, so saturating the
  per-IP slot from a parallel Hyper client is unreliable.
- **`evict_on_queue_full`** — `eviction_tests.rs` exercises the H1
  cleartext path with raw TCP socket holds. Same connection-pinning
  prerequisite as 429.

Each feature × cell pair runs in `repeat_until_error_or(2, ...)` so a
single transient failure surfaces as a stable fail — the harness
matches the existing redirect/auth tests' retry budget.

## Backend-TLS expansion (preview)

When backend TLS lands ([#1218](https://github.com/sozu-proxy/sozu/issues/1218)),
the cardinality helper's `backend_h2: bool` axis grows to a
`backend: BackendKind { H1Cleartext, H2cCleartext, H1Tls, H2Tls }`
variant. Tests already opting into the matrix get the new axis "for
free" via the macro proposed above.

## Mock backend taxonomy

| Mock                    | h1-h1 | h1-h2c | h2-h1 | h2-h2c | Notes                                                            |
| ----------------------- | ----- | ------ | ----- | ------ | ---------------------------------------------------------------- |
| `AsyncBackend`          | ✓     | ✗      | ✓     | ✗      | H1 only on the wire.                                             |
| `SyncBackend`           | ✓     | ✗      | ✓     | ✗      | Synchronous H1.                                                  |
| `H2Backend`             | ✗     | ✓      | ✗     | ✓      | h2c only.                                                        |
| `RawH2ResponseBackend`  | ✗     | ✓      | ✗     | ✓      | Adversarial H2 — for security tests, not protocol-pair backfill. |
| `ChunkedFlushH1Backend` | ✓     | ✗      | ✓     | ✗      | H1-only chunked-encoding repros.                                 |
| `SingleReadH1Backend`   | ✓     | ✗      | ✓     | ✗      | H1-only single-read repros.                                      |

Tests opt into mocks that match their target cells. Don't pair
`H2Backend` with an `h1-h1` cell — the cluster still has
`http2 = false` and would reject the connection preface bytes.

## Where the matrix lives in CI

The CI feature-coverage matrix at `.github/workflows/ci.yml` runs each
crypto provider × full features (`opentelemetry,splice,simd`) over the
same e2e suite. The 4-cell protocol matrix runs **inside each CI cell**.
Total:

```
CI cells × tested protocol-pair cells × applicable-features ⊆ matrix surface
```

Coverage gaps are listed in the priority-1 backfill table above. A test
covering a feature that ships in a release without any matrix backfill
is a release-blocker.

## Related

- `e2e/src/tests/tests.rs::try_tls_cardinality_cell` — harness implementation.
- `e2e/src/mock/` — mock backends.
