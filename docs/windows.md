# Windows setup

Watchmpv v0.3 supports the desktop client and server on Windows. The desktop client uses **Windows named pipes** for mpv JSON IPC; Unix builds continue using private Unix-domain sockets. Windows and Linux viewers can join the same server.

Rebuild both server and clients when upgrading from v0.2: shared playback controls use a new revision-aware protocol. A Windows installer is not included yet.

## Runtime tools

- Clients need **mpv 0.35+** (`mpv.exe`). Obtain a Windows build from the links on [mpv's installation page](https://mpv.io/installation/). Retain the DLLs shipped with your chosen build.
- Servers need **FFmpeg and FFprobe** (`ffmpeg.exe` and `ffprobe.exe`), with libx264/AAC support. Windows builds are linked from [FFmpeg's download page](https://ffmpeg.org/download.html).
- For NVENC, install working NVIDIA drivers and an FFmpeg build containing `h264_nvenc`. `--encoder auto` probes real encoding and falls back to CPU if unavailable; `--encoder nvenc` requires the probe to succeed.

Extract the tools somewhere permanent, for example:

```text
C:\Tools\mpv\mpv.exe
C:\Tools\ffmpeg\bin\ffmpeg.exe
C:\Tools\ffmpeg\bin\ffprobe.exe
```

For the current PowerShell session:

```powershell
$env:Path = "C:\Tools\mpv;C:\Tools\ffmpeg\bin;$env:Path"
mpv --version
ffmpeg -version
ffprobe -version
```

Add those directories to your user PATH in Windows settings for persistent configuration. Prefer the actual executables over package-manager wrapper scripts/shims: process cancellation needs to terminate FFmpeg/mpv itself, not only a launcher.

The client selects mpv in this order:

1. `WATCHMPV_MPV_PATH`, if set, pointing directly to the executable.
2. `mpv.exe` beside the running `watchmpv-client.exe`, for a portable folder layout.
3. `mpv` on PATH.

An explicit path can contain spaces; do not embed extra quote characters in the value:

```powershell
$env:WATCHMPV_MPV_PATH = 'C:\Tools\mpv\mpv.exe'
```

## Build

Install the current Rust **MSVC** toolchain and Visual Studio Build Tools with **Desktop development with C++** (including a Windows SDK). Open a fresh PowerShell so Cargo and the build tools are available, then run from the repository:

```powershell
cargo build --locked --release --features client --bins
```

Outputs:

```text
target\release\watchmpv-client.exe
target\release\watchmpv-server.exe
```

The release client does not open an extra console window. Video still opens in a separate mpv window. The Windows CI job builds these executables and uploads a `watchmpv-windows-x64` artifact when that job succeeds; it does not bundle mpv, FFmpeg, or their runtime files.

## Join from Windows

```powershell
.\target\release\watchmpv-client.exe
```

Enter the server address, display name, and password in the companion window. All connected participants can use **Play/Pause** and **Seek** in either the companion window or mpv itself (Space, seek keys, and the on-screen timeline). Update the client to v0.3.1 or newer for native-player controls; a v0.3.0 server is compatible. Commands are ordered by the server and affect everyone. Audio, subtitles, and quality stay per-viewer. Only the host can cancel/retry shared transcoding jobs.

## Host on Windows

Read a password without putting its literal value in command history:

```powershell
$securePassword = Read-Host 'Server password (at least 8 bytes)' -AsSecureString
$env:WATCHMPV_PASSWORD = [System.Net.NetworkCredential]::new('', $securePassword).Password
try {
    .\target\release\watchmpv-server.exe `
        --media 'C:\Videos\movie.mkv' `
        --listen '0.0.0.0:8787' `
        --encoder auto `
        --transcode-dir 'D:\Watchmpv Transcodes'
} finally {
    Remove-Item Env:WATCHMPV_PASSWORD -ErrorAction SilentlyContinue
}
```

Alternatively use `--password-file 'C:\Private\server.password'` containing the password as UTF-8 without a BOM. Restrict its Windows file permissions to the server account; do not commit it.

`--transcode-dir` holds both temporary encodes and completed renditions, with no copy of the original media. It can point at another writable drive. The legacy `--cache-dir` alias and `WATCHMPV_TRANSCODE_DIR` setting also work. Ensure the drive is available before startup and do not unplug it while hosting.

Allow the server through Windows Defender Firewall on the intended private network. Only the media/sync TCP port (8787 by default) needs to be reachable; do not expose mpv's IPC pipe or SMB for this application. Use HTTPS through a reverse proxy or a trusted VPN outside a trusted LAN. The built-in Rust listener is HTTP/WS, not TLS.

## Security and diagnostics

- Named-pipe names are random per player instance. Windows pipe access relies on the mpv build and OS access permissions; a random name is **not** a substitute for an ACL. Use a trusted Windows login/session and do not treat local IPC as a boundary against untrusted local accounts. Unix sockets remain inside a mode-0700 directory.
- Tokens travel over IPC to configure mpv headers, not in process arguments or URLs. Tokio's Windows pipe client uses identification-level security, avoiding server impersonation of the client.
- If mpv fails to start, set `WATCHMPV_MPV_PATH` directly to a real `mpv.exe`, check the build's required DLLs, and run it manually with `--version`.
- A failed NVENC startup probe can be diagnosed with `RUST_LOG=watchmpv=debug` in the server environment. `--encoder cpu` does not require an NVIDIA GPU.

Run the same tests on Windows:

```powershell
cargo test --locked --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings
```

Tests use null mpv audio/video outputs, so they do not open playback windows. Windows-specific tests exercise real named-pipe transport; the two-mpv integration tests cover guest play/pause/seeking through both companion controls and real mpv keypress/seek commands, sync-feedback suppression, host measurement updates, quality retry, language selection, and handoff. Native Windows execution is delegated to Windows CI; a successful cross-compilation alone is not a runtime validation.
