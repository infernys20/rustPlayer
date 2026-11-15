# Watchmpv

A Rust watch-together companion for **mpv**, with a password-protected media server. Guests do **not** need the video file: the server sends it over HTTP, and a WebSocket connection keeps playback synchronized.

This is an early development build (v0.3), not a production release. The client provides a native connection/control menu and opens video in a **separate mpv window**. Embedding libmpv into the application is a later milestone.

## Implemented

- Server address, display name, and password connection menu.
- One local video / one watch room per server process; up to 32 connected viewers.
- Authenticated, seekable HTTP streaming; no file copy required on clients.
- Original, 1080p, 720p, and 480p quality choices, with no upscaling.
- Shared, on-demand FFmpeg transcode jobs using CPU (`libx264`) or NVIDIA **NVENC** (`h264_nvenc`), with an opt-in automatic encoder mode.
- A live transcoding panel showing queued/encoding/cancelling states, progress, speed, and estimated time remaining. The connected host can cancel or retry shared jobs; guests can keep waiting or stay on their current quality.
- Independent audio/subtitle choices on each viewer. Prepared Matroska renditions retain all source audio/subtitle languages and font attachments. Audio is converted to AAC; subtitles are copied, not burned in. This supports formats such as ASS and PGS when the installed FFmpeg/mpv support them.
- Shared play/pause/seek controls for every connected participant, from either mpv's keyboard/on-screen player controls or the companion window, plus late joining, clock-offset estimation, and drift correction. Revision-tagged host measurements cannot overwrite newer shared commands. Small errors use ±3% playback speed; larger errors seek.
- Windows desktop support through mpv named-pipe IPC, alongside Unix sockets on Linux/macOS.
- Automatic host handoff to the oldest connected guest, with playback paused on handoff.
- Argon2 password hashes, random 12-hour bearer sessions, bounded login attempts, authenticated media/prepare/WebSocket endpoints, and local mpv IPC (private Unix sockets; see Windows pipe security notes in the Windows guide). Tokens are not passed in URLs or mpv command-line arguments.

### Important first-version trade-offs

**Transcoded qualities are prepared in full before switching.** Original quality starts immediately. While another quality is queued/encoding, the existing stream keeps playing. Once ready, viewers can seek anywhere in the prepared file. This is deliberately simpler and more reliable than treating an unfinished live encode as a seekable movie. It does mean a long initial wait and disk use for each requested rendition. Jobs are shared per quality and run one at a time; existing cached renditions are reused after restart.

All audio/subtitle tracks travel in the selected file, allowing per-viewer selection without a separate encode for every language combination. This is not yet bandwidth-optimal for sources with many tracks.

Buffering policy: slow guests catch up independently; **host buffering pauses the shared timeline** until the host recovers. There is no wait-for-all policy yet. Original streams use the source bitrate; quality profiles use resolution + constant-quality encoding, not strict bandwidth caps.

## Requirements

- A current Rust stable toolchain, including Cargo.
- Server: `ffmpeg` and `ffprobe` on `PATH`, with `libx264` and AAC support.
- Client: **mpv 0.35+** with JSON IPC support, and a Windows or Linux/macOS desktop environment. Put mpv on `PATH` or set `WATCHMPV_MPV_PATH` to its executable. Windows also supports `mpv.exe` beside the client. Linux is tested locally; native Windows tests/builds are configured in CI; macOS is not yet verified. See [Windows setup](docs/windows.md) for PowerShell instructions, tool installation, and named-pipe security notes.
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

For Windows prerequisites, `.exe` output paths, and PowerShell commands, see **[docs/windows.md](docs/windows.md)**. Windows and Unix clients can watch together on the same server.

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

Encoder modes:

- `--encoder cpu` (default): use portable CPU encoding without probing NVIDIA hardware.
- `--encoder nvenc`: perform a real one-frame NVENC encode at startup and fail early if it is unusable. This strict mode never silently uses CPU resources.
- `--encoder auto`: run the same probe and prefer NVENC; if the probe fails or times out, select CPU and show the fallback in the server log and client transcoding panel.

For automatic selection, replace `--encoder nvenc` in the command above with `--encoder auto`. The decision is made **once at startup**; this does not retry source-specific failures or GPU failures mid-encode on CPU. Restart with `--encoder cpu` if necessary. This version uses GPU **encoding**, not GPU decode/scaling. Server diagnostics are available with `RUST_LOG=watchmpv=debug`.

### Store transcodes on another drive

Use **`--transcode-dir`** to choose the directory for **both temporary encodes and completed renditions**. For example:

```sh
./target/release/watchmpv-server \
  --media '/path/to/movie.mkv' \
  --password-file '/path/to/server.password' \
  --listen 0.0.0.0:8787 \
  --encoder auto \
  --transcode-dir '/mnt/second-drive/watchmpv-transcodes'
```

Alternatively set `WATCHMPV_TRANSCODE_DIR=/mnt/second-drive/watchmpv-transcodes` in the server environment. An explicit command-line path takes precedence. The old **`--cache-dir`** flag remains an alias, and the default is still `.watchmpv/cache` relative to the server's working directory.

The server creates the directory if needed, checks it can write there, and logs the resolved absolute per-video directory at startup. Both partial and completed video files stay on the chosen drive; finishing an encode only renames the file within that directory. Original media is not moved or copied there. The directory is a server/operator setting, not a guest-selectable path.

**Mount the drive before starting the server** and ensure the server account can write to it. A path under an unmounted mount point can otherwise refer to the system drive. Allow enough free space for each requested rendition; a successful startup write check does not reserve space for an entire video.

The cache key includes source path, size, modification time, encoder, and profile version. Do not modify a source file while hosting it. Existing files are not automatically moved when the directory changes; completed caches can be moved while the server is stopped, retaining their per-video subdirectories. There is no automatic eviction yet; stop the server and remove old cache directories to reclaim disk space. Do not share a writable cache between untrusted users.

## Join

```sh
./target/release/watchmpv-client
# Or during development:
cargo run --features client --bin watchmpv-client
```

1. Enter the server's LAN IP/hostname and port (e.g. `192.168.1.20:8787`) or an HTTPS URL (`https://watch.example.com`). A bare hostname without a port defaults to port 8787. Explicit HTTP/HTTPS URLs use their standard ports unless a port is given.
2. Enter your name and the server password.
3. The first connected viewer becomes host. Everyone initially joins paused.
4. **Every connected participant** can play, pause, and seek using **either the mpv player itself or the companion window**. Space toggles playback; arrow-key and on-screen timeline seeks are shared too. The server orders simultaneous commands; the last accepted command wins. Synchronization corrections and quality loads are filtered so they do not get broadcast as fresh player input. Quality, volume, audio, and subtitle choices affect only you.
5. Expand **Server transcoding** to see preparation progress for all qualities. The host can cancel queued/running jobs or retry failed/cancelled jobs. Cancellation affects everyone waiting for that quality, but never stops a completed stream. A retry is limited to once per five seconds per quality; viewers still waiting switch automatically once it succeeds. **Keep current quality** stops your own pending switch without cancelling the shared job.
6. The password field is cleared after connecting. If the connection drops, re-enter the password to reconnect; playback stops instead of continuing unsynchronized.

The server must be reachable by every client. For LAN use allow TCP port 8787 through the server firewall. For remote use, prefer HTTPS or a trusted VPN rather than exposing unencrypted HTTP.

### Native-player controls fix (v0.3.1)

v0.3.0 shared companion-window controls but still corrected a guest's native mpv input away. **Update/rebuild the clients** for the fix; the v0.3.1 client remains compatible with a v0.3.0 server and uses the same shared-control protocol. No media/cache migration is needed.

### Upgrading from v0.1/v0.2

Rebuild/restart **both server and clients**: v0.3 changes the synchronization protocol to distinguish shared controls from revision-tagged host measurements. Old clients are not compatible with this protocol. v0.2 also added inventory/control endpoints and `idle`/`cancelled` job states. Existing password files, CLI media paths, and completed CPU/NVENC cache files remain compatible. Failed/cancelled jobs no longer require a server restart to retry.

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

Tests require FFmpeg/FFprobe; all-feature tests also require mpv 0.35+. They generate disposable multi-audio/multi-subtitle video fixtures. No display or GPU is needed: mpv tests use null video/audio outputs. Player-control regression tests issue real mpv keypresses (Space/Right) and raw absolute/relative/percentage seek commands, not just companion-window actions. They also verify that managed drift-correction seeks do not move the room and that native volume/language changes stay local. Tests cover authentication, byte ranges, metadata/language preservation, CPU transcoding, queued/running cancellation and partial-file cleanup, retry cooldowns and host permissions, progress parsing, automatic encoder probing/fallback, duplicate sessions, clock ping/pong, shared guest controls and stale-report rejection, host handoff, and actual synchronized mpv playback/quality switching after a failed preparation and host retry. Hardware NVENC execution must be validated separately on an NVIDIA machine.

## Structure

```text
src/bin/server.rs       Server CLI and startup
src/bin/client.rs       Desktop entry point
src/server.rs           HTTP authentication, streams, and WebSocket transport
src/media.rs            FFprobe inspection
src/media/jobs.rs       Shared job lifecycle, cancellation, retry, and atomic cache
src/media/transcode.rs  Encoder probing, FFmpeg arguments, progress, bounded diagnostics
src/room.rs             Authoritative playback timeline and host election
src/protocol.rs         Typed wire protocol shared by both applications
src/sync.rs             Drift-correction policy
src/client/ui.rs       Native egui menus and playback controls
src/client/mod.rs      Network/player synchronization actor
src/client/transcoding.rs  Asynchronous inventory/control HTTP requests
src/client/mpv.rs      mpv process, JSON IPC events, and managed seek/pause wrappers
src/client/native_controls.rs  Native-player input detection and sync-feedback suppression
src/client/ipc.rs      Unix socket / Windows named-pipe transport
```

See [docs/architecture.md](docs/architecture.md) for protocol and synchronization details, and [docs/roadmap.md](docs/roadmap.md) for the remaining milestones.

## License

Copyright (c) 2026 Watchmpv contributors.

Watchmpv is free software licensed under the **GNU General Public License, version 3 only** (`GPL-3.0-only`). You may redistribute and modify it under those terms. It is provided without warranty; see [LICENSE](LICENSE) for the full license text. Third-party dependencies retain their respective licenses.
