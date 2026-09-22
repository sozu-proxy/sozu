# Sōzu self-hosted GitHub Actions runner image

`Dockerfile` extends the upstream Actions Runner Controller (ARC) default runner image
(`ghcr.io/actions/actions-runner:latest`, runner UID `1001`, home `/home/runner`) with the
system packages and Rust toolchains that `.github/workflows/ci.yml`'s pipeline job currently
installs on every run: `protobuf-compiler`, `cmake`, h2spec `2.6.0`, and the four toolchains
`1.93.1` (default, matches `rust-toolchain`), `stable`, `beta`, and `nightly` (with `clippy` on
all four and `rustfmt` on `nightly`). Baking these in removes that per-run install cost from
every CI job that runs on the CKE-hosted runners.

## Image reference

```
ghcr.io/sozu-proxy/sozu-ci-runner:latest
```

The canonical location is GHCR under the `sozu-proxy` org, alongside the upstream repo. ARC
scale-set manifests under `ci/runners/arc/` reference this exact tag in
`template.spec.containers[0].image`. As of this writing the image has only been built and
smoke-tested locally (`sozu-ci-runner:local`); nothing has been pushed to GHCR yet — see "Pushing
to GHCR" below.

Tagging scheme: `latest` always points at the most recently pushed build and is mutable, so any
consumer pulling it should expect `imagePullPolicy: Always` semantics. Until a dedicated
build-and-push workflow exists, a maintainer rebuilds and pushes by hand whenever `Dockerfile`
changes or the toolchains it pins (`rust-toolchain`, `ci.yml`'s matrix, h2spec version) drift. A
future automated workflow may additionally tag pushes with the commit SHA or a date-based tag for
rollback; `latest` is the only tag that can be assumed to exist today. The upstream base image
(`ghcr.io/actions/actions-runner:latest`) is itself a mutable `latest` tag, so a rebuild can pick
up an unreviewed upstream change; pinning it to a digest is a possible future hardening step, not
done here.

Built size is large — `docker images sozu-ci-runner:local` reports roughly 6 GB, mostly the four
Rust toolchains. Budget node disk and image-pull time accordingly when sizing the runner nodes
that will pull this image.

## Rebuilding locally

```bash
docker build -t sozu-ci-runner:local -f ci/runners/Dockerfile ci/runners
```

Smoke-test the toolchains after a rebuild:

```bash
docker run --rm sozu-ci-runner:local bash -lc \
  'set -e; for tc in 1.93.1 stable beta nightly; do rustup run $tc rustc --version; done; \
   cargo clippy --version; cargo +nightly fmt --version; h2spec --version; protoc --version; cmake --version'
```

Expected: four `rustc X.Y.Z` lines (one per toolchain, `1.93.1` exact for the default), plus
clippy/fmt/h2spec/protoc/cmake version output, with no error.

## Pushing to GHCR

Pushing requires a GHCR-scoped token with `write:packages` on the `sozu-proxy` org. Never hardcode
that token in this repo or in the image. Two supported ways to push:

1. **Interactively**, for a manual rebuild:

   ```bash
   docker login ghcr.io -u <your-github-username>
   docker tag sozu-ci-runner:local ghcr.io/sozu-proxy/sozu-ci-runner:latest
   docker push ghcr.io/sozu-proxy/sozu-ci-runner:latest
   ```

2. **From a dedicated build-and-push GitHub Actions workflow**, authenticating with a repo/org
   Actions secret (e.g. `GITHUB_TOKEN` with `packages: write` permission, or a dedicated PAT).
   That workflow does not exist yet — building and wiring it up is out of scope for this change.
   The runner operations runbook (not yet written) documents the full operational procedure,
   including when and how this image gets rebuilt and republished.

## Docker-in-Docker: explicitly out of scope here

This image deliberately does **not** bundle a Docker daemon, `dind`, or a Docker CLI beyond what
the base ARC image already ships. Upstream ARC discourages baking Docker-in-Docker into the
runner image itself (see `actions/actions-runner-controller`'s
`docs/deploying-alternative-runners.md`).

`.github/workflows/release.yml` has a `docker-release` job that builds and pushes
`clevercloud/sozu:*` to Docker Hub via `docker/setup-buildx-action`, `docker/login-action`, and
`docker/build-push-action`. For that job to run on a self-hosted CKE runner, the `sozu-general`
ARC scale-set values (not covered by this change) need `containerMode: dind` (or an equivalent
docker-socket/sidecar setup) — this runner image itself stays Docker-free.
