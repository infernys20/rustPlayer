use super::*;

use crate::test_media as common;

async fn terminal(store: &MediaStore, quality: Quality) -> RenditionStatus {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let status = store.status(quality).await;
            if !matches!(status, RenditionStatus::Preparing { .. }) {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}

async fn partials(store: &MediaStore) -> Vec<PathBuf> {
    let mut entries = tokio::fs::read_dir(&store.cache).await.unwrap();
    let mut result = Vec::new();
    while let Some(entry) = entries.next_entry().await.unwrap() {
        if entry.file_name().to_string_lossy().contains(".partial.") {
            result.push(entry.path());
        }
    }
    result
}

#[tokio::test]
async fn selected_storage_is_created_resolved_and_checked_without_leaving_probe_files() {
    let temp = tempfile::tempdir().unwrap();
    let requested = temp.path().join("another drive").join("transcodes");
    let directory = prepare_directory(requested.clone()).await.unwrap();
    assert!(directory.is_absolute());
    assert_eq!(directory, tokio::fs::canonicalize(requested).await.unwrap());
    assert!(
        tokio::fs::read_dir(directory)
            .await
            .unwrap()
            .next_entry()
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn invalid_storage_reports_the_selected_path() {
    let temp = tempfile::tempdir().unwrap();
    let blocked = temp.path().join("not-a-directory");
    tokio::fs::write(&blocked, b"file").await.unwrap();
    let error = prepare_directory(blocked.join("transcodes"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Create transcode directory"));
    assert!(error.to_string().contains("not-a-directory"));
}

#[tokio::test]
async fn transcodes_are_written_to_selected_storage_not_the_source_directory() {
    let (_source_storage, source) = common::fixture(4).await;
    let other_storage = tempfile::tempdir().unwrap();
    let root = other_storage
        .path()
        .join("drive with spaces")
        .join("transcodes");
    let store = MediaStore::new(source.clone(), root.clone(), Encoder::Cpu)
        .await
        .unwrap();
    assert!(
        store
            .transcode_directory()
            .starts_with(tokio::fs::canonicalize(root).await.unwrap())
    );
    assert_eq!(
        store.path(Quality::Original),
        tokio::fs::canonicalize(&source).await.unwrap()
    );
    store.prepare(Quality::P480).await;
    assert!(matches!(
        terminal(&store, Quality::P480).await,
        RenditionStatus::Ready
    ));
    let output = store.path(Quality::P480);
    assert_eq!(output.parent(), Some(store.transcode_directory()));
    assert!(tokio::fs::metadata(output).await.unwrap().len() > 0);
    assert!(partials(&store).await.is_empty());
    assert!(tokio::fs::try_exists(source).await.unwrap());
}

#[tokio::test]
async fn queued_cancellation_is_sticky_and_retry_is_bounded_and_shared() {
    let (temp, source) = common::fixture(4).await;
    let store = MediaStore::new(source, temp.path().join("cache"), Encoder::Cpu)
        .await
        .unwrap();
    let permit = store.slots.acquire().await.unwrap();
    for _ in 0..3 {
        assert!(
            matches!(store.prepare(Quality::P480).await, RenditionStatus::Preparing { progress } if progress.phase == PreparationPhase::Queued)
        );
    }
    assert_eq!(store.jobs.lock().await.len(), 1);
    assert!(
        matches!(store.cancel(Quality::P480).await, RenditionStatus::Preparing { progress } if progress.phase == PreparationPhase::Cancelling)
    );
    assert!(matches!(
        terminal(&store, Quality::P480).await,
        RenditionStatus::Cancelled
    ));
    assert!(partials(&store).await.is_empty());
    assert!(matches!(
        store.prepare(Quality::P480).await,
        RenditionStatus::Cancelled
    ));
    assert_eq!(
        store.retry(Quality::P480).await.unwrap_err(),
        JobControlError::RetryTooSoon
    );
    store
        .jobs
        .lock()
        .await
        .get_mut(&Quality::P480)
        .unwrap()
        .started -= RETRY_COOLDOWN;
    assert!(matches!(
        store.retry(Quality::P480).await.unwrap(),
        RenditionStatus::Preparing { .. }
    ));
    assert!(matches!(
        store.retry(Quality::P480).await.unwrap(),
        RenditionStatus::Preparing { .. }
    ));
    assert_eq!(store.jobs.lock().await.len(), 1);
    drop(permit);
    assert!(matches!(
        terminal(&store, Quality::P480).await,
        RenditionStatus::Ready
    ));
    assert!(partials(&store).await.is_empty());
    assert!(matches!(
        store.cancel(Quality::P480).await,
        RenditionStatus::Ready
    ));
    assert!(store.ready(Quality::P480).await);
}

#[tokio::test]
async fn running_cancellation_reaps_ffmpeg_and_removes_partial_output() {
    let (temp, source) = common::fixture(60).await;
    let store = MediaStore::new(source, temp.path().join("cache"), Encoder::Cpu)
        .await
        .unwrap();
    store.prepare(Quality::P480).await;
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let RenditionStatus::Preparing { progress } = store.status(Quality::P480).await else {
                panic!("Expected live FFmpeg progress before completion");
            };
            if progress.phase == PreparationPhase::Encoding
                && progress.encoded_seconds > 0.0
                && !partials(&store).await.is_empty()
            {
                assert!(progress.fraction > 0.0 && progress.fraction <= 1.0);
                assert!(
                    progress
                        .eta_seconds
                        .is_none_or(|eta| eta.is_finite() && eta >= 0.0)
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    store.cancel(Quality::P480).await;
    assert!(matches!(
        terminal(&store, Quality::P480).await,
        RenditionStatus::Cancelled
    ));
    assert!(!store.ready(Quality::P480).await);
    assert!(partials(&store).await.is_empty());
    // No slot is released until the old FFmpeg process has terminated and cleanup finishes.
    let _permit = tokio::time::timeout(Duration::from_secs(2), store.slots.acquire())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn failed_jobs_require_explicit_retry_and_completed_cache_survives_restart() {
    let (temp, source) = common::fixture(4).await;
    let moved = temp.path().join("moved.mkv");
    let cache = temp.path().join("cache");
    let store = MediaStore::new(source.clone(), cache.clone(), Encoder::Cpu)
        .await
        .unwrap();
    tokio::fs::rename(&source, &moved).await.unwrap();
    store.prepare(Quality::P480).await;
    assert!(matches!(
        terminal(&store, Quality::P480).await,
        RenditionStatus::Failed { .. }
    ));
    assert!(matches!(
        store.prepare(Quality::P480).await,
        RenditionStatus::Failed { .. }
    ));
    assert!(partials(&store).await.is_empty());
    tokio::fs::rename(moved, &source).await.unwrap();
    store
        .jobs
        .lock()
        .await
        .get_mut(&Quality::P480)
        .unwrap()
        .started -= RETRY_COOLDOWN;
    store.retry(Quality::P480).await.unwrap();
    assert!(matches!(
        terminal(&store, Quality::P480).await,
        RenditionStatus::Ready
    ));
    let restarted = MediaStore::new(source, cache, Encoder::Cpu).await.unwrap();
    assert!(matches!(
        restarted.prepare(Quality::P480).await,
        RenditionStatus::Ready
    ));
    assert!(restarted.jobs.lock().await.is_empty());
}
