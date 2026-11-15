use super::*;
use crate::{
    media::{Encoder, MediaStore},
    server::{AppState, router},
};

#[path = "../../tests/common/mod.rs"]
mod common;

struct Peer {
    actions: async_mpsc::UnboundedSender<Action>,
    events: mpsc::Receiver<Event>,
    task: tokio::task::JoinHandle<Result<()>>,
    room: Option<RoomSnapshot>,
    id: Option<uuid::Uuid>,
    player: PlayerState,
    quality: Quality,
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
    host.actions.send(Action::Pause(true)).unwrap();
    host.actions.send(Action::Seek(8.0)).unwrap();
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
    guest.actions.send(Action::Quality(Quality::P480)).unwrap();
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
