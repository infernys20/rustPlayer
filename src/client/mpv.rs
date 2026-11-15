use std::{ffi::OsString, process::Stdio, time::Duration};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines, ReadHalf, WriteHalf},
    process::{Child, Command},
};

use super::{
    PlayerState, PlayerTrack,
    ipc::{Endpoint, Stream},
    native_controls::{NativeControl, NativeInput},
};

pub struct Mpv {
    child: Child,
    _endpoint: Endpoint,
    reader: Lines<BufReader<ReadHalf<Stream>>>,
    writer: WriteHalf<Stream>,
    request_id: u64,
    indexed_loadfile: bool,
    native_input: NativeInput,
}

impl Mpv {
    pub async fn launch(token: &str, headless: bool) -> Result<Self> {
        let endpoint = Endpoint::new()?;
        let mut command = Command::new(executable());
        #[cfg(windows)]
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW: do not open an extra console.
        command
            .args([
                "--no-config",
                "--load-scripts=no",
                "--ytdl=no",
                "--audio-file-auto=no",
                "--sub-auto=no",
                "--idle=yes",
                "--keep-open=yes",
                "--force-window=yes",
                "--pause=yes",
                "--speed=1",
                "--input-terminal=no",
                "--network-timeout=10",
                "--tls-verify=yes",
                "--title=Watchmpv",
            ])
            .arg(endpoint.argument());
        if headless {
            command.args(["--vo=null", "--ao=null", "--force-window=no"]);
        }
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(if headless {
                Stdio::inherit()
            } else {
                Stdio::null()
            })
            .kill_on_drop(true)
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(error) => {
                return Err(error).context("Start mpv; install mpv 0.35+ on PATH, set WATCHMPV_MPV_PATH, or place mpv.exe next to the Windows client");
            }
        };
        let connection = async {
            for _ in 0..200 {
                match endpoint.connect().await {
                    Ok(stream) => return Ok(stream),
                    Err(error) if Endpoint::retryable(&error) => {}
                    Err(error) => return Err(error).context("Connect to mpv IPC"),
                }
                if let Some(status) = child.try_wait()? {
                    bail!("mpv exited on startup: {status}");
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            bail!("Timed out connecting to mpv IPC");
        }
        .await;
        let stream = match connection {
            Ok(stream) => stream,
            Err(error) => {
                let _ = child.kill().await;
                return Err(error);
            }
        };
        let (reader, writer) = tokio::io::split(stream);
        let mut mpv = Self {
            child,
            _endpoint: endpoint,
            reader: BufReader::new(reader).lines(),
            writer,
            request_id: 0,
            indexed_loadfile: false,
            native_input: NativeInput::default(),
        };
        // Transfer credentials over IPC, never command-line arguments or media URLs.
        mpv.set(
            "http-header-fields",
            json!([format!("Authorization: Bearer {token}")]),
        )
        .await?;
        // mpv 0.38 added an insertion index before loadfile's options argument.
        let commands = mpv.get("command-list").await?.unwrap_or(json!([]));
        mpv.indexed_loadfile = commands
            .as_array()
            .into_iter()
            .flatten()
            .find(|command| command["name"] == "loadfile")
            .and_then(|command| command["args"].as_array())
            .is_some_and(|args| args.iter().any(|arg| arg["name"] == "index"));
        Ok(mpv)
    }

    async fn request(&mut self, command: Value) -> Result<Value> {
        self.request_id += 1;
        let id = self.request_id;
        let operation = format!("{} {}", command[0], command[1]);
        let mut wire = serde_json::to_vec(&json!({"command": command, "request_id": id}))?;
        wire.push(b'\n');
        tokio::time::timeout(Duration::from_secs(10), async {
            self.writer.write_all(&wire).await?;
            loop {
                let line = self
                    .reader
                    .next_line()
                    .await?
                    .context("mpv closed its IPC connection")?;
                let message: Value = serde_json::from_str(&line)?;
                if message["event"] == "seek" {
                    self.native_input.seek_event();
                }
                if message["request_id"].as_u64() == Some(id) {
                    return Ok(message);
                }
            }
        })
        .await
        .with_context(|| format!("mpv IPC timed out ({operation})"))?
    }

    pub async fn command(&mut self, command: Value) -> Result<()> {
        let response = self.request(command).await?;
        if response["error"] != "success" {
            bail!("mpv: {}", response["error"]);
        }
        Ok(())
    }

    pub async fn set(&mut self, property: &str, value: Value) -> Result<()> {
        if property == "pause"
            && let Some(paused) = value.as_bool()
        {
            self.native_input.managed_pause(paused);
        }
        self.command(json!(["set_property", property, value])).await
    }

    pub async fn get(&mut self, property: &str) -> Result<Option<Value>> {
        let response = self.request(json!(["get_property", property])).await?;
        Ok((response["error"] == "success").then(|| response["data"].clone()))
    }

    pub async fn load(&mut self, url: &str, position: f64) -> Result<()> {
        self.native_input.loading();
        let options = json!({"start": position.to_string(), "pause": "yes"});
        if self.indexed_loadfile {
            self.command(json!(["loadfile", url, "replace", -1, options]))
                .await
        } else {
            self.command(json!(["loadfile", url, "replace", options]))
                .await
        }
    }

    /// Every synchronization seek goes through this wrapper. Raw player/OSC/key commands
    /// do not, allowing their seek events to be forwarded rather than corrected away.
    pub async fn seek(&mut self, position: f64) -> Result<()> {
        self.native_input.managed_seek(position);
        self.command(json!(["seek", position, "absolute+exact"]))
            .await
    }

    pub fn arm_native_input(&mut self) {
        self.native_input.arm();
    }

    pub async fn native_control(&mut self, player: &PlayerState) -> Result<NativeControl> {
        let mut position = player.position;
        let mut seeking = player.seeking;
        if self.native_input.seek_pending() && !seeking {
            // An event can arrive during a later sample property request. Read the landing
            // position again, bracketed by seeking checks, rather than publishing a pre-seek sample.
            seeking = self
                .get("seeking")
                .await?
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            if !seeking {
                position = self.get("time-pos").await?.and_then(|v| v.as_f64());
                seeking = self
                    .get("seeking")
                    .await?
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
            }
        }
        Ok(self.native_input.observe(position, player.paused, seeking))
    }

    pub async fn sample(&mut self) -> Result<PlayerState> {
        if let Some(status) = self.child.try_wait()? {
            bail!("mpv exited: {status}");
        }
        Ok(PlayerState {
            position: self.get("time-pos").await?.and_then(|v| v.as_f64()),
            seeking: self
                .get("seeking")
                .await?
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            paused: self
                .get("pause")
                .await?
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
            buffering: self
                .get("paused-for-cache")
                .await?
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            audio: self.get("aid").await?.and_then(|v| v.as_i64()),
            subtitle: self.get("sid").await?.and_then(|v| v.as_i64()),
            tracks: Vec::new(),
        })
    }

    pub async fn tracks(&mut self) -> Result<Vec<PlayerTrack>> {
        let value = self.get("track-list").await?.unwrap_or(json!([]));
        Ok(value
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|track| {
                let kind = track["type"].as_str()?;
                if kind != "audio" && kind != "sub" {
                    return None;
                }
                Some(PlayerTrack {
                    id: track["id"].as_i64()?,
                    kind: kind.to_owned(),
                    label: [
                        track["lang"].as_str(),
                        track["title"].as_str(),
                        track["codec"].as_str(),
                    ]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" · "),
                })
            })
            .collect())
    }
}

fn executable() -> OsString {
    if let Some(path) = std::env::var_os("WATCHMPV_MPV_PATH").filter(|path| !path.is_empty()) {
        return path;
    }
    #[cfg(windows)]
    if let Some(path) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("mpv.exe")))
        && path.is_file()
    {
        return path.into_os_string();
    }
    OsString::from("mpv")
}

impl Drop for Mpv {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}
