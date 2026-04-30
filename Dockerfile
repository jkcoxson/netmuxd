FROM rust:1.85-bookworm AS builder
WORKDIR /work
COPY . .
RUN cargo build --release --bin netmuxd

FROM debian:bookworm-slim
RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /work/target/release/netmuxd /usr/local/bin/netmuxd
ENTRYPOINT ["/usr/local/bin/netmuxd"]
