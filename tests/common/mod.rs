use std::path::PathBuf;

use tempfile::TempDir;
use tokio::process::Command;

/// A real seekable file with two audio languages and two soft subtitle tracks.
pub async fn fixture(seconds: u32) -> (TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let subtitle = temp.path().join("captions.srt");
    tokio::fs::write(
        &subtitle,
        "1\n00:00:00,100 --> 00:00:02,500\nWatch together!\n",
    )
    .await
    .unwrap();
    let source = temp.path().join("movie.mkv");
    let result = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=640x480:rate=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=660:sample_rate=48000",
            "-i",
        ])
        .arg(&subtitle)
        .args([
            "-t",
            &seconds.to_string(),
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-map",
            "2:a",
            "-map",
            "3:s",
            "-map",
            "3:s",
            "-metadata:s:a:0",
            "language=eng",
            "-metadata:s:a:1",
            "language=jpn",
            "-metadata:s:s:0",
            "language=eng",
            "-metadata:s:s:1",
            "language=fra",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-threads",
            "2",
            "-c:a",
            "aac",
            "-c:s",
            "srt",
        ])
        .arg(&source)
        .output()
        .await
        .expect("Tests require FFmpeg on PATH");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    (temp, source)
}
