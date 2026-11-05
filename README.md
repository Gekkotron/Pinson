# Music

*Gekkotron*

Music is a small house music server. It searches YouTube Music, keeps a "play next" queue, and plays the queue through the home speakers. Audio comes from `yt-dlp` piped into `ffmpeg`, then streams as paced PCM over TCP to `nidoo-sound`, the home audio service that drives the speakers. The server also serves a phone web page and an HTTP/WebSocket API on port 8082.

## Prerequisites (macOS)

- Install the tools:

  ```sh
  brew install ffmpeg yt-dlp
  ```

- Rust, stable toolchain.
- A reachable `nidoo-sound`. The default address is `192.168.1.91:4953`.

## Run

```sh
cargo run --release
```

Open `http://<this-mac's-ip>:8082` on a phone on the same network.

To try the page without speakers, add `--engine log`. Songs do not play, and they do not advance on their own. Skip still works.

## Configuration

Every flag has a matching `MUSIC_*` environment variable. If both are set, the flag wins. Run `cargo run --release -- --help` for the same list.

| Flag | Env var | Default | Meaning |
|---|---|---|---|
| `--bind` | `MUSIC_BIND` | `0.0.0.0` | Address to listen on. |
| `--port` | `MUSIC_PORT` | `8082` | Port for the web page and the API. |
| `--yt-dlp` | `MUSIC_YT_DLP` | `yt-dlp` | Path to the `yt-dlp` executable. |
| `--ffmpeg` | `MUSIC_FFMPEG` | `ffmpeg` | Path to the `ffmpeg` executable. |
| `--cookies-from-browser` | `MUSIC_COOKIES_FROM_BROWSER` | unset | Passed to `yt-dlp` as `--cookies-from-browser`, for example `firefox`. |
| `--cookies-file` | `MUSIC_COOKIES_FILE` | unset | Passed to `yt-dlp` as `--cookies`. Netscape cookie file. |
| `--ytm-cookie-file` | `MUSIC_YTM_COOKIE_FILE` | unset | Browser cookie file for YouTube Music search. Use it if anonymous search fails. |
| `--engine` | `MUSIC_ENGINE` | `nidoo` | Audio output: `nidoo` (speakers) or `log` (log calls only, no sound). |
| `--nidoo-sound` | `MUSIC_NIDOO_SOUND` | `192.168.1.91:4953` | `nidoo-sound` PCM input, as `host:port`. |
| `--nidoo-latency-ms` | `MUSIC_NIDOO_LATENCY_MS` | `750` | Delay from a written byte to the speakers, in ms. Covers `nidoo-sound`'s buffer and lead. |

Logging uses `RUST_LOG`. The default is `info`. For more detail, try `RUST_LOG=debug cargo run --release`.

## Troubleshooting

**Songs fail, or you see "Skipped ..." notices.**
YouTube changes often. Update `yt-dlp`:

```sh
brew upgrade yt-dlp
```

**Premium-only or age-restricted tracks fail.**
Pass your browser cookies to `yt-dlp`:

```sh
cargo run --release -- --cookies-from-browser firefox
```

You can also use a cookie file with `--cookies-file <path>`.

**Search fails (HTTP 502, "search failed").**
Retry first. If anonymous search is blocked, export a browser cookie file and start the server with `--ytm-cookie-file <path>`.

**No sound.**

1. Check that `nidoo-sound` is reachable:

   ```sh
   nc -z 192.168.1.91 4953
   ```

2. Check that the satellite is approved in Home Assistant.
3. Look at the server log for `nidoo-sound: ... retrying`.

**The progress bar runs ahead when a song starts.**
This lasts 1 to 3 seconds while the song loads. It corrects itself.

## API

For app developers, see [docs/api.md](docs/api.md) for the HTTP routes and the WebSocket.
