ARG ALPINE_VERSION=edge

FROM alpine:$ALPINE_VERSION as builder

RUN apk update && apk add --no-cache --virtual .build-dependencies \
  cargo \
  build-base \
  file \
  libgcc \
  musl-dev \
  protobuf \
  protobuf-dev \
  rust

RUN apk add --no-cache llvm-libunwind \
  pkgconfig

COPY . /usr/src/sozu
WORKDIR /usr/src/sozu

RUN cargo vendor --locked
RUN cargo build --release --frozen

FROM alpine:$ALPINE_VERSION as bin

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
COPY lib/assets/404.html /etc/sozu/html/404.html
COPY lib/assets/503.html /etc/sozu/html/503.html

ENTRYPOINT ["/usr/local/bin/sozu"]
CMD ["start", "-c", "/etc/sozu/config.toml"]
