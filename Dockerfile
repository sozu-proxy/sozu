FROM alpine:edge as builder

COPY . /source/
COPY bin/config.toml /etc/sozu/sozu.toml

RUN apk add --no-cache --virtual .build-dependencies \
  cargo \
  build-base \
  file \
  libgcc \
  musl-dev \
  rust
RUN apk add --no-cache openssl-dev llvm-libunwind pkgconfig
ENV SOZU_CONFIG /etc/sozu/sozu.toml
ENV SOZU_PID_FILE_PATH /run/sozu/sozu.pid
WORKDIR /source/ctl
RUN cargo build --release
WORKDIR /source/bin
RUN cargo build --release



FROM alpine:edge
COPY bin/config.toml /etc/sozu/sozu.toml
RUN apk add --no-cache openssl llvm-libunwind libgcc
COPY --from=builder /source/target/release/sozu /sozu
COPY --from=builder /source/target/release/sozuctl /sozuctl

EXPOSE 80
EXPOSE 443

CMD ["/sozu", "--help"]
