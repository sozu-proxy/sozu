FROM ghcr.io/actions/actions-runner:latest

USER root
RUN apt-get update && apt-get install -y --no-install-recommends \
        protobuf-compiler cmake build-essential curl git ca-certificates \
        pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

ARG H2SPEC_VERSION=2.6.0
RUN curl -fsSL "https://github.com/summerwind/h2spec/releases/download/v${H2SPEC_VERSION}/h2spec_linux_amd64.tar.gz" \
      | tar -xz -C /usr/local/bin h2spec \
    && h2spec --version

# aws-lc-fips-sys (the `fips` feature) needs a Go toolchain at build time: its CMake build
# invokes aws-lc/cmake/go.cmake, which shells out to `go` to build BoringSSL's FIPS module.
# ubuntu-latest carries Go preinstalled; this image did not, so the `fips` cell (and anything
# building with --all-features, e.g. msrv-full's `cargo doc`) failed with "Could not find Go".
# Exact version doesn't matter here (unlike Rust's MSRV pin) — any recent Go satisfies the CMake
# check — so this floats loosely rather than being kept in lockstep with anything else.
ARG GO_VERSION=1.24.0
RUN curl -fsSL "https://go.dev/dl/go${GO_VERSION}.linux-amd64.tar.gz" -o /tmp/go.tar.gz \
    && tar -C /usr/local -xzf /tmp/go.tar.gz \
    && rm /tmp/go.tar.gz
ENV PATH=/usr/local/go/bin:$PATH
RUN go version

USER runner
ENV RUSTUP_HOME=/home/runner/.rustup \
    CARGO_HOME=/home/runner/.cargo \
    PATH=/home/runner/.cargo/bin:$PATH

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --profile minimal --default-toolchain 1.93.1 --component clippy \
    && rustup toolchain install stable beta nightly --profile minimal --component clippy \
    && rustup component add rustfmt --toolchain nightly \
    && rustup default 1.93.1 \
    && rustc --version && cargo --version
