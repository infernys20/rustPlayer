pub mod media;
pub mod protocol;
pub mod room;
pub mod server;
pub mod sync;

#[cfg(all(feature = "client", any(unix, windows)))]
pub mod client;

#[cfg(test)]
#[path = "../tests/common/mod.rs"]
pub(crate) mod test_media;
