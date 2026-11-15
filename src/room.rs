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
    control_revision: u64,
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
            control_revision: 0,
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
            self.control_revision += 1;
        }
        self.revision += 1;
    }

    /// Shared controls are serialized by the room mutex: the last accepted command wins.
    /// Pause-only commands sample the projected position, rather than rewinding to a client sample.
    pub fn control(
        &mut self,
        id: Uuid,
        position: Option<f64>,
        paused: Option<bool>,
    ) -> Result<(), &'static str> {
        if !self.participants.iter().any(|p| p.id == id) {
            return Err("Join the room before controlling playback");
        }
        if position.is_none() && paused.is_none() {
            return Err("A control must specify a position or pause state");
        }
        if let Some(position) = position {
            self.validate_position(position)?;
        }
        let current = self.current_position();
        let was_paused = self.paused || current >= self.duration;
        self.position = position.unwrap_or(current).min(self.duration);
        self.paused = paused.unwrap_or(was_paused);
        self.anchor = Instant::now();
        self.control_revision += 1;
        self.revision += 1;
        Ok(())
    }

    /// Only the host measures the playback clock. Reject old measurements silently: a report
    /// already in flight must never undo a newer play/pause/seek from another participant.
    pub fn report(
        &mut self,
        id: Uuid,
        position: f64,
        paused: bool,
        control_revision: u64,
    ) -> Result<bool, &'static str> {
        if self.leader != Some(id) {
            return Err("Only the host can report playback measurements");
        }
        self.validate_position(position)?;
        if control_revision != self.control_revision {
            return Ok(false);
        }
        self.position = position.min(self.duration);
        self.paused = paused;
        self.anchor = Instant::now();
        self.revision += 1;
        Ok(true)
    }

    fn validate_position(&self, position: f64) -> Result<(), &'static str> {
        if !position.is_finite() || position < 0.0 || position > self.duration + 1.0 {
            Err("Invalid playback position")
        } else {
            Ok(())
        }
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
        let position = self.current_position();
        RoomSnapshot {
            leader: self.leader,
            participants: self.participants.clone(),
            position,
            paused: self.paused || position >= self.duration,
            server_time_ms: unix_ms(),
            revision: self.revision,
            control_revision: self.control_revision,
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
    fn all_connected_participants_can_control_but_only_host_reports() {
        let mut room = Room::new(100.0);
        let host = participant();
        let guest = participant();
        room.join(host.clone()).unwrap();
        room.join(guest.clone()).unwrap();
        assert!(room.join(host.clone()).is_err());
        room.control(guest.id, Some(10.0), Some(false)).unwrap();
        let revision = room.snapshot().control_revision;
        assert!(!room.snapshot().paused);
        assert!(room.report(guest.id, 10.0, false, revision).is_err());
        assert!(room.report(host.id, 10.5, false, revision).unwrap());
        room.control(guest.id, None, Some(true)).unwrap();
        assert!(room.snapshot().paused);
        assert!(room.control(Uuid::new_v4(), None, Some(false)).is_err());
        assert!(room.control(guest.id, None, None).is_err());
        for bad in [f64::NAN, f64::INFINITY, -1.0, 200.0] {
            assert!(room.control(guest.id, Some(bad), None).is_err());
        }
    }

    #[test]
    fn stale_reports_cannot_undo_guest_controls_and_latest_control_wins() {
        let mut room = Room::new(100.0);
        let host = participant();
        let guest = participant();
        room.join(host.clone()).unwrap();
        room.join(guest.clone()).unwrap();
        let old = room.snapshot().control_revision;
        room.control(guest.id, Some(50.0), Some(true)).unwrap();
        assert!(!room.report(host.id, 2.0, false, old).unwrap());
        assert_eq!(room.snapshot().position, 50.0);
        assert!(room.snapshot().paused);
        room.control(host.id, Some(60.0), None).unwrap();
        room.control(guest.id, Some(70.0), None).unwrap();
        let snapshot = room.snapshot();
        assert_eq!(snapshot.position, 70.0);
        assert!(snapshot.paused);
        assert!(
            room.report(host.id, 70.0, true, snapshot.control_revision)
                .unwrap()
        );
        assert!(
            !room
                .report(host.id, 0.0, false, snapshot.control_revision + 1)
                .unwrap()
        );
    }

    #[test]
    fn timeline_projects_from_monotonic_time_and_pause_preserves_projected_position() {
        let mut room = Room::new(100.0);
        let host = participant();
        room.join(host.clone()).unwrap();
        room.control(host.id, Some(50.0), Some(false)).unwrap();
        room.anchor -= std::time::Duration::from_secs(2);
        assert!((52.0..53.0).contains(&room.snapshot().position));
        room.control(host.id, None, Some(true)).unwrap();
        let paused = room.snapshot().position;
        assert!((52.0..53.0).contains(&paused));
        room.anchor -= std::time::Duration::from_secs(2);
        assert_eq!(room.snapshot().position, paused);
        room.control(host.id, Some(99.0), Some(false)).unwrap();
        room.anchor -= std::time::Duration::from_secs(2);
        assert_eq!(room.snapshot().position, 100.0);
        assert!(room.snapshot().paused);
        room.control(host.id, Some(20.0), None).unwrap();
        assert!(room.snapshot().paused);
    }

    #[test]
    fn host_disconnect_elects_oldest_guest_pauses_and_invalidates_old_reports() {
        let mut room = Room::new(100.0);
        let host = participant();
        let guest = participant();
        room.join(host.clone()).unwrap();
        room.join(guest.clone()).unwrap();
        room.control(host.id, Some(25.0), Some(false)).unwrap();
        let old = room.snapshot().control_revision;
        room.leave(host.id);
        let snapshot = room.snapshot();
        assert_eq!(snapshot.leader, Some(guest.id));
        assert!(snapshot.paused);
        assert!((25.0..26.0).contains(&snapshot.position));
        assert!(!room.report(guest.id, 0.0, false, old).unwrap());
        room.leave(guest.id);
        assert_eq!(room.snapshot().leader, None);
    }
}
