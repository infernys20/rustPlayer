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
    /// Increases only for explicit shared controls and host handoff, not position reports.
    pub control_revision: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Periodic measurements from the host, tagged with the controls already applied to mpv.
    Playback {
        position: f64,
        paused: bool,
        control_revision: u64,
    },
    /// Explicit, atomic control from any connected participant. Omitted fields are preserved.
    Control {
        position: Option<f64>,
        paused: Option<bool>,
    },
    Ping {
        client_time_ms: f64,
    },
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

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VideoEncoder {
    Cpu,
    Nvenc,
}

impl VideoEncoder {
    pub fn label(self) -> &'static str {
        match self {
            Self::Cpu => "CPU (libx264)",
            Self::Nvenc => "NVIDIA NVENC",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EncodingInfo {
    pub encoder: VideoEncoder,
    pub automatic: bool,
    pub fallback_reason: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PreparationPhase {
    #[default]
    Queued,
    Encoding,
    Cancelling,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct PreparationProgress {
    pub phase: PreparationPhase,
    /// Fraction of source duration encoded, in [0, 1]. Ready is reported only after publication.
    pub fraction: f64,
    pub encoded_seconds: f64,
    pub speed: Option<f64>,
    pub eta_seconds: Option<f64>,
    pub encoder: Option<VideoEncoder>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RenditionStatus {
    Idle,
    Preparing {
        // Allows the new client to decode old servers' bare preparing responses.
        #[serde(default)]
        progress: PreparationProgress,
    },
    Ready,
    Cancelled,
    Failed {
        message: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RenditionInfo {
    pub quality: Quality,
    pub status: RenditionStatus,
}

pub fn unix_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_preparing_responses_default_to_queued_progress() {
        let status: RenditionStatus = serde_json::from_str(r#"{"state":"preparing"}"#).unwrap();
        assert!(
            matches!(status, RenditionStatus::Preparing { progress } if progress.phase == PreparationPhase::Queued && progress.fraction == 0.0)
        );
    }

    #[test]
    fn preparation_status_round_trips_without_losing_encoder_or_eta() {
        let status = RenditionStatus::Preparing {
            progress: PreparationProgress {
                phase: PreparationPhase::Encoding,
                fraction: 0.5,
                encoded_seconds: 30.0,
                speed: Some(2.0),
                eta_seconds: Some(15.0),
                encoder: Some(VideoEncoder::Nvenc),
            },
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["state"], "preparing");
        assert_eq!(json["progress"]["encoder"], "nvenc");
        assert!(
            matches!(serde_json::from_value::<RenditionStatus>(json).unwrap(), RenditionStatus::Preparing { progress } if progress.eta_seconds == Some(15.0))
        );
    }
}
