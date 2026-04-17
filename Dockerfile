# syntax=docker/dockerfile:1.7

ARG RUST_VERSION=1.94

FROM rust:${RUST_VERSION}-alpine AS builder

RUN apk add --no-cache \
        musl-dev \
        pkgconfig \
        openssl-dev \
        openssl-libs-static \
        cmake \
        make \
        perl \
        build-base \
        clang \
        clang-dev

ENV CARGO_TERM_COLOR=always \
    RUSTFLAGS="-C target-feature=+crt-static" \
    PKG_CONFIG_ALL_STATIC=1

WORKDIR /app

COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/ ./crates/
RUN cargo fetch --locked

RUN cargo build \
        --release \
        --bin zatat \
        --target x86_64-unknown-linux-musl

RUN cp /app/target/x86_64-unknown-linux-musl/release/zatat /app/zatat-bin \
 && strip /app/zatat-bin

FROM gcr.io/distroless/static-debian12:nonroot AS runtime

COPY --from=builder /app/zatat-bin /usr/local/bin/zatat
COPY zatat.toml.example /etc/zatat/zatat.toml

EXPOSE 8080 9090

ENV RUST_LOG=info \
    ZATAT_CONFIG=/etc/zatat/zatat.toml

USER 65532:65532

ENTRYPOINT ["/usr/local/bin/zatat"]
CMD ["start"]
