FROM clux/muslrust:1.93.1-stable AS base
WORKDIR /app
COPY --link --from=bare-repo . .
COPY --link Cargo* .

RUN --mount=type=cache,target=/usr/local/cargo/registry          \
    cargo build --release --target x86_64-unknown-linux-musl     \
    && cargo test --locked --target x86_64-unknown-linux-musl    \
    && cargo clippy --release --target x86_64-unknown-linux-musl \
        -- -D warnings

FROM base AS test
COPY --link Cargo* .
COPY --link src/ src/
RUN touch src/main.rs
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo test --locked --target x86_64-unknown-linux-musl

FROM test AS lint
RUN --mount=type=cache,target=/usr/local/cargo/registry \
   cargo clippy --release --target x86_64-unknown-linux-musl \
   -- -D warnings

FROM lint AS builder
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo build --release --target x86_64-unknown-linux-musl

FROM scratch AS runtime
COPY --link --from=builder /app/target/x86_64-unknown-linux-musl/release/signaling_server .
CMD ["/signaling_server"]
