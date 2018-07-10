FROM alpine:latest as builder

COPY . /source/

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

WORKDIR /source/bin

RUN cargo build --release --features use-openssl



FROM alpine:latest as bin

COPY os-build/docker/config.toml /etc/sozu/config.toml

RUN apk update && apk add --no-cache openssl-dev \
  llvm-libunwind \
  libgcc

COPY --from=builder /source/target/release/sozu sozu

ENV SOZU_CONFIG /etc/sozu/config.toml

VOLUME /etc/sozu

RUN mkdir -p /run/sozu
VOLUME /run/sozu

EXPOSE 80
EXPOSE 443

ENTRYPOINT ["/sozu"]

CMD ["start", "-c", "/etc/sozu/config.toml"]
