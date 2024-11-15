# Stage 1: Build the application
FROM docker.io/library/rust:1.80-alpine AS builder

# Update Alpine packages and install build dependencies
RUN apk update && \
    apk add --no-cache --virtual .build-dependencies \
        musl-dev \
        libgcc \
        cmake \
        build-base \
        file \
        protobuf \
        protobuf-dev && \
    apk add --no-cache \
        llvm-libunwind

# Copy the source code into the image
COPY . /usr/src/sozu
WORKDIR /usr/src/sozu

# Build the application in release mode with a frozen lockfile
RUN cargo vendor --locked
RUN cargo build --release --frozen

# Stage 2: Create the runtime environment
FROM docker.io/library/alpine:3.20 AS bin

# Expose ports for the application
EXPOSE 80
EXPOSE 443

# Define volumes for configuration and runtime state
VOLUME /etc/sozu
VOLUME /run/sozu

# Create a directory for persistent state
RUN mkdir -p /var/lib/sozu

# Install runtime dependencies
RUN apk update && apk add --no-cache \
  llvm-libunwind \
  libgcc \
  ca-certificates

# Copy the built binary from the builder stage
COPY --from=builder /usr/src/sozu/target/release/sozu /usr/local/bin/sozu

# Copy the default configuration file
COPY os-build/config.toml /etc/sozu/config.toml

# Set the default entry point to the binary and provide default command
# to start the application with a specific config
ENTRYPOINT ["/usr/local/bin/sozu"]
CMD ["start", "-c", "/etc/sozu/config.toml"]
