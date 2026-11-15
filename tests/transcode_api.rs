mod common;

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use reqwest::{Client, StatusCode};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};
use watchmpv::{
    media::{Encoder, MediaStore},
    protocol::*,
    server::{AppState, router},
};

#[tokio::test]
async fn only_connected_host_can_cancel_or_retry_and_polling_never_restarts_cancelled_jobs() {
    let (temp, source) = common::fixture(60).await;
    let media = MediaStore::new(source, temp.path().join("cache"), Encoder::Cpu)
        .await
        .unwrap();
    let state = AppState::new(media.clone(), "test-password".into())
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let base = format!("http://{address}");
    let server = tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    let http = Client::new();
    for action in ["cancel", "retry"] {
        assert_eq!(
            http.post(format!("{base}/api/renditions/p720/{action}"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
    let host: LoginResponse = http
        .post(format!("{base}/api/login"))
        .json(&LoginRequest {
            name: "Host".into(),
            password: "test-password".into(),
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let guest: LoginResponse = http
        .post(format!("{base}/api/login"))
        .json(&LoginRequest {
            name: "Guest".into(),
            password: "test-password".into(),
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // Merely logging in does not grant host rights.
    assert_eq!(
        http.post(format!("{base}/api/renditions/p720/cancel"))
            .bearer_auth(&host.token)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let encoding: EncodingInfo = http
        .get(format!("{base}/api/encoding"))
        .bearer_auth(&host.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(encoding.encoder, VideoEncoder::Cpu);
    assert!(!encoding.automatic);
    let before: Vec<RenditionInfo> = http
        .get(format!("{base}/api/renditions"))
        .bearer_auth(&host.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(before.len(), 4);
    assert!(
        before
            .iter()
            .any(|r| r.quality == Quality::Original && matches!(r.status, RenditionStatus::Ready))
    );
    assert!(
        before
            .iter()
            .filter(|r| r.quality != Quality::Original)
            .all(|r| matches!(r.status, RenditionStatus::Idle))
    );

    let mut upgrade = format!("ws://{address}/api/ws")
        .into_client_request()
        .unwrap();
    upgrade.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", host.token).parse().unwrap(),
    );
    let (mut socket, _) = connect_async(upgrade).await.unwrap();
    // Wait for the socket to own the room before testing host HTTP controls.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Message::Text(text) = socket.next().await.unwrap().unwrap()
                && matches!(serde_json::from_str::<ServerMessage>(&text).unwrap(), ServerMessage::Room { snapshot } if snapshot.leader == Some(host.session_id)) { break; }
        }
    }).await.unwrap();
    for action in ["cancel", "retry"] {
        assert_eq!(
            http.post(format!("{base}/api/renditions/p720/{action}"))
                .bearer_auth(&guest.token)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        http.post(format!("{base}/api/renditions/unknown/cancel"))
            .bearer_auth(&host.token)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    for quality in [Quality::P1080, Quality::P720] {
        let status: RenditionStatus = http
            .post(format!("{base}/api/prepare"))
            .bearer_auth(&guest.token)
            .json(&PrepareRequest { quality })
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(matches!(status, RenditionStatus::Preparing { .. }));
    }
    let cancelled: RenditionStatus = http
        .post(format!("{base}/api/renditions/p720/cancel"))
        .bearer_auth(&host.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        matches!(cancelled, RenditionStatus::Preparing { progress } if progress.phase == PreparationPhase::Cancelling)
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while !matches!(
            media.status(Quality::P720).await,
            RenditionStatus::Cancelled
        ) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    for _ in 0..3 {
        let status: RenditionStatus = http
            .post(format!("{base}/api/prepare"))
            .bearer_auth(&guest.token)
            .json(&PrepareRequest {
                quality: Quality::P720,
            })
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(matches!(status, RenditionStatus::Cancelled));
    }
    let limited = http
        .post(format!("{base}/api/renditions/p720/retry"))
        .bearer_auth(&host.token)
        .send()
        .await
        .unwrap();
    assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(limited.headers()["retry-after"], "5");
    assert_eq!(
        http.get(format!("{base}/api/stream/original"))
            .bearer_auth(&guest.token)
            .header("Range", "bytes=0-15")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::PARTIAL_CONTENT
    );
    tokio::time::sleep(Duration::from_millis(5100)).await;
    socket
        .send(Message::Text(
            serde_json::to_string(&ClientMessage::Ping {
                client_time_ms: unix_ms(),
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    let retry: RenditionStatus = http
        .post(format!("{base}/api/renditions/p720/retry"))
        .bearer_auth(&host.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(matches!(retry, RenditionStatus::Preparing { .. }));
    media.cancel(Quality::P720).await;
    media.cancel(Quality::P1080).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if !matches!(
                media.status(Quality::P720).await,
                RenditionStatus::Preparing { .. }
            ) && !matches!(
                media.status(Quality::P1080).await,
                RenditionStatus::Preparing { .. }
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    socket.close(None).await.unwrap();
    server.abort();
}
