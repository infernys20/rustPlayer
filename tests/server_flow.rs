mod common;

use std::{sync::Arc, time::Duration};

use futures_util::{SinkExt, StreamExt};
use reqwest::{Client, StatusCode};
use tokio::{net::TcpStream, time::timeout};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};
use watchmpv::{
    media::{Encoder, MediaStore, inspect},
    protocol::*,
    server::{AppState, router},
};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn login(http: &Client, base: &str, name: &str) -> LoginResponse {
    http.post(format!("{base}/api/login"))
        .json(&LoginRequest {
            name: name.into(),
            password: "test-password".into(),
        })
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn connect(base: &str, token: &str) -> Socket {
    let mut request = format!("{}/api/ws", base.replacen("http", "ws", 1))
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {token}").parse().unwrap());
    connect_async(request).await.unwrap().0
}

async fn next_message(socket: &mut Socket) -> ServerMessage {
    timeout(Duration::from_secs(5), async {
        loop {
            let message = socket.next().await.unwrap().unwrap();
            if let Message::Text(text) = message {
                return serde_json::from_str(&text).unwrap();
            }
        }
    })
    .await
    .unwrap()
}

async fn until_room(
    socket: &mut Socket,
    predicate: impl Fn(&RoomSnapshot) -> bool,
) -> RoomSnapshot {
    timeout(Duration::from_secs(5), async {
        loop {
            if let ServerMessage::Room { snapshot } = next_message(socket).await
                && predicate(&snapshot)
            {
                return snapshot;
            }
        }
    })
    .await
    .unwrap()
}

async fn send(socket: &mut Socket, message: ClientMessage) {
    socket
        .send(Message::Text(
            serde_json::to_string(&message).unwrap().into(),
        ))
        .await
        .unwrap();
}

#[tokio::test]
async fn authenticated_streaming_transcoding_and_host_handoff() {
    let (temp, source) = common::fixture(4).await;
    let media = MediaStore::new(source.clone(), temp.path().join("cache"), Encoder::Cpu)
        .await
        .unwrap();
    assert_eq!(
        media
            .info
            .tracks
            .iter()
            .filter(|t| t.kind == "audio")
            .count(),
        2
    );
    assert_eq!(
        media
            .info
            .tracks
            .iter()
            .filter(|t| t.kind == "subtitle")
            .count(),
        2
    );
    let state = AppState::new(Arc::clone(&media), "test-password".into())
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    let http = Client::new();
    for path in [
        "api/media",
        "api/encoding",
        "api/renditions",
        "api/stream/original",
        "api/stream/p480",
        "api/ws",
    ] {
        assert_eq!(
            http.get(format!("{base}/{path}"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        http.post(format!("{base}/api/prepare"))
            .json(&PrepareRequest {
                quality: Quality::P480
            })
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        http.post(format!("{base}/api/login"))
            .json(&LoginRequest {
                name: "intruder".into(),
                password: "incorrect".into()
            })
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let host = login(&http, &base, "Host").await;
    let guest = login(&http, &base, "Guest").await;
    assert!(!host.token.contains(&host.session_id.to_string()));
    let range = http
        .get(format!("{base}/api/stream/original"))
        .bearer_auth(&host.token)
        .header("Range", "bytes=0-63")
        .send()
        .await
        .unwrap();
    assert_eq!(range.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(range.headers()["cache-control"], "private, no-store");
    assert_eq!(
        range.bytes().await.unwrap().as_ref(),
        &tokio::fs::read(&source).await.unwrap()[..64]
    );
    let head = http
        .head(format!("{base}/api/stream/original"))
        .bearer_auth(&guest.token)
        .send()
        .await
        .unwrap();
    assert_eq!(head.status(), StatusCode::OK);
    assert!(head.headers().contains_key("content-length"));
    assert_eq!(
        http.get(format!("{base}/api/stream/not-a-profile"))
            .bearer_auth(&host.token)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        http.get(format!("{base}/api/stream/p480"))
            .bearer_auth(&host.token)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );

    let mut browser = format!("{}/api/ws", base.replacen("http", "ws", 1))
        .into_client_request()
        .unwrap();
    browser.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", host.token).parse().unwrap(),
    );
    browser
        .headers_mut()
        .insert("Origin", "https://untrusted.example".parse().unwrap());
    let rejected = connect_async(browser).await.unwrap_err();
    assert!(
        matches!(rejected, tokio_tungstenite::tungstenite::Error::Http(response) if response.status() == StatusCode::FORBIDDEN)
    );

    let mut host_ws = connect(&base, &host.token).await;
    until_room(&mut host_ws, |s| s.leader == Some(host.session_id)).await;
    let mut guest_ws = connect(&base, &guest.token).await;
    until_room(&mut guest_ws, |s| s.participants.len() == 2).await;
    let mut duplicate = connect(&base, &guest.token).await;
    assert!(matches!(
        next_message(&mut duplicate).await,
        ServerMessage::Error { .. }
    ));
    send(
        &mut guest_ws,
        ClientMessage::Playback {
            position: 2.0,
            paused: false,
            control_revision: 0,
        },
    )
    .await;
    loop {
        if let ServerMessage::Error { message } = next_message(&mut guest_ws).await {
            assert!(message.contains("host"));
            break;
        }
    }
    send(
        &mut host_ws,
        ClientMessage::Control {
            position: Some(1.0),
            paused: Some(true),
        },
    )
    .await;
    until_room(&mut guest_ws, |s| s.position == 1.0 && s.paused).await;
    // The guest controls both clients. Pause-only controls must not rewind to a stale sample.
    send(
        &mut guest_ws,
        ClientMessage::Control {
            position: Some(2.0),
            paused: Some(false),
        },
    )
    .await;
    let shared = until_room(&mut host_ws, |s| s.position >= 2.0 && !s.paused).await;
    send(
        &mut guest_ws,
        ClientMessage::Control {
            position: None,
            paused: Some(true),
        },
    )
    .await;
    let paused = until_room(&mut host_ws, |s| {
        s.control_revision > shared.control_revision && s.paused
    })
    .await;
    assert!(paused.position >= 2.0);
    // An in-flight measurement from before the guest's controls is silently ignored.
    send(
        &mut host_ws,
        ClientMessage::Playback {
            position: 0.0,
            paused: false,
            control_revision: 0,
        },
    )
    .await;
    until_room(&mut guest_ws, |s| {
        s.control_revision == paused.control_revision && s.paused && s.position >= 2.0
    })
    .await;
    send(
        &mut guest_ws,
        ClientMessage::Ping {
            client_time_ms: 123.0,
        },
    )
    .await;
    loop {
        if let ServerMessage::Pong {
            client_time_ms,
            server_time_ms,
        } = next_message(&mut guest_ws).await
        {
            assert_eq!(client_time_ms, 123.0);
            assert!(server_time_ms > 0.0);
            break;
        }
    }
    send(
        &mut host_ws,
        ClientMessage::Control {
            position: Some(1.0),
            paused: Some(false),
        },
    )
    .await;
    until_room(&mut guest_ws, |s| !s.paused && s.position >= 1.0).await;
    host_ws.close(None).await.unwrap();
    until_room(&mut guest_ws, |s| {
        s.leader == Some(guest.session_id) && s.paused && s.participants.len() == 1
    })
    .await;

    // Concurrent viewers share a single job and a single complete, seekable file.
    let (first, second) = tokio::join!(media.prepare(Quality::P480), media.prepare(Quality::P480));
    assert!(matches!(first, RenditionStatus::Preparing { .. }));
    assert!(matches!(second, RenditionStatus::Preparing { .. }));
    timeout(Duration::from_secs(60), async {
        loop {
            let status: RenditionStatus = http
                .post(format!("{base}/api/prepare"))
                .bearer_auth(&guest.token)
                .json(&PrepareRequest {
                    quality: Quality::P480,
                })
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            match status {
                RenditionStatus::Ready => break,
                RenditionStatus::Failed { message } => panic!("{message}"),
                RenditionStatus::Preparing { progress } => {
                    assert!(
                        progress.fraction.is_finite() && (0.0..=1.0).contains(&progress.fraction)
                    );
                    assert!(
                        progress
                            .eta_seconds
                            .is_none_or(|eta| eta.is_finite() && eta >= 0.0)
                    );
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                other => panic!("Unexpected status: {other:?}"),
            }
        }
    })
    .await
    .unwrap();
    let transcoded = inspect(&media.path(Quality::P480)).await.unwrap();
    assert_eq!(transcoded.height, Some(480));
    assert_eq!(
        transcoded
            .tracks
            .iter()
            .filter(|t| t.kind == "audio")
            .count(),
        2
    );
    assert_eq!(
        transcoded
            .tracks
            .iter()
            .filter(|t| t.kind == "subtitle")
            .count(),
        2
    );
    for language in ["eng", "jpn", "fra"] {
        assert!(
            transcoded
                .tracks
                .iter()
                .any(|t| t.language.as_deref() == Some(language))
        );
    }
    let stream = http
        .get(format!("{base}/api/stream/p480"))
        .bearer_auth(&guest.token)
        .header("Range", "bytes=100-199")
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(stream.bytes().await.unwrap().len(), 100);
    // Prepared outputs remain protected, too; tokens in query strings are not accepted.
    assert_eq!(
        http.get(format!("{base}/api/stream/p480?token={}", guest.token))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        http.get(format!("{base}/api/stream/p480"))
            .bearer_auth("invalid")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    guest_ws.close(None).await.unwrap();
    server.abort();
}
