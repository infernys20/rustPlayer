use std::{process::Stdio, time::Duration};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader},
    process::Command,
    sync::watch,
};

use crate::protocol::{
    EncodingInfo, PreparationPhase, PreparationProgress, RenditionStatus, VideoEncoder,
};

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Encoder {
    Cpu,
    /// Require usable NVIDIA hardware; fail startup if the probe fails.
    Nvenc,
    /// Probe NVENC at startup and use CPU if it is unavailable.
    Auto,
}

impl Encoder {
    pub async fn resolve(self) -> Result<EncodingInfo> {
        let probe = if matches!(self, Self::Cpu) {
            Ok(())
        } else {
            probe_nvenc().await
        };
        resolve_probe(self, probe)
    }
}

fn resolve_probe(mode: Encoder, probe: Result<()>) -> Result<EncodingInfo> {
    let (encoder, fallback_reason) = match (mode, probe) {
        (Encoder::Cpu, _) => (VideoEncoder::Cpu, None),
        (Encoder::Nvenc | Encoder::Auto, Ok(())) => (VideoEncoder::Nvenc, None),
        (Encoder::Nvenc, Err(error)) => {
            return Err(error).context("NVENC is not usable; use --encoder cpu or --encoder auto");
        }
        (Encoder::Auto, Err(error)) => {
            tracing::warn!(%error, "NVENC probe failed; automatic mode selected CPU encoding");
            (
                VideoEncoder::Cpu,
                Some("NVENC is unavailable; automatic mode selected CPU encoding.".into()),
            )
        }
    };
    Ok(EncodingInfo {
        encoder,
        automatic: matches!(mode, Encoder::Auto),
        fallback_reason,
    })
}

pub fn encoder_args(encoder: VideoEncoder) -> &'static [&'static str] {
    match encoder {
        VideoEncoder::Cpu => &["-c:v", "libx264", "-preset", "fast", "-crf", "22"],
        VideoEncoder::Nvenc => &[
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
        ],
    }
}

async fn probe_nvenc() -> Result<()> {
    // Actually open an encoder, rather than merely checking FFmpeg's encoder list.
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        Command::new("ffmpeg")
            .kill_on_drop(true)
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=320x240:r=10",
                "-frames:v",
                "1",
            ])
            .args(encoder_args(VideoEncoder::Nvenc))
            .args(["-pix_fmt", "yuv420p", "-an", "-f", "null", "-"])
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .context("NVENC probe timed out")?
    .context("Start FFmpeg NVENC probe")?;
    if !output.status.success() {
        bail!(
            "NVENC probe failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

/// Bound diagnostics even when a failing decoder writes continuously.
pub async fn diagnostic_tail(mut reader: impl AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    const LIMIT: usize = 64 * 1024;
    let mut tail = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        let count = reader.read(&mut chunk).await?;
        if count == 0 {
            return Ok(tail);
        }
        tail.extend_from_slice(&chunk[..count]);
        if tail.len() > LIMIT {
            tail.drain(..tail.len() - LIMIT);
        }
    }
}

#[derive(Default)]
struct ProgressParser {
    seconds: f64,
    speed: Option<f64>,
}

impl ProgressParser {
    fn line(
        &mut self,
        line: &str,
        duration: f64,
        encoder: VideoEncoder,
    ) -> Option<PreparationProgress> {
        let (key, value) = line.trim().split_once('=')?;
        match key {
            // FFmpeg's out_time_us and legacy out_time_ms both contain microseconds.
            "out_time_us" | "out_time_ms" => {
                if let Ok(micros) = value.parse::<f64>()
                    && micros.is_finite()
                {
                    // Audio timestamps can be negative at startup; progress never goes backwards.
                    self.seconds = self
                        .seconds
                        .max((micros / 1_000_000.0).clamp(0.0, duration));
                }
            }
            "speed" => {
                self.speed = value
                    .trim_end_matches('x')
                    .parse::<f64>()
                    .ok()
                    .filter(|speed| speed.is_finite() && *speed > 0.0)
            }
            "progress" => {
                return Some(PreparationProgress {
                    phase: PreparationPhase::Encoding,
                    fraction: (self.seconds / duration).clamp(0.0, 1.0),
                    encoded_seconds: self.seconds,
                    speed: self.speed,
                    eta_seconds: self
                        .speed
                        .map(|speed| (duration - self.seconds).max(0.0) / speed)
                        .filter(|eta| eta.is_finite()),
                    encoder: Some(encoder),
                });
            }
            _ => {}
        }
        None
    }
}

pub async fn read_progress(
    reader: impl AsyncRead + Unpin,
    status: watch::Sender<RenditionStatus>,
    duration: f64,
    encoder: VideoEncoder,
) -> std::io::Result<()> {
    let mut lines = BufReader::new(reader).lines();
    let mut parser = ProgressParser::default();
    while let Some(line) = lines.next_line().await? {
        if let Some(next) = parser.line(&line, duration, encoder) {
            status.send_if_modified(|state| {
                // Late progress must not erase cancellation or a terminal state.
                if let RenditionStatus::Preparing { progress } = state
                    && progress.phase == PreparationPhase::Encoding
                {
                    *progress = next;
                    true
                } else {
                    false
                }
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_nvenc_fails_but_auto_falls_back_without_hiding_details_in_client_metadata() {
        assert_eq!(
            resolve_probe(Encoder::Cpu, Ok(())).unwrap().encoder,
            VideoEncoder::Cpu
        );
        let nvenc = resolve_probe(Encoder::Auto, Ok(())).unwrap();
        assert_eq!(nvenc.encoder, VideoEncoder::Nvenc);
        assert!(nvenc.automatic);
        let auto = resolve_probe(
            Encoder::Auto,
            Err(anyhow::anyhow!("private driver diagnostic")),
        )
        .unwrap();
        assert_eq!(auto.encoder, VideoEncoder::Cpu);
        assert!(auto.fallback_reason.is_some());
        assert!(!auto.fallback_reason.unwrap().contains("private"));
        assert!(resolve_probe(Encoder::Nvenc, Err(anyhow::anyhow!("driver missing"))).is_err());
    }

    #[tokio::test]
    async fn actual_auto_probe_selects_hardware_or_reports_cpu_fallback() {
        let info = Encoder::Auto.resolve().await.unwrap();
        assert!(info.automatic);
        match info.encoder {
            VideoEncoder::Cpu => assert!(info.fallback_reason.is_some()),
            VideoEncoder::Nvenc => assert!(info.fallback_reason.is_none()),
        }
    }

    #[test]
    fn progress_handles_unknown_speeds_negative_timestamps_and_bounded_eta() {
        let mut parser = ProgressParser::default();
        parser.line("out_time_us=-50000", 100.0, VideoEncoder::Cpu);
        parser.line("speed=N/A", 100.0, VideoEncoder::Cpu);
        let start = parser
            .line("progress=continue", 100.0, VideoEncoder::Cpu)
            .unwrap();
        assert_eq!(start.fraction, 0.0);
        assert!(start.eta_seconds.is_none());
        parser.line("out_time_ms=25000000", 100.0, VideoEncoder::Cpu);
        parser.line("speed=2.0x", 100.0, VideoEncoder::Cpu);
        let progress = parser
            .line("progress=continue", 100.0, VideoEncoder::Cpu)
            .unwrap();
        assert_eq!(progress.fraction, 0.25);
        assert_eq!(progress.eta_seconds, Some(37.5));
        parser.line("out_time_us=nan", 100.0, VideoEncoder::Cpu);
        parser.line("out_time_us=1000000", 100.0, VideoEncoder::Cpu);
        parser.line("speed=inf", 100.0, VideoEncoder::Cpu);
        let next = parser
            .line("progress=end", 100.0, VideoEncoder::Cpu)
            .unwrap();
        assert_eq!(next.fraction, 0.25);
        assert!(next.speed.is_none());
        parser.line("out_time_us=200000000", 100.0, VideoEncoder::Cpu);
        assert_eq!(
            parser
                .line("progress=end", 100.0, VideoEncoder::Cpu)
                .unwrap()
                .fraction,
            1.0
        );
    }

    #[tokio::test]
    async fn diagnostics_are_bounded_and_progress_does_not_override_cancellation() {
        let data = vec![b'x'; 100_000];
        assert_eq!(
            diagnostic_tail(data.as_slice()).await.unwrap().len(),
            64 * 1024
        );
        let (status, _) = watch::channel(RenditionStatus::Preparing {
            progress: PreparationProgress {
                phase: PreparationPhase::Cancelling,
                ..Default::default()
            },
        });
        read_progress(
            b"out_time_us=1000000\nspeed=2x\nprogress=continue\n".as_slice(),
            status.clone(),
            10.0,
            VideoEncoder::Cpu,
        )
        .await
        .unwrap();
        assert!(
            matches!(&*status.borrow(), RenditionStatus::Preparing { progress } if progress.phase == PreparationPhase::Cancelling)
        );
    }
}
