# Release process

Sozu has a lot of moving pieces and some dependent projects, so coordinate releases carefully.

## Checklist

### Documentation

In cases of changes to:

- the configuration file format: update `bin/config.toml`, `doc/configure.md`, and `doc/how_to_use.md`
- the configuration state: update `lib/src/lib.rs` documentation, and examples in `lib/examples`
- the command messages: update `command/README.md` and `lib/src/lib.rs`
- the user-visible behaviour: add an entry under `## [Unreleased]` in `CHANGELOG.md`

### Testing

### Worker liveness

Launch sozu with `worker_automatic_restart` on, test a request, kill a worker, test a request.

### Upgrades

- compile a sozu of the previous published version, store the binary somewhere
- compile a sozu of the latest version
- start the old sozu version
- query its configuration state via the unix command socket (`sozu` CLI)
- replace the old sozu binary with the new one
- launch a hot upgrade through the unix command socket
- check that the new sozu runs properly (listens for new connections, handles requests, etc.)
- check the new sozu's state, compared to the previous one
- verify that the old workers quit correctly (if they were handling WebSocket connections, check that the master detects when you kill them)

For minor and patch releases, the upgrade process must always pass correctly.

### Update the changelog

Move the contents of `## [Unreleased]` into a new `## X.Y.Z - DATE` release section and reset the unreleased block. Do this **before** pushing the tag — the release workflow's `awk` extractor at `.github/workflows/release.yml` keys off the `## <version>` heading (the `## X.Y.Z - DATE` form used by every released section; a bracketed `## [X.Y.Z]` is also accepted) and reads until the next `## ` heading. If the section is missing, the draft release body falls back to a placeholder and the workflow emits a `::warning::`.

## Publish the new version

Update `version` in each per-crate `[package]` table — `bin/Cargo.toml`, `lib/Cargo.toml`, `command/Cargo.toml`, `e2e/Cargo.toml` — keeping all four in lockstep. The release workflow's preflight job (`.github/workflows/release.yml`) validates that all four match the pushed tag and aborts the release if any drift remains.

The four workspace crates currently published from this repository are:

- `command/` (`sozu-command-lib`)
- `lib/` (`sozu-lib`)
- `bin/` (`sozu`)
- `e2e/` (`sozu-e2e`, internal harness — generally not published to crates.io)

Run `cargo build --locked` at the root of the project to refresh `Cargo.lock`.

Commit the `Cargo.toml` and `Cargo.lock` changes.

Wait for the GitHub Actions build to pass (<https://github.com/sozu-proxy/sozu/actions>).

## Pre-built binaries

After the tag triggers `.github/workflows/release.yml`, ten `sozu-${VERSION}-${TARGET}-${PROVIDER}.tar.gz` archives plus a `SHA256SUMS` file (signed with sigstore cosign keyless — `SHA256SUMS.sig` + `SHA256SUMS.pem`) land on a GitHub draft release alongside SLSA build provenance. The matrix covers three published targets (`x86_64-unknown-linux-{gnu,musl}`, `aarch64-unknown-linux-gnu`) crossed with four crypto providers — `crypto-ring` (default, all 3 published targets), `crypto-aws-lc-rs` (all 3 published targets), `crypto-openssl` (gnu targets only — `rustls-openssl` needs a vendored OpenSSL on musl), and `fips` (gnu targets only — `aws-lc-fips-sys` precompiled FIPS module assumes glibc). `aarch64-unknown-linux-musl` is intentionally excluded from the prebuilt matrix: `jemalloc-sys 0.5.4+5.3.0-patched`'s autoconf cross-compile probe (host=aarch64-unknown-linux-musl, build=…-gnu) cannot link any of its three atomics test programs against the `musl-tools`-supplied `musl-gcc` wrapper on the arm64 runner, so jemalloc 5.3.0 falls through to `#error "Don't have atomics implemented on this platform."`. Re-introduce the cell once a real `aarch64-linux-musl-gcc` cross toolchain is wired into CI. Each tarball contains the stripped `sozu` binary, `LICENSE-AGPL3`, `LICENSE-LGPL3`, `README.md`, `CHANGELOG.md`, the production-shaped `os-build/config.toml`, both shipped systemd units, and a `SOURCE.txt` corresponding-source pointer that records the target and crypto provider for the cell.

For stable tags (`X.Y.Z`), the workflow also pushes `clevercloud/sozu:${VERSION}` and `clevercloud/sozu:latest` to Docker Hub. RC tags (`X.Y.Z-rc.N`) are marked as GitHub prereleases and skip the `:latest` move; they also do not push a tagged Docker image. `workflow_dispatch` rebuilds of an existing stable tag publish only `clevercloud/sozu:${VERSION}` and do **not** move `clevercloud/sozu:latest`. To deliberately move `:latest` you must push the corresponding git tag (force-push of an immutable tag is blocked by repo policy — bump the patch version instead).

The pre-existing `dockerhub` job in `.github/workflows/ci.yml` is gated off on tag pushes — version-pinned and `:latest` images come exclusively from the release workflow. Branch and PR pushes still produce `clevercloud/sozu:${SHA}` aliases as before.

Verify the Docker Hub tag, then click 'Publish' on the draft release.

## Following checks

Verify that the [Docker container](https://hub.docker.com/r/clevercloud/sozu/) was built correctly.

Update related projects to use the latest version of `sozu-command-lib`:

- [sozu-client](https://github.com/CleverCloud/sozu-client)
- [kawa](https://github.com/CleverCloud/kawa)
- [clever-operator](https://github.com/CleverCloud/clever-operator)
- [sozu-acme](https://github.com/sozu-proxy/sozu-acme)
- [sozu-demo](https://github.com/sozu-proxy/sozu-demo)
- the family of `sozu-*-connector` and `sozu-*-discovery` repositories under `CleverCloud`
