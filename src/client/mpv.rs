use std::{os::unix::fs::DirBuilderExt, path::PathBuf, process::Stdio, time::Duration};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    net::{
        UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    process::{Child, Command},
};

use super::{PlayerState, PlayerTrack};

pub struct Mpv {
    child: Child,
    directory: PathBuf,
    reader: Lines<BufReader<OwnedReadHalf>>,
    writer: OwnedWriteHalf,
    request_id: u64,
    indexed_loadfile: bool,
}

impl Mpv {
    pub async fn launch(token: &str, headless: bool) -> Result<Self> {
        let directory = std::env::temp_dir().join(format!("watchmpv-{}", uuid::Uuid::new_v4()));
        std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
        let socket = directory.join("ipc.sock");
        let mut command = Command::new("mpv");
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
            .arg(format!("--input-ipc-server={}", socket.display()));
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
                let _ = std::fs::remove_dir_all(&directory);
                return Err(error).context("Start mpv; install mpv 0.35+ and put it on PATH");
            }
        };
        let connection = async {
            for _ in 0..100 {
                if let Ok(stream) = UnixStream::connect(&socket).await {
                    return Ok(stream);
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
                let _ = std::fs::remove_dir_all(&directory);
                return Err(error);
            }
        };
        let (reader, writer) = stream.into_split();
        let mut mpv = Self {
            child,
            directory,
            reader: BufReader::new(reader).lines(),
            writer,
            request_id: 0,
            indexed_loadfile: false,
        };
        // Transfer credentials over the private IPC socket, never command-line arguments.
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
        self.command(json!(["set_property", property, value])).await
    }

    pub async fn get(&mut self, property: &str) -> Result<Option<Value>> {
        let response = self.request(json!(["get_property", property])).await?;
        Ok((response["error"] == "success").then(|| response["data"].clone()))
    }

    pub async fn load(&mut self, url: &str, position: f64) -> Result<()> {
        let options = json!({"start": position.to_string(), "pause": "yes"});
        if self.indexed_loadfile {
            self.command(json!(["loadfile", url, "replace", -1, options]))
                .await
        } else {
            self.command(json!(["loadfile", url, "replace", options]))
                .await
        }
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

impl Drop for Mpv {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}
