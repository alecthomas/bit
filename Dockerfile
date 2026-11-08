# Keep the version in step with rust-toolchain.toml, or rustup downloads the
# pinned toolchain on every build. Renovate bumps both in one PR; see
# renovate.json5. The Debian release must match the runtime
# stage below, because the binary links against the builder's glibc.
FROM rust:1.99.1-bookworm AS builder
WORKDIR /src
COPY rust-toolchain.toml Cargo.toml Cargo.lock README.md ./
COPY src/ src/
COPY bit-derive/ bit-derive/
COPY bit-s3/ bit-s3/
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release && \
    cp target/release/bit /usr/local/bin/bit

FROM debian:bookworm-slim
COPY --from=builder /usr/local/bin/bit /usr/local/bin/bit

ENTRYPOINT ["bit"]
