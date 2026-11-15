//! Platform-specific mpv transport; the JSON protocol and player actor are shared.
use std::{ffi::OsString, io};

use anyhow::Result;
use tokio::io::{AsyncRead, AsyncWrite};

pub trait AsyncIo: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncIo for T {}
pub type Stream = Box<dyn AsyncIo>;

#[cfg(unix)]
pub struct Endpoint {
    directory: std::path::PathBuf,
}

#[cfg(windows)]
pub struct Endpoint {
    pipe: String,
}

impl Endpoint {
    pub fn new() -> Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            let directory = std::env::temp_dir().join(format!("watchmpv-{}", uuid::Uuid::new_v4()));
            std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
            Ok(Self { directory })
        }
        #[cfg(windows)]
        {
            Ok(Self {
                pipe: format!(r"\\.\pipe\watchmpv-{}", uuid::Uuid::new_v4()),
            })
        }
    }

    pub fn argument(&self) -> OsString {
        let mut argument = OsString::from("--input-ipc-server=");
        #[cfg(unix)]
        argument.push(self.directory.join("ipc.sock"));
        #[cfg(windows)]
        argument.push(&self.pipe);
        argument
    }

    pub async fn connect(&self) -> io::Result<Stream> {
        #[cfg(unix)]
        {
            Ok(Box::new(
                tokio::net::UnixStream::connect(self.directory.join("ipc.sock")).await?,
            ))
        }
        #[cfg(windows)]
        {
            // Tokio defaults to SECURITY_IDENTIFICATION, preventing the server from
            // impersonating this client. Busy/not-yet-created pipes are retried by the launcher.
            Ok(Box::new(
                tokio::net::windows::named_pipe::ClientOptions::new().open(&self.pipe)?,
            ))
        }
    }

    pub fn retryable(error: &io::Error) -> bool {
        #[cfg(unix)]
        {
            matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            )
        }
        #[cfg(windows)]
        {
            matches!(error.raw_os_error(), Some(2 | 231))
        } // FILE_NOT_FOUND / PIPE_BUSY
    }
}

#[cfg(unix)]
impl Drop for Endpoint {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_player_gets_a_unique_transport_and_no_auth_token_in_its_argument() {
        let first = Endpoint::new().unwrap();
        let second = Endpoint::new().unwrap();
        assert_ne!(first.argument(), second.argument());
        assert!(
            first
                .argument()
                .to_string_lossy()
                .starts_with("--input-ipc-server=")
        );
        assert!(!first.argument().to_string_lossy().contains("Bearer"));
        #[cfg(windows)]
        assert!(first.pipe.starts_with(r"\\.\pipe\watchmpv-"));
    }

    #[cfg(unix)]
    #[test]
    fn unix_directory_is_private_and_removed_on_drop() {
        use std::os::unix::fs::PermissionsExt;
        let endpoint = Endpoint::new().unwrap();
        let directory = endpoint.directory.clone();
        assert_eq!(
            std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        drop(endpoint);
        assert!(!directory.exists());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_named_pipe_exchanges_data_through_the_shared_transport() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::windows::named_pipe::ServerOptions;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let endpoint = Endpoint::new().unwrap();
            let mut server = ServerOptions::new()
                .first_pipe_instance(true)
                .reject_remote_clients(true)
                .create(&endpoint.pipe)
                .unwrap();
            let mut client = endpoint.connect().await.unwrap();
            server.connect().await.unwrap();
            client.write_all(b"mpv request\n").await.unwrap();
            let mut request = [0; 12];
            server.read_exact(&mut request).await.unwrap();
            assert_eq!(&request, b"mpv request\n");
            server.write_all(b"ok\n").await.unwrap();
            let mut reply = [0; 3];
            client.read_exact(&mut reply).await.unwrap();
            assert_eq!(&reply, b"ok\n");
        })
        .await
        .expect("Windows named-pipe exchange timed out");
    }

    #[cfg(windows)]
    #[test]
    fn retries_only_missing_or_busy_pipes() {
        assert!(Endpoint::retryable(&io::Error::from_raw_os_error(2)));
        assert!(Endpoint::retryable(&io::Error::from_raw_os_error(231)));
        assert!(!Endpoint::retryable(&io::Error::from_raw_os_error(5)));
    }
}
