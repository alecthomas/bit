FROM rust:1.93 AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY bit-derive/ bit-derive/
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release && \
    cp target/release/bit /usr/local/bin/bit

FROM debian:bookworm-slim
COPY --from=builder /usr/local/bin/bit /usr/local/bin/bit

ENTRYPOINT ["bit"]
