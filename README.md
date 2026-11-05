# Pinson

<img src="web/img/pinson.svg" width="96" alt="Pinson logo: a chaffinch on a sky-blue tile">

*Gekkotron*

Pinson (French for chaffinch, as in *gai comme un pinson*) is a small house music server and music app. It does the following:

- Searches YouTube Music for songs, albums and playlists, and opens an album or playlist to play it, shuffle it or add it to the queue.
- Keeps a "play next" queue.
- Remembers favorite songs (one tap shuffles and plays them all).
- Stores your own playlists (create, rename, reorder, save an album as a playlist).
- Shows synced lyrics from LRCLIB, with plain lyrics from LRCLIB or YouTube Music as a fallback.
- Plays the queue through the home speakers.

Audio comes from `yt-dlp` piped into `ffmpeg`, then streams as paced PCM over TCP to `nidoo-sound`, the home audio service that drives the speakers. The server also serves a web app (a phone layout and a laptop layout) and an HTTP/WebSocket API on port 8082.

**Sendspin speakers.** [`nidoo-sound`](https://github.com/Gekkotron/Nidoo/tree/main/crates/nidoo-sound) is a Sendspin-compatible server, so Pinson plays on any [Sendspin](https://www.sendspin-audio.com/) player it finds: Nidoo satellites, other Sendspin speakers, or a browser player. Every approved player plays the same music in sync. Pinson itself does not speak Sendspin; it only feeds `nidoo-sound`.

## Prerequisites (macOS)

- Install the tools:

  ```sh
  brew install ffmpeg yt-dlp
  ```

- Rust, stable toolchain.
- A reachable `nidoo-sound`, with at least one approved Sendspin player. The default address is `192.168.1.91:4953`; set `MUSIC_NIDOO_SOUND` if yours is elsewhere.

## Run

```sh
cargo run --release
```

Open `http://<this-mac's-ip>:8082` on a phone or laptop on the same network.

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
| `--favorites-file` | `MUSIC_FAVORITES_FILE` | `favorites.json` | File where favorites are saved. Relative paths are resolved from the working directory. Created on the first change. |
| `--playlists-file` | `MUSIC_PLAYLISTS_FILE` | `playlists.json` | File where your playlists are saved. Relative paths are resolved from the working directory. Created on the first change. |
| `--nidoo-sound` | `MUSIC_NIDOO_SOUND` | `192.168.1.91:4953` | `nidoo-sound` PCM input, as `host:port`. |
| `--nidoo-control` | `MUSIC_NIDOO_CONTROL` | `http://<host of --nidoo-sound>:4954` | `nidoo-sound` HTTP API, as `http://host:port`. Used for each speaker's volume and mute. |
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

2. Check that the satellite is approved in Nidoo.
3. Look at the server log for `nidoo-sound: ... retrying`.

**The progress bar runs ahead when a song starts.**
This lasts 1 to 3 seconds while the song loads. It corrects itself.

## Themes

The app has three themes, chosen with the **Theme** button (sidebar on a laptop, Home header on a phone) and saved on each device: **Crépuscule** (dark, the default), **Matin** (light paper) and **Ciel** (sky blue). To add one, add an entry to `THEMES` in `web/js/theme.js`, a `[data-theme="<id>"]` block in `web/css/themes.css`, and the same id in the inline script of `web/index.html`.

## API

For app developers, see [docs/api.md](docs/api.md) for the HTTP routes and the WebSocket.

## License

MIT, see [LICENSE](LICENSE). The fonts in `web/fonts/` (Nunito, Fraunces, Inter, Quicksand) are under the SIL Open Font License, with their license files next to them.

Pinson is a personal project, not affiliated with or endorsed by YouTube or Google. It plays music through `yt-dlp`; using it may go against YouTube's terms of service, so use it for your own listening at home.
