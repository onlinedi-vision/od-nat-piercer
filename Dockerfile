FROM alpine:3.22.1

LABEL org.opencontainers.image.source=https://github.com/rust-lang/docker-rust

RUN apk add --no-cache \
        ca-certificates \
        gcc \
	curl \
	rust \
	cargo

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs 

COPY ./src ./src
COPY ./Cargo.toml ./Cargo.toml

RUN cargo build --release

CMD ["./target/release/od_nat_piercer"]
