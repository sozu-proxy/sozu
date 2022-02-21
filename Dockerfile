ARG ALPINE_VERSION=edge
FROM alpine:$ALPINE_VERSION as builder

RUN apk update && apk add --no-cache --virtual .build-dependencies \
  cargo \
  build-base \
  file \
  libgcc \
  musl-dev \
  rust
RUN apk add --no-cache openssl-dev \
  llvm-libunwind \
  pkgconfig

COPY . /source/
WORKDIR /source/bin
RUN cargo build --release --features use-openssl



FROM alpine:$ALPINE_VERSION as bin

EXPOSE 80
EXPOSE 443

ENTRYPOINT ["/sozu"]
CMD ["start", "-c", "/etc/sozu/config.toml"]

VOLUME /etc/sozu
VOLUME /run/sozu

ENV SOZU_CONFIG /etc/sozu/config.toml
RUN mkdir /var/lib/sozu

RUN apk update && apk add --no-cache openssl-dev \
  llvm-libunwind \
  libgcc

COPY --from=builder /source/target/release/sozu sozu
COPY os-build/docker/config.toml $SOZU_CONFIG
