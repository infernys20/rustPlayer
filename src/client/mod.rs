mod ipc;
mod mpv;
mod native_controls;
mod transcoding;
mod ui;

use std::{
    sync::mpsc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use reqwest::{Client, Url};
use serde_json::json;
use tokio::{sync::mpsc as async_mpsc, task::JoinSet};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};

use crate::{
    protocol::*,
    sync::{Correction, correction},
};

pub use ui::WatchApp;

pub struct ConnectOptions {
    pub address: String,
    pub name: String,
    pub password: String,
}

pub enum Action {
    Pause(bool),
    Seek(f64),
    Quality(Quality),
    CancelQuality(Quality),
    RetryQuality(Quality),
    Audio(i64),
    Subtitle(Option<i64>),
    Disconnect,
    /// Exercise the exact native mpv command path (keyboard/OSC), not companion controls.
    #[cfg(test)]
    PlayerCommand(serde_json::Value),
    #[cfg(test)]
    ManagedPlayerSeek(f64),
}

pub enum Event {
    Connected {
        media: MediaInfo,
        session_id: uuid::Uuid,
    },
    Room(RoomSnapshot),
    Player(PlayerState),
    Quality(Quality),
    Renditions(Vec<RenditionInfo>),
    Encoding(EncodingInfo),
    Status(String),
    Failed(String),
    Disconnected,
}

#[derive(Clone, Default)]
pub struct PlayerState {
    pub position: Option<f64>,
    pub paused: bool,
    pub buffering: bool,
    pub seeking: bool,
    pub audio: Option<i64>,
    pub subtitle: Option<i64>,
    pub tracks: Vec<PlayerTrack>,
}

#[derive(Clone)]
pub struct PlayerTrack {
    pub id: i64,
    pub kind: String,
    pub label: String,
}

pub fn start(
    options: ConnectOptions,
    repaint: eframe::egui::Context,
) -> (async_mpsc::UnboundedSender<Action>, mpsc::Receiver<Event>) {
    let (actions, receiver) = async_mpsc::unbounded_channel();
    let (events, output) = mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new();
        match runtime {
            Ok(runtime) => {
                if let Err(error) =
                    runtime.block_on(run(options, receiver, &events, &repaint, false))
                {
                    let _ = events.send(Event::Failed(format!("{error:#}")));
                }
            }
            Err(error) => {
                let _ = events.send(Event::Failed(error.to_string()));
            }
        }
        let _ = events.send(Event::Disconnected);
        repaint.request_repaint();
    });
    (actions, output)
}

fn server_url(address: &str) -> Result<Url> {
    let address = address.trim();
    let implicit_scheme = !address.contains("://");
    let address = if !implicit_scheme {
        address.to_owned()
    } else {
        format!("http://{address}")
    };
    let mut url =
        Url::parse(&address).context("Enter an IP/hostname and port, or an http(s) URL")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        bail!("Use http(s)://host:port without credentials, path, query, or fragment");
    }
    if implicit_scheme && url.port().is_none() {
        url.set_port(Some(8787))
            .map_err(|_| anyhow::anyhow!("Invalid port"))?;
    }
    Ok(url)
}

async fn checked(response: reqwest::Response) -> Result<reqwest::Response> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let message: serde_json::Value = response.json().await.unwrap_or_default();
    bail!(
        "Server {status}: {}",
        message["error"].as_str().unwrap_or("Request failed")
    );
}

fn emit(events: &mpsc::Sender<Event>, repaint: &eframe::egui::Context, event: Event) {
    let _ = events.send(event);
    repaint.request_repaint();
}

async fn run(
    options: ConnectOptions,
    mut actions: async_mpsc::UnboundedReceiver<Action>,
    events: &mpsc::Sender<Event>,
    repaint: &eframe::egui::Context,
    headless: bool,
) -> Result<()> {
    let base = server_url(&options.address)?;
    let http = Client::builder()
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let login: LoginResponse = checked(
        http.post(base.join("api/login")?)
            .json(&LoginRequest {
                name: options.name,
                password: options.password,
            })
            .send()
            .await
            .context("Connect to server")?,
    )
    .await?
    .json()
    .await?;
    let encoding: EncodingInfo = checked(
        http.get(base.join("api/encoding")?)
            .bearer_auth(&login.token)
            .send()
            .await?,
    )
    .await?
    .json()
    .await?;
    emit(events, repaint, Event::Encoding(encoding));
    let mut ws_url = base.join("api/ws")?;
    ws_url
        .set_scheme(if base.scheme() == "https" {
            "wss"
        } else {
            "ws"
        })
        .map_err(|_| anyhow::anyhow!("Invalid websocket URL"))?;
    let mut request = ws_url.as_str().into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {}", login.token).parse()?);
    let (mut socket, _) = tokio::time::timeout(Duration::from_secs(15), connect_async(request))
        .await
        .context("WebSocket connection timed out")??;
    emit(events, repaint, Event::Status("Starting mpv…".into()));
    let mut mpv = mpv::Mpv::launch(&login.token, headless).await?;
    let mut current_url = base.join("api/stream/original")?.to_string();
    mpv.load(&current_url, 0.0).await?;
    emit(
        events,
        repaint,
        Event::Connected {
            media: login.media.clone(),
            session_id: login.session_id,
        },
    );
    emit(
        events,
        repaint,
        Event::Status("Connected. The video opens in a separate mpv window.".into()),
    );

    let mut snapshot: Option<RoomSnapshot> = None;
    let mut last_snapshot = Instant::now();
    let mut clock_offset: Option<f64> = None;
    let mut last_ping = Instant::now() - Duration::from_secs(10);
    let mut last_report = Instant::now() - Duration::from_secs(1);
    let mut last_tracks = Instant::now() - Duration::from_secs(10);
    let mut known_tracks = Vec::new();
    let mut needs_alignment = true;
    let mut pending_load = true;
    let mut pending_player_control: Option<(u64, Instant)> = None;
    let mut load_started = Instant::now();
    let mut wanted_quality = Quality::Original;
    let mut loading_quality = Quality::Original;
    let mut preferred_audio = None;
    let mut preferred_subtitle = None;
    // HTTP work is kept off the playback loop; a slow transcode/status request cannot stall sync.
    let mut transcode_requests = JoinSet::new();
    let mut inventory_pending = false;
    let mut inventory_tick = tokio::time::interval(Duration::from_secs(1));
    inventory_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            action = actions.recv() => {
                let Some(action) = action else { break; };
                let is_host = snapshot.as_ref().is_some_and(|s| s.leader == Some(login.session_id));
                match action {
                    Action::Disconnect => break,
                    #[cfg(test)]
                    Action::PlayerCommand(command) => { mpv.command(command).await?; }
                    #[cfg(test)]
                    Action::ManagedPlayerSeek(position) => { mpv.seek(position).await?; }
                    Action::Pause(paused) if snapshot.is_some() => {
                        socket.send(Message::Text(serde_json::to_string(&ClientMessage::Control { position: None, paused: Some(paused) })?.into())).await?;
                    }
                    Action::Seek(position) if snapshot.is_some() && position.is_finite() => {
                        socket.send(Message::Text(serde_json::to_string(&ClientMessage::Control { position: Some(position.clamp(0.0, login.media.duration)), paused: None })?.into())).await?;
                    }
                    Action::Audio(id) => { mpv.set("aid", json!(id)).await?; preferred_audio = Some(id); }
                    Action::Subtitle(id) => { mpv.set("sid", id.map_or(json!("no"), |id| json!(id))).await?; preferred_subtitle = Some(id); }
                    Action::Quality(quality) => {
                        wanted_quality = quality;
                        let request = transcoding::operate(http.clone(), base.clone(), login.token.clone(), quality, transcoding::Operation::Prepare);
                        transcode_requests.spawn(async move { (false, request.await) });
                        emit(events, repaint, Event::Status(format!("Requested {}; current stream keeps playing until it is ready.", quality.label())));
                    }
                    Action::CancelQuality(quality) if is_host => {
                        let request = transcoding::operate(http.clone(), base.clone(), login.token.clone(), quality, transcoding::Operation::Cancel);
                        transcode_requests.spawn(async move { (false, request.await) });
                    }
                    Action::RetryQuality(quality) if is_host => {
                        let request = transcoding::operate(http.clone(), base.clone(), login.token.clone(), quality, transcoding::Operation::Retry);
                        transcode_requests.spawn(async move { (false, request.await) });
                    }
                    _ => {}
                }
            }
            _ = inventory_tick.tick(), if !inventory_pending => {
                inventory_pending = true;
                let http = http.clone();
                let base = base.clone();
                let token = login.token.clone();
                transcode_requests.spawn(async move { (true, transcoding::inventory(&http, &base, &token).await) });
            }
            reply = transcode_requests.join_next(), if !transcode_requests.is_empty() => {
                match reply {
                    Some(Ok((poll, result))) => {
                        if poll { inventory_pending = false; }
                        match result {
                            Ok(renditions) => {
                                let ready = renditions.iter().any(|r| r.quality == wanted_quality && matches!(r.status, RenditionStatus::Ready));
                                emit(events, repaint, Event::Renditions(renditions));
                                if ready && wanted_quality != loading_quality {
                                    current_url = base.join(&format!("api/stream/{}", wanted_quality.key()))?.to_string();
                                    let position = snapshot.as_ref().map(|s| target(s, clock_offset, login.media.duration)).unwrap_or(0.0);
                                    mpv.load(&current_url, position).await?;
                                    pending_load = true;
                                    load_started = Instant::now();
                                    needs_alignment = true;
                                    last_tracks = Instant::now() - Duration::from_secs(10);
                                    loading_quality = wanted_quality;
                                    emit(events, repaint, Event::Status(format!("Loading {}…", wanted_quality.label())));
                                }
                            }
                            Err(error) => emit(events, repaint, Event::Status(format!("Transcode request failed: {error:#}. Playback is unchanged."))),
                        }
                    }
                    _ => { inventory_pending = false; }
                }
            }
            message = socket.next() => {
                let Some(message) = message else { bail!("Server closed the connection"); };
                match message? {
                    Message::Text(text) => match serde_json::from_str::<ServerMessage>(&text)? {
                        ServerMessage::Room { snapshot: next } => {
                            if snapshot.as_ref().is_none_or(|previous| previous.leader != next.leader || previous.control_revision != next.control_revision) {
                                needs_alignment = true;
                            }
                            if pending_player_control.is_some_and(|(revision, _)| next.control_revision > revision) {
                                pending_player_control = None;
                            }
                            last_snapshot = Instant::now();
                            emit(events, repaint, Event::Room(next.clone()));
                            snapshot = Some(next);
                        }
                        ServerMessage::Pong { client_time_ms, server_time_ms } => {
                            let now = unix_ms();
                            let rtt = now - client_time_ms;
                            if (0.0..5000.0).contains(&rtt) {
                                let measured = server_time_ms - (client_time_ms + now) / 2.0;
                                clock_offset = Some(clock_offset.map_or(measured, |old| old * 0.8 + measured * 0.2));
                            }
                        }
                        ServerMessage::Error { message } => emit(events, repaint, Event::Status(message)),
                    },
                    Message::Close(_) => bail!("Server closed the connection (session expired or connection lost)"),
                    Message::Ping(bytes) => { socket.send(Message::Pong(bytes)).await?; }
                    _ => {}
                }
            }
            _ = tick.tick() => {
                if last_snapshot.elapsed() > Duration::from_secs(8) {
                    let _ = mpv.set("pause", json!(true)).await;
                    bail!("Synchronization connection timed out. Playback stopped; reconnect to rejoin.");
                }
                if last_ping.elapsed() >= Duration::from_secs(2) {
                    socket.send(Message::Text(serde_json::to_string(&ClientMessage::Ping { client_time_ms: unix_ms() })?.into())).await?;
                    last_ping = Instant::now();
                }
                if pending_load && load_started.elapsed() > Duration::from_secs(200) {
                    bail!("mpv could not load the stream within 200 seconds. Check network access, server logs, and media format support.");
                }
                let mut player = mpv.sample().await?;
                if last_tracks.elapsed() >= Duration::from_secs(2) {
                    known_tracks = mpv.tracks().await?;
                    last_tracks = Instant::now();
                }
                if !pending_load && let Some(state) = &snapshot {
                    let native = mpv.native_control(&player).await?;
                    let position = native.position.filter(|p| p.is_finite()).map(|p| p.clamp(0.0, login.media.duration));
                    if !native.is_empty() && (position.is_some() || native.paused.is_some()) {
                        socket.send(Message::Text(serde_json::to_string(&ClientMessage::Control { position, paused: native.paused })?.into())).await?;
                        pending_player_control = Some((state.control_revision, Instant::now()));
                        needs_alignment = true;
                    }
                }
                if let Some((_, sent)) = pending_player_control {
                    if sent.elapsed() < Duration::from_secs(2) {
                        // Keep the player's own input intact while its authoritative echo is in flight.
                        // In particular, do not snap a guest back or publish a host measurement yet.
                        player.tracks = known_tracks.clone();
                        emit(events, repaint, Event::Player(player));
                        continue;
                    }
                    pending_player_control = None;
                    needs_alignment = true;
                    emit(events, repaint, Event::Status("Player control was not acknowledged; realigning to the room.".into()));
                }
                if player.seeking {
                    player.tracks = known_tracks.clone();
                    emit(events, repaint, Event::Player(player));
                    continue;
                }
                if let (Some(state), Some(position)) = (&snapshot, player.position) {
                    let path = mpv.get("path").await?.and_then(|v| v.as_str().map(str::to_owned));
                    if pending_load && path.as_deref() == Some(current_url.as_str()) {
                        pending_load = false;
                        emit(events, repaint, Event::Quality(loading_quality));
                        emit(events, repaint, Event::Status(format!("Playing {}", loading_quality.label())));
                        known_tracks = mpv.tracks().await?;
                        if let Some(id) = preferred_audio
                            && known_tracks.iter().any(|t| t.kind == "audio" && t.id == id) {
                            mpv.set("aid", json!(id)).await?;
                        }
                        if let Some(id) = preferred_subtitle
                            && id.is_none_or(|id| known_tracks.iter().any(|t| t.kind == "sub" && t.id == id)) {
                            mpv.set("sid", id.map_or(json!("no"), |id| json!(id))).await?;
                        }
                    }
                    if !pending_load {
                        let is_host = state.leader == Some(login.session_id);
                        let desired = target(state, clock_offset, login.media.duration);
                        if needs_alignment {
                            mpv.arm_native_input();
                            mpv.set("pause", json!(state.paused)).await?;
                            mpv.seek(desired).await?;
                            mpv.set("speed", json!(1.0)).await?;
                            needs_alignment = false;
                            // Do not publish the pre-seek sample as a host report.
                            player.tracks = known_tracks.clone();
                            emit(events, repaint, Event::Player(player));
                            continue;
                        }
                        if is_host {
                            if last_report.elapsed() >= Duration::from_millis(500) {
                                mpv.set("speed", json!(1.0)).await?;
                                socket.send(Message::Text(serde_json::to_string(&ClientMessage::Playback {
                                    position, paused: player.paused || player.buffering, control_revision: state.control_revision,
                                })?.into())).await?;
                                last_report = Instant::now();
                            }
                        } else {
                            if player.paused != state.paused { mpv.set("pause", json!(state.paused)).await?; }
                            match correction(position, desired, state.paused) {
                                Correction::Seek(position) => { mpv.seek(position).await?; }
                                Correction::Speed(speed) => { mpv.set("speed", json!(speed)).await?; }
                                _ => {}
                            }
                        }
                    }
                }
                player.tracks = known_tracks.clone();
                emit(events, repaint, Event::Player(player));
            }
        }
    }
    transcode_requests.abort_all();
    let _ = tokio::time::timeout(Duration::from_secs(1), socket.close(None)).await;
    Ok(())
}

fn target(snapshot: &RoomSnapshot, clock_offset: Option<f64>, duration: f64) -> f64 {
    let elapsed = if snapshot.paused {
        0.0
    } else {
        clock_offset.map_or(0.0, |offset| {
            ((unix_ms() + offset - snapshot.server_time_ms) / 1000.0).max(0.0)
        })
    };
    (snapshot.position + elapsed).clamp(0.0, duration)
}

#[cfg(test)]
mod playback_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_addresses_are_strict_and_support_tls() {
        assert_eq!(
            server_url("127.0.0.1").unwrap().as_str(),
            "http://127.0.0.1:8787/"
        );
        assert_eq!(
            server_url("http://example.org:80").unwrap().as_str(),
            "http://example.org/"
        );
        assert_eq!(
            server_url("https://example.org").unwrap().as_str(),
            "https://example.org/"
        );
        assert_eq!(
            server_url("[::1]:9000").unwrap().as_str(),
            "http://[::1]:9000/"
        );
        for bad in [
            "file:///etc/passwd",
            "https://user:pass@host",
            "http://host/path",
            "http://host?token=secret",
        ] {
            assert!(server_url(bad).is_err(), "{bad}");
        }
    }
}
