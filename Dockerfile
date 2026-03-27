FROM rust:1.93 AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN cargo build --release

FROM debian:bookworm-slim
COPY --from=builder /src/target/release/bit /usr/local/bin/bit
ENTRYPOINT ["bit"]
