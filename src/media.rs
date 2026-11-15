mod jobs;
mod transcode;

pub use jobs::{JobControlError, MediaStore};
pub use transcode::Encoder;

use std::{collections::HashMap, path::Path, time::Duration};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tokio::process::Command;

use crate::protocol::{MediaInfo, TrackInfo};

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
