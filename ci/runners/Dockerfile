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
