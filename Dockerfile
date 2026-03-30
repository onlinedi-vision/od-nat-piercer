FROM rust:1.93-alpine AS base
RUN --mount=type=cache,target=/var/cache/apk \
    apk add openssl-dev

FROM base AS bare
COPY --link --from=bare-repo . .
COPY --link Cargo* .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo build --release

FROM bare AS builder
COPY --link src Cargo* .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo build --release

# TODO: what is the entrypoint now?
#       how do you run the server now? 
ENTRYPOINT ["./target/release/signaling_server"]
