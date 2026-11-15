use std::sync::mpsc;

use eframe::egui::{self, Color32, RichText};
use tokio::sync::mpsc::UnboundedSender;

use super::{Action, ConnectOptions, Event, PlayerState, start};
use crate::protocol::{MediaInfo, Quality, RoomSnapshot};

pub struct WatchApp {
    address: String,
    name: String,
    password: String,
    status: String,
    error: Option<String>,
    actions: Option<UnboundedSender<Action>>,
    events: Option<mpsc::Receiver<Event>>,
    media: Option<MediaInfo>,
    room: Option<RoomSnapshot>,
    session_id: Option<uuid::Uuid>,
    player: PlayerState,
    quality: Quality,
    requested_quality: Quality,
    seek: f64,
    dragging_seek: bool,
}

impl Default for WatchApp {
    fn default() -> Self {
        Self {
            address: "127.0.0.1:8787".into(),
            name: "Viewer".into(),
            password: String::new(),
            status: "Enter the server address and password to join a watch party.".into(),
            error: None,
            actions: None,
            events: None,
            media: None,
            room: None,
            session_id: None,
            player: PlayerState::default(),
            quality: Quality::Original,
            requested_quality: Quality::Original,
            seek: 0.0,
            dragging_seek: false,
        }
    }
}

impl WatchApp {
    fn send(&self, action: Action) {
        if let Some(sender) = &self.actions {
            let _ = sender.send(action);
        }
    }

    fn connect(&mut self, context: &egui::Context) {
        self.error = None;
        self.status = "Connecting…".into();
        self.player = PlayerState::default();
        self.quality = Quality::Original;
        self.requested_quality = Quality::Original;
        let (actions, events) = start(
            ConnectOptions {
                address: self.address.clone(),
                name: self.name.clone(),
                password: std::mem::take(&mut self.password),
            },
            context.clone(),
        );
        self.actions = Some(actions);
        self.events = Some(events);
    }

    fn drain_events(&mut self) {
        let events: Vec<Event> = self
            .events
            .as_ref()
            .map(|rx| rx.try_iter().collect())
            .unwrap_or_default();
        for event in events {
            match event {
                Event::Connected { media, session_id } => {
                    self.media = Some(media);
                    self.session_id = Some(session_id);
                }
                Event::Room(room) => self.room = Some(room),
                Event::Player(player) => {
                    if !self.dragging_seek {
                        self.seek = player.position.unwrap_or(0.0);
                    }
                    self.player = player;
                }
                Event::Quality(quality) => self.quality = quality,
                Event::Status(status) => self.status = status,
                Event::Failed(error) => self.error = Some(error),
                Event::Disconnected => {
                    self.actions = None;
                    self.media = None;
                    self.room = None;
                    self.session_id = None;
                    self.status = "Disconnected. Enter your password to reconnect.".into();
                }
            }
        }
    }

    fn connection_menu(&mut self, ui: &mut egui::Ui, context: &egui::Context) {
        ui.heading("Join a watch party");
        ui.add_space(12.0);
        egui::Grid::new("connect")
            .spacing([16.0, 12.0])
            .show(ui, |ui| {
                ui.label("Server");
                ui.add(
                    egui::TextEdit::singleline(&mut self.address)
                        .hint_text("192.168.1.20:8787 or https://watch.example.com")
                        .desired_width(390.0),
                );
                ui.end_row();
                ui.label("Your name");
                ui.add(egui::TextEdit::singleline(&mut self.name).char_limit(40));
                ui.end_row();
                ui.label("Password");
                ui.add(
                    egui::TextEdit::singleline(&mut self.password)
                        .password(true)
                        .char_limit(1024),
                );
                ui.end_row();
            });
        ui.add_space(12.0);
        let valid = !self.address.trim().is_empty()
            && !self.name.trim().is_empty()
            && !self.password.is_empty();
        if ui
            .add_enabled(valid, egui::Button::new("Connect"))
            .clicked()
        {
            self.connect(context);
        }
        ui.add_space(16.0);
        ui.label("No local video needed. The server sends the media to your mpv player.");
        ui.label(
            RichText::new("Use HTTPS or a trusted VPN outside your local network.")
                .color(Color32::YELLOW),
        );
    }

    fn playback_menu(&mut self, ui: &mut egui::Ui) {
        let Some(media) = self.media.clone() else {
            return;
        };
        let is_host = self
            .room
            .as_ref()
            .is_some_and(|room| room.leader == self.session_id);
        ui.heading(&media.title);
        ui.label(if is_host {
            "You are the host. Playback controls affect everyone."
        } else {
            "Following the host. Quality and language selections affect only you."
        });
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    is_host,
                    egui::Button::new(if self.player.paused {
                        "▶ Play"
                    } else {
                        "⏸ Pause"
                    }),
                )
                .clicked()
            {
                self.send(Action::Pause(!self.player.paused));
            }
            ui.label(format!(
                "{} / {}",
                timestamp(self.player.position.unwrap_or(0.0)),
                timestamp(media.duration)
            ));
            if self.player.buffering || self.player.seeking {
                ui.spinner();
                ui.label(if self.player.seeking {
                    "Seeking…"
                } else {
                    "Buffering…"
                });
            }
        });
        let slider = ui.add_enabled(
            is_host,
            egui::Slider::new(&mut self.seek, 0.0..=media.duration)
                .show_value(false)
                .text("Seek"),
        );
        self.dragging_seek = slider.dragged();
        if slider.drag_stopped() || (slider.changed() && !slider.dragged()) {
            self.send(Action::Seek(self.seek));
        }
        ui.add_space(12.0);
        egui::Grid::new("preferences")
            .spacing([16.0, 12.0])
            .show(ui, |ui| {
                ui.label("Video quality");
                let previous = self.requested_quality;
                egui::ComboBox::from_id_salt("quality")
                    .selected_text(self.requested_quality.label())
                    .show_ui(ui, |ui| {
                        for quality in Quality::ALL {
                            // No upscaling; do not offer profiles above the source resolution.
                            let enabled = quality
                                .height()
                                .zip(media.height)
                                .is_none_or(|(height, source)| height <= source);
                            ui.add_enabled_ui(enabled, |ui| {
                                ui.selectable_value(
                                    &mut self.requested_quality,
                                    quality,
                                    quality.label(),
                                );
                            });
                        }
                    });
                if previous != self.requested_quality {
                    self.send(Action::Quality(self.requested_quality));
                }
                ui.end_row();
                ui.label("Audio language");
                let mut audio = self.player.audio;
                let label = self
                    .player
                    .tracks
                    .iter()
                    .find(|t| t.kind == "audio" && Some(t.id) == audio)
                    .map(|t| t.label.as_str())
                    .unwrap_or("Default");
                egui::ComboBox::from_id_salt("audio")
                    .selected_text(label)
                    .show_ui(ui, |ui| {
                        for track in self.player.tracks.iter().filter(|t| t.kind == "audio") {
                            ui.selectable_value(
                                &mut audio,
                                Some(track.id),
                                format!("{}: {}", track.id, track.label),
                            );
                        }
                    });
                if audio != self.player.audio
                    && let Some(id) = audio
                {
                    self.send(Action::Audio(id));
                }
                ui.end_row();
                ui.label("Subtitles");
                let mut subtitle = self.player.subtitle;
                let label = self
                    .player
                    .tracks
                    .iter()
                    .find(|t| t.kind == "sub" && Some(t.id) == subtitle)
                    .map(|t| t.label.as_str())
                    .unwrap_or("Off");
                egui::ComboBox::from_id_salt("subtitle")
                    .selected_text(label)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut subtitle, None, "Off");
                        for track in self.player.tracks.iter().filter(|t| t.kind == "sub") {
                            ui.selectable_value(
                                &mut subtitle,
                                Some(track.id),
                                format!("{}: {}", track.id, track.label),
                            );
                        }
                    });
                if subtitle != self.player.subtitle {
                    self.send(Action::Subtitle(subtitle));
                }
                ui.end_row();
            });
        ui.label(format!("Active quality: {}", self.quality.label()));
        ui.small("A new transcoded quality is prepared in full before switching; this can take time. All included languages are preserved.");
        ui.separator();
        ui.heading("Watching together");
        if let Some(room) = &self.room {
            for person in &room.participants {
                ui.label(format!(
                    "{}{}{}",
                    person.name,
                    if Some(person.id) == room.leader {
                        " · host"
                    } else {
                        ""
                    },
                    if Some(person.id) == self.session_id {
                        " · you"
                    } else {
                        ""
                    }
                ));
            }
            if let Some(position) = self.player.position {
                ui.small(format!(
                    "Last server position: {} · local difference: {:+.2}s",
                    timestamp(room.position),
                    position - room.position
                ));
            }
        }
        ui.add_space(12.0);
        if ui.button("Disconnect").clicked() {
            self.send(Action::Disconnect);
            self.status = "Disconnecting…".into();
        }
    }
}

impl eframe::App for WatchApp {
    fn update(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();
        egui::TopBottomPanel::top("title").show(context, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Watchmpv");
                ui.label("Watch together, powered by mpv");
            });
        });
        egui::TopBottomPanel::bottom("status").show(context, |ui| {
            ui.label(&self.status);
            if let Some(error) = &self.error {
                ui.colored_label(Color32::LIGHT_RED, error);
            }
        });
        egui::CentralPanel::default().show(context, |ui| {
            ui.add_space(12.0);
            if self.media.is_some() {
                self.playback_menu(ui);
            } else if self.actions.is_some() {
                ui.spinner();
                ui.label("Connecting to the watch party…");
                if ui.button("Cancel").clicked() {
                    self.send(Action::Disconnect);
                }
            } else {
                self.connection_menu(ui, context);
            }
        });
        context.request_repaint_after(std::time::Duration::from_millis(250));
    }
}

impl Drop for WatchApp {
    fn drop(&mut self) {
        self.send(Action::Disconnect);
    }
}

fn timestamp(seconds: f64) -> String {
    let seconds = seconds.max(0.0) as u64;
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}
