# House Music Server API

*Gekkotron · 2026-11-04*

The server searches YouTube Music (songs, albums and playlists), keeps a "play next" queue, stores favorites and your own playlists, finds synced lyrics, and plays the queue through the home speakers. Your app calls HTTP routes to search, queue, and control playback. It reads live state from one WebSocket. A home hub can also control it over [MQTT](#mqtt).

## Basics

| Item | Value |
|---|---|
| Base URL | `http://<host>:8082` (port 8082 by default, configurable at startup) |
| Authentication | None |
| Bodies | JSON, keys in camelCase |
| Times | `positionMs` in milliseconds; `durationSecs` in seconds |
| Errors | Non-2xx status with body `{"error": "<message>"}` |

Errors: show the `error` text to the user, especially for 502.

## Data shapes

### Song

```json
{
  "videoId": "FGBhQbmPwH8",
  "title": "One More Time",
  "artists": "Daft Punk",
  "album": "Discovery",
  "durationSecs": 321,
  "artworkUrl": "https://example.com/art-544.jpg"
}
```

| Field | Type | Notes |
|---|---|---|
| `videoId` | string | Required. Never blank. |
| `title` | string | Required. |
| `artists` | string | Required. Artist names as one display string. |
| `album` | string or `null` | |
| `durationSecs` | integer or `null` | |
| `artworkUrl` | string or `null` | |

Nullable fields are sent as `null`, never omitted. When you POST a song, send the object exactly as search returned it.

### QueueEntry

`{"id": 7, "song": Song}`

| Field | Type | Notes |
|---|---|---|
| `id` | integer | Assigned by the server. Unique per queueing: the same song queued twice gets two ids. Use it for `DELETE`. |
| `song` | Song | |

### StateView

```json
{
  "current": { "id": 7, "song": { "videoId": "FGBhQbmPwH8", "title": "One More Time", "artists": "Daft Punk", "album": "Discovery", "durationSecs": 321, "artworkUrl": null } },
  "status": "playing",
  "positionMs": 42500,
  "positionRunning": true,
  "upcoming": [
    { "id": 8, "song": { "videoId": "abc123", "title": "Around the World", "artists": "Daft Punk", "album": null, "durationSecs": null, "artworkUrl": null } }
  ],
  "autoplay": { "enabled": true, "basedOn": { "videoId": "abc123", "title": "Around the World", "artists": "Daft Punk", "album": null, "durationSecs": null, "artworkUrl": null }, "songs": [] },
  "version": 12
}
```

| Field | Type | Notes |
|---|---|---|
| `current` | QueueEntry or `null` | The song playing or paused. `null` when idle. |
| `status` | string | `"playing"`, `"paused"`, or `"stopped"`. `stopped` means idle. |
| `positionMs` | integer | See [Position](#position). `0` when idle. |
| `positionRunning` | boolean | `true` only while the song is playing and `positionMs` is advancing. See [Position](#position). |
| `upcoming` | QueueEntry[] | Songs after `current`, in play order. |
| `autoplay` | AutoplayView | The autoplay switch and its suggestions. See [AutoplayView](#autoplayview). |
| `version` | integer | Increases on every change. |

Idle: `current` is `null`, `status` is `"stopped"`, `positionMs` is `0`, `positionRunning` is `false`, `upcoming` is `[]`, and `autoplay` is `{"enabled": <switch>, "basedOn": null, "songs": []}`.

### AutoplayView

```json
{
  "enabled": true,
  "basedOn": { "videoId": "abc123", "title": "Around the World", "artists": "Daft Punk", "album": null, "durationSecs": null, "artworkUrl": null },
  "songs": [Song, ...]
}
```

| Field | Type | Notes |
|---|---|---|
| `enabled` | boolean | The autoplay switch. Always shows the switch, even when nothing is playing. On by default. |
| `basedOn` | Song or `null` | The song the listed suggestions came from, normally the last song in the queue. It can lag behind for a moment while a new radio loads, and it can be set while `songs` is empty. |
| `songs` | Song[] | Up to 10 suggested songs, best first. Songs in the queue and recently played songs are left out. Empty only when autoplay is off, nothing is playing, the last fetch failed, or before the first load. |

When the queue runs out (the last song ends, is skipped, or fails) and autoplay is on, the server plays the first suggestion by itself.

### PlaylistSummary

```json
{
  "id": "k3Jd9sLq0Z",
  "name": "Party",
  "songCount": 12,
  "artworkUrls": ["https://example.com/art-1.jpg", "https://example.com/art-2.jpg"],
  "updatedAt": "2026-11-05T18:30:00.123456789+00:00"
}
```

| Field | Type | Notes |
|---|---|---|
| `id` | string | 10 letters and digits. Use it in the playlist routes. |
| `name` | string | 1 to 100 characters. |
| `songCount` | integer | Songs in the playlist. |
| `artworkUrls` | array of string | Up to 4 different `artworkUrl` values (no repeats, song order) from the first songs that have one. Use them for a cover collage. Empty if none. |
| `updatedAt` | string | RFC 3339 time of the last change. Lists are sorted by it, newest first. |

### Playlist

```json
{
  "id": "k3Jd9sLq0Z",
  "name": "Party",
  "songs": [Song, ...],
  "createdAt": "2026-11-05T18:00:00.482913000+00:00",
  "updatedAt": "2026-11-05T18:30:00.123456789+00:00"
}
```

`songs` are in playlist order, and a `videoId` appears at most once.

### CollectionSummary

An album, a YouTube playlist or a mix, as a search result or a Home tile.

```json
{
  "kind": "album",
  "id": "MPREb_7ltM34kr0mH",
  "title": "Discovery",
  "subtitle": "Daft Punk \u00b7 2001",
  "artworkUrl": "https://example.com/cover-544.jpg"
}
```

| Field | Type | Notes |
|---|---|---|
| `kind` | string | `"album"`, `"playlist"` or `"mix"`. |
| `id` | string | Pass it unchanged to `GET /api/albums/{id}` or `GET /api/ytplaylists/{id}` (or `GET /api/mixes/{id}` for mixes). Album ids look like `MPREb_...`. Playlist ids look like `PL...` (never with the `VL` browse prefix). |
| `title` | string | Album or playlist name. Mixes: the seed song's title and "radio". |
| `subtitle` | string or null | Albums: artist and year. Playlists: the author and the track count or view count, when YouTube gives it. Mixes: the first three artists (each name as YouTube gives it), then "…" when there are more. |
| `artworkUrl` | string or null | Cover image. |

### Collection

A CollectionSummary with its songs, in one flat object.

```json
{
  "kind": "album",
  "id": "MPREb_7ltM34kr0mH",
  "title": "Discovery",
  "subtitle": "Daft Punk \u00b7 2001",
  "artworkUrl": "https://example.com/cover-544.jpg",
  "songs": [Song, ...]
}
```

`songs` are in track order. Tracks YouTube cannot play are left out. Songs without their own cover take the album cover. A YouTube playlist holds at most 200 songs; the server reads as many pages of the playlist as needed and cuts it at 200. Every song has a non-empty `videoId`, so you can send the list to `POST /api/queue/play` or `POST /api/queue/add`.

## HTTP routes

### GET /api/search

Search YouTube Music for songs, albums or playlists.

Query:

- `q` (required, not blank, URL-encoded).
- `kind` (optional): `songs` (default), `albums` or `playlists`.

Request:

```http
GET /api/search?q=daft%20punk
GET /api/search?q=daft%20punk&kind=albums
```

Response `200` with `kind=songs` or no `kind` (at most 20 songs):

```json
[
  { "videoId": "FGBhQbmPwH8", "title": "One More Time", "artists": "Daft Punk", "album": "Discovery", "durationSecs": 321, "artworkUrl": "https://example.com/art-544.jpg" }
]
```

Response `200` with `kind=albums` or `kind=playlists` (at most 20 CollectionSummary):

```json
[
  { "kind": "album", "id": "MPREb_7ltM34kr0mH", "title": "Discovery", "subtitle": "Daft Punk \u00b7 2001", "artworkUrl": "https://example.com/cover-544.jpg" }
]
```

| Status | Body | Cause |
|---|---|---|
| 400 | `{"error": "q must not be blank"}` | `q` missing, empty, or only spaces. |
| 400 | `{"error": "unknown kind \"artists\": use songs, albums or playlists"}` | `kind` is not one of the three values. |
| 502 | `{"error": "search failed: <upstream message>"}` | YouTube Music failed. Show the message to the user. |

### GET /api/search/suggestions

Autocomplete suggestions for the search box, from YouTube Music.

Query:

- `q` (required, not blank, URL-encoded). `kind` is ignored.

Request:

```http
GET /api/search/suggestions?q=daft%20pu
```

Response `200` (0 to 5 Suggestion, in YouTube Music's order):

```json
[
  { "text": "daft punk", "runs": [ { "text": "daft pu", "bold": true }, { "text": "nk", "bold": false } ] }
]
```

Suggestion fields:

| Field | Type | Meaning |
|---|---|---|
| `text` | string | The whole suggestion, as YouTube Music sent it. Plain text: do not parse it as HTML. |
| `runs` | array | The same text in pieces. `bold: true` marks the part YouTube Music shows in bold (in practice, the text the user typed, e.g. **daft pu**nk). |

| Status | Body | Cause |
|---|---|---|
| 400 | `{"error": "q must not be blank"}` | `q` missing, empty, or only spaces. |
| 502 | `{"error": "suggestions failed: <upstream message>"}` | YouTube Music failed. |
| 504 | `{"error": "suggestions timed out"}` | YouTube Music took more than 3 seconds. |

Debounce the calls while the user types, drop any answer that is not for the latest text, and stay silent on errors.

### GET /api/albums/{id}

Open an album from search. `id` is the CollectionSummary `id`.

Request:

```http
GET /api/albums/MPREb_7ltM34kr0mH
```

Response `200`: a Collection with `kind` `"album"`.

| Status | Body | Cause |
|---|---|---|
| 404 | `{"error": "no album <id>"}` | YouTube Music does not know this album. |
| 502 | `{"error": "album failed: <upstream message>"}` | YouTube Music failed. |

### GET /api/ytplaylists/{id}

Open a YouTube playlist from search. It is not one of the user's own playlists (`/api/playlists`). `id` is the CollectionSummary `id`. The server accepts it with or without the `VL` prefix (`VLPLabc` and `PLabc` open the same playlist), and always answers with the bare form (`PL...`).

Request:

```http
GET /api/ytplaylists/PLPqYb9Y6tgMXJIe_nOcgXl-Zq-C47EUDL
```

Response `200`: a Collection with `kind` `"playlist"`, at most 200 songs.

| Status | Body | Cause |
|---|---|---|
| 404 | `{"error": "no playlist <id>"}` | YouTube Music does not know this playlist. |
| 502 | `{"error": "playlist failed: <upstream message>"}` | YouTube Music failed. |

### GET /api/history

Every song that actually played, newest first. Each play is its own entry, so a song played twice appears twice. Pages are cut by `id`: to get the next page, pass the `id` of the last play you got as `before`. A page shorter than `limit` is the last one. Plays are never deleted.

Query:

- `q` (optional): keep only plays whose title or artists contain this text, ignoring case. `%` and `_` match literally. Blank means no filter.
- `before` (optional): only plays with an `id` lower than this one.
- `limit` (optional): page size, default `50`, clamped to 1–200.

Request:

```http
GET /api/history?q=daft&limit=2
```

Response `200`: a list of plays. `playedAt` is UTC (RFC 3339); group by day in the viewer's own time zone.

```json
[
  {
    "id": 412,
    "playedAt": "2026-11-06T20:40:12.345678+00:00",
    "song": { "videoId": "5NV6Rdv1a3I", "title": "Get Lucky", "artists": "Daft Punk", "album": "Random Access Memories", "durationSecs": 248, "artworkUrl": null }
  },
  {
    "id": 398,
    "playedAt": "2026-11-05T21:10:03.120000+00:00",
    "song": { "videoId": "FGBhQbmPwH8", "title": "One More Time", "artists": "Daft Punk", "album": "Discovery", "durationSecs": 320, "artworkUrl": null }
  }
]
```

| Status | Body | Cause |
|---|---|---|
| 400 | `{"error": "<message>"}` | `before` or `limit` is not a whole number. |
| 500 | `{"error": "history failed: <message>"}` | The history could not be read. |

### GET /api/mixes

The mixes for Home: the YouTube Music radio of songs played recently or favorited. Up to 6, in order. Recent plays come first, with different artists before more songs by the same artist, and favorites fill in. A radio that fails, times out or is empty is left out, so the list may be shorter or empty.

Request:

```http
GET /api/mixes
```

Response `200`: a list of Collections with `kind` `"mix"`.

```json
[
  {
    "kind": "mix",
    "id": "dQw4w9WgXcQ",
    "title": "Midnight City radio",
    "subtitle": "M83, Phoenix, MGMT\u2026",
    "artworkUrl": "https://example.com/cover-544.jpg",
    "songs": [
      {
        "videoId": "dQw4w9WgXcQ",
        "title": "Midnight City",
        "artists": "M83",
        "album": "Hurry Up, We're Dreaming",
        "durationSecs": 243,
        "artworkUrl": "https://example.com/cover-544.jpg"
      }
    ]
  }
]
```

A mix holds at most 25 songs, without repeats. Its `id` is always the `videoId` you asked for (or the seed song's, in the list).

| Status | Body | Cause |
|---|---|---|
| 500 | `{"error": "mixes failed: <message>"}` | The play history could not be read. |

### GET /api/mixes/{videoId}

Open one mix. `videoId` is the `id` of a mix from `GET /api/mixes`. Any song id works: the mix is built from its radio.

Request:

```http
GET /api/mixes/dQw4w9WgXcQ
```

Response `200`: a Collection with `kind` `"mix"`, at most 25 songs.

| Status | Body | Cause |
|---|---|---|
| 404 | `{"error": "no mix <id>"}` | The radio is empty. |
| 502 | `{"error": "mix failed: <upstream message>"}` | YouTube Music failed or timed out. (A failed history read is only logged: the mix still opens.) |

### GET /api/artwork

Fetches a cover image through the server. Use it when the client must read the pixels (for example colour extraction on a canvas, which cross-origin images block). To simply display a cover, load `artworkUrl` directly.

Query: `url` (required, URL-encoded). Only `https://lh3.googleusercontent.com`, `https://yt3.googleusercontent.com`, `https://yt3.ggpht.com` and `https://i.ytimg.com` images are served (`http` is accepted for the same hosts), at most 5 MB.

Request:

```http
GET /api/artwork?url=https%3A%2F%2Flh3.googleusercontent.com%2Fabc%3Dw1200-h1200
```

Response `200`: the image bytes, with the upstream `Content-Type` (`image/*`) and `Cache-Control: public, max-age=86400`.

| Status | Body | Cause |
|---|---|---|
| 400 | `{"error": "url is not an allowed image host"}` | `url` missing, not a URL, or not on an allowed host. |
| 502 | `{"error": "artwork fetch failed: <message>"}` | The image host failed, or answered with an error status. |
| 502 | `{"error": "artwork is not an image"}` | The response is not `image/*`. |
| 502 | `{"error": "artwork is larger than 5 MB"}` | The image is too big. |

### GET /api/artwork/color

The dominant colour of a cover, computed by the server so every client shows the same tint. Samples the cover at 120x120 (for lh3/yt3 cover URLs; other URLs are fetched as they are) and caches the result per URL (including "no colour").

Query: `url` (required, URL-encoded): the song's `artworkUrl`, on the same hosts as [`/api/artwork`](#get-apiartwork).

Request:

```http
GET /api/artwork/color?url=https%3A%2F%2Flh3.googleusercontent.com%2Fabc%3Dw544-h544
```

Response `200`, with `Cache-Control: public, max-age=86400` (or `no-store` when the cover could not be fetched, so a client retries):

```json
{ "color": "#1f3a5c", "glow": "#285ac8" }
```

| Field | Type | Notes |
|---|---|---|
| `color` | string or `null` | `#rrggbb`. A background-safe tint: dark and not too loud, so white text stays readable. |
| `glow` | string or `null` | `#rrggbb`. The raw dominant colour. The web app draws it at about 45 % opacity. |

Both fields are `null` when the cover cannot be coloured: the image host failed, the response is not an image, it is larger than 5 MB, or it does not decode. This is not an error. Only an upstream failure (answered with `no-store`) is retried on the next request; the other cases are remembered.

| Status | Body | Cause |
|---|---|---|
| 400 | `{"error": "url is not an allowed image host"}` | `url` missing, not a URL, or not on an allowed host. |

### GET /api/lyrics

Lyrics for one song, synced to the time when LRCLIB has them. Send the song's own fields; the server never looks the song up by id alone.

Query: `videoId`, `title` and `artists` (required, URL-encoded); `album` and `durationSecs` (optional, but `durationSecs` makes the match much better).

Request:

```http
GET /api/lyrics?videoId=FGBhQbmPwH8&title=One%20More%20Time&artists=Daft%20Punk&album=Discovery&durationSecs=320
```

Response `200`:

```json
{
  "videoId": "FGBhQbmPwH8",
  "source": "lrclib",
  "synced": true,
  "lines": [
    { "timeMs": 12340, "text": "One more time" },
    { "timeMs": 15900, "text": "" }
  ],
  "plain": "One more time\n...",
  "instrumental": false
}
```

| Field | Type | Notes |
|---|---|---|
| `videoId` | string | Echo of the request. |
| `source` | string | `"lrclib"` or `"youtube"`. |
| `synced` | boolean | `true` when `lines` holds time-stamped lines. |
| `lines` | array | `{timeMs, text}`, sorted by `timeMs`. Empty when `synced` is `false`. An empty `text` is a gap between verses; keep it as spacing. |
| `plain` | string or `null` | Unsynced text. Set when there are no `lines`; may also accompany them. |
| `instrumental` | boolean | `true` for an instrumental track: `lines` is empty and `plain` is `null`. Show "Instrumental". |

Source order: synced lyrics from LRCLIB, then plain lyrics from LRCLIB, then plain lyrics from YouTube Music. The server uses the first source that has lyrics. An LRCLIB match must be within 3 s of `durationSecs` (when sent). Otherwise the server searches LRCLIB for a record that is.

To highlight a line, find the last line whose `timeMs` is at most the extrapolated position (see [Position](#position)). Show the first line as current only once `positionRunning` is true.

The server remembers answers for the last 200 songs, including "no lyrics". Failures are not remembered, so retrying later works.

| Status | Body | Cause |
|---|---|---|
| 400 | `{"error": "videoId is required"}` | `videoId`, `title` or `artists` is missing or blank (the message names the field). |
| 400 | `{"error": "durationSecs must be a whole number of seconds"}` | `durationSecs` is not a non-negative integer. An empty value counts as absent. |
| 404 | `{"error": "no lyrics"}` | No source has lyrics for this song. |
| 502 | `{"error": "lyrics failed: <message>"}` | LRCLIB or YouTube Music failed. |

### GET /api/state

Read the current state once. Use the WebSocket for live updates.

Request:

```http
GET /api/state
```

Response `200`: a StateView (see [StateView](#stateview)).

### PUT /api/autoplay

Turn autoplay on or off. The choice is stored and survives a restart.

Request:

```http
PUT /api/autoplay
Content-Type: application/json

{"enabled": false}
```

Response `204` (no body). The new value shows in `autoplay.enabled` of the next `state` message.

| Status | Body | Cause |
|---|---|---|
| 400 | `{"error": "…"}` | Body missing, not JSON, or `enabled` is not a boolean. |
| 500 | `{"error": "…"}` | The setting could not be saved. Nothing changed. |

### POST /api/autoplay/play

Play one of the current suggestions now. The playing song is replaced; songs already in the queue stay.

Request:

```http
POST /api/autoplay/play
Content-Type: application/json

{"videoId": "abc123"}
```

Response `204` (no body).

| Status | Body | Cause |
|---|---|---|
| 400 | `{"error": "…"}` | Body missing or invalid, or `videoId` is blank. |
| 409 | `{"error": "that song is no longer suggested"}` | `videoId` is not in `autoplay.songs` right now. Nothing changed. |

### POST /api/queue/next

Add a song with "Play next". Send the full Song object from search.

Request:

```http
POST /api/queue/next
Content-Type: application/json
```

```json
{ "videoId": "abc123", "title": "Around the World", "artists": "Daft Punk", "album": null, "durationSecs": null, "artworkUrl": null }
```

Response `200`:

```json
{ "id": 8, "started": false }
```

| Field | Notes |
|---|---|
| `id` | The new QueueEntry id. Use it to remove the entry. |
| `started` | `true` when nothing was playing, so the song started at once. `false` when it was queued. |

| Status | Body | Cause |
|---|---|---|
| 400 | `{"error": "videoId must not be blank"}` | `videoId` is empty or spaces. |
| 400 | `{"error": "<JSON parser message>"}` | Missing `videoId`, `title`, or `artists`; malformed JSON; or no `Content-Type: application/json`. |

#### How Play next orders songs

Play next puts the song right after the current song. It goes after any songs added with Play next that have not started yet.

Example, starting from `A` playing:

| Action | Upcoming (in order) |
|---|---|
| Play next `X` | `X` |
| Play next `Y` | `X, Y` |
| `X` starts (current is now `X`) | `Y` |
| Play next `Z` | `Z, Y` |

When the current song changes, the insertion point resets. The next Play next song goes in front of all remaining upcoming songs.

### POST /api/queue/play

Replace the whole queue with a list of songs and start playing at once, even if playback was paused. The current song stops. Send full Song objects (1 to 500).

Request:

```http
POST /api/queue/play
Content-Type: application/json
```

```json
{ "songs": [ { "videoId": "a", "title": "A", "artists": "X", "album": null, "durationSecs": null, "artworkUrl": null } ], "startIndex": 0, "shuffle": false }
```

| Field | Notes |
|---|---|
| `songs` | 1 to 500 Song objects. |
| `startIndex` | Optional, default `0`. Must be less than `songs.length` (unless `shuffle` is `true`). Songs before it are dropped. |
| `shuffle` | Optional, default `false`. When `true` the whole list is shuffled and `startIndex` is ignored, not even validated. |

Response `200`:

```json
{ "count": 2 }
```

`count` is how many songs were queued. Every entry gets a fresh `id`, so entry ids from before the call are stale: `skip` or `previous` with an old `?entryId=` answers `409`.

| Status | Body | Cause |
|---|---|---|
| 400 | `{"error": "songs must hold 1 to 500 songs"}` | Empty list, or more than 500 songs. |
| 400 | `{"error": "startIndex out of range"}` | `startIndex` is not less than the number of songs. |
| 400 | `{"error": "videoId must not be blank"}` | A song has an empty `videoId`. |
| 400 | `{"error": "<JSON parser message>"}` | Malformed JSON, missing fields, or no `Content-Type: application/json`. |

### POST /api/queue/add

Add a list of songs to the queue without interrupting the current song.

Request:

```http
POST /api/queue/add
Content-Type: application/json
```

```json
{ "songs": [ { "videoId": "y", "title": "Y", "artists": "X", "album": null, "durationSecs": null, "artworkUrl": null } ], "at": "next" }
```

| Field | Notes |
|---|---|
| `songs` | 1 to 500 Song objects, added in the order sent. |
| `at` | `"next"` puts them right after the current song, following the [Play next](#how-play-next-orders-songs) rule. `"end"` appends them to the end of the queue. |

Response `200`:

```json
{ "count": 2, "started": false }
```

| Field | Notes |
|---|---|
| `count` | How many songs were added. |
| `started` | `true` when nothing was playing, so the first song started at once and the rest were queued after it. |

Every added song gets a fresh entry `id`; read them from the state.

Example, starting from `A` playing, with `X` added by Play next:

| Action | Upcoming (in order) |
|---|---|
| `queue/add` `[Y, Z]` with `at: "next"` | `X, Y, Z` |
| Play next `W` | `X, Y, Z, W` |

`at: "end"` leaves the Play next insertion point alone: from `A` playing and `B` upcoming, adding `[Y]` at `end` gives `B, Y`, and then Play next `X` gives `X, B, Y`.

| Status | Body | Cause |
|---|---|---|
| 400 | `{"error": "songs must hold 1 to 500 songs"}` | Empty list, or more than 500 songs. |
| 400 | `{"error": "videoId must not be blank"}` | A song has an empty `videoId`. |
| 400 | `{"error": "<JSON parser message>"}` | `at` is missing or not `"next"`/`"end"`; malformed JSON; or no `Content-Type: application/json`. |

### DELETE /api/queue/{entryId}

Remove an upcoming entry. The current song cannot be removed; use skip instead.

Request:

```http
DELETE /api/queue/8
```

Response `204` (no body).

| Status | Body | Cause |
|---|---|---|
| 404 | `{"error": "no upcoming entry 8 (the playing song can't be removed)"}` | Unknown id, or the id is the current entry. |

### POST /api/player/pause

Request:

```http
POST /api/player/pause
```

Response `204` (no body). Sets `status` to `"paused"`. Does nothing when idle or already paused.

### POST /api/player/resume

Response `204` (no body). Sets `status` to `"playing"` and continues from the same position. Does nothing when idle or already playing.

### POST /api/player/skip

Query: `entryId` (optional). The id of the `current` entry the user saw. Send it.

```http
POST /api/player/skip?entryId=7
```

Response `204` (no body). Starts the next upcoming song. Also resumes if paused.

- If there is no next song, playback stops: `current` becomes `null` and `status` becomes `"stopped"`.
- Does nothing when idle.

| Status | Body | Cause |
|---|---|---|
| 409 | `{"error": "entry 7 is no longer playing"}` | `entryId` is not the current entry any more. Nothing changed. |

Why `entryId`: a song's end is reported about 1 s before the speakers finish it. A skip tapped in that last second would otherwise skip the next song too. Two phones skipping the same song would also skip two. With `entryId`, the second skip gets `409` and does nothing.

### POST /api/player/previous

Query: `entryId` (optional), same as skip.

Response `204` (no body). What it does depends on the position in the current song:

- Less than 3 s in (the position shown in `state`, so also while the song is still loading), and a song was played before it: goes back to that song. It becomes current again with its original entry `id` and plays from `0`. The song you left goes back to the head of `upcoming`.
- 3 s or more in, or nothing played before: restarts the current song from `0`. The queue does not change.

Also resumes if paused. Does nothing when idle. `409` when `entryId` is not the current entry.

Pinson remembers up to 100 earlier songs. Songs that failed to play are not remembered. So one tap restarts the song, and a second tap goes back.

### POST /api/player/seek

Query: `positionMs` (required), where to jump in the current song; `entryId` (optional), same as skip.

```http
POST /api/player/seek?positionMs=83500&entryId=7
```

Response `204` (no body). Restarts the current song `positionMs` into it, forwards or backwards. The queue does not change, and a paused song stays paused. A position past the end lands on the song's last second.

- The song is reopened from YouTube, so the speakers go quiet for about as long as starting a song takes. Meanwhile `positionMs` already shows the target, with `positionRunning: false`.
- Does nothing when idle.

| Status | Body | Cause |
|---|---|---|
| 400 | `Failed to deserialize query string: …` | `positionMs` missing or not a whole number. |
| 409 | `{"error": "entry 7 is no longer playing"}` | `entryId` is not the current entry any more. Nothing changed. |

### GET /api/favorites

List the favorite songs, newest added first.

Request:

```http
GET /api/favorites
```

Response `200`: an array of Song, `[]` when there are none. Same shape as search results.

### PUT /api/favorites/{videoId}

Add a favorite. Send the full Song object from search. It is safe to repeat: a song that is already a favorite keeps its position and gets the new metadata.

Request:

```http
PUT /api/favorites/abc123
Content-Type: application/json
```

```json
{ "videoId": "abc123", "title": "Around the World", "artists": "Daft Punk", "album": null, "durationSecs": null, "artworkUrl": null }
```

Response `204` (no body).

| Status | Body | Cause |
|---|---|---|
| 400 | `{"error": "videoId must not be blank"}` | `videoId` in the path or the body is empty or spaces. |
| 400 | `{"error": "videoId in the body must match the one in the path"}` | The body's `videoId` differs from the path's. |
| 400 | `{"error": "<JSON parser message>"}` | Missing `videoId`, `title`, or `artists`; malformed JSON; or no `Content-Type: application/json`. |

### DELETE /api/favorites/{videoId}

Remove a favorite.

Request:

```http
DELETE /api/favorites/abc123
```

Response `204` (no body).

| Status | Body | Cause |
|---|---|---|
| 404 | `{"error": "abc123 is not a favorite"}` | That song is not a favorite. |

### POST /api/favorites/shuffle

Play all favorites in random order. The whole queue is replaced: the current song stops, the shuffled favorites become `current` and `upcoming`, and the first one starts at once, even if playback was paused.

Request:

```http
POST /api/favorites/shuffle
```

Response `200`:

```json
{ "count": 12 }
```

| Field | Notes |
|---|---|
| `count` | How many songs were queued. |

| Status | Body | Cause |
|---|---|---|
| 409 | `{"error": "no favorites"}` | There are no favorites. Nothing changed. |

### GET /api/playlists

List your playlists as PlaylistSummary, most recently updated first.

Request:

```http
GET /api/playlists
```

Response `200`: an array of PlaylistSummary, `[]` when there are none.

### POST /api/playlists

Create a playlist. `songs` is optional. When given, the playlist starts as a copy of those songs (a repeated `videoId` is kept once). This is how you save a YouTube playlist or the queue as your own.

Request:

```http
POST /api/playlists
Content-Type: application/json
```

```json
{ "name": "Party", "songs": [Song, ...] }
```

Response `201`: the Playlist.

| Status | Body | Cause |
|---|---|---|
| 400 | `{"error": "name must not be blank"}` | `name` is empty or spaces. |
| 400 | `{"error": "name must be at most 100 characters"}` | The trimmed name is longer than 100 characters. |
| 400 | `{"error": "name must not contain control characters"}` | The name has a control character. |
| 400 | `{"error": "songs must hold 1 to 500 songs"}` | `songs` is given and holds more than 500 songs. |
| 400 | `{"error": "videoId must not be blank"}` | A song has a blank `videoId`. |
| 400 | `{"error": "<JSON parser message>"}` | Missing `name`; malformed JSON; or no `Content-Type: application/json`. |

### GET /api/playlists/{id}

Get one playlist with its songs.

Request:

```http
GET /api/playlists/k3Jd9sLq0Z
```

Response `200`: the Playlist.

| Status | Body | Cause |
|---|---|---|
| 404 | `{"error": "no playlist k3Jd9sLq0Z"}` | No such playlist. |

### PATCH /api/playlists/{id}

Rename a playlist.

Request:

```http
PATCH /api/playlists/k3Jd9sLq0Z
Content-Type: application/json
```

```json
{ "name": "Kitchen" }
```

Response `200`: the updated Playlist.

| Status | Body | Cause |
|---|---|---|
| 400 | `{"error": "name must ..."}` | Same name rules as create. |
| 400 | `{"error": "<JSON parser message>"}` | Missing `name`; malformed JSON; or no `Content-Type: application/json`. |
| 404 | `{"error": "no playlist k3Jd9sLq0Z"}` | No such playlist. |

### DELETE /api/playlists/{id}

Delete a playlist and its song list. Favorites and the queue are not affected.

Request:

```http
DELETE /api/playlists/k3Jd9sLq0Z
```

Response `204` (no body).

| Status | Body | Cause |
|---|---|---|
| 404 | `{"error": "no playlist k3Jd9sLq0Z"}` | No such playlist. |

### POST /api/playlists/{id}/songs

Append songs to the end. Songs already in the playlist are skipped.

Request:

```http
POST /api/playlists/k3Jd9sLq0Z/songs
Content-Type: application/json
```

```json
{ "songs": [Song, ...] }
```

Response `200`:

```json
{ "added": 2, "skipped": 1 }
```

| Field | Notes |
|---|---|
| `added` | Songs appended. |
| `skipped` | Songs left out because the playlist already had that `videoId` (or it was repeated in the request). |

| Status | Body | Cause |
|---|---|---|
| 400 | `{"error": "songs must hold 1 to 500 songs"}` | `songs` is empty or holds more than 500 songs. |
| 400 | `{"error": "videoId must not be blank"}` | A song has a blank `videoId`. |
| 400 | `{"error": "<JSON parser message>"}` | Missing `songs`; malformed JSON; or no `Content-Type: application/json`. |
| 404 | `{"error": "no playlist k3Jd9sLq0Z"}` | No such playlist. |

### DELETE /api/playlists/{id}/songs/{videoId}

Remove one song from a playlist.

Request:

```http
DELETE /api/playlists/k3Jd9sLq0Z/songs/abc123
```

Response `204` (no body).

| Status | Body | Cause |
|---|---|---|
| 404 | `{"error": "no playlist k3Jd9sLq0Z"}` | No such playlist. |
| 404 | `{"error": "abc123 is not in this playlist"}` | The song is not in the playlist. |

### PUT /api/playlists/{id}/order

Set the song order. `videoIds` must list every song in the playlist exactly once. Send the whole new order, not a single move. If someone else changed the playlist since you loaded it, you get `409`: reload and try again.

Request:

```http
PUT /api/playlists/k3Jd9sLq0Z/order
Content-Type: application/json
```

```json
{ "videoIds": ["def456", "abc123"] }
```

Response `200`: the updated Playlist.

| Status | Body | Cause |
|---|---|---|
| 400 | `{"error": "videoIds must not repeat"}` | A `videoId` appears twice. |
| 400 | `{"error": "<JSON parser message>"}` | Missing `videoIds`; malformed JSON; or no `Content-Type: application/json`. |
| 404 | `{"error": "no playlist k3Jd9sLq0Z"}` | No such playlist. |
| 409 | `{"error": "videoIds must list exactly the songs in the playlist"}` | The list misses a song, or names one that is not in the playlist. Nothing changed. |

### GET /api/output

The output Pinson plays on, and the outputs a client may pick. With no output chosen, `current` is `null`: starting a song leaves it paused and plays nothing until you choose one.

Request:

```http
GET /api/output
```

Response `200`:

```json
{
  "current": {"id": "nidoo:192.168.1.91:4953", "kind": "nidoo", "name": "Nidoo", "address": "192.168.1.91:4953", "hint": "Approve speakers in Nidoo.", "reachable": true, "speakers": true, "browserPlayer": true},
  "startupId": null,
  "available": [{"kind": "nidoo", "name": "Nidoo", "needsAddress": true, "defaultAddress": "192.168.1.91:4953", "note": "Speakers through nidoo-sound"}]
}
```

| Field | Type | Notes |
|---|---|---|
| `current` | object or null | The output in use; `null` when none is chosen. |
| `current.id` | string | Stable id, such as `nidoo:host:port`, `snapcast:host:port`, `browser:<deviceId>`, `chromecast:<address>`, `upnp:<address>`, `airplay:<address>`, `stream`, `local` or `log`. |
| `current.kind` | string | `browser` (a browser tab, offered as "This device"), `nidoo`, `snapcast`, `chromecast`, `upnp`, `airplay`, `stream`, `local` (This computer) or `log`. |
| `current.name` | string | Display name. |
| `current.address` | string or null | `host:port`, for outputs that need one. |
| `current.hint` | string or null | A short help line to show next to the output. |
| `current.reachable` | boolean or null | Whether the output answers; `null` when unknown or not applicable. |
| `current.speakers` | boolean | Whether `/api/speakers` works. |
| `current.browserPlayer` | boolean | Whether `/api/browser-player` works. |
| `startupId` | string or null | The id of the output fixed at startup by `MUSIC_ENGINE` and friends, else `null`. When set, a switch lasts until restart and is not saved. |
| `available` | array | Outputs a client may pick, in this order: This device, Nidoo, Snapcast, Stream URL, This computer, then one entry per discovered Chromecast, then one per discovered UPnP renderer, then one per discovered AirPlay speaker. `needsAddress` says to ask for an address; `defaultAddress` pre-fills it. `note` is a one-line description for the picker; it is omitted when absent. `id` and `address` are set on Chromecast, UPnP and AirPlay entries: send `address` back as `address` when picking one, and match `id` against `current.id`. |

The `browser` kind (`{"kind": "browser", "name": "This device", "needsAddress": false, "defaultAddress": null}`) plays through a browser tab that is connected to the [WebSocket](#websocket). Its `address` is the tab's device id, not a `host:port`.

The other kinds:

| Kind | `name` | `current.hint` | Speakers | Notes |
|---|---|---|---|---|
| `snapcast` | Snapcast | "Add Pinson's tcp source to snapserver.conf (see the README)." | yes | `address` is the snapserver `host:port`; `defaultAddress` is the `--snapcast` value. Current id `snapcast:<host:port>`. |
| `stream` | Stream URL | "Open /stream.mp3 in VLC or a browser. The progress bar is approximate." | no | `needsAddress` false. Current id `stream`. Listen at [`GET /stream.mp3`](#get-streammp3). |
| `chromecast` | the device's name | "The Cast device fetches the music from Pinson, so keep both on the same network." | yes: one speaker, `cast` | One `available` entry per device found by mDNS, with `needsAddress` false, `id` `chromecast:<cast id>` and `address` the Cast id. Current id `chromecast:<address>`, where `address` is what was picked: a Cast id, `host:port`, or a device name not found on the network yet (`chromecast:Living Room`). The entries change as devices appear and disappear, and the server sends the new `available` over the [WebSocket](#websocket) like other output updates. See [`GET /api/cast/stream/{generation}`](#get-apicaststreamgeneration). |
| `upnp` | the renderer's name | "The renderer fetches the music from Pinson, so keep both on the same network." | yes: one speaker, `upnp` | One `available` entry per renderer found by SSDP, with `needsAddress` false, `note` "Sonos", else the model name, else "DLNA renderer", `id` `upnp:<udn>` and `address` the UDN (without `uuid:`). Current id `upnp:<address>`, where `address` is what was picked: a UDN, a description URL, or a name not found on the network yet (`upnp:Living Room`). The entries change as renderers appear and disappear, and the server sends the new `available` over the [WebSocket](#websocket). See [`GET /api/upnp/stream/{generation}/song.mp3`](#get-apiupnpstreamgenerationsongmp3). |
| `airplay` | the speaker's name | "AirPlay 1 only: speakers that need AirPlay 2 pairing, such as HomePods, won't play." | yes: one speaker, `airplay` | One `available` entry per AirPlay 1 speaker found by mDNS, with `needsAddress` false, `note` the model name, else "AirPlay speaker", `id` `airplay:<id>` and `address` the speaker's id (its MAC address, such as `0A1B2C3D4E5F`). Speakers protected by a password are not listed. AirPlay 2-only speakers may be listed but won't play: Pinson pauses and logs why (they need pairing). Current id `airplay:<address>`, where `address` is what was picked: a speaker id, `host:port`, or a name not found on the network yet (`airplay:Kitchen`). The entries change as speakers appear and disappear, and the server sends the new `available` over the [WebSocket](#websocket). Pause, skip and seek are heard after about `--airplay-latency-ms` (2 s by default). |
| `local` | This computer | none | yes: one speaker, `local` | `needsAddress` false. Listed only when the server has a usable sound card. Current id `local`. |

### PUT /api/output

Switch the output. The current song moves to the new output at the position the listener heard, and play or pause carries over. Unless the output was fixed at startup, the choice is saved and used on the next start.

Request:

```http
PUT /api/output
Content-Type: application/json

{"kind": "nidoo", "address": "192.168.1.91:4953"}
```

| Field | Type | Notes |
|---|---|---|
| `kind` | string | One of `available[].kind`. |
| `address` | string | Optional. `host:port`. Defaults to the server's default for the kind. For `browser`, the device id: 8 to 64 letters, digits, `_` or `-`. For `snapcast`, the snapserver address. For `chromecast`, a Cast id (with or without dashes), a device name, `host:port`, or a bare host, which gets port 8009; without `address`, the server's `--chromecast` value. For `upnp`, a renderer's friendly name, its UDN (with or without `uuid:`), or its description URL (`http://` or `https://`); without `address`, the server's `--upnp` value. For `airplay`, a speaker's name, its id (with or without `:` or `-`), `host:port`, or a bare host, which gets port 5000; without `address`, the server's `--airplay` value. Ignored for `stream` and `local`. |
| `name` | string | Optional. A label for a `browser` device, shown as the output's name. Trimmed, at most 40 characters; blank means "Browser". For `chromecast`, the label used when discovery doesn't know the device; blank means "Chromecast". For `upnp`, the same for a renderer discovery doesn't know; blank means the name it was picked by when `address` (or `--upnp`) is a name, else "UPnP renderer". For `airplay`, the same for a speaker discovery doesn't know; blank means the name it was picked by when `address` (or `--airplay`) is a name, else "AirPlay speaker". |

Response `200`: the same body as `GET /api/output`.

Errors:

| Status | Body | Cause |
|---|---|---|
| 400 | `{"error": "expected host:port, got \"x\""}` | The address is not `host:port`. |
| 400 | `{"error": "unknown output"}` | The kind is not available. |
| 400 | `{"error": "expected a Chromecast: its name, id or host:port"}` | A `chromecast` output with no `address` and no `--chromecast` value. |
| 400 | `{"error": "expected a UPnP renderer: its name, UDN or description URL"}` | A `upnp` output with no `address` and no `--upnp` value. |
| 400 | `{"error": "a renderer name has at most 64 characters"}` | A `upnp` `address` that is not a URL or UDN and is longer than 64 characters. |
| 400 | `{"error": "expected an AirPlay speaker: its name, id or host:port"}` | An `airplay` output with no `address` and no `--airplay` value. |
| 400 | `{"error": "a speaker name has at most 64 characters"}` | An `airplay` `address` that is not a discovered speaker or `host[:port]` and is longer than 64 characters. |
| 400 | `{"error": "expected a device id"}` | A `browser` output with a missing or invalid device id. |
| 400 | `{"error": "<JSON parser message>"}` | Malformed JSON, an unknown `kind`, or no `Content-Type: application/json`. |
| 500 | `{"error": "couldn't switch the output"}` | The switch failed inside the server. |

### GET /api/speakers

List the approved Nidoo speakers with their volume and mute state. Only available when the current output has speaker control: Nidoo, Snapcast, Chromecast, UPnP, AirPlay or This computer. With no output chosen, or another output, it answers `404`.

For Snapcast, the list is the snapserver clients: `id` is the Snapcast client id, and `name` is the client's configured name, or its host name when none is set. `connected` is the client's connection state. For This computer, the list is one speaker, `{"id": "local", "name": "<sound card name>", ...}` (the name of the default output device, as the sound system reports it), whose volume is a software volume kept until restart. For Chromecast, the list is one speaker, `{"id": "cast", "name": "<device name>", ...}`, whose volume and mute are the device's own. For UPnP, the list is one speaker, `{"id": "upnp", "name": "<renderer name>", ...}`, whose volume and mute are the renderer's: `volume` is the renderer's RenderingControl `Volume`, read as a linear 0 to 100 (not scaled to the device's own range). Pinson reads it again every 5 seconds, so a change made with the device's remote shows up; when a read fails, the last known value stays. For AirPlay, the list is one speaker, `{"id": "airplay", "name": "<speaker name>", ...}`. AirPlay 1 speakers don't report their volume, so `volume` and `muted` are the ones Pinson last sent: Pinson sends 50 when it connects to the speaker, and the level is reset to 50 each time the output is selected again.

Request:

```http
GET /api/speakers
```

Response `200`: an array, `[]` when no speaker is approved. Snapcast keeps the order snapserver reports; the other outputs sort by name.

```json
[{"id": "0123456789ab", "name": "Kitchen", "volume": 45, "muted": false, "connected": true}]
```

| Field | Type | Notes |
|---|---|---|
| `id` | string | Short speaker id. Use it in `PUT /api/speakers/{id}`. |
| `name` | string | Display name. |
| `volume` | integer | 0 to 100. |
| `muted` | boolean | |
| `connected` | boolean | `false` when the speaker is approved but offline. |

Errors:

| Status | Body | Cause |
|---|---|---|
| 404 | `{"error": "no speaker control for this output"}` | There is no output, or the current output has no speaker control. |
| 502 | `{"error": "Can't reach nidoo-sound"}` | `nidoo-sound` did not answer within 2 seconds, or failed. |
| 502 | `{"error": "Can't reach snapserver"}` | Snapcast only: snapserver did not answer in time, or failed. |
| 502 | `{"error": "Can't reach the Chromecast"}` | Chromecast only: the device did not answer, or is not connected. |
| 502 | `{"error": "Can't reach the renderer"}` | UPnP only: the renderer did not answer, or is not connected, or its volume hasn't been read yet. |
| 502 | `{"error": "Can't reach the AirPlay speaker"}` | AirPlay only: Pinson is not connected to the speaker. |

### PUT /api/speakers/{id}

Set a speaker's volume, mute, or both. Send at least one field. When both are sent, both apply.

Request:

```http
PUT /api/speakers/0123456789ab
Content-Type: application/json

{"volume": 30, "muted": false}
```

| Field | Type | Notes |
|---|---|---|
| `volume` | integer | Optional. 0 to 100. |
| `muted` | boolean | Optional. |

Response `204`: no body.

Errors:

| Status | Body | Cause |
|---|---|---|
| 400 | `{"error": "volume must be 0-100"}` | `volume` is above 100. |
| 400 | `{"error": "nothing to change"}` | Nidoo only: neither field was sent. For Snapcast and This computer an empty change does nothing and answers `204`. |
| 400 | `{"error": "<JSON parser message>"}` | Malformed JSON, wrong field type, or no `Content-Type: application/json`. |
| 404 | `{"error": "no approved speaker 0123456789ab"}` | Unknown or unapproved speaker. |
| 404 | `{"error": "unknown speaker"}` | Snapcast or This computer: no client with that id. For This computer, the only valid id is `local`. |
| 404 | `{"error": "no speaker control for this output"}` | There is no output, or the current output has no speaker control. |
| 400 | `{"error": "volume must be 0 to 100"}` | Snapcast or This computer: `volume` is above 100. |
| 502 | `{"error": "Can't reach nidoo-sound"}` | `nidoo-sound` did not answer within 2 seconds, or failed. |
| 502 | `{"error": "Can't reach snapserver"}` | Snapcast only: snapserver did not answer in time, or failed. |
| 502 | `{"error": "Can't reach the renderer"}` | UPnP only: the renderer did not answer in time, or is not connected. |
| 502 | `{"error": "the renderer refused the change"}` | UPnP only: the renderer answered with an error (for example, it has no volume control). |
| 502 | `{"error": "Can't reach the AirPlay speaker"}` | AirPlay only: the speaker did not answer in time, or Pinson is not connected to it. |

### GET /api/browser-player

Get the WebSocket a browser uses to join `nidoo-sound` as a synced speaker, with a Sendspin player library. Only available when the current output offers it (Nidoo).

Request:

```http
GET /api/browser-player
```

Response `200`:

```json
{"url": "ws://192.168.1.91:8927/sendspin"}
```

| Field | Type | Notes |
|---|---|---|
| `url` | string | `nidoo-sound`'s Sendspin player WebSocket. |

Errors:

| Status | Body | Cause |
|---|---|---|
| 404 | `{"error": "no browser player for this output"}` | There is no output, or the current output has no browser player. |

### GET /api/device/stream/{generation}

The MP3 of the current song, for the `<audio>` element of the tab that plays ("This device" output). The tab gets `generation` and the URL from the `device.load` WebSocket message. The audio is 192 kbit/s, 48 kHz stereo, constant bit rate: 24 bytes per millisecond.

Request:

```http
GET /api/device/stream/3?device=3f9c1a7e5b2d4c60&fromMs=42500
```

| Query | Type | Notes |
|---|---|---|
| `device` | string | The tab's device id. Required. |
| `fromMs` | integer | Where to start, in milliseconds into the song. A missing or invalid value means the start of the stream. The server seeks, to a 24 ms frame boundary. |

Response `200`: `Content-Type: audio/mpeg` and `Cache-Control: no-store`. There is no `Content-Length` and no `Range` support. The body keeps streaming while the song is still being encoded, and ends when the song does, or when the server shuts down.

Errors:

| Status | Body | Cause |
|---|---|---|
| 404 | `{"error": "no such stream"}` | The device is missing, not the current output, or `generation` is not the current stream (a newer song replaced it). |

### GET /api/cast/stream/{generation}

The MP3 of the current song, for a Cast device (Chromecast output). Pinson sends the device this URL; clients don't call it. The audio is the same as [`GET /api/device/stream/{generation}`](#get-apidevicestreamgeneration): 192 kbit/s, 48 kHz stereo, 24 bytes per millisecond. Unlike it, there is no device id.

Request:

```http
GET /api/cast/stream/3?fromMs=42500
Range: bytes=1024-
```

| Query | Type | Notes |
|---|---|---|
| `fromMs` | integer | Where to start, in milliseconds into the song. A missing or invalid value means the start of the stream. |

Response `200`: `Content-Type: audio/mpeg` and `Cache-Control: no-store`. The body keeps streaming while the song is still being encoded, and ends when the song does, or when the server shuts down. Once the song is fully encoded, responses carry `Accept-Ranges: bytes`, and an open range (`Range: bytes=N-`) answers `206 Partial Content` with `Content-Range` and `Content-Length`, because the device may seek inside what it was sent. Before that, `Range` is ignored and the answer is a `200` from `fromMs`: the length isn't known yet.

Errors:

| Status | Body | Cause |
|---|---|---|
| 404 | `{"error": "no such stream"}` | `generation` is not a current Cast stream (a newer song replaced it, or the output isn't Chromecast). |
| 416 | empty, with `Content-Range: bytes */<length>` | The song is fully encoded and `N` in `Range: bytes=N-` is at or past its length (counted from `fromMs`). |

### GET /api/upnp/stream/{generation}/song.mp3

The MP3 of the current song, for a UPnP/DLNA renderer (UPnP output). Pinson sends the renderer this URL; clients don't call it. `HEAD` works too. It serves the same audio as [`GET /api/cast/stream/{generation}`](#get-apicaststreamgeneration), with the same `fromMs` query, `Range` rules and errors.

Every `200` and `206` also carries the DLNA headers:

- `transferMode.dlna.org: Streaming`
- `contentFeatures.dlna.org: DLNA.ORG_PN=MP3;DLNA.ORG_OP=<op>;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000`, where `<op>` is `01` once the song is fully encoded (the renderer may use ranges) and `00` while it is still encoding.

Once the song is fully encoded, a `200` has a `Content-Length` (the bytes from `fromMs`); while it is still encoding, there is none. The Cast route sends the same headers.

### GET /api/device/next

The stream after `after`, so the tab can play the next song without a gap. It tells Pinson that song `after` ended, then waits up to 30 seconds for the player to pick the next one.

Request:

```http
GET /api/device/next?device=3f9c1a7e5b2d4c60&after=3
```

| Query | Type | Notes |
|---|---|---|
| `device` | string | The tab's device id. |
| `after` | integer | The generation that just ended. Optional: when missing, the generation last served to the device (for a tab whose WebSocket was suspended and never learned the new generation). |

Response `200`: the same body as `GET /api/device/stream/{generation}`, from the start of the next song.

Response `204`: nothing follows (the queue ended, the output changed, or the wait timed out), also when `device` is missing.

### GET /stream.mp3

One endless MP3 for the Stream URL output, for VLC or a browser. It is available only while Stream URL is the current output. It is 192 kbit/s, 48 kHz stereo, and plays silence while paused. Several listeners at once are fine.

Request:

```http
GET /stream.mp3
```

Response `200`: `Content-Type: audio/mpeg` and `Cache-Control: no-cache, no-store`. There is no `Content-Length`, and the body never ends. The stream starts on an MP3 frame boundary. The progress bar is approximate: `MUSIC_STREAM_LATENCY_MS` sets how far behind the listener is assumed to be.

Errors:

| Status | Body | Cause |
|---|---|---|
| 404 | `{"error": "Stream URL isn't the current output"}` | Another output is chosen, or none. The stream ends when the output changes. |

### Server errors

Any route can return `500` with `{"error": "player has stopped"}` if the internal player task has stopped. Playlist routes can return `500` if the playlists file cannot be saved; nothing changed. Treat both as server failures and retry later.

## WebSocket

### Connect

```
GET /api/ws   (WebSocket upgrade)
```

The server sends text frames. Each frame is a JSON object with a `type` field. Clients send nothing, except a browser tab that plays for "This device": see [Client messages](#client-messages). Any other frame you send is ignored.

### Message types

| `type` | When | Shape |
|---|---|---|
| `state` | First on connect, then after every change | `{"type": "state", ...StateView fields}` |
| `favorites` | Right after the first `state`, then after every change to the favorites | `{"type": "favorites", "songs": [Song, ...]}`, newest added first |
| `playlists` | Right after the first `favorites`, then after every change to any playlist | `{"type": "playlists", "playlists": [PlaylistSummary, ...]}`, most recently updated first |
| `output` | Right after the first `playlists`, then after every change to the output (a switch, or `reachable` changing) | `{"type": "output", ...GET /api/output body}` |
| `noOutput` | A song was started while no output is chosen. It stays paused | `{"type": "noOutput"}` |
| `songFailed` | A song could not be played and was skipped | `{"type": "songFailed", "entryId": 5, "title": "...", "reason": "..."}` |
| `autoplayFailed` | Autoplay suggestions could not be loaded. Sent once, then not again until a load succeeds | `{"type": "autoplayFailed"}` |
| `device.load` | To the tab that plays, when a song starts or a tab (re)connects | `{"type": "device.load", "generation": 3, "url": "/api/device/stream/3?device=3f9c1a7e5b2d4c60&fromMs=0", "startMs": 0, "chained": false, "paused": false}` |
| `device.pause` | To the tab that plays, when Pinson pauses | `{"type": "device.pause"}` |
| `device.resume` | To the tab that plays, when Pinson resumes | `{"type": "device.resume"}` |
| `device.stop` | To the tab that plays, when the output changes or nothing is playing | `{"type": "device.stop"}` |

`device.*` messages go only to the tab that is the current "This device" output. `startMs` is where the stream starts, in milliseconds into the song. `chained` is `true` when the tab already holds this stream and should keep playing it. `paused` is `true` when the tab should load without playing.

Example `state` message: `{"type": "state", "current": {...}, "status": "playing", "positionMs": 42500, "positionRunning": true, "upcoming": [], "autoplay": {"enabled": true, "basedOn": null, "songs": []}, "version": 12}`.

Example `favorites` message: `{"type": "favorites", "songs": [{"videoId": "abc123", "title": "Around the World", "artists": "Daft Punk", "album": null, "durationSecs": null, "artworkUrl": null}]}`.

Example `playlists` message: `{"type": "playlists", "playlists": [{"id": "k3Jd9sLq0Z", "name": "Party", "songCount": 12, "artworkUrls": [], "updatedAt": "2026-11-05T18:30:00.123456789+00:00"}]}`.

Example `output` message: `{"type": "output", "current": null, "startupId": null, "available": [{"kind": "nidoo", "name": "Nidoo", "needsAddress": true, "defaultAddress": "192.168.1.91:4953"}]}`.

Example notice: `{"type": "songFailed", "entryId": 5, "title": "Some Track", "reason": "Video unavailable"}`.

Rules:

- **State messages are snapshots.** Replace your whole state with each one. Do not merge.
- **Use `version` to drop old data.** Ignore any state whose `version` is not greater than the one you hold. Under bursts of changes the server may skip versions. That is safe because every message is complete. Versions restart at 0 when the server restarts, so always accept the first message after a (re)connect, whatever its version.
- **Favorites messages are snapshots too.** Replace your whole favorites list with each one. They carry no `version`; the latest message wins.
- **Playlists messages are snapshots too.** Replace your whole playlist list with each one. They hold summaries only, so fetch `GET /api/playlists/{id}` for the songs. The latest message wins.
- **Output messages are snapshots too.** Replace your whole output view with each one. The latest message wins.
- **Notices are not replayed.** A client that falls far behind can miss them.
- **Unknown `type` values must be ignored.** New message types may be added later.

### Client messages

A browser tab that plays for the "This device" output sends these text frames. Anything else, or a `device.*` message before `hello`, is ignored.

| `type` | When | Shape |
|---|---|---|
| `hello` | Right after the WebSocket opens | `{"type": "hello", "deviceId": "3f9c1a7e5b2d4c60", "name": "Pixel", "loaded": {"generation": 3, "positionMs": 1234, "playing": true}}` |
| `device.position` | While the `<audio>` element plays | `{"type": "device.position", "generation": 3, "positionMs": 1234}` |
| `device.failed` | The `<audio>` element could not play the stream | `{"type": "device.failed", "generation": 3, "reason": "..."}` |

- `deviceId` is 8 to 64 letters, digits, `_` or `-`; a `hello` with another id is ignored. The id is the `address` of the `browser` output.
- `loaded` is what the tab's `<audio>` holds now, or `null` when it holds nothing. `generation` may be `null` too.
- A second `hello` on the same WebSocket replaces the first.
- If the tab closes, or its WebSocket closes cleanly, Pinson pauses after about 10 s and the output stays "This device". Pinson sends no keepalive, so a connection that drops silently (the device left the network) is not noticed until the server's next send fails. When the same device says hello again, it is the player again, still paused.

### Position

`positionMs` is the playback position at the moment the message was built. It is already corrected for speaker latency (about 750 ms). It is the position the speakers are playing now. Accuracy is about ±1 s.

`positionRunning` says whether that position is moving. It is `false` while the song is paused or idle, while the server is still opening the song, and for the first moments of audio (the speaker latency). When the position starts moving, the server sends a new `state` message with `positionRunning` set to `true`. Extrapolate only while it is `true`.

Extrapolate on the client, using your own receive time. Client and server clocks never need to match.

```kotlin
fun currentPositionMs(state: StateView, receivedAtMs: Long): Long =
    if (state.status == "playing" && state.positionRunning) {
        state.positionMs + (SystemClock.elapsedRealtime() - receivedAtMs)
    } else {
        state.positionMs // paused, stopped or not started yet: frozen
    }
```

- While `status` is `"playing"` and `positionRunning` is `true`, add the time since you received the message.
- Otherwise show `positionMs` as is. Right after a song becomes current it is `0` and frozen until the next `state` message sets `positionRunning` to `true`.

### Reconnect

- On disconnect, reconnect with backoff: start at 1 s, double on each failure, cap at 10 s.
- The first message after every connect is a full `state`, followed by a full `favorites` message and a full `playlists` message. You do not need a separate `GET /api/state`, `GET /api/favorites` or `GET /api/playlists`.

## MQTT

Pinson can also be a media player on an MQTT broker, so a home hub such as Nidoo can show what is playing and control it from a dashboard card or by voice. It uses Home Assistant-style MQTT discovery. MQTT is off unless `--mqtt-broker` is set; Pinson then runs standalone as before.

### Settings

| Flag | Env var | Default | Meaning |
|---|---|---|---|
| `--mqtt-broker` | `MUSIC_MQTT_BROKER` | unset: MQTT off | Broker as `host[:port]`. The port defaults to `1883`. No TLS. |
| `--mqtt-user` | `MUSIC_MQTT_USER` | unset | User name, if the broker needs one. |
| `--mqtt-password` | `MUSIC_MQTT_PASSWORD` | unset | Password, if the broker needs one. Never logged. |
| `--mqtt-discovery-prefix` | `MUSIC_MQTT_DISCOVERY_PREFIX` | `homeassistant` | Discovery prefix. |
| `--mqtt-root` | `MUSIC_MQTT_ROOT` | `pinson` | Topic root. Also the discovery `node_id`. |

The MQTT client id is `pinson-<root>`.

### Topics

With the defaults:

| Topic | Direction | Retained | Content |
|---|---|---|---|
| `homeassistant/media_player/pinson/player/config` | Pinson → broker | yes | [Discovery](#discovery) config |
| `pinson/status` | Pinson → broker | yes | `online`, or `offline` (clean stop, and the last will) |
| `pinson/player/state` | Pinson → broker | yes | [State](#state) JSON |
| `pinson/player/set` | hub → Pinson | no | [Commands](#commands) JSON |

In general: `<discovery_prefix>/media_player/<root>/player/config`, `<root>/status`, `<root>/player/state`, `<root>/player/set`. Everything is sent with QoS 1.

### Connection

- On every connect, including a reconnect after the broker was gone, Pinson subscribes to the command topic and publishes the discovery config, `online` and the current state.
- The last will is `offline`, retained. A clean stop (Ctrl-C) publishes `offline` itself.
- While the broker is unreachable Pinson retries every few seconds and logs one warning per outage. Playback and the HTTP API carry on unaffected.

### Discovery

```json
{
  "name": "Music",
  "unique_id": "pinson_player",
  "state_topic": "pinson/player/state",
  "command_topic": "pinson/player/set",
  "availability_topic": "pinson/status",
  "supported_features": ["play", "pause", "next", "previous", "play_media"],
  "device": {
    "identifiers": ["pinson"],
    "name": "Pinson",
    "model": "Music server",
    "manufacturer": "Gekkotron"
  }
}
```

`unique_id` is `<root>_player` and `identifiers` is `[<root>]`. All five features are always listed.

### State

```json
{
  "state": "playing",
  "title": "One More Time",
  "artist": "Daft Punk",
  "album": "Discovery",
  "image_url": "https://lh3.googleusercontent.com/abc=w544-h544",
  "color": "#3d2466",
  "duration_s": 321,
  "position_s": 42.5,
  "position_at": "2026-11-06T10:13:43.020Z"
}
```

Every key is always present.

| Field | Type | Notes |
|---|---|---|
| `state` | `"playing"`, `"paused"` or `"idle"` | `idle`: nothing is current. Every other field is then `null`. |
| `title` | string or `null` | |
| `artist` | string or `null` | The song's `artists` display string. |
| `album` | string or `null` | |
| `image_url` | string or `null` | The song's `artworkUrl` as is. A browser can load it directly. |
| `color` | string or `null` | `#rrggbb`, the same value as `color` from [`GET /api/artwork/color`](#get-apiartworkcolor). `null` while it is being computed and when the cover cannot be coloured. |
| `duration_s` | integer or `null` | Seconds. |
| `position_s` | number or `null` | Seconds into the song, already corrected for speaker latency. |
| `position_at` | string or `null` | RFC 3339 UTC time at which `position_s` holds. `null` while `state` is `playing` but the song is still loading: hold the bar at `position_s`. |

Position: while `state` is `playing`, show `position_s + max(0, now - position_at)`, clamped to `duration_s`. `position_at` can be up to about 1 s in the future right after playback starts: the speakers have not caught up yet, so the position holds still until then. While a song is still loading (after `play_media`, `next`, `previous` or a seek), `state` is `playing` and `position_at` is `null`: hold the bar still at `position_s` (0 after `play_media`, `next` and `previous`, the new offset after a seek). Pinson publishes again with a real `position_at` as soon as the audio starts.

When a state is published:

- On play, pause, song change, seek, restart, when a song that was loading starts moving, and when the cover colour arrives (first with `color: null`, then again with the colour).
- Not on position ticks, queue changes, or Autoplay suggestion changes.
- Changes within 250 ms go out as one message.
- After `play_media`, and after `next` or `previous` that acted, at once and always with a fresh `position_at`, even if nothing else changed. A hub can take that message as the reply.

### Commands

JSON on `<root>/player/set`, not retained. Retained messages on this topic are ignored. There is no reply topic: watch the state.

| Payload | Effect |
|---|---|
| `{"command": "play"}` | Resumes. When nothing is current, plays something: all favorites shuffled (as [`POST /api/favorites/shuffle`](#post-apifavoritesshuffle)); with no favorites, the first Home mix (see [`GET /api/mixes`](#get-apimixes)); with neither, logs a warning and does nothing. |
| `{"command": "pause"}` | Pauses. |
| `{"command": "next"}` | Skips the current song, as [`POST /api/player/skip`](#post-apiplayerskip). |
| `{"command": "previous"}` | Goes back to the earlier song if the current one is less than 3 s in, otherwise restarts the current song, as [`POST /api/player/previous`](#post-apiplayerprevious). |
| `{"command": "play_media", "query": "daft punk"}` | Searches YouTube Music songs and plays the top one now, replacing the queue. With Autoplay on, Autoplay continues with similar songs after it. With Autoplay off, only that song plays. The Autoplay switch is never changed. |

Malformed JSON, a missing `command`, an unknown command, and `play_media` with a missing or blank `query` are logged as warnings and ignored. So are a search that fails or finds nothing: nothing changes.

### Try it

```sh
mosquitto_sub -h <broker> -v -t 'pinson/#' -t 'homeassistant/media_player/pinson/#'
mosquitto_pub -h <broker> -t pinson/player/set -m '{"command":"play_media","query":"daft punk"}'
```

## Typical client flow

1. **Search.** `GET /api/search?q=<text>`. Show the list.
2. **Queue.** When the user picks a song, `POST /api/queue/next` with that Song object. The response's `started` tells you whether it began at once.
3. **Listen.** Open `GET /api/ws` once and keep it open. Render each `state` message. Show `songFailed` and `autoplayFailed` notices as a message to the user.
4. **Control.** Send pause, resume, skip, previous, or delete as `POST` or `DELETE` calls. Pass `entryId` on skip and previous. Do not update the UI from their `204` responses. Wait for the next `state` message.
5. **Albums and playlists.** `GET /api/search?q=<text>&kind=albums` (or `playlists`) lists them. Open one with `GET /api/albums/{id}` or `GET /api/ytplaylists/{id}`. To play it, send its `songs` to `POST /api/queue/play` (replaces the queue, optional `startIndex` and `shuffle`) or `POST /api/queue/add`.
6. **Your playlists.** Read them from the `playlists` WebSocket message, or `GET /api/playlists`. Create one with `POST /api/playlists`. Add songs with `POST /api/playlists/{id}/songs`. Reorder with `PUT /api/playlists/{id}/order`. To play one, `GET /api/playlists/{id}` and send its `songs` to `POST /api/queue/play`. To save an album or YouTube playlist as your own, `POST /api/playlists` with its `songs`.
7. **Artwork.** Show covers straight from `artworkUrl`. Use `GET /api/artwork?url=<artworkUrl>` only when the client must read the pixels.
8. **Lyrics.** When a song starts, `GET /api/lyrics` with its `videoId`, `title`, `artists`, `album` and `durationSecs`. A `404` means no lyrics: show that, do not retry. Highlight lines from the extrapolated position (see [Position](#position)).
