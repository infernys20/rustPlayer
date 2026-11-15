# Changelog

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
