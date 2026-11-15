# Architecture

## Data flow

```text
Local movie ── FFprobe ──> media/track metadata
     │
     ├── authenticated HTTP Range ─────────────────> original-quality mpv clients
     │
     └── FFmpeg (CPU or NVENC) ──> atomic MKV cache ─> lower-quality mpv clients

mpv <── local JSON IPC ── desktop actor <── WebSocket ──> authoritative room
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
| POST | `/api/prepare` | `{ "quality": "p720" }` → start an idle rendition, otherwise return its existing status |
| GET | `/api/encoding` | Selected encoder, automatic-mode flag, and sanitized fallback reason |
| GET | `/api/renditions` | Read-only status/progress for all four quality choices |
| POST | `/api/renditions/{quality}/cancel` | Connected-host-only cancellation of a shared job |
| POST | `/api/renditions/{quality}/retry` | Connected-host-only retry of a failed/cancelled job |
| GET/HEAD | `/api/stream/{quality}` | Seekable original or completed rendition; supports byte ranges |
| GET | `/api/ws` | Authenticated WebSocket upgrade |

Quality values: `original`, `p1080`, `p720`, `p480`. The API never accepts a source path, executable name, arbitrary filter, or FFmpeg argument from a client. Only the server operator chooses the source and encoder. Preparation is idempotent; multiple requests share the same job, and ordinary preparation requests never restart failed/cancelled jobs. Only the connected host can retry or cancel; the room guard serializes these operations with host handoff. Retry attempts for a quality must be at least five seconds apart (HTTP 429 with `Retry-After: 5` otherwise). Repeated retry requests against an active or ready rendition are idempotent. Clients poll the read-only inventory once per second, with HTTP work running outside the playback loop. An unprepared stream returns HTTP 409.

The JSON/body limit is 16 KiB, WebSocket messages/frames are limited to 4 KiB, and each socket is limited to 30 messages per second. There is a 32-participant room limit and a 128-session limit (including disconnected sessions until expiry). There is no CORS permission, and browser-Origin WebSocket upgrades are rejected; this API currently targets the native client only.

## WebSocket messages

Client → server:

```json
{ "type": "control", "paused": false }
{ "type": "control", "position": 42.5 }
{ "type": "playback", "position": 42.5, "paused": false, "control_revision": 3 }
{ "type": "ping", "client_time_ms": 1790000000000.0 }
```

Every connected participant can send explicit `control` commands. `position` and `paused` are optional, but at least one must be specified; omitted fields preserve the current state. A pause-only command samples the server's projected position, avoiding rewinds from a stale client sample. Commands are applied atomically under the room mutex, and the last accepted command wins. Each accepted command increments `control_revision`.

Only the host sends periodic `playback` **measurements**. Their `control_revision` must match the current room control revision; stale or future-revision measurements are silently discarded, preventing in-flight reports from undoing another participant's command. Invalid positions and guest measurements are rejected. Positions must be finite and within the source duration (with the existing one-second end tolerance, clamped at the end). The server projects the latest accepted position using a monotonic `Instant`, not wall time.

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
    "revision": 8,
    "control_revision": 3
  }
}
```

The server sends snapshots every 500 ms, and promptly after controls, membership changes, or host handoff. A bounded broadcast notification wakes socket tasks; lagging tasks send the latest state, not an obsolete queued snapshot. It also sends `pong` with the echoed client timestamp and server wall-clock timestamp, or `error` with a human-readable message. A socket with no incoming messages for 20 seconds is removed; clients send pings every two seconds. Clients stop playback on sync loss (eight seconds without a room snapshot), socket close, or IPC failure. Native clients currently reconnect manually.

First connection becomes host. If that host disconnects, the server freezes the projected timeline, pauses playback, and elects the oldest remaining participant. A session can have only one socket; use separate logins for separate instances. Host election is separate from login, so logging in without connecting does not claim the room.

## Playback synchronization

1. Clients estimate server clock offset from the midpoint of their ping round trip, with exponential smoothing.
2. Guest target position = snapshot position + elapsed server time when playing, clamped to source duration.
3. A discrepancy up to 150 ms is tolerated. Between 150 ms and 1.5 seconds, mpv runs at 0.97× or 1.03×. Larger discrepancies seek; while paused, discrepancies over 150 ms seek immediately.
4. Every client's companion controls send explicit commands to the server rather than changing mpv locally and hoping a later measurement propagates. The host still reports measured position and pause/buffering state every 500 ms.
5. New loads, leader changes, and changes in `control_revision` trigger alignment, including on the host. No host measurement is sent from the pre-alignment sample, and measurements carry the revision that the player applied. This prevents stale reports overwriting shared controls. Host handoff also increments the control revision.
6. Qualities share the complete source timeline. A quality switch loads the prepared file near the current server position, then realigns. Explicitly selected track IDs are reapplied when those tracks exist in the new rendition; per-type track order is retained.

This is approximate real-time synchronization, not frame-accurate network playback. Clock asymmetry, mpv cache behavior, audio output latency, and long media seeks can affect convergence. The host itself is not continuously corrected against its own delayed reports. Playback speed is intended to remain 1× except for guest drift correction.

## Desktop IPC platforms

The mpv process and JSON protocol share one implementation over split async read/write halves. Unix uses a socket inside an owner-only mode-0700 temporary directory, removed on drop. Windows uses a unique `\\.\pipe\watchmpv-<UUID>` named pipe, with Tokio's identification-level client security; missing/busy pipes are retried during startup. The launcher suppresses an extra Windows console and accepts `WATCHMPV_MPV_PATH`; Windows also checks for `mpv.exe` beside the client before PATH. Tokens are configured over IPC, not process arguments.

Windows named-pipe ACLs are controlled by mpv/Windows, not by random naming alone. See [windows.md](windows.md) for setup and the local-account trust caveat. Platform-specific pipe tests and the shared two-mpv integration test are included in the Windows CI job; cross-compilation does not replace native execution.

## Media preparation

Original media is served untouched. A prepared rendition:

- Selects the first non-cover-art video stream.
- Scales to at most the requested height, retaining aspect ratio and even dimensions.
- Encodes H.264 with libx264 CRF 22 or h264_nvenc VBR/CQ 23.
- Converts every audio track to AAC at 192 kbit/s per track.
- Copies all subtitle streams and font attachments into Matroska.
- Preserves metadata, chapters, and language labels.

Only one encode runs at once. Outputs are written to unique temporary files and atomically renamed only on success, so no viewer sees a partial MKV. FFmpeg runs without a shell, without stdin, with kill-on-drop and a 24-hour timeout. Remote clients get a generic failure message; FFmpeg diagnostics stay in operator logs, with a bounded 64 KiB stderr tail rather than unbounded accumulation.

### Transcode storage

`--transcode-dir` (legacy alias `--cache-dir`) or `WATCHMPV_TRANSCODE_DIR` selects the server's storage root. The CLI overrides the environment; the default remains `.watchmpv/cache`. The per-media cache key is unchanged. Startup creates and canonicalizes the per-media directory, writes/flushes/removes a small unique check file, and logs the absolute path; failures identify the selected path. This checks present writability, not capacity for an entire movie or whether an expected drive is mounted.

All partial files and completed renditions are created under this directory, so atomic publication never moves data from the system temporary directory to another volume. Original media stays at its configured source path. Moving existing caches is an operator task while the server is stopped; changing the flag does not automatically migrate files. Storage paths are never accepted from remote viewers or exposed through metadata endpoints.

### Job lifecycle and progress

```text
idle → preparing/queued → preparing/encoding → ready
                    ↘ preparing/cancelling → cancelled
                                         ↘ failed
cancelled or failed → explicit host retry → preparing/queued
```

The API returns `idle`, `preparing`, `ready`, `cancelled`, or `failed`. A preparing response contains a `progress` object with `phase` (`queued`, `encoding`, or `cancelling`), `fraction` (0–1), `encoded_seconds`, optional `speed`/`eta_seconds`, and the actual `encoder` (`cpu` or `nvenc`). FFmpeg's machine-readable `-progress pipe:1` output is drained while the process runs. Negative timestamps, missing/invalid speeds, and non-finite values are handled without exposing invalid JSON. Progress is estimated from encoded timestamps, so reaching 100% is not the same as being ready: only successful process exit and atomic publication mark a rendition ready.

Cancellation is a signal, not an abrupt task abort. A queued job exits without spawning FFmpeg. A running job kills and reaps FFmpeg, drains its pipes, removes its temporary file, and only then becomes cancelled and releases its encoder slot. Progress updates cannot overwrite a cancelling state. Cancellation and publication are serialized: a rendition that finishes publication first stays ready and remains playable. Failed/cancelled jobs are sticky so a guest's old preparation poll cannot unexpectedly restart them. Retrying creates a fresh attempt only after terminal cleanup. Abrupt server termination can still leave orphaned partial files, which may be deleted while the server is stopped.

Clients continue playing the active rendition while waiting for another one. Changing back to the active quality only cancels the viewer's pending switch, not the shared server job. A host retry is observed through inventory polling; all viewers still waiting for that quality automatically switch once ready, preserving the source timeline and their selected languages.

### Encoder selection

CPU remains the default. Explicit NVENC and automatic mode perform an actual one-frame H.264 NVENC encode using the same encoder options as real jobs, with a ten-second timeout. Strict `nvenc` mode fails startup if this test fails. `auto` chooses CPU on probe failure and exposes only a generic fallback reason to viewers. Source-specific failures or later GPU failures do **not** trigger silent mid-job CPU fallback. The selected encoder is fixed for the server lifetime; cache keys use the actual encoder, so CPU selected by auto can reuse existing CPU output.

This design intentionally avoids unfinished-HLS seeking problems in the first milestone. Future progressive segmented delivery needs a shared presentation timeline, keyframe-aligned renditions, seek-triggered generation and bounded caches; it cannot simply expose arbitrary encoder stdout as a fully seekable movie.
