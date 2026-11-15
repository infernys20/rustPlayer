use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::{
    process::Command,
    sync::{Mutex, Semaphore},
};

use crate::protocol::{MediaInfo, Quality, RenditionStatus, TrackInfo};

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Encoder {
    Cpu,
    Nvenc,
}

#[derive(Deserialize)]
struct Probe {
    streams: Vec<ProbeStream>,
    format: ProbeFormat,
}

#[derive(Deserialize)]
struct ProbeFormat {
    duration: Option<String>,
}

#[derive(Deserialize)]
struct ProbeStream {
    index: u32,
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    #[serde(default)]
    tags: HashMap<String, String>,
    #[serde(default)]
    disposition: HashMap<String, u32>,
}

pub async fn inspect(path: &Path) -> Result<MediaInfo> {
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_format",
                "-show_streams",
                "-of",
                "json",
            ])
            .arg(path)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("ffprobe timed out")?
    .context("Start ffprobe; install FFmpeg and ensure ffprobe is on PATH")?;
    if !output.status.success() {
        bail!(
            "ffprobe failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let probe: Probe = serde_json::from_slice(&output.stdout).context("Invalid ffprobe output")?;
    let video = probe
        .streams
        .iter()
        .find(|s| {
            s.codec_type.as_deref() == Some("video")
                && s.disposition.get("attached_pic").copied().unwrap_or(0) == 0
        })
        .context("The source does not contain a video stream")?;
    let duration: f64 = probe
        .format
        .duration
        .context("Source has no duration (live inputs are not supported)")?
        .parse()?;
    if !duration.is_finite() || duration <= 0.0 {
        bail!("Invalid media duration");
    }
    Ok(MediaInfo {
        title: path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        duration,
        width: video.width,
        height: video.height,
        tracks: probe
            .streams
            .iter()
            .filter_map(|s| {
                let kind = s.codec_type.as_deref()?;
                if kind != "audio" && kind != "subtitle" {
                    return None;
                }
                Some(TrackInfo {
                    index: s.index,
                    kind: kind.into(),
                    codec: s.codec_name.clone().unwrap_or_default(),
                    language: s.tags.get("language").cloned(),
                    title: s.tags.get("title").cloned(),
                })
            })
            .collect(),
    })
}

pub struct MediaStore {
    pub source: PathBuf,
    pub info: MediaInfo,
    cache: PathBuf,
    encoder: Encoder,
    jobs: Mutex<HashMap<Quality, RenditionStatus>>,
    slots: Semaphore,
}

impl MediaStore {
    pub async fn new(source: PathBuf, cache_root: PathBuf, encoder: Encoder) -> Result<Arc<Self>> {
        let source = tokio::fs::canonicalize(source)
            .await
            .context("Open media source")?;
        let metadata = tokio::fs::metadata(&source).await?;
        if !metadata.is_file() {
            bail!("Media source must be a regular file");
        }
        let info = inspect(&source).await?;
        let mut hash = Sha256::new();
        hash.update(format!(
            "v1|{}|{}|{:?}|{:?}",
            source.display(),
            metadata.len(),
            metadata.modified()?,
            encoder
        ));
        let cache = cache_root.join(format!("{:x}", hash.finalize()));
        tokio::fs::create_dir_all(&cache).await?;
        Ok(Arc::new(Self {
            source,
            info,
            cache,
            encoder,
            jobs: Mutex::new(HashMap::new()),
            slots: Semaphore::new(1),
        }))
    }

    pub fn path(&self, quality: Quality) -> PathBuf {
        if quality == Quality::Original {
            self.source.clone()
        } else {
            self.cache.join(format!("{}.mkv", quality.key()))
        }
    }

    pub async fn prepare(self: &Arc<Self>, quality: Quality) -> RenditionStatus {
        if quality == Quality::Original
            || tokio::fs::try_exists(self.path(quality))
                .await
                .unwrap_or(false)
        {
            return RenditionStatus::Ready;
        }
        let mut jobs = self.jobs.lock().await;
        if let Some(status) = jobs.get(&quality) {
            return status.clone();
        }
        jobs.insert(quality, RenditionStatus::Preparing);
        let store = self.clone();
        tokio::spawn(async move {
            let result = store.transcode(quality).await;
            let status = match result {
                Ok(()) => RenditionStatus::Ready,
                Err(error) => {
                    tracing::error!(?quality, %error, "Transcode failed");
                    // No source paths or FFmpeg diagnostics are exposed to remote clients.
                    RenditionStatus::Failed { message: "Transcoding failed. Ask the server operator to check logs and encoder availability.".into() }
                }
            };
            store.jobs.lock().await.insert(quality, status);
        });
        RenditionStatus::Preparing
    }

    pub async fn ready(&self, quality: Quality) -> bool {
        quality == Quality::Original
            || tokio::fs::try_exists(self.path(quality))
                .await
                .unwrap_or(false)
    }

    async fn transcode(&self, quality: Quality) -> Result<()> {
        let _slot = self.slots.acquire().await?;
        let temporary = self.cache.join(format!(
            "{}.{}.partial.mkv",
            quality.key(),
            uuid::Uuid::new_v4()
        ));
        let mut command = Command::new("ffmpeg");
        command
            .kill_on_drop(true)
            .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(&self.source)
            // V excludes attached cover art. Keep every language and font attachment.
            .args([
                "-map",
                "0:V:0",
                "-map",
                "0:a?",
                "-map",
                "0:s?",
                "-map",
                "0:t?",
                "-map_metadata",
                "0",
                "-map_chapters",
                "0",
            ])
            .args([
                "-vf",
                &format!(
                    "scale=-2:trunc(min({}\\,ih)/2)*2",
                    quality
                        .height()
                        .context("Original does not need transcoding")?
                ),
            ]);
        match self.encoder {
            Encoder::Cpu => {
                command.args(["-c:v", "libx264", "-preset", "fast", "-crf", "22"]);
            }
            Encoder::Nvenc => {
                command.args([
                    "-c:v",
                    "h264_nvenc",
                    "-preset",
                    "p4",
                    "-rc",
                    "vbr",
                    "-cq",
                    "23",
                    "-b:v",
                    "0",
                ]);
            }
        }
        command
            .args([
                "-pix_fmt", "yuv420p", "-c:a", "aac", "-b:a", "192k", "-c:s", "copy", "-c:t",
                "copy",
            ])
            .arg(&temporary);
        let result = tokio::time::timeout(Duration::from_secs(24 * 3600), command.output()).await;
        let result = match result {
            Ok(Ok(output)) if output.status.success() => {
                tokio::fs::rename(&temporary, self.path(quality))
                    .await
                    .context("Publish prepared rendition")
            }
            Ok(Ok(output)) => Err(anyhow::anyhow!(
                "FFmpeg {:?}: {}",
                self.encoder,
                String::from_utf8_lossy(&output.stderr)
            )),
            Ok(Err(error)) => Err(error).context("Start ffmpeg"),
            Err(error) => Err(error).context("Transcoding exceeded 24 hour limit"),
        };
        if result.is_err() {
            let _ = tokio::fs::remove_file(&temporary).await;
        }
        result
    }
}
