# Architecture

## Data flow

```text
Local movie ── FFprobe ──> media/track metadata
     │
     ├── authenticated HTTP Range ─────────────────> original-quality mpv clients
     │
     └── FFmpeg (CPU or NVENC) ──> atomic MKV cache ─> lower-quality mpv clients

mpv <── private JSON IPC ── desktop actor <── WebSocket ──> authoritative room
                                     ↑
                             native connection/menu UI
```

The UI never parses media itself and never owns the server password after login. The client actor authenticates, launches mpv, controls its network headers over IPC, and exchanges typed room messages. mpv makes its own authenticated byte-range requests. The HTTP client refuses redirects, avoiding accidental login redirection. Server streams do not redirect.

## HTTP API

Only `/health` and `/api/login` are public. All other endpoints require `Authorization: Bearer <token>`. Successful protected responses use `Cache-Control: private, no-store`.

| Method | Path | Purpose |
|---|---|---|
| GET | `/health` | Minimal health check, no source metadata |
| POST | `/api/login` | `{ "name": "Alice", "password": "…" }` → session ID, token, media metadata, expiry |
| GET | `/api/media` | Source duration, resolution, audio/subtitle metadata |
| POST | `/api/prepare` | `{ "quality": "p720" }` → `preparing`, `ready`, or `failed` |
| GET/HEAD | `/api/stream/{quality}` | Seekable original or completed rendition; supports byte ranges |
| GET | `/api/ws` | Authenticated WebSocket upgrade |

Quality values: `original`, `p1080`, `p720`, `p480`. The API never accepts a source path, executable name, arbitrary filter, or FFmpeg argument from a client. Only the server operator chooses the source and encoder. Preparation is idempotent; polling the same quality shares the same job. A failed job remains failed until restart to prevent repeated resource-consuming retries. Preparing clients poll every two seconds. An unprepared stream returns HTTP 409.

The JSON/body limit is 16 KiB, WebSocket messages/frames are limited to 4 KiB, and each socket is limited to 30 messages per second. There is a 32-participant room limit and a 128-session limit (including disconnected sessions until expiry). There is no CORS permission, and browser-Origin WebSocket upgrades are rejected; this API currently targets the native client only.

## WebSocket messages

Client → server:

```json
{ "type": "playback", "position": 42.5, "paused": false }
{ "type": "ping", "client_time_ms": 1790000000000.0 }
```

Only the current host can send playback updates. Positions must be finite and within the source duration. The server projects the latest accepted position using a monotonic `Instant`, not wall time. Positions are clamped at the end of the source.

Server → client:

```json
{
  "type": "room",
  "snapshot": {
    "leader": "<session UUID>",
    "participants": [{ "id": "<session UUID>", "name": "Alice" }],
    "position": 43.0,
    "paused": false,
    "server_time_ms": 1790000000500.0,
    "revision": 8
  }
}
```

The server sends snapshots every 500 ms. It also sends `pong` with the echoed client timestamp and server wall-clock timestamp, or `error` with a human-readable message. A socket with no incoming messages for 20 seconds is removed; clients send pings every two seconds. Clients stop playback on sync loss (eight seconds without a room snapshot), socket close, or IPC failure. Native clients currently reconnect manually.

First connection becomes host. If that host disconnects, the server freezes the projected timeline, pauses playback, and elects the oldest remaining participant. A session can have only one socket; use separate logins for separate instances. Host election is separate from login, so logging in without connecting does not claim the room.

## Playback synchronization

1. Clients estimate server clock offset from the midpoint of their ping round trip, with exponential smoothing.
2. Guest target position = snapshot position + elapsed server time when playing, clamped to source duration.
3. A discrepancy up to 150 ms is tolerated. Between 150 ms and 1.5 seconds, mpv runs at 0.97× or 1.03×. Larger discrepancies seek; while paused, discrepancies over 150 ms seek immediately.
4. The host reports measured mpv position and pause/buffering state every 500 ms. Other clients never publish their own timeline.
5. New loads and leader changes align once before a host is allowed to publish, avoiding a stale pre-load position overwriting the authoritative timeline.
6. Qualities share the complete source timeline. A quality switch loads the prepared file near the current server position, then realigns. Explicitly selected track IDs are reapplied when those tracks exist in the new rendition; per-type track order is retained.

This is approximate real-time synchronization, not frame-accurate network playback. Clock asymmetry, mpv cache behavior, audio output latency, and long media seeks can affect convergence. The host itself is not continuously corrected against its own delayed reports. Playback speed is intended to remain 1× except for guest drift correction.

## Media preparation

Original media is served untouched. A prepared rendition:

- Selects the first non-cover-art video stream.
- Scales to at most the requested height, retaining aspect ratio and even dimensions.
- Encodes H.264 with libx264 CRF 22 or h264_nvenc VBR/CQ 23.
- Converts every audio track to AAC at 192 kbit/s per track.
- Copies all subtitle streams and font attachments into Matroska.
- Preserves metadata, chapters, and language labels.

Only one encode runs at once. Outputs are written to unique temporary files and atomically renamed only on success, so no viewer sees a partial MKV. FFmpeg runs without a shell, without stdin, with kill-on-drop and a 24-hour timeout. Remote clients get a generic failure message; FFmpeg diagnostics stay in operator logs. Restarting the server permits retrying a failed job. Failed temporary files are removed; abrupt process termination can leave orphaned partial files that may be deleted while the server is stopped.

This design intentionally avoids unfinished-HLS seeking problems in the first milestone. Future progressive segmented delivery needs a shared presentation timeline, keyframe-aligned renditions, seek-triggered generation and bounded caches; it cannot simply expose arbitrary encoder stdout as a fully seekable movie.
