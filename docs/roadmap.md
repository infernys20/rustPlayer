# Roadmap

## Initial vertical slice (implemented)

- [x] Rust server and native Rust/egui companion menus.
- [x] Server IP/hostname and password login.
- [x] Stream a server-local video to guests without local copies.
- [x] Authenticated HTTP seeking and WebSocket synchronization.
- [x] Host-controlled play/pause/seeking and host handoff.
- [x] Independent quality selection with shared cached transcodes.
- [x] CPU H.264 encoding and explicit NVENC encoding mode.
- [x] Per-viewer audio/subtitle selection; preserve multiple source languages.
- [x] Real FFmpeg + two-headless-mpv integration coverage.

## Next: media delivery

- [ ] Progressive, keyframe-aligned segmented delivery with fast first-frame startup.
- [ ] Seek-aware generation of missing segments on the shared presentation timeline.
- [ ] Separate reusable audio renditions and subtitle sidecars to avoid sending every language to everyone.
- [ ] Bitrate-constrained quality profiles and automatic quality selection.
- [ ] Preparation progress, cancellation/retry controls, disk quotas, and LRU eviction.
- [ ] NVENC capability probing and configurable automatic CPU fallback.
- [ ] Optional hardware decode/scaling, with explicit GPU concurrency budgets.
- [ ] More coverage for PGS/VobSub, ASS font attachments, HDR/tone mapping, rotation, and unusual timestamps.

## Next: desktop UX

- [ ] Embed libmpv (render API) in the native window; currently playback uses a separate mpv process/window.
- [ ] Windows named-pipe IPC or cross-platform embedded-player integration.
- [ ] Remember non-secret connection preferences and recent servers.
- [ ] Reconnect/backoff and secure credential persistence in the OS keyring, if desired.
- [ ] Loading/readiness indicators, accurate network/drift statistics, and quality preparation progress.
- [ ] Media library selection by host, playlists, and external subtitle uploads.
- [ ] Packaging/installers and verified macOS support.

## Next: room policy and operations

- [ ] Configurable wait-for-all versus independent catch-up buffering policy.
- [ ] Explicit host transfer, host-only moderation, shared-control permission setting.
- [ ] Multi-room server, per-room passwords, optional individual accounts/revocation.
- [ ] Per-IP login throttling in addition to the global bound, with trusted-proxy configuration.
- [ ] Graceful WebSocket shutdown, session renewal, structured metrics, and resource/load tests.
- [ ] Bundled TLS/deployment tooling; currently TLS is provided by a reverse proxy.
- [ ] Network-simulation tests for jitter, packet loss, asymmetric latency, and stalled clients.
