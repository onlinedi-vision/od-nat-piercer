FROM rust:1.90-trixie AS builder

LABEL maintainer=kickhead13<ana.alexandru.gabriel@proton.me>

COPY . .
RUN cargo build --release

ENTRYPOINT ["./target/release/signaling_server"]
