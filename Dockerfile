# =============================================================================
# hc-sonos — HomeCore Sonos Plugin
# Alpine Linux — minimal, static-friendly runtime
# =============================================================================
#
# Build:
#   docker build -t hc-sonos:latest .
#
# Run:
#   docker run -d \
#     --network host \
#     -v ./config/config.toml:/opt/hc-sonos/config/config.toml:ro \
#     -v hc-sonos-logs:/opt/hc-sonos/logs \
#     hc-sonos:latest
#
# Note: --network host is recommended for UPnP device discovery.
#
# Volumes:
#   /opt/hc-sonos/config   config.toml (credentials)
#   /opt/hc-sonos/logs     rolling log files
# =============================================================================

# -----------------------------------------------------------------------------
# Stage 1 — Build
# -----------------------------------------------------------------------------
FROM rust:alpine AS builder

RUN apk upgrade --no-cache && apk add --no-cache musl-dev openssl-dev pkgconfig

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY src/ ./src/

RUN cargo build --release --bin hc-sonos

# -----------------------------------------------------------------------------
# Stage 2 — Runtime
# -----------------------------------------------------------------------------
FROM alpine:3

# `apk upgrade` first pulls CVE patches for packages baked into the
# alpine:3 base since the upstream image was last rebuilt. Defense
# in depth — without this, `apk add --no-cache` only refreshes the
# named packages, leaving busybox/musl/etc. on the base's frozen
# versions.
RUN apk upgrade --no-cache && \
    apk add --no-cache \
        ca-certificates \
        libssl3 \
        tzdata

RUN adduser -D -h /opt/hc-sonos hcsonos

COPY --from=builder /build/target/release/hc-sonos /usr/local/bin/hc-sonos
RUN chmod 755 /usr/local/bin/hc-sonos

RUN mkdir -p /opt/hc-sonos/config /opt/hc-sonos/logs

COPY config/config.toml.example /opt/hc-sonos/config/config.toml.example

RUN chown -R hcsonos:hcsonos /opt/hc-sonos

USER hcsonos
WORKDIR /opt/hc-sonos

VOLUME ["/opt/hc-sonos/config", "/opt/hc-sonos/logs"]

ENV RUST_LOG=info

ENTRYPOINT ["hc-sonos"]
