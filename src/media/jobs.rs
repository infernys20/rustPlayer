use std::{
    collections::HashMap,
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use tokio::{
    io::AsyncWriteExt,
    process::Command,
    sync::{Mutex, Semaphore, watch},
};

use super::{
    Encoder, inspect,
    transcode::{diagnostic_tail, encoder_args, read_progress},
};
use crate::protocol::*;

const RETRY_COOLDOWN: Duration = Duration::from_secs(5);

struct Job {
    status: watch::Sender<RenditionStatus>,
    cancel: watch::Sender<bool>,
    started: Instant,
}

#[derive(Debug, PartialEq, Eq)]
pub enum JobControlError {
    RetryTooSoon,
}

pub struct MediaStore {
    pub source: PathBuf,
    pub info: MediaInfo,
    pub encoding: EncodingInfo,
    cache: PathBuf,
    jobs: Mutex<HashMap<Quality, Job>>,
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
        let encoding = encoder.resolve().await?;
        let mut hash = Sha256::new();
        // Key by actual encoder: auto/cpu can reuse CPU output from the first release.
        hash.update(format!(
            "v1|{}|{}|{:?}|{:?}",
            source.display(),
            metadata.len(),
            metadata.modified()?,
            encoding.encoder
        ));
        let cache = prepare_directory(cache_root.join(format!("{:x}", hash.finalize()))).await?;
        Ok(Arc::new(Self {
            source,
            info,
            encoding,
            cache,
            jobs: Mutex::new(HashMap::new()),
            slots: Semaphore::new(1),
        }))
    }

    /// Absolute, per-media directory containing both partial and completed transcodes.
    pub fn transcode_directory(&self) -> &std::path::Path {
        &self.cache
    }

    pub fn path(&self, quality: Quality) -> PathBuf {
        if quality == Quality::Original {
            self.source.clone()
        } else {
            self.cache.join(format!("{}.mkv", quality.key()))
        }
    }

    pub async fn ready(&self, quality: Quality) -> bool {
        quality == Quality::Original
            || tokio::fs::try_exists(self.path(quality))
                .await
                .unwrap_or(false)
    }

    pub async fn status(&self, quality: Quality) -> RenditionStatus {
        if self.ready(quality).await {
            return RenditionStatus::Ready;
        }
        self.jobs
            .lock()
            .await
            .get(&quality)
            .map(|job| job.status.borrow().clone())
            .unwrap_or(RenditionStatus::Idle)
    }

    pub async fn renditions(&self) -> Vec<RenditionInfo> {
        let mut result = Vec::new();
        for quality in Quality::ALL {
            result.push(RenditionInfo {
                quality,
                status: self.status(quality).await,
            });
        }
        result
    }

    /// Idempotent: an ordinary prepare never restarts a cancelled or failed shared job.
    pub async fn prepare(self: &Arc<Self>, quality: Quality) -> RenditionStatus {
        self.start(quality, false)
            .await
            .expect("ordinary prepare cannot hit retry cooldown")
    }

    /// Only the HTTP host-authorized route should call this for remote requests.
    pub async fn retry(
        self: &Arc<Self>,
        quality: Quality,
    ) -> Result<RenditionStatus, JobControlError> {
        self.start(quality, true).await
    }

    async fn start(
        self: &Arc<Self>,
        quality: Quality,
        retry: bool,
    ) -> Result<RenditionStatus, JobControlError> {
        let mut jobs = self.jobs.lock().await;
        if self.ready(quality).await {
            return Ok(RenditionStatus::Ready);
        }
        if let Some(job) = jobs.get(&quality) {
            let status = job.status.borrow().clone();
            if !retry
                || !matches!(
                    status,
                    RenditionStatus::Cancelled | RenditionStatus::Failed { .. }
                )
            {
                return Ok(status);
            }
            if job.started.elapsed() < RETRY_COOLDOWN {
                return Err(JobControlError::RetryTooSoon);
            }
        }
        let initial = RenditionStatus::Preparing {
            progress: PreparationProgress {
                encoder: Some(self.encoding.encoder),
                ..Default::default()
            },
        };
        let (status, _) = watch::channel(initial.clone());
        let (cancel, cancellation) = watch::channel(false);
        jobs.insert(
            quality,
            Job {
                status: status.clone(),
                cancel,
                started: Instant::now(),
            },
        );
        let store = self.clone();
        tokio::spawn(async move {
            store.run_job(quality, status, cancellation).await;
        });
        Ok(initial)
    }

    pub async fn cancel(&self, quality: Quality) -> RenditionStatus {
        let jobs = self.jobs.lock().await;
        let Some(job) = jobs.get(&quality) else {
            return if self.ready(quality).await {
                RenditionStatus::Ready
            } else {
                RenditionStatus::Idle
            };
        };
        if matches!(&*job.status.borrow(), RenditionStatus::Preparing { .. }) {
            job.cancel.send_replace(true);
            job.status.send_modify(|state| {
                if let RenditionStatus::Preparing { progress } = state {
                    progress.phase = PreparationPhase::Cancelling;
                }
            });
        }
        job.status.borrow().clone()
    }

    async fn run_job(
        self: Arc<Self>,
        quality: Quality,
        status: watch::Sender<RenditionStatus>,
        mut cancel: watch::Receiver<bool>,
    ) {
        // A cancelled queued job never takes an encoder slot or starts a child process.
        let permit = tokio::select! {
            biased;
            _ = cancelled(&mut cancel) => {
                status.send_replace(RenditionStatus::Cancelled);
                return;
            }
            permit = self.slots.acquire() => permit.expect("encoder semaphore is never closed"),
        };
        // Serialize startup state with cancellation, so Cancelling cannot revert to Encoding.
        {
            let _jobs = self.jobs.lock().await;
            if *cancel.borrow() {
                status.send_replace(RenditionStatus::Cancelled);
                return;
            }
            status.send_replace(RenditionStatus::Preparing {
                progress: PreparationProgress {
                    phase: PreparationPhase::Encoding,
                    encoder: Some(self.encoding.encoder),
                    ..Default::default()
                },
            });
        }
        let temporary = self.cache.join(format!(
            "{}.{}.partial.mkv",
            quality.key(),
            uuid::Uuid::new_v4()
        ));
        let result = self
            .encode(quality, &temporary, status.clone(), &mut cancel)
            .await;
        // Publish and cancel are serialized. A completed rendition is never removed by cancel.
        let _jobs = self.jobs.lock().await;
        let terminal = if *cancel.borrow() {
            let _ = tokio::fs::remove_file(&temporary).await;
            RenditionStatus::Cancelled
        } else {
            let result = match result {
                Ok(()) => tokio::fs::rename(&temporary, self.path(quality))
                    .await
                    .context("Publish prepared rendition"),
                error => error,
            };
            match result {
                Ok(()) => RenditionStatus::Ready,
                Err(error) => {
                    let _ = tokio::fs::remove_file(&temporary).await;
                    tracing::error!(?quality, %error, "Transcode failed");
                    RenditionStatus::Failed { message: "Transcoding failed. Ask the server operator to check logs and encoder availability; the host can retry.".into() }
                }
            }
        };
        status.send_replace(terminal);
        drop(permit);
    }

    async fn encode(
        &self,
        quality: Quality,
        temporary: &std::path::Path,
        status: watch::Sender<RenditionStatus>,
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<()> {
        let mut command = self.command(quality)?;
        let mut child = command.arg(temporary).spawn().context("Start ffmpeg")?;
        let stdout = child.stdout.take().context("FFmpeg progress pipe")?;
        let stderr = child.stderr.take().context("FFmpeg diagnostic pipe")?;
        let mut progress = tokio::spawn(read_progress(
            stdout,
            status,
            self.info.duration,
            self.encoding.encoder,
        ));
        let mut diagnostics = tokio::spawn(diagnostic_tail(stderr));
        let result = tokio::select! {
            biased;
            _ = cancelled(cancel) => {
                // kill().await both terminates and reaps FFmpeg before cleanup or retry.
                let _ = child.kill().await;
                Ok(None)
            }
            result = tokio::time::timeout(Duration::from_secs(24 * 3600), child.wait()) => {
                match result {
                    Ok(result) => result.context("Wait for FFmpeg").map(Some),
                    Err(error) => {
                        let _ = child.kill().await;
                        Err(error).context("Transcoding exceeded 24 hour limit")
                    }
                }
            }
        };
        // Drain pipes before changing to a terminal state. Bound hangs from inherited descriptors.
        if tokio::time::timeout(Duration::from_secs(2), &mut progress)
            .await
            .is_err()
        {
            progress.abort();
        }
        let tail = match tokio::time::timeout(Duration::from_secs(2), &mut diagnostics).await {
            Ok(Ok(Ok(tail))) => tail,
            _ => {
                diagnostics.abort();
                Vec::new()
            }
        };
        if let Some(exit) = result?
            && !exit.success()
        {
            bail!(
                "FFmpeg {:?}: {}",
                self.encoding.encoder,
                String::from_utf8_lossy(&tail)
            );
        }
        Ok(())
    }

    fn command(&self, quality: Quality) -> Result<Command> {
        let mut command = Command::new("ffmpeg");
        command
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostats",
                "-stats_period",
                "0.5",
                "-progress",
                "pipe:1",
                "-y",
                "-i",
            ])
            .arg(&self.source)
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
            ])
            .args(encoder_args(self.encoding.encoder))
            .args([
                "-pix_fmt", "yuv420p", "-c:a", "aac", "-b:a", "192k", "-c:s", "copy", "-c:t",
                "copy",
            ]);
        Ok(command)
    }
}

async fn prepare_directory(directory: PathBuf) -> Result<PathBuf> {
    tokio::fs::create_dir_all(&directory)
        .await
        .with_context(|| format!("Create transcode directory {}", directory.display()))?;
    let directory = tokio::fs::canonicalize(&directory)
        .await
        .with_context(|| format!("Resolve transcode directory {}", directory.display()))?;
    let probe = directory.join(format!(".write-check-{}", uuid::Uuid::new_v4()));
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .await
        .with_context(|| {
            format!(
                "Transcode directory {} must be writable",
                directory.display()
            )
        })?;
    let written = async {
        file.write_all(b"Watchmpv transcode storage check\n")
            .await?;
        file.flush().await
    }
    .await;
    drop(file);
    let cleanup = tokio::fs::remove_file(&probe).await;
    written.with_context(|| {
        format!(
            "Write to transcode directory {} (check permissions and free space)",
            directory.display()
        )
    })?;
    cleanup.with_context(|| {
        format!(
            "Remove storage check from transcode directory {}",
            directory.display()
        )
    })?;
    Ok(directory)
}

// Do not let watch::Ref escape into select!'s output: it is not Send and can block writers.
async fn cancelled(receiver: &mut watch::Receiver<bool>) {
    let _ = receiver.wait_for(|value| *value).await;
}

#[cfg(test)]
mod tests;
