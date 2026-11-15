use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use axum::{
    Json, Router,
    body::Body,
    extract::{
        DefaultBodyLimit, Path, Request, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::StreamExt;
use tokio::sync::{Mutex, Semaphore};
use tower::ServiceExt;
use tower_http::services::ServeFile;
use uuid::Uuid;

use crate::{media::MediaStore, protocol::*, room::Room};

const SESSION_TTL: Duration = Duration::from_secs(12 * 3600);

#[derive(Clone)]
struct Session {
    id: Uuid,
    name: String,
    expires: Instant,
}

pub struct AppState {
    pub media: Arc<MediaStore>,
    password_hash: String,
    sessions: Mutex<HashMap<String, Session>>,
    room: Mutex<Room>,
    login_attempts: Mutex<Vec<Instant>>,
    password_slots: Arc<Semaphore>,
}

impl AppState {
    pub async fn new(media: Arc<MediaStore>, password: String) -> Result<Arc<Self>> {
        if password.len() < 8 || password.len() > 1024 {
            bail!("Use a server password between 8 and 1024 bytes");
        }
        let password_hash = tokio::task::spawn_blocking(move || {
            let salt =
                SaltString::encode_b64(Uuid::new_v4().as_bytes()).expect("UUID fits salt length");
            Argon2::default()
                .hash_password(password.as_bytes(), &salt)
                .map(|hash| hash.to_string())
                .map_err(|error| anyhow::anyhow!("{error}"))
        })
        .await
        .context("Password hashing task")??;
        Ok(Arc::new(Self {
            room: Mutex::new(Room::new(media.info.duration)),
            media,
            password_hash,
            sessions: Mutex::new(HashMap::new()),
            login_attempts: Mutex::new(Vec::new()),
            password_slots: Arc::new(Semaphore::new(2)),
        }))
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    let protected = Router::new()
        .route("/api/media", get(media_info))
        .route("/api/prepare", post(prepare))
        .route("/api/stream/{quality}", get(stream))
        .route("/api/ws", get(websocket))
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize));
    Router::new()
        .route("/health", get(|| async { "watchmpv ok" }))
        .route("/api/login", post(login))
        .merge(protected)
        .layer(DefaultBodyLimit::max(16 * 1024))
        .with_state(state)
}

fn api_error(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({"error": message}))).into_response()
}

async fn login(State(state): State<Arc<AppState>>, Json(request): Json<LoginRequest>) -> Response {
    let name = request.name.trim();
    if name.is_empty()
        || name.chars().count() > 40
        || name.chars().any(char::is_control)
        || request.password.len() > 1024
    {
        return api_error(
            StatusCode::BAD_REQUEST,
            "Name must be 1–40 printable characters",
        );
    }
    {
        let mut attempts = state.login_attempts.lock().await;
        attempts.retain(|time| time.elapsed() < Duration::from_secs(60));
        if attempts.len() >= 20 {
            return api_error(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many login attempts; wait a minute",
            );
        }
        attempts.push(Instant::now());
    }
    let Ok(permit) = state.password_slots.clone().try_acquire_owned() else {
        return api_error(
            StatusCode::TOO_MANY_REQUESTS,
            "Login busy; try again shortly",
        );
    };
    let hash = state.password_hash.clone();
    let valid = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        PasswordHash::new(&hash).is_ok_and(|hash| {
            Argon2::default()
                .verify_password(request.password.as_bytes(), &hash)
                .is_ok()
        })
    })
    .await
    .unwrap_or(false);
    if !valid {
        return api_error(StatusCode::UNAUTHORIZED, "Incorrect server password");
    }
    let mut sessions = state.sessions.lock().await;
    sessions.retain(|_, session| session.expires > Instant::now());
    if sessions.len() >= 128 {
        return api_error(StatusCode::SERVICE_UNAVAILABLE, "Too many active sessions");
    }
    // Two independently random UUIDs give more than 240 random bits.
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let session = Session {
        id: Uuid::new_v4(),
        name: name.to_owned(),
        expires: Instant::now() + SESSION_TTL,
    };
    sessions.insert(token.clone(), session.clone());
    Json(LoginResponse {
        token,
        session_id: session.id,
        media: state.media.info.clone(),
        expires_in_seconds: SESSION_TTL.as_secs(),
    })
    .into_response()
}

async fn authorize(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let token = bearer(request.headers());
    let session = match token {
        Some(token) => state
            .sessions
            .lock()
            .await
            .get(token)
            .filter(|s| s.expires > Instant::now())
            .cloned(),
        None => None,
    };
    let Some(session) = session else {
        return api_error(
            StatusCode::UNAUTHORIZED,
            "Login required or session expired",
        );
    };
    request.extensions_mut().insert(session);
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert("cache-control", "private, no-store".parse().unwrap());
    response
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

async fn media_info(State(state): State<Arc<AppState>>) -> Json<MediaInfo> {
    Json(state.media.info.clone())
}

async fn prepare(
    State(state): State<Arc<AppState>>,
    Json(request): Json<PrepareRequest>,
) -> Json<RenditionStatus> {
    Json(state.media.prepare(request.quality).await)
}

async fn stream(
    State(state): State<Arc<AppState>>,
    Path(quality): Path<String>,
    request: Request,
) -> Response {
    let Some(quality) = Quality::ALL.into_iter().find(|q| q.key() == quality) else {
        return api_error(StatusCode::NOT_FOUND, "Unknown quality");
    };
    if !state.media.ready(quality).await {
        return api_error(
            StatusCode::CONFLICT,
            "Prepare this quality before opening the stream",
        );
    }
    // ServeFile provides byte ranges/HEAD, essential for mpv to seek remote files.
    match ServeFile::new(state.media.path(quality))
        .oneshot(request)
        .await
    {
        Ok(response) => response.map(Body::new),
        Err(error) => match error {},
    }
}

async fn websocket(
    State(state): State<Arc<AppState>>,
    axum::Extension(session): axum::Extension<Session>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    // Native clients do not send Origin. Reject browser cross-origin websocket use.
    if headers.contains_key("origin") {
        return api_error(
            StatusCode::FORBIDDEN,
            "Browser websocket connections are not supported",
        );
    }
    upgrade
        .max_message_size(4096)
        .max_frame_size(4096)
        .on_upgrade(move |socket| connection(state, session, socket))
}

async fn send(socket: &mut WebSocket, message: ServerMessage) -> bool {
    let Ok(text) = serde_json::to_string(&message) else {
        return false;
    };
    matches!(
        tokio::time::timeout(
            Duration::from_secs(5),
            socket.send(Message::Text(text.into()))
        )
        .await,
        Ok(Ok(()))
    )
}

async fn connection(state: Arc<AppState>, session: Session, mut socket: WebSocket) {
    let joined = state.room.lock().await.join(Participant {
        id: session.id,
        name: session.name.clone(),
    });
    if let Err(error) = joined {
        send(
            &mut socket,
            ServerMessage::Error {
                message: error.into(),
            },
        )
        .await;
        return;
    }
    tracing::info!(id = %session.id, "Viewer connected");
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_message = Instant::now();
    let mut budget_start = Instant::now();
    let mut message_count = 0;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if session.expires <= Instant::now() || last_message.elapsed() > Duration::from_secs(20) { break; }
                let snapshot = state.room.lock().await.snapshot();
                if !send(&mut socket, ServerMessage::Room { snapshot }).await { break; }
            }
            message = socket.next() => {
                let Some(Ok(message)) = message else { break; };
                last_message = Instant::now();
                if budget_start.elapsed() >= Duration::from_secs(1) {
                    budget_start = Instant::now(); message_count = 0;
                }
                message_count += 1;
                if message_count > 30 { break; }
                match message {
                    Message::Text(text) => {
                        let response = match serde_json::from_str::<ClientMessage>(&text) {
                            Ok(ClientMessage::Ping { client_time_ms }) if client_time_ms.is_finite() => {
                                Some(ServerMessage::Pong { client_time_ms, server_time_ms: unix_ms() })
                            }
                            Ok(ClientMessage::Playback { position, paused }) => {
                                state.room.lock().await.update(session.id, position, paused).err()
                                    .map(|error| ServerMessage::Error { message: error.into() })
                            }
                            _ => Some(ServerMessage::Error { message: "Invalid message".into() }),
                        };
                        if let Some(response) = response && !send(&mut socket, response).await { break; }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        }
    }
    state.room.lock().await.leave(session.id);
    tracing::info!(id = %session.id, "Viewer disconnected");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_is_header_only_and_requires_bearer() {
        let mut headers = HeaderMap::new();
        assert_eq!(bearer(&headers), None);
        headers.insert("authorization", "Basic secret".parse().unwrap());
        assert_eq!(bearer(&headers), None);
        headers.insert("authorization", "Bearer secret".parse().unwrap());
        assert_eq!(bearer(&headers), Some("secret"));
    }
}
