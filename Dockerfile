FROM rust:slim-trixie AS builder

LABEL maintainer=kickhead13<ana.alexandru.gabriel@proton.me>

COPY . .
RUN cargo build --release

ENTRYPOINT ["./target/release/signaling_server"]
