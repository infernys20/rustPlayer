use std::{net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use watchmpv::{
    media::{Encoder, MediaStore},
    server::{AppState, router},
};

#[derive(Parser)]
#[command(
    version,
    about = "Stream a local video to a password-protected mpv watch party"
)]
struct Args {
    /// Local video file. The server exposes only this file and its prepared renditions.
    #[arg(long)]
    media: PathBuf,
    #[arg(long, default_value = "127.0.0.1:8787")]
    listen: SocketAddr,
    /// Read the password from a file instead of WATCHMPV_PASSWORD.
    #[arg(long)]
    password_file: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "cpu")]
    encoder: Encoder,
    #[arg(long, default_value = ".watchmpv/cache")]
    cache_dir: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "watchmpv=info".into()),
        )
        .init();
    let args = Args::parse();
    let password = if let Some(path) = args.password_file {
        tokio::fs::read_to_string(path)
            .await
            .context("Read password file")?
            .trim_end_matches(['\r', '\n'])
            .to_owned()
    } else {
        std::env::var("WATCHMPV_PASSWORD")
            .context("Set WATCHMPV_PASSWORD or use --password-file (at least 8 bytes)")?
    };
    let media = MediaStore::new(args.media, args.cache_dir, args.encoder).await?;
    tracing::info!(title = %media.info.title, duration = media.info.duration, encoder = ?args.encoder, "Media ready");
    let state = AppState::new(media, password).await?;
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    tracing::info!(address = %listener.local_addr()?, "Watchmpv server listening; use HTTPS/WSS via a reverse proxy outside trusted networks");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
