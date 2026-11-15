use std::time::Instant;

use uuid::Uuid;

use crate::protocol::{Participant, RoomSnapshot, unix_ms};

pub struct Room {
    participants: Vec<Participant>,
    leader: Option<Uuid>,
    position: f64,
    paused: bool,
    anchor: Instant,
    duration: f64,
    revision: u64,
}

impl Room {
    pub fn new(duration: f64) -> Self {
        Self {
            participants: Vec::new(),
            leader: None,
            position: 0.0,
            paused: true,
            anchor: Instant::now(),
            duration,
            revision: 0,
        }
    }

    pub fn join(&mut self, participant: Participant) -> Result<(), &'static str> {
        if self.participants.iter().any(|p| p.id == participant.id) {
            return Err("This session already has an open connection");
        }
        if self.participants.len() >= 32 {
            return Err("Room is full (32 participants)");
        }
        self.leader.get_or_insert(participant.id);
        self.participants.push(participant);
        self.revision += 1;
        Ok(())
    }

    pub fn leave(&mut self, id: Uuid) {
        self.participants.retain(|p| p.id != id);
        if self.leader == Some(id) {
            self.position = self.current_position();
            self.anchor = Instant::now();
            self.paused = true;
            self.leader = self.participants.first().map(|p| p.id);
        }
        self.revision += 1;
    }

    pub fn update(&mut self, id: Uuid, position: f64, paused: bool) -> Result<(), &'static str> {
        if self.leader != Some(id) {
            return Err("Only the host can control shared playback");
        }
        if !position.is_finite() || position < 0.0 || position > self.duration + 1.0 {
            return Err("Invalid playback position");
        }
        self.position = position.min(self.duration);
        self.paused = paused;
        self.anchor = Instant::now();
        self.revision += 1;
        Ok(())
    }

    fn current_position(&self) -> f64 {
        (self.position
            + if self.paused {
                0.0
            } else {
                self.anchor.elapsed().as_secs_f64()
            })
        .min(self.duration)
    }

    pub fn snapshot(&self) -> RoomSnapshot {
        RoomSnapshot {
            leader: self.leader,
            participants: self.participants.clone(),
            position: self.current_position(),
            paused: self.paused || self.current_position() >= self.duration,
            server_time_ms: unix_ms(),
            revision: self.revision,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn participant() -> Participant {
        Participant {
            id: Uuid::new_v4(),
            name: "Viewer".into(),
        }
    }

    #[test]
    fn only_host_can_control_and_duplicate_connections_are_rejected() {
        let mut room = Room::new(100.0);
        let host = participant();
        let guest = participant();
        room.join(host.clone()).unwrap();
        room.join(guest.clone()).unwrap();
        assert!(room.join(host.clone()).is_err());
        assert!(room.update(guest.id, 10.0, false).is_err());
        room.update(host.id, 10.0, true).unwrap();
        assert_eq!(room.snapshot().position, 10.0);
        assert!(room.update(host.id, f64::NAN, false).is_err());
        assert!(room.update(host.id, -1.0, false).is_err());
        assert!(room.update(host.id, 200.0, false).is_err());
    }

    #[test]
    fn timeline_projects_from_monotonic_time_and_stops_at_the_end() {
        let mut room = Room::new(100.0);
        let host = participant();
        room.join(host.clone()).unwrap();
        room.update(host.id, 50.0, false).unwrap();
        room.anchor -= std::time::Duration::from_secs(2);
        assert!((52.0..53.0).contains(&room.snapshot().position));
        room.update(host.id, 50.0, true).unwrap();
        room.anchor -= std::time::Duration::from_secs(2);
        assert_eq!(room.snapshot().position, 50.0);
        room.update(host.id, 99.0, false).unwrap();
        room.anchor -= std::time::Duration::from_secs(2);
        assert_eq!(room.snapshot().position, 100.0);
        assert!(room.snapshot().paused);
    }

    #[test]
    fn host_disconnect_elects_oldest_guest_and_pauses() {
        let mut room = Room::new(100.0);
        let host = participant();
        let guest = participant();
        room.join(host.clone()).unwrap();
        room.join(guest.clone()).unwrap();
        room.update(host.id, 25.0, false).unwrap();
        room.leave(host.id);
        let snapshot = room.snapshot();
        assert_eq!(snapshot.leader, Some(guest.id));
        assert!(snapshot.paused);
        assert!((25.0..26.0).contains(&snapshot.position));
        room.leave(guest.id);
        assert_eq!(room.snapshot().leader, None);
    }
}
