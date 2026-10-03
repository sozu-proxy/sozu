# Runtime: a stable Alpine release, deliberately not pinned by digest so a
# rebuild picks up the release's security updates. The builder's musl target
# links statically (`crt-static`), so this release need not match the
# builder's, and `ALPINE_VERSION` selects only the runtime stage.
ARG ALPINE_VERSION=3.24

# Builder: the official Rust image at the toolchain pinned in `rust-toolchain`,
# on a stable Alpine release, pinned by digest. It replaces `alpine:edge` and
# its rolling `rust`/`cargo` packages: on 2026-10-02 edge shipped
# `rust 1.99.0-r0` with a broken standard library ("only metadata stub found
# for `rlib` dependency `std`"), and the image build on `main` failed until
# Alpine published `1.99.0-r1`. The toolchain now moves only when this line
# does. Raise it together with `rust-toolchain`, every `rust-version` and the
# workflow pins; take the new digest from
# `docker buildx imagetools inspect rust:<version>-alpine<release>`.
FROM rust:1.93.1-alpine3.23@sha256:4fec02de605563c297c78a31064c8335bc004fa2b0bf406b1b99441da64e2d2d AS builder

# The image's toolchain must be the one `rust-toolchain` names: fail instead
# of letting rustup download another one behind the build's back.
ENV RUSTUP_AUTO_INSTALL=0

RUN apk add --no-cache \
  build-base \
  cmake \
  pkgconf \
  protobuf \
  protobuf-dev

ARG CRYPTO_PROVIDER=crypto-ring

# Provider-specific build dependencies: `crypto-openssl` links OpenSSL
# statically, and `fips` builds aws-lc's FIPS module, whose CMake build needs
# Go and Perl.
RUN case "${CRYPTO_PROVIDER}" in \
  crypto-openssl) apk add --no-cache openssl-dev openssl-libs-static ;; \
  fips) apk add --no-cache go perl ;; \
  esac

COPY . /usr/src/sozu
WORKDIR /usr/src/sozu

RUN mkdir .cargo
RUN cargo vendor --locked >.cargo/config.toml
RUN cargo build --release --frozen --no-default-features --features jemallocator,${CRYPTO_PROVIDER}

FROM alpine:${ALPINE_VERSION} AS bin

EXPOSE 80
EXPOSE 443

VOLUME /etc/sozu
VOLUME /run/sozu

RUN mkdir -p /var/lib/sozu

RUN apk update && apk add --no-cache \
  llvm-libunwind \
  libgcc \
  ca-certificates

COPY --from=builder /usr/src/sozu/target/release/sozu /usr/local/bin/sozu
COPY os-build/config.toml /etc/sozu/config.toml

ENTRYPOINT ["/usr/local/bin/sozu"]
CMD ["start", "-c", "/etc/sozu/config.toml"]
