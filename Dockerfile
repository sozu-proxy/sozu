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

COPY bin/config.toml /etc/sozu/config.toml

RUN apk update && apk add --no-cache openssl-dev \
  llvm-libunwind \
  libgcc

COPY --from=builder /source/target/release/sozu sozu

ENV SOZU_CONFIG /etc/sozu/sozu.toml

VOLUME /etc/sozu

RUN mkdir command_folder
VOLUME command_folder

EXPOSE 80
EXPOSE 443

ENTRYPOINT ["/sozu"]

CMD ["start", "-c", "/etc/sozu/config.toml"]
