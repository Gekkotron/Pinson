# Pinson

<img src="web/img/pinson.svg" width="96" alt="Pinson logo: a chaffinch on a sky-blue tile">

*Gekkotron*

Pinson (French for chaffinch, as in *gai comme un pinson*) is a small house music server and music app. It does the following:

- Searches YouTube Music for songs, albums and playlists (suggests searches as you type), and opens an album or playlist to play it, shuffle it or add it to the queue.
- Keeps a "play next" queue.
- Autoplay: when the queue ends, keeps playing songs like the last queued one (from the YouTube Music radio), and lists them under Up next.
- Mixes for you on Home: the YouTube Music radio of songs you played recently or favorited.
- Remembers favorite songs (one tap shuffles and plays them all).
- Keeps every song you play; find one again in Recently played (search it, grouped by day).
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

**TV.** Open `http://<this-mac's-ip>:8082/lyrics` to go straight to Now playing with the lyrics shown. This big-screen view has a smaller cover, which leaves more room for the lyrics. Save it as a bookmark or shortcut in the TV browser.

Your data lives in `pinson.db`. An existing `favorites.json` or `playlists.json` is imported on the first start.

**Autoplay.** Suggestions come from the YouTube Music radio of the last song in the queue (the playing song if nothing is queued). Songs already queued or played recently are left out. When the queue ends, the first suggestion plays. The Autoplay switch in Up next turns this on or off; the choice is stored in `pinson.db`. It is on by default.

**Mixes.** Home shows up to 6 mixes. Each one is the YouTube Music radio of a seed song, cut to 25 songs. Seeds are the songs you played recently, with different artists first, and your favorites fill in when there is not enough history. A radio that fails or is empty is skipped. Radios are kept for 3 hours.

To try the page without speakers, add `--engine log`. Songs do not play, and they do not advance on their own. Skip still works.

## Configuration

Every flag has a matching `MUSIC_*` environment variable. If both are set, the flag wins. Run `cargo run --release -- --help` for the same list.

You can also put these variables in a `.env` file in the folder you start Pinson from. Copy `.env.example` to get started. `.env` is git-ignored, so the MQTT password stays out of the repo. A variable already set in the shell wins over `.env`, and a flag wins over both.

| Flag | Env var | Default | Meaning |
|---|---|---|---|
| `--bind` | `MUSIC_BIND` | `0.0.0.0` | Address to listen on. |
| `--port` | `MUSIC_PORT` | `8082` | Port for the web page and the API. |
| `--yt-dlp` | `MUSIC_YT_DLP` | `yt-dlp` | Path to the `yt-dlp` executable. |
| `--ffmpeg` | `MUSIC_FFMPEG` | `ffmpeg` | Path to the `ffmpeg` executable. |
| `--cookies-from-browser` | `MUSIC_COOKIES_FROM_BROWSER` | unset | Passed to `yt-dlp` as `--cookies-from-browser`, for example `firefox`. |
| `--cookies-file` | `MUSIC_COOKIES_FILE` | unset | Passed to `yt-dlp` as `--cookies`. Netscape cookie file. |
| `--yt-clients` | `MUSIC_YT_CLIENTS` | `visionos` | YouTube clients `yt-dlp` asks for the stream (its `player_client`), comma-separated. `visionos` starts songs about 5 s sooner: the default clients get a pre-roll ad, and `yt-dlp` waits for it. A song that fails with these is retried with `yt-dlp`'s default. Set it empty to always use `yt-dlp`'s default. |
| `--ytm-cookie-file` | `MUSIC_YTM_COOKIE_FILE` | unset | Browser cookie file for YouTube Music search. Use it if anonymous search fails. |
| `--engine` | `MUSIC_ENGINE` | `nidoo` | Audio output: `nidoo` (speakers) or `log` (log calls only, no sound). |
| `--db` | `MUSIC_DB` | `pinson.db` | SQLite file holding favorites, playlists, play history and settings. Relative paths are resolved from the working directory. |
| `--favorites-file` | `MUSIC_FAVORITES_FILE` | `favorites.json` | Old favorites file. Imported into the database once, then renamed to `favorites.json.imported`. |
| `--playlists-file` | `MUSIC_PLAYLISTS_FILE` | `playlists.json` | Old playlists file. Imported into the database once, then renamed to `playlists.json.imported`. |
| `--nidoo-sound` | `MUSIC_NIDOO_SOUND` | `192.168.1.91:4953` | `nidoo-sound` PCM input, as `host:port`. |
| `--nidoo-control` | `MUSIC_NIDOO_CONTROL` | `http://<host of --nidoo-sound>:4954` | `nidoo-sound` HTTP API, as `http://host:port`. Used for each speaker's volume and mute. |
| `--nidoo-latency-ms` | `MUSIC_NIDOO_LATENCY_MS` | `750` | Delay from a written byte to the speakers, in ms. Covers `nidoo-sound`'s buffer and lead. |
| `--mqtt-broker` | `MUSIC_MQTT_BROKER` | unset: MQTT off | MQTT broker, as `host[:port]`. The port defaults to `1883`. |
| `--mqtt-user` | `MUSIC_MQTT_USER` | unset | MQTT user name, if the broker needs one. |
| `--mqtt-password` | `MUSIC_MQTT_PASSWORD` | unset | MQTT password, if the broker needs one. Optional. Never logged. Prefer the env var: a flag shows in `ps`. Needs `--mqtt-user`. |
| `--mqtt-discovery-prefix` | `MUSIC_MQTT_DISCOVERY_PREFIX` | `homeassistant` | Home Assistant MQTT discovery prefix. |
| `--mqtt-root` | `MUSIC_MQTT_ROOT` | `pinson` | Topic root for Pinson's state, commands and status. |

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

## Nidoo (MQTT)

Pinson can be a media player for the Nidoo home hub: start it with `--mqtt-broker <host[:port]>` and the hub shows what is playing, tinted with the cover colour, and can play, pause, skip and "play <something>" by voice. It uses Home Assistant-style MQTT discovery. Without a broker, MQTT stays off. Topics, payloads and commands are in [docs/api.md](docs/api.md#mqtt).

## Tests

```sh
cargo test
TZ=Europe/Paris node --test tests/web/   # web logic (Recently played day grouping)
```

## API

For app developers, see [docs/api.md](docs/api.md) for the HTTP routes, the WebSocket and MQTT.

## License

MIT, see [LICENSE](LICENSE). The fonts in `web/fonts/` (Nunito, Fraunces, Inter, Quicksand) are under the SIL Open Font License, with their license files next to them.

Pinson is a personal project, not affiliated with or endorsed by YouTube or Google. It plays music through `yt-dlp`; using it may go against YouTube's terms of service, so use it for your own listening at home.
