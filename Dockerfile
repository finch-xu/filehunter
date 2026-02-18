FROM rust:1.93-slim AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock* ./
COPY src/ src/

RUN cargo build --release

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN groupadd -r filehunter && useradd -r -g filehunter -s /usr/sbin/nologin filehunter

COPY --from=builder /build/target/release/filehunter /usr/local/bin/filehunter
COPY config.toml /etc/filehunter/config.toml

USER filehunter

EXPOSE 8080

ENTRYPOINT ["filehunter"]
CMD ["--config", "/etc/filehunter/config.toml"]
