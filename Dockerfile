# Pinson in Docker, for a Linux host such as the one running nidoo-sound.
# See "Run on Linux with Docker" in README.md. compose.yml builds and runs it.

FROM rust:1.99.1-trixie AS build
# aws-lc-sys (rustls' crypto) may want cmake.
RUN apt-get update \
 && apt-get install -y --no-install-recommends cmake \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
 && cp target/release/Pinson /usr/local/bin/pinson

FROM debian:trixie-slim
# yt-dlp needs a JavaScript runtime (deno) to read YouTube's player.
# Bump YT_DLP_VERSION when YouTube breaks playback: yt-dlp fixes land fast.
ARG YT_DLP_VERSION=2026.10.26
ARG DENO_VERSION=2.9.8
RUN apt-get update \
 && apt-get install -y --no-install-recommends ffmpeg python3 python3-venv ca-certificates curl unzip \
 && python3 -m venv /opt/yt-dlp \
 && /opt/yt-dlp/bin/pip install --no-cache-dir "yt-dlp[default]==${YT_DLP_VERSION}" \
 && ln -s /opt/yt-dlp/bin/yt-dlp /usr/local/bin/yt-dlp \
 && curl -fsSL -o /tmp/deno.zip \
      "https://github.com/denoland/deno/releases/download/v${DENO_VERSION}/deno-$(uname -m)-unknown-linux-gnu.zip" \
 && unzip /tmp/deno.zip -d /usr/local/bin \
 && rm /tmp/deno.zip \
 && apt-get purge -y curl unzip \
 && apt-get autoremove -y \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/bin/pinson /usr/local/bin/pinson
# pinson.db and .env live here (./data on the host).
WORKDIR /data
EXPOSE 8082
CMD ["pinson"]
