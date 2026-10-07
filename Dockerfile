FROM rust:1.90-bookworm AS builder

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
  && printf 'fn main() {}\n' > src/main.rs \
  && cargo build --locked --release \
  && rm -rf src

COPY src ./src
RUN touch src/main.rs \
  && cargo build --locked --release

FROM debian:bookworm-slim

RUN apt-get update \
  && apt-get install -y \
  ca-certificates \
  ffmpeg \
  --no-install-recommends \
  && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/cueback /usr/local/bin/cueback

USER 1000:100

ENTRYPOINT ["cueback"]
CMD ["--config", "/etc/cueback.toml"]
