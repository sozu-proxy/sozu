# E2E protocol-pair matrix — coverage spec

This document is the index for the four-cell `(frontend × backend)`
protocol-pair matrix that v2.x e2e tests opt into. The harness already
exists at `e2e/src/tests/tests.rs::try_tls_cardinality_cell`; this
document captures **how to use it**, **where to apply it**, and the
**applicable-cells-only** rule.

## The matrix

| Cell | Frontend | Backend | Mock backend | Notes |
|---|---|---|---|---|
| `h1-h1` | h1 + TLS | h1 cleartext | `AsyncBackend::http_handler` | Baseline; covered by `try_tls_endpoint`, `try_tls_rsa_2048`, `try_tls_ecdsa`. |
| `h1-h2c` | h1 + TLS | h2c cleartext | `H2Backend::start` | Covered by `try_tls_cardinality_h1_h2_*`. |
| `h2-h1` | h2 + TLS (ALPN) | h1 cleartext | `AsyncBackend::http_handler` | Covered by `try_tls_cardinality_h2_h1_*`. |
| `h2-h2c` | h2 + TLS (ALPN) | h2c cleartext | `H2Backend::start` | Covered by `try_tls_cardinality_h2_h2_*`. |

Backend **TLS** (h1-over-TLS, h2-over-TLS) is **not** in v2.0.0; it
lands in v2.1.0 with [#1218](https://github.com/sozu-proxy/sozu/issues/1218).
At that point the matrix grows to 8 cells (each backend axis gains a
TLS variant).

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

| Feature under test | h1-h1 | h1-h2c | h2-h1 | h2-h2c | Notes |
|---|---|---|---|---|---|
| HTTP Basic auth | ✓ | ✓ | ✓ | ✓ | All cells; auth gate is router-layer. |
| 301/302/308 redirect | ✓ | ✓ | ✓ | ✓ | All cells; redirect renders same answer-template. |
| X-Real-IP injection | ✓ | ✓ | ✓ | ✓ | All cells; H2 trailer-elision needs H2 frontend cells specifically. |
| Per-IP `429` limit | ✓ | ✓ | ✓ | ✓ | All cells; one of each cert kind is enough. |
| `evict_on_queue_full` | ✓ | ✓ | ✓ | ✓ | All cells. |
| Custom answer template | ✓ | ✓ | ✓ | ✓ | All cells. |
| RFC 9218 priority | ✗ | ✗ | ✓ | ✓ | H2 frontend only — RFC 9218 is HTTP/2 priorities. |
| HPACK rejection counters | ✗ | ✗ | ✓ | ✓ | H2 frontend only — HPACK is H2 wire. |
| H2 flood detector | ✗ | ✗ | ✓ | ✓ | H2 frontend only. |
| HTTP/1 pipelining | ✓ | ✓ | ✗ | ✗ | H1 frontend only — H2 multiplexing replaces pipelining. |
| H2 trailer header elision | ✗ | ✗ | ✓ | ✓ | H2 frontend only — trailers are H2 frames. |
| RFC 7239 `Forwarded` (v2.1.0) | ✓ | ✓ | ✓ | ✓ | All cells; header semantic is protocol-agnostic. |
| Backend TLS / mTLS-up (v2.1.0) | (new TLS-axis cells) | | | | Adds 4 TLS-backend cells; matrix grows to 8. |
| ACME HTTP-01 challenge (v2.2.0) | ✓ | ✓ | ✓ | ✓ | Challenge route is HTTP/1.1 GET on port 80; ALPN-agnostic. |
| OCSP stapling (v2.2.0) | ✓ | ✓ | ✓ | ✓ | Stapling is TLS-handshake, before HTTP. |

When adding a test, declare the applicable cells in a comment and use
the relevant `try_tls_cardinality_*` wrapper(s).

## Convenience pattern (to be added)

A future small commit should add a `protocol_pair_matrix!()` macro that
auto-generates the wrapper functions for a feature:

```rust
// HYPOTHETICAL — not landed yet:
protocol_pair_matrix! {
    name = basic_auth,
    fn = try_basic_auth_cell,
    cells = [h1_h1, h1_h2, h2_h1, h2_h2],
    cert_kinds = [rsa, ecdsa],
}
```

This is a Week 2+ ergonomics pass. Until then, hand-author the wrappers
following the existing `try_tls_cardinality_*` shape.

## v2.0.0 priority-1 backfill (Week 2 deliverable)

The features below ship in v2.0.0 release notes; their e2e coverage
should pass through every applicable cell before the v2.0.0 tag:

- Basic auth (`required_auth = true` + `authorized_hashes`) ←
  `redirect_rewrite_auth_tests.rs` already covers H1+TLS; add H2 cells.
- 301 redirects (`RedirectPolicy::PERMANENT`) ←
  `redirect_rewrite_auth_tests.rs` covers H1+TLS; add H2 cells.
- X-Real-IP injection + anti-spoof elision ← H2 cells especially
  important for trailer-frame elision regression.
- Per-IP `429` limit ← `cluster_ip_limit_tests.rs` covers H1; add H2.
- `evict_on_queue_full` ← `eviction_tests.rs`.
- Custom answer template (`answers."503"`).

Each feature × applicable-cell pair gets one test and uses the matching
wrapper. The check-in commit lists which (feature × cell) combinations
were verified.

## v2.1.0 expansion (preview)

When backend TLS lands ([#1218](https://github.com/sozu-proxy/sozu/issues/1218)),
the cardinality helper's `backend_h2: bool` axis grows to a
`backend: BackendKind { H1Cleartext, H2cCleartext, H1Tls, H2Tls }`
variant. Tests already opting into the matrix get the new axis "for
free" via the macro proposed above.

## Mock backend taxonomy (codex MED-2 — applicable cells only)

| Mock | h1-h1 | h1-h2c | h2-h1 | h2-h2c | Notes |
|---|---|---|---|---|---|
| `AsyncBackend` | ✓ | ✗ | ✓ | ✗ | H1 only on the wire. |
| `SyncBackend` | ✓ | ✗ | ✓ | ✗ | Synchronous H1. |
| `H2Backend` | ✗ | ✓ | ✗ | ✓ | h2c only. |
| `RawH2ResponseBackend` | ✗ | ✓ | ✗ | ✓ | Adversarial H2 — for security tests, not protocol-pair backfill. |
| `ChunkedFlushH1Backend` | ✓ | ✗ | ✓ | ✗ | H1-only chunked-encoding repros. |
| `SingleReadH1Backend` | ✓ | ✗ | ✓ | ✗ | H1-only single-read repros. |
| `RawH2ResponseBackend` | ✗ | ✓ | ✗ | ✓ | Already listed; here for ✗-symmetry. |

Tests opt into mocks that match their target cells. Don't pair
`H2Backend` with an `h1-h1` cell — the cluster still has
`http2 = false` and would reject the connection preface bytes.

## Where the matrix lives in CI

The §3.1.g feature-coverage matrix runs each crypto provider × full
features (`opentelemetry,splice,simd`) over the same e2e suite. The
4-cell protocol matrix runs **inside each CI cell**. Total:

```
CI cells × tested protocol-pair cells × applicable-features ⊆ matrix surface
```

Coverage gaps are listed in the §v2.0.0 priority-1 backfill table above.
A test marked "shipped in v2.0.0 release notes" without any matrix
backfill is a v2.0.0 release-blocker.

## Related

- `e2e/src/tests/tests.rs::try_tls_cardinality_cell` — harness implementation.
- `e2e/src/mock/` — mock backends.
- `tasks/todo.md` §3.1.h — the spec this document fulfils.
- consolidated review codex MED-2 — "applicable cells only" rule.
