use super::*;
use crate::{
    media::{Encoder, MediaStore},
    server::{AppState, router},
};

use crate::test_media as common;

struct Peer {
    actions: async_mpsc::UnboundedSender<Action>,
    events: mpsc::Receiver<Event>,
    task: tokio::task::JoinHandle<Result<()>>,
    room: Option<RoomSnapshot>,
    id: Option<uuid::Uuid>,
    player: PlayerState,
    quality: Quality,
    renditions: Vec<RenditionInfo>,
}

impl Peer {
    fn launch(address: &str, name: &str) -> Self {
        let (actions, receiver) = async_mpsc::unbounded_channel();
        let (events, output) = mpsc::channel();
        let options = ConnectOptions {
            address: address.into(),
            name: name.into(),
            password: "test-password".into(),
        };
        let task = tokio::spawn(async move {
            run(
                options,
                receiver,
                &events,
                &eframe::egui::Context::default(),
                true,
            )
            .await
        });
        Self {
            actions,
            events: output,
            task,
            room: None,
            id: None,
            player: PlayerState::default(),
            quality: Quality::Original,
            renditions: Vec::new(),
        }
    }

    async fn wait(&mut self, predicate: impl Fn(&Self) -> bool) {
        tokio::time::timeout(Duration::from_secs(45), async {
            loop {
                for event in self.events.try_iter() {
                    match event {
                        Event::Connected { session_id, .. } => self.id = Some(session_id),
                        Event::Room(room) => self.room = Some(room),
                        Event::Player(player) => self.player = player,
                        Event::Quality(quality) => self.quality = quality,
                        Event::Renditions(renditions) => self.renditions = renditions,
                        Event::Status(status) => eprintln!("client: {status}"),
                        _ => {}
                    }
                }
                if self.task.is_finished() {
                    panic!("Client unexpectedly exited: {:?}", (&mut self.task).await);
                }
                if predicate(self) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "Timed out: position={:?}, paused={}, quality={:?}",
                self.player.position, self.player.paused, self.quality
            )
        });
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn two_real_mpv_players_follow_play_pause_seek_quality_and_languages() {
    let (temp, source) = common::fixture(20).await;
    let media = MediaStore::new(source.clone(), temp.path().join("cache"), Encoder::Cpu)
        .await
        .unwrap();
    let state = AppState::new(media, "test-password".into()).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    let mut host = Peer::launch(&address, "Host");
    host.wait(|p| p.player.position.is_some() && p.room.as_ref().is_some_and(|r| r.leader == p.id))
        .await;
    let mut guest = Peer::launch(&address, "Guest");
    guest
        .wait(|p| {
            p.player.position.is_some()
                && p.room.as_ref().is_some_and(|r| r.participants.len() == 2)
        })
        .await;
    assert_eq!(
        guest
            .player
            .tracks
            .iter()
            .filter(|t| t.kind == "audio")
            .count(),
        2
    );
    assert_eq!(
        guest
            .player
            .tracks
            .iter()
            .filter(|t| t.kind == "sub")
            .count(),
        2
    );

    host.actions.send(Action::Pause(false)).unwrap();
    guest
        .wait(|p| !p.player.paused && p.player.position.is_some_and(|position| position > 1.0))
        .await;
    guest.actions.send(Action::Pause(true)).unwrap();
    guest.actions.send(Action::Seek(8.0)).unwrap();
    host.wait(|p| {
        p.player.paused
            && p.player
                .position
                .is_some_and(|position| (position - 8.0).abs() < 0.2)
    })
    .await;
    guest
        .wait(|p| {
            p.player.paused
                && p.player
                    .position
                    .is_some_and(|position| (position - 8.0).abs() < 0.2)
        })
        .await;

    guest.actions.send(Action::Pause(false)).unwrap();
    host.wait(|p| !p.player.paused && p.player.position.is_some_and(|position| position > 8.5))
        .await;
    guest.actions.send(Action::Pause(true)).unwrap();
    guest.actions.send(Action::Seek(8.0)).unwrap();
    host.wait(|p| {
        p.player.paused
            && p.player
                .position
                .is_some_and(|position| (position - 8.0).abs() < 0.2)
    })
    .await;
    guest
        .wait(|p| {
            p.player.paused
                && p.player
                    .position
                    .is_some_and(|position| (position - 8.0).abs() < 0.2)
        })
        .await;

    guest.actions.send(Action::Audio(2)).unwrap();
    guest.actions.send(Action::Subtitle(Some(2))).unwrap();
    guest
        .wait(|p| p.player.audio == Some(2) && p.player.subtitle == Some(2))
        .await;
    // A failed preparation must not interrupt the original stream. When the host retries,
    // everyone still waiting for that quality should switch without re-selecting it.
    let moved = temp.path().join("temporarily-unavailable.mkv");
    tokio::fs::rename(&source, &moved).await.unwrap();
    guest.actions.send(Action::Quality(Quality::P480)).unwrap();
    guest
        .wait(|p| {
            p.renditions.iter().any(|r| {
                r.quality == Quality::P480 && matches!(r.status, RenditionStatus::Failed { .. })
            })
        })
        .await;
    assert_eq!(guest.quality, Quality::Original);
    assert!(guest.player.paused);
    assert!((guest.player.position.unwrap() - 8.0).abs() < 0.2);
    tokio::fs::rename(&moved, &source).await.unwrap();
    tokio::time::sleep(Duration::from_millis(5100)).await;
    host.actions
        .send(Action::RetryQuality(Quality::P480))
        .unwrap();
    guest
        .wait(|p| {
            p.quality == Quality::P480
                && p.player.audio == Some(2)
                && p.player.subtitle == Some(2)
                && p.player
                    .position
                    .is_some_and(|position| (position - 8.0).abs() < 0.2)
        })
        .await;
    guest.actions.send(Action::Subtitle(None)).unwrap();
    guest.wait(|p| p.player.subtitle.is_none()).await;

    host.actions.send(Action::Disconnect).unwrap();
    tokio::time::timeout(Duration::from_secs(5), &mut host.task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    guest
        .wait(|p| {
            p.room
                .as_ref()
                .is_some_and(|r| r.leader == p.id && r.paused && r.participants.len() == 1)
        })
        .await;
    guest.actions.send(Action::Disconnect).unwrap();
    tokio::time::timeout(Duration::from_secs(5), &mut guest.task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    server.abort();
}

async fn player_control_applied(
    peer: &mut Peer,
    revision: u64,
    paused: bool,
    position: Option<f64>,
) {
    peer.wait(|p| {
        p.room
            .as_ref()
            .is_some_and(|r| r.control_revision >= revision && r.paused == paused)
            && p.player.paused == paused
            && !p.player.seeking
            && p.player
                .position
                .is_some_and(|actual| position.is_none_or(|target| (actual - target).abs() < 0.2))
    })
    .await;
    assert_eq!(
        peer.room.as_ref().unwrap().control_revision,
        revision,
        "Native input or sync correction echoed more than once"
    );
}

#[tokio::test]
async fn native_guest_keyboard_and_osc_commands_control_everyone_without_sync_feedback() {
    let (temp, source) = common::fixture(30).await;
    let media = MediaStore::new(source, temp.path().join("cache"), Encoder::Cpu)
        .await
        .unwrap();
    let state = AppState::new(media, "test-password".into()).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    let mut host = Peer::launch(&address, "Host");
    host.wait(|p| p.player.position.is_some() && p.room.as_ref().is_some_and(|r| r.leader == p.id))
        .await;
    let mut guest = Peer::launch(&address, "Guest");
    guest
        .wait(|p| {
            p.player.position.is_some()
                && p.room.as_ref().is_some_and(|r| r.participants.len() == 2)
        })
        .await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    player_control_applied(&mut guest, 0, true, Some(0.0)).await;

    // These are raw mpv commands, not Action::Pause/Seek from the companion. keypress uses
    // the player's real default key bindings; the seek commands are the same path used by OSC.
    guest
        .actions
        .send(Action::PlayerCommand(json!(["keypress", "SPACE"])))
        .unwrap();
    player_control_applied(&mut host, 1, false, None).await;
    guest
        .wait(|p| p.player.position.is_some_and(|position| position > 0.5))
        .await;
    guest
        .actions
        .send(Action::PlayerCommand(json!(["keypress", "SPACE"])))
        .unwrap();
    player_control_applied(&mut host, 2, true, None).await;
    player_control_applied(&mut guest, 2, true, None).await;
    let before_arrow = guest.player.position.unwrap();
    guest
        .actions
        .send(Action::PlayerCommand(json!(["keypress", "RIGHT"])))
        .unwrap();
    guest
        .wait(|p| {
            p.room.as_ref().is_some_and(|r| r.control_revision >= 3)
                && p.player
                    .position
                    .is_some_and(|position| (position - before_arrow).abs() > 0.3)
                && !p.player.seeking
        })
        .await;
    let arrow_position = guest.room.as_ref().unwrap().position;
    player_control_applied(&mut host, 3, true, Some(arrow_position)).await;

    guest
        .actions
        .send(Action::PlayerCommand(json!([
            "seek",
            8.0,
            "absolute+exact"
        ])))
        .unwrap();
    player_control_applied(&mut host, 4, true, Some(8.0)).await;
    player_control_applied(&mut guest, 4, true, Some(8.0)).await;
    guest
        .actions
        .send(Action::PlayerCommand(json!([
            "seek",
            2.0,
            "relative+exact"
        ])))
        .unwrap();
    player_control_applied(&mut host, 5, true, Some(10.0)).await;
    guest
        .actions
        .send(Action::PlayerCommand(json!([
            "seek",
            40.0,
            "absolute-percent+exact"
        ])))
        .unwrap();
    player_control_applied(&mut host, 6, true, Some(12.0)).await;
    player_control_applied(&mut guest, 6, true, Some(12.0)).await;

    // Inject local drift using the *managed* path, then let the follower correct it. Neither
    // application-issued seek may become a new shared command or rewind the host to 2 seconds.
    guest.actions.send(Action::ManagedPlayerSeek(2.0)).unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    player_control_applied(&mut guest, 6, true, Some(12.0)).await;
    player_control_applied(&mut host, 6, true, Some(12.0)).await;
    guest
        .actions
        .send(Action::PlayerCommand(json!(["set_property", "volume", 45])))
        .unwrap();
    guest
        .actions
        .send(Action::PlayerCommand(json!(["set_property", "aid", 2])))
        .unwrap();
    guest
        .actions
        .send(Action::PlayerCommand(json!(["set_property", "sid", 2])))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    player_control_applied(&mut host, 6, true, Some(12.0)).await;

    // Native host controls take the same explicit, revisioned path, rather than being
    // inferred only from periodic measurements. They must not double-publish either.
    host.actions
        .send(Action::PlayerCommand(json!(["keypress", "SPACE"])))
        .unwrap();
    player_control_applied(&mut guest, 7, false, None).await;
    // A playing quality switch internally pauses, loads, and seeks the guest. None of these
    // managed transitions may become a room pause/seek or echo a new control revision.
    guest.actions.send(Action::Quality(Quality::P480)).unwrap();
    guest
        .wait(|p| p.quality == Quality::P480 && !p.player.paused && !p.player.seeking)
        .await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    player_control_applied(&mut host, 7, false, None).await;
    player_control_applied(&mut guest, 7, false, None).await;
    host.actions
        .send(Action::PlayerCommand(json!(["keypress", "SPACE"])))
        .unwrap();
    player_control_applied(&mut guest, 8, true, None).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    player_control_applied(&mut host, 8, true, None).await;
    player_control_applied(&mut guest, 8, true, None).await;

    host.actions.send(Action::Disconnect).unwrap();
    tokio::time::timeout(Duration::from_secs(5), &mut host.task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    guest.actions.send(Action::Disconnect).unwrap();
    tokio::time::timeout(Duration::from_secs(5), &mut guest.task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    server.abort();
}
