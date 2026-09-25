# syntax=docker/dockerfile:1

FROM rust:1.98.1-alpine AS builder

RUN apk add --no-cache build-base perl

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
    && echo 'fn main() {}' > src/main.rs \
    && : > src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src

# Touch so cargo rebuilds after the stub.
COPY src ./src
COPY templates ./templates
RUN find src -type f -exec touch {} + \
    && cargo build --release --locked \
    && strip target/release/reposilo

FROM alpine:3.24 AS runtime

# git at runtime; the symlink is git's askpass target.
RUN apk add --no-cache git ca-certificates \
    && ln -sf /bin/true /usr/bin/true \
    && addgroup -g 1000 reposilo \
    && adduser -S -u 1000 -G reposilo -h /home/reposilo reposilo \
    && mkdir -p /config /archive \
    && chown reposilo:reposilo /config /archive

COPY --from=builder /build/target/release/reposilo /usr/local/bin/reposilo

USER reposilo
WORKDIR /home/reposilo
ENV HOME=/home/reposilo \
    REPOSILO_BIND=0.0.0.0:8765

EXPOSE 8765
VOLUME ["/config", "/archive"]

HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
    CMD wget -q -O /dev/null "http://127.0.0.1:${REPOSILO_BIND##*:}/healthz" || exit 1

ENTRYPOINT ["reposilo"]
CMD ["--config", "/config/config.toml", "serve"]
