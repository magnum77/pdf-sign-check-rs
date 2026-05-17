FROM rust:1-bookworm AS build

WORKDIR /app

COPY Cargo.toml ./
COPY src ./src

RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential pkg-config ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && cargo build --release

FROM debian:bookworm-slim AS runtime

ENV RUST_LOG=info \
    PDF_SIGN_CHECK_BIND=0.0.0.0:3000 \
    PDF_SIGN_CHECK_DEBUG_DIR=/app/debug \
    PDF_SIGN_CHECK_LOG_DIR=/app/logs \
    PDF_SIGN_CHECK_TEMP_DIR=/tmp/pdf-sign-check-rs

WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl poppler-utils \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /app/target/release/pdf-sign-check-rs /usr/local/bin/pdf-sign-check-rs

EXPOSE 3000

CMD ["pdf-sign-check-rs"]
