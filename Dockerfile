
FROM alpine:edge

#COPY ["Cargo.*", "/source/"]
#COPY bin/ /source/bin/
#COPY command/ /source/command/
#COPY lib/ /source/lib/
#COPY ctl/ /source/ctl/
COPY . /source/
COPY bin/config.toml /etc/sozu/sozu.toml

RUN apk add --no-cache --virtual .build-dependencies \
  cargo \
  g++ \
  gcc \
  musl-dev \
  rust && \
  apk add --no-cache openssl-dev llvm-libunwind && \
  cd /source && cd ctl && \
  cargo build --release && \
  cd ../bin && \
  cargo build --release && \
  cp /source/target/release/sozu /bin/sozu && \
  cp /source/target/release/sozuctl /bin/sozuctl && \
  cd / && \
  apk del .build-dependencies && \
  apk del && \
  rm -rf /source && \
  rm -rf /root/.cargo

EXPOSE 80
EXPOSE 443

ENTRYPOINT ["/bin/sozu"]
