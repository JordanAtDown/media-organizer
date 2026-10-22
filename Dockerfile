# syntax=docker/dockerfile:1

# ── build ─────────────────────────────────────────────────────────────────────
# Built natively on an arm64 runner (no QEMU, no cross toolchain): the target is
# the Raspberry Pi 5 only. The release profile favours speed over binary size.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# Cache mounts are not part of the image, hence the explicit `install` into /out.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
 && install -D target/release/media-organizer /out/media-organizer

# ── runtime ───────────────────────────────────────────────────────────────────
# glibc (not musl: a better allocator under 8 worker threads) and not distroless
# (PUID/PGID needs a shell). `setpriv` ships in util-linux, already in -slim.
FROM debian:bookworm-slim

# tzdata is functional, not cosmetic: without it `Local` silently becomes UTC and
# files land in the wrong month directory.
RUN apt-get update \
 && apt-get install -y --no-install-recommends tzdata \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd -g 1000 app \
 && useradd -u 1000 -g 1000 -M -s /usr/sbin/nologin app \
 && mkdir -p /var/lib/media-organizer /etc/media-organizer \
 && chown app:app /var/lib/media-organizer

COPY --from=build /out/media-organizer /usr/local/bin/media-organizer
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh

LABEL org.opencontainers.image.title="media-organizer" \
      org.opencontainers.image.description="Organizes photos and videos by capture date, transactionally" \
      org.opencontainers.image.source="https://github.com/JordanAtDown/media-organizer" \
      org.opencontainers.image.licenses="MIT"

ENV MO_CONFIG=/etc/media-organizer/config.toml \
    MO_STATE_DIR=/var/lib/media-organizer \
    MO_LOG_FORMAT=json

VOLUME /var/lib/media-organizer
EXPOSE 8080

# The binary probes itself: no curl in the image.
HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
    CMD ["media-organizer", "healthcheck"]

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
