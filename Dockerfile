FROM rust:1.93-alpine AS base
RUN --mount=type=cache,target=/var/cache/apk \
    apk add openssl-dev

FROM base AS bare
COPY --link --from=bare-repo . .
COPY --link Cargo* .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo build --release

FROM bare AS builder
COPY --link Cargo* .
COPY --link src/ src/
RUN touch src/main.rs
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo build --release

ENTRYPOINT ["./target/release/signaling_server"]
