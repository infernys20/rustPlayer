use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    #[default]
    Original,
    P1080,
    P720,
    P480,
}

impl Quality {
    pub const ALL: [Self; 4] = [Self::Original, Self::P1080, Self::P720, Self::P480];

    pub fn key(self) -> &'static str {
        match self {
            Self::Original => "original",
            Self::P1080 => "p1080",
            Self::P720 => "p720",
            Self::P480 => "p480",
        }
    }

    pub fn height(self) -> Option<u32> {
        match self {
            Self::Original => None,
            Self::P1080 => Some(1080),
            Self::P720 => Some(720),
            Self::P480 => Some(480),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Original => "Original (no transcode)",
            Self::P1080 => "1080p",
            Self::P720 => "720p",
            Self::P480 => "480p",
        }
    }
}

#[derive(Deserialize, Serialize)]
pub struct LoginRequest {
    pub name: String,
    pub password: String,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct LoginResponse {
    pub token: String,
    pub session_id: Uuid,
    pub media: MediaInfo,
    pub expires_in_seconds: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MediaInfo {
    pub title: String,
    pub duration: f64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub tracks: Vec<TrackInfo>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TrackInfo {
    pub index: u32,
    pub kind: String,
    pub codec: String,
    pub language: Option<String>,
    pub title: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Participant {
    pub id: Uuid,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RoomSnapshot {
    pub leader: Option<Uuid>,
    pub participants: Vec<Participant>,
    pub position: f64,
    pub paused: bool,
    pub server_time_ms: f64,
    pub revision: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Playback { position: f64, paused: bool },
    Ping { client_time_ms: f64 },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Room {
        snapshot: RoomSnapshot,
    },
    Pong {
        client_time_ms: f64,
        server_time_ms: f64,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PrepareRequest {
    pub quality: Quality,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RenditionStatus {
    Preparing,
    Ready,
    Failed { message: String },
}

pub fn unix_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        * 1000.0
}
