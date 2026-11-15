# Changelog

## 0.3.1

- Fixed native mpv playback controls for guests: Space, seek keys, and on-screen-controller pause/seek changes now become shared room commands, rather than being corrected away. Host native input uses the same explicit control path.
- Capture unsolicited mpv seek events; evaluate their landing positions after seeking finishes and track application-issued pauses/seeks to avoid synchronization feedback. Loading/quality switching is disarmed until alignment.
- Keep native input intact while awaiting its server revision; withhold follower corrections and host measurements until acknowledgement, with a bounded fallback on missing acknowledgement.
- Added two-real-mpv regression coverage using actual keypresses and raw absolute/relative/percentage seeks, plus checks for exactly-once controls, managed drift correction, and local volume/language choices.
- Client-only fix: compatible with v0.3.0 servers; rebuild/update clients to enable native-player controls. Windows and Unix share the same event handling over their existing transports.

## 0.3.0

- Every connected participant can now play, pause, and seek through the companion controls. Explicit shared commands increment a control revision; stale host measurements cannot overwrite them. Commands and host handoff broadcast promptly.
- Added Windows desktop support with mpv named-pipe IPC, a shared cross-platform JSON/player implementation, optional `WATCHMPV_MPV_PATH`, portable adjacent `mpv.exe` lookup, and no extra release-client console.
- Added Windows named-pipe tests, native Windows CI builds/tests and executable artifacts, and `docs/windows.md` setup instructions. Native Windows runtime validation runs in CI, not on the Linux development machine.
- Added guest-to-host mpv playback tests and stale-measurement regression tests. Shared transcode cancellation/retry remains host-only.
- Rebuild both server and clients when upgrading: the v0.3 synchronization protocol is not compatible with v0.2.
- Added the explicit `--transcode-dir` option and `WATCHMPV_TRANSCODE_DIR` environment setting for storing temporary/completed transcodes on another drive. `--cache-dir` remains a compatible alias and the default location is unchanged.
- Resolve/log the storage directory and verify it is writable at startup, with actionable storage-path errors.
- Added CLI compatibility and separate-storage output tests. Progressive playback during encoding is not part of this change.

## 0.2.0

- Changed the project license from MIT to GNU GPL version 3 only (`GPL-3.0-only`).

### Transcoding usability and recovery

- Live server-wide rendition inventory with queued, encoding, cancelling, ready, failed, and cancelled states.
- FFmpeg progress percentage, encode speed, and estimated time remaining in the desktop UI.
- Connected-host-only cancellation and retry, with a five-second per-quality retry cooldown.
- Queued cancellation avoids starting an encoder. Running cancellation terminates/reaps FFmpeg and removes partial output before a retry can start.
- Ordinary preparation/status requests never restart failed or cancelled jobs.
- Viewers retain their current stream during preparation/failure and automatically observe a host retry for the quality they are waiting for.
- A keep-current-quality button abandons only a viewer's pending switch, not a shared server job.
- Real NVENC startup probing: strict `nvenc` fails fast; opt-in `auto` selects NVENC or reports CPU fallback. Default remains CPU; no silent mid-job fallback.
- Bounded FFmpeg diagnostic capture and a scrollable control panel.
- Added real-process cancellation, cache/retry, authorization/cooldown, encoder-probe, progress-parser, and two-mpv failure/retry tests.

Rebuild both server and clients when upgrading. Completed rendition caches remain compatible with v0.1.

## 0.1.0

Initial Rust server/native companion release: password-protected original and transcoded media streaming, CPU/NVENC encoding, synchronized mpv playback, per-viewer quality/language selection, and host handoff.
