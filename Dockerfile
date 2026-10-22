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

# dev.dozzle.icon: logo shown by Dozzle (source: docker/dozzle-icon.svg, base64, <16 KB).
LABEL org.opencontainers.image.title="media-organizer" \
      org.opencontainers.image.description="Organizes photos and videos by capture date, transactionally" \
      org.opencontainers.image.source="https://github.com/JordanAtDown/media-organizer" \
      org.opencontainers.image.licenses="MIT" \
      dev.dozzle.icon="data:image/svg+xml;base64,PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciIHZpZXdCb3g9IjAgMCA2NCA2NCI+PHJlY3Qgd2lkdGg9IjY0IiBoZWlnaHQ9IjY0IiByeD0iMTQiIGZpbGw9IiMwZjc2NmUiLz48ZyB0cmFuc2Zvcm09InJvdGF0ZSgtMTAgMjYgMjIpIj48cmVjdCB4PSIxMCIgeT0iOSIgd2lkdGg9IjI4IiBoZWlnaHQ9IjI0IiByeD0iMyIgZmlsbD0iI2ZkZTY4YSIvPjxwYXRoIGQ9Ik0yMSAxNXYxMmwxMC02eiIgZmlsbD0iIzBmNzY2ZSIvPjwvZz48ZyB0cmFuc2Zvcm09InJvdGF0ZSg5IDM5IDIwKSI+PHJlY3QgeD0iMjYiIHk9IjciIHdpZHRoPSIyNiIgaGVpZ2h0PSIyNSIgcng9IjMiIGZpbGw9IiNmMGZkZmEiLz48Y2lyY2xlIGN4PSIzNCIgY3k9IjE0IiByPSIzIiBmaWxsPSIjZjU5ZTBiIi8+PHBhdGggZD0iTTI4IDMwbDgtMTAgNSA2IDMtMyA2IDd6IiBmaWxsPSIjMGY3NjZlIi8+PC9nPjxwYXRoIGQ9Ik02IDM3YTQgNCAwIDAgMSA0LTRoNDRhNCA0IDAgMCAxIDQgNHYxN2E0IDQgMCAwIDEtNCA0SDEwYTQgNCAwIDAgMS00LTR6IiBmaWxsPSIjY2NmYmYxIi8+PHBhdGggZD0iTTE0IDQyaDI2TTE0IDQ5aDE2IiBzdHJva2U9IiMwZjc2NmUiIHN0cm9rZS13aWR0aD0iMyIgc3Ryb2tlLWxpbmVjYXA9InJvdW5kIi8+PC9zdmc+Cg=="

ENV MO_CONFIG=/etc/media-organizer/config.toml \
    MO_STATE_DIR=/var/lib/media-organizer \
    MO_LOG_FORMAT=json

VOLUME /var/lib/media-organizer
EXPOSE 8080

# The binary probes itself: no curl in the image.
HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
    CMD ["media-organizer", "healthcheck"]

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
