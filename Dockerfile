# syntax=docker/dockerfile:1.7

ARG RUST_VERSION=1.94

FROM rust:${RUST_VERSION}-alpine AS builder

# buildx sets this to the platform being built (amd64/arm64); each release.yml
# runner builds its own native arch, so this only ever picks the matching
# musl target triple, it never triggers cross-compilation.
ARG TARGETARCH

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

# TARGETARCH is only populated by BuildKit/buildx (what release.yml uses).
# Fall back to the builder container's own arch so a plain, non-buildx
# `docker build` still works for a local test build.
RUN arch="${TARGETARCH:-$(uname -m | sed -e 's/x86_64/amd64/' -e 's/aarch64/arm64/')}" \
 && case "${arch}" in \
        amd64) echo x86_64-unknown-linux-musl > /tmp/rust_target ;; \
        arm64) echo aarch64-unknown-linux-musl > /tmp/rust_target ;; \
        *) echo "unsupported arch: ${arch}" >&2; exit 1 ;; \
    esac \
 && rustup target add "$(cat /tmp/rust_target)"

# rust-toolchain.toml is deliberately NOT copied: it pins `channel = "stable"`
# for dev machines, which would make rustup re-sync a toolchain inside the
# build. The base image's pinned RUST_VERSION is authoritative here.
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
RUN cargo fetch --locked

RUN cargo build \
        --release \
        --bin zatat \
        --target "$(cat /tmp/rust_target)"

RUN cp "/app/target/$(cat /tmp/rust_target)/release/zatat" /app/zatat-bin \
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
