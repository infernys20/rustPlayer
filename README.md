# Watchmpv

A Rust watch-together companion for **mpv**, with a password-protected media server. Guests do **not** need the video file: the server sends it over HTTP, and a WebSocket connection keeps playback synchronized.

This is the first working vertical slice, not a production release. The client provides a native connection/control menu and opens video in a **separate mpv window**. Embedding libmpv into the application is a later milestone.

## Implemented

- Server address, display name, and password connection menu.
- One local video / one watch room per server process; up to 32 connected viewers.
- Authenticated, seekable HTTP streaming; no file copy required on clients.
- Original, 1080p, 720p, and 480p quality choices, with no upscaling.
- Shared, on-demand FFmpeg transcode jobs using CPU (`libx264`) or NVIDIA **NVENC** (`h264_nvenc`).
- Independent audio/subtitle choices on each viewer. Prepared Matroska renditions retain all source audio/subtitle languages and font attachments. Audio is converted to AAC; subtitles are copied, not burned in. This supports formats such as ASS and PGS when the installed FFmpeg/mpv support them.
- Host play/pause/seek, late joining, clock-offset estimation, and drift correction. Small errors use ±3% playback speed; larger errors seek.
- Automatic host handoff to the oldest connected guest, with playback paused on handoff.
- Argon2 password hashes, random 12-hour bearer sessions, bounded login attempts, authenticated media/prepare/WebSocket endpoints, and private local mpv IPC. Tokens are not passed in URLs or mpv command-line arguments.

### Important first-version trade-offs

**Transcoded qualities are prepared in full before switching.** Original quality starts immediately. While another quality is queued/encoding, the existing stream keeps playing. Once ready, viewers can seek anywhere in the prepared file. This is deliberately simpler and more reliable than treating an unfinished live encode as a seekable movie. It does mean a long initial wait and disk use for each requested rendition. Jobs are shared per quality and run one at a time; existing cached renditions are reused after restart.

All audio/subtitle tracks travel in the selected file, allowing per-viewer selection without a separate encode for every language combination. This is not yet bandwidth-optimal for sources with many tracks.

Buffering policy: slow guests catch up independently; **host buffering pauses the shared timeline** until the host recovers. There is no wait-for-all policy yet. Original streams use the source bitrate; quality profiles use resolution + constant-quality encoding, not strict bandwidth caps.

## Requirements

- A current Rust stable toolchain, including Cargo.
- Server: `ffmpeg` and `ffprobe` on `PATH`, with `libx264` and AAC support.
- Client: **mpv 0.35+** on `PATH`, with JSON IPC support, and a Linux/macOS desktop environment. The initial client uses Unix sockets; Windows named pipes are not implemented. Linux is the tested platform; macOS is not yet verified.
- For NVENC: a supported NVIDIA GPU, working NVIDIA drivers, and an FFmpeg build with `h264_nvenc`. CUDA/NVIDIA runtime libraries must be available to FFmpeg. Encoding support listed by FFmpeg does not by itself prove usable hardware exists.

Example Debian/Ubuntu runtime packages:

```sh
sudo apt install ffmpeg mpv
```

Building the desktop UI also requires the platform's window-system development libraries. On Debian/Ubuntu:

```sh
sudo apt install build-essential pkg-config libxkbcommon-dev libwayland-dev libegl1-mesa-dev libgl1-mesa-dev
```

## Build

Server only (does not pull in desktop UI dependencies):

```sh
cargo build --release --bin watchmpv-server
```

Desktop client and server:

```sh
cargo build --release --features client --bins
```

## Start a server

Supply a strong password through the environment, not a command-line flag:

```sh
read -rsp 'Server password (at least 8 bytes): ' WATCHMPV_PASSWORD; echo
export WATCHMPV_PASSWORD
./target/release/watchmpv-server \
  --media '/path/to/movie.mkv' \
  --listen 0.0.0.0:8787 \
  --encoder cpu
unset WATCHMPV_PASSWORD
```

Alternatively use `--password-file /path/to/server.password` (UTF-8; trailing CR/LF are removed). Keep it outside version control with owner-only permissions, e.g. `chmod 600 server.password`. No default password is provided. The default listen address is loopback (`127.0.0.1:8787`); use `0.0.0.0:8787` explicitly for LAN access.

NVIDIA encoding:

```sh
./target/release/watchmpv-server \
  --media '/path/to/movie.mkv' \
  --password-file '/path/to/server.password' \
  --listen 0.0.0.0:8787 \
  --encoder nvenc
```

Test the actual encoder before hosting:

```sh
ffmpeg -hide_banner -f lavfi -i 'testsrc2=size=640x480:rate=30' \
  -t 1 -c:v h264_nvenc -f null -
```

`--encoder cpu` is the portable fallback. NVENC failures are reported rather than silently using CPU resources. This version uses GPU **encoding**, not GPU decode/scaling. Server diagnostics are available with `RUST_LOG=watchmpv=debug`.

Prepared files live under `.watchmpv/cache` by default (`--cache-dir` changes this). The cache key includes source path, size, modification time, encoder, and profile version. Do not modify a source file while hosting it. There is no automatic eviction yet; stop the server and remove old cache directories to reclaim disk space. Choose a cache with enough space and do not share a writable cache between untrusted users.

## Join

```sh
./target/release/watchmpv-client
# Or during development:
cargo run --features client --bin watchmpv-client
```

1. Enter the server's LAN IP/hostname and port (e.g. `192.168.1.20:8787`) or an HTTPS URL (`https://watch.example.com`). A bare hostname without a port defaults to port 8787. Explicit HTTP/HTTPS URLs use their standard ports unless a port is given.
2. Enter your name and the server password.
3. The first connected viewer becomes host. Everyone initially joins paused.
4. Use the companion window to play, pause, seek, change quality, and select an audio/subtitle language. Guests cannot change the shared timeline. Playback controls inside a guest's mpv window will be corrected back to the host state.
5. The password field is cleared after connecting. If the connection drops, re-enter the password to reconnect; playback stops instead of continuing unsynchronized.

The server must be reachable by every client. For LAN use allow TCP port 8787 through the server firewall. For remote use, prefer HTTPS or a trusted VPN rather than exposing unencrypted HTTP.

## Internet deployment / TLS

The built-in listener speaks HTTP/WS. **Do not send passwords, session tokens, or media over plain HTTP on an untrusted network.** Terminate HTTPS/WSS at a reverse proxy and keep the Rust server on loopback. For example, a Caddy configuration:

```caddyfile
watch.example.com {
    reverse_proxy 127.0.0.1:8787
}
```

Start the Rust server with `--listen 127.0.0.1:8787` and connect to `https://watch.example.com`. Caddy handles certificates and WebSocket upgrades. Preserve HTTP byte-range requests/responses and allow long-lived media/WebSocket connections. Both the Rust client and mpv verify TLS certificates; use a publicly trusted certificate, not an ad-hoc self-signed one.

There are no accounts, individual access revocation, or DRM. Anyone with the password can join; an authenticated viewer can download the video. Session credentials expire after 12 hours and are invalidated on server restart. Login throttling is global (20 attempts/minute, two simultaneous password verifications), suitable for small private groups rather than a public multi-tenant service.

## Tests and checks

```sh
cargo fmt --all -- --check
cargo test                         # includes real FFmpeg server integration tests
cargo test --all-features          # also launches two headless mpv clients
cargo clippy --all-targets --all-features -- -D warnings
```

Tests require FFmpeg/FFprobe; all-feature tests also require mpv 0.35+. They generate disposable multi-audio/multi-subtitle video fixtures. No display or GPU is needed: mpv tests use null video/audio outputs. Tests cover authentication, byte ranges, metadata/language preservation, CPU transcoding, duplicate sessions, host-only commands, clock ping/pong, host handoff, and actual synchronized mpv playback/quality switching. Hardware NVENC execution must be validated separately on an NVIDIA machine.

## Structure

```text
src/bin/server.rs       Server CLI and startup
src/bin/client.rs       Desktop entry point
src/server.rs           HTTP authentication, streams, and WebSocket transport
src/media.rs            FFprobe inspection, cache, FFmpeg CPU/NVENC jobs
src/room.rs             Authoritative playback timeline and host election
src/protocol.rs         Typed wire protocol shared by both applications
src/sync.rs             Drift-correction policy
src/client/ui.rs       Native egui menus and playback controls
src/client/mod.rs      Network/player synchronization actor
src/client/mpv.rs      Private mpv process and JSON IPC
```

See [docs/architecture.md](docs/architecture.md) for protocol and synchronization details, and [docs/roadmap.md](docs/roadmap.md) for the remaining milestones.
