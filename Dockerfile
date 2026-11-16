# syntax=docker/dockerfile:1.7
# Debian slim image (~136 MB). The prebuilt ONNX Runtime 1.31 is statically
# linked into subsyncer, and ffmpeg/ffprobe are a minimal static build
# (docker/build-ffmpeg.sh) instead of Debian's ffmpeg package, which would add
# ~450 MB of video, GPU and X11 libraries subsyncer never uses.
# ONNX Runtime needs glibc >= 2.38 (trixie or newer; bookworm can't link it).
# Alpine was tried: 209 MB and ~1.6x slower VAD (musl, distro ONNX Runtime).

ARG RUST_VERSION=1.99
ARG DEBIAN_RELEASE=trixie
ARG FFMPEG_VERSION=8.1.3
ARG FFMPEG_SHA256=7138d28c96d9d3e3af4ee3d8cad72741f8ffb40da90c1112235dea3ecd3178a3

FROM debian:${DEBIAN_RELEASE}-slim AS ffmpeg
ARG FFMPEG_VERSION
ARG FFMPEG_SHA256
RUN apt-get update \
 && apt-get install -y --no-install-recommends build-essential pkg-config zlib1g-dev xz-utils curl ca-certificates \
 && if [ "$(dpkg --print-architecture)" = amd64 ]; then apt-get install -y --no-install-recommends nasm; fi \
 && rm -rf /var/lib/apt/lists/*
RUN curl -fsSLo /tmp/ffmpeg.tar.xz "https://ffmpeg.org/releases/ffmpeg-${FFMPEG_VERSION}.tar.xz" \
 && echo "${FFMPEG_SHA256}  /tmp/ffmpeg.tar.xz" | sha256sum -c - \
 && mkdir /tmp/ffmpeg && tar -xJf /tmp/ffmpeg.tar.xz -C /tmp/ffmpeg --strip-components=1
COPY docker/build-ffmpeg.sh /usr/local/bin/build-ffmpeg.sh
RUN build-ffmpeg.sh /tmp/ffmpeg /opt/ffmpeg

FROM rust:${RUST_VERSION}-${DEBIAN_RELEASE} AS chef
RUN cargo install cargo-chef --locked --version 0.1.78
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json -p subsyncer
COPY . .
RUN cargo build --release --locked -p subsyncer \
 && cp target/release/subsyncer /usr/local/bin/subsyncer \
 && /usr/local/bin/subsyncer --version

FROM debian:${DEBIAN_RELEASE}-slim AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends tini gosu \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd --gid 1000 subsyncer \
 && useradd --uid 1000 --gid 1000 --no-create-home --home-dir /data --shell /usr/sbin/nologin subsyncer \
 && mkdir -p /data && chown subsyncer:subsyncer /data
COPY --from=ffmpeg /opt/ffmpeg/bin/ffmpeg /opt/ffmpeg/bin/ffprobe /usr/local/bin/
COPY --from=builder /usr/local/bin/subsyncer /usr/local/bin/subsyncer
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
ENV SUBSYNCER_DATA_DIR=/data \
    SUBSYNCER_LISTEN=0.0.0.0:8080 \
    PUID=1000 \
    PGID=1000
VOLUME ["/data"]
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 CMD ["subsyncer", "health"]
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/entrypoint.sh"]
CMD ["serve"]

LABEL org.opencontainers.image.title="subsyncer" \
      org.opencontainers.image.description="Sync subtitles to video (Bazarr post-processing replacement for ffsubsync)" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"
