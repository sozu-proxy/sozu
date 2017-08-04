FROM alpine:edge as builder

COPY . /source/
COPY bin/config.toml /etc/sozu/sozu.toml

RUN apk add --no-cache --virtual .build-dependencies \
  cargo \
  g++ \
  gcc \
  musl-dev \
  rust
RUN apk add --no-cache openssl-dev llvm-libunwind
WORKDIR /source/ctl
RUN cargo build --release
WORKDIR /source/bin
RUN cargo build --release



FROM alpine:edge
COPY bin/config.toml /etc/sozu/sozu.toml
RUN apk add --no-cache openssl llvm-libunwind
COPY --from=builder /source/target/release/sozu /sozu
COPY --from=builder /source/target/release/sozuctl /sozuctl

EXPOSE 80
EXPOSE 443

CMD ["/sozu", "--help"]
