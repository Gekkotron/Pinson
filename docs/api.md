# House Music Server API

*Gekkotron · 2026-11-04*

The server searches YouTube Music, keeps a "play next" queue, and plays it through the home speakers. Your app calls HTTP routes to search, queue, and control playback. It reads live state from one WebSocket.

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
  "upcoming": [
    { "id": 8, "song": { "videoId": "abc123", "title": "Around the World", "artists": "Daft Punk", "album": null, "durationSecs": null, "artworkUrl": null } }
  ],
  "version": 12
}
```

| Field | Type | Notes |
|---|---|---|
| `current` | QueueEntry or `null` | The song playing or paused. `null` when idle. |
| `status` | string | `"playing"`, `"paused"`, or `"stopped"`. `stopped` means idle. |
| `positionMs` | integer | See [Position](#position). `0` when idle. |
| `upcoming` | QueueEntry[] | Songs after `current`, in play order. |
| `version` | integer | Increases on every change. |

Idle: `current` is `null`, `status` is `"stopped"`, `positionMs` is `0`, `upcoming` is `[]`.

## HTTP routes

### GET /api/search

Search YouTube Music for songs.

Query: `q` (required, not blank, URL-encoded).

Request:

```http
GET /api/search?q=daft%20punk
```

Response `200` (at most 20 songs):

```json
[
  { "videoId": "FGBhQbmPwH8", "title": "One More Time", "artists": "Daft Punk", "album": "Discovery", "durationSecs": 321, "artworkUrl": "https://example.com/art-544.jpg" }
]
```

| Status | Body | Cause |
|---|---|---|
| 400 | `{"error": "q must not be blank"}` | `q` missing, empty, or only spaces. |
| 502 | `{"error": "search failed: <upstream message>"}` | YouTube Music failed. Show the message to the user. |

### GET /api/state

Read the current state once. Use the WebSocket for live updates.

Request:

```http
GET /api/state
```

Response `200`: a StateView (see [StateView](#stateview)).

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

Response `204` (no body). Starts the next upcoming song. Also resumes if paused.

- If there is no next song, playback stops: `current` becomes `null` and `status` becomes `"stopped"`.
- Does nothing when idle.

### POST /api/player/previous

Response `204` (no body). Restarts the current song from `0`. The queue does not change. Also resumes if paused. Does nothing when idle.

### Server errors

Any route can return `500` with `{"error": "player has stopped"}` if the internal player task has stopped. Treat it as a server failure and retry later.

## WebSocket

### Connect

```
GET /api/ws   (WebSocket upgrade)
```

The server sends text frames. Each frame is a JSON object with a `type` field. Your client sends nothing. Any frame you send is ignored.

### Message types

| `type` | When | Shape |
|---|---|---|
| `state` | First on connect, then after every change | `{"type": "state", ...StateView fields}` |
| `songFailed` | A song could not be played and was skipped | `{"type": "songFailed", "entryId": 5, "title": "...", "reason": "..."}` |

Example `state` message: `{"type": "state", "current": {...}, "status": "playing", "positionMs": 42500, "upcoming": [], "version": 12}`.

Example notice: `{"type": "songFailed", "entryId": 5, "title": "Some Track", "reason": "Video unavailable"}`.

Rules:

- **State messages are snapshots.** Replace your whole state with each one. Do not merge.
- **Use `version` to drop old data.** Ignore any state whose `version` is not greater than the one you hold. Under bursts of changes the server may skip versions. That is safe because every message is complete. Versions restart at 0 when the server restarts, so always accept the first message after a (re)connect, whatever its version.
- **Notices are not replayed.** A client that falls far behind can miss them.
- **Unknown `type` values must be ignored.** New message types may be added later.

### Position

`positionMs` is the playback position at the moment the message was built. It is already corrected for speaker latency (about 750 ms). It is the position the speakers are playing now. Accuracy is about ±1 s.

Extrapolate on the client, using your own receive time. Client and server clocks never need to match.

```kotlin
fun currentPositionMs(state: StateView, receivedAtMs: Long): Long =
    if (state.status == "playing") {
        state.positionMs + (SystemClock.elapsedRealtime() - receivedAtMs)
    } else {
        state.positionMs // paused or stopped: frozen
    }
```

- While `status` is `"playing"`, add the time since you received the message.
- While `"paused"` or `"stopped"`, show `positionMs` as is.

Known gap: when a song becomes current, `status` is `"playing"` but `positionMs` stays at `0` until audio starts. That can take a few seconds. Extrapolation runs during that time, so the displayed position runs ahead. The next state message (sent when audio starts) corrects it.

### Reconnect

- On disconnect, reconnect with backoff: start at 1 s, double on each failure, cap at 10 s.
- The first message after every connect is a full `state`. You do not need a separate `GET /api/state`.

## Typical client flow

1. **Search.** `GET /api/search?q=<text>`. Show the list.
2. **Queue.** When the user picks a song, `POST /api/queue/next` with that Song object. The response's `started` tells you whether it began at once.
3. **Listen.** Open `GET /api/ws` once and keep it open. Render each `state` message. Show `songFailed` notices as a message to the user.
4. **Control.** Send pause, resume, skip, previous, or delete as `POST` or `DELETE` calls. Do not update the UI from their `204` responses. Wait for the next `state` message.
