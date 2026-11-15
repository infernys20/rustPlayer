//! Separate player input from changes issued by synchronization. mpv's seek events do not
//! include their origin, so managed seeks retain their expected landing point until settled.
use std::time::Instant;

#[derive(Debug, Default, PartialEq)]
pub struct NativeControl {
    pub position: Option<f64>,
    pub paused: Option<bool>,
}

impl NativeControl {
    pub fn is_empty(&self) -> bool {
        self.position.is_none() && self.paused.is_none()
    }
}

struct ManagedSeek {
    position: f64,
    issued: Instant,
    count: u32,
}

#[derive(Default)]
pub struct NativeInput {
    armed: bool,
    expected_pause: bool,
    seeks: u32,
    managed_seek: Option<ManagedSeek>,
}

impl NativeInput {
    pub fn loading(&mut self) {
        self.armed = false;
        self.expected_pause = true;
        self.seeks = 0;
        self.managed_seek = None;
    }

    pub fn arm(&mut self) {
        self.armed = true;
    }

    pub fn managed_pause(&mut self, paused: bool) {
        self.expected_pause = paused;
    }

    pub fn managed_seek(&mut self, position: f64) {
        let count = self
            .managed_seek
            .as_ref()
            .map_or(1, |pending| pending.count.saturating_add(1));
        self.managed_seek = Some(ManagedSeek {
            position,
            issued: Instant::now(),
            count,
        });
    }

    pub fn seek_event(&mut self) {
        if self.armed {
            self.seeks = self.seeks.saturating_add(1);
        }
    }
    pub fn seek_pending(&self) -> bool {
        self.seeks > 0
    }

    pub fn observe(&mut self, position: Option<f64>, paused: bool, seeking: bool) -> NativeControl {
        if !self.armed {
            return NativeControl::default();
        }
        // A no-op/coalesced managed seek need not emit an event. Do not let that expectation
        // survive indefinitely and swallow a later native seek to a nearby position.
        if self.seeks == 0
            && !seeking
            && self
                .managed_seek
                .as_ref()
                .is_some_and(|seek| seek.issued.elapsed().as_secs_f64() > 2.0)
        {
            self.managed_seek = None;
        }
        let mut control = NativeControl::default();
        if paused != self.expected_pause {
            self.expected_pause = paused;
            control.paused = Some(paused);
            // Freeze where the user actually paused, not at a later server-clock sample.
            if paused && !seeking {
                control.position = position;
            }
        }
        if self.seeks > 0
            && !seeking
            && let Some(position) = position.filter(|position| position.is_finite())
        {
            let managed = self.managed_seek.take().is_some_and(|expected| {
                // Permit frame rounding and natural advancement while a managed seek settles.
                // Additional seek events or another landing point are native input, not sync.
                let advance = if paused {
                    0.0
                } else {
                    expected.issued.elapsed().as_secs_f64().min(2.0)
                };
                self.seeks <= expected.count
                    && position >= expected.position - 0.2
                    && position <= expected.position + advance + 0.2
            });
            self.seeks = 0;
            if !managed {
                control.position = Some(position);
            }
        }
        control
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn ready() -> NativeInput {
        let mut input = NativeInput::default();
        input.loading();
        input.managed_pause(true);
        input.arm();
        input
    }

    #[test]
    fn native_pause_resume_and_small_seek_are_forwarded_once() {
        let mut input = ready();
        assert_eq!(
            input.observe(Some(2.0), false, false),
            NativeControl {
                paused: Some(false),
                position: None
            }
        );
        assert!(input.observe(Some(2.1), false, false).is_empty());
        assert_eq!(
            input.observe(Some(2.2), true, false),
            NativeControl {
                paused: Some(true),
                position: Some(2.2)
            }
        );
        input.seek_event();
        assert_eq!(input.observe(Some(2.25), true, false).position, Some(2.25));
        assert!(input.observe(Some(2.25), true, false).is_empty());
    }

    #[test]
    fn managed_pause_seek_and_frame_rounding_do_not_echo() {
        let mut input = ready();
        input.managed_pause(false);
        assert!(input.observe(Some(1.0), false, false).is_empty());
        input.managed_pause(true);
        input.managed_seek(8.0);
        input.seek_event();
        assert!(input.observe(Some(8.08), true, false).is_empty());
        assert!(input.observe(Some(8.08), true, false).is_empty());
    }

    #[test]
    fn seek_is_evaluated_only_after_it_settles_and_native_seek_during_sync_is_kept() {
        let mut input = ready();
        input.managed_seek(8.0);
        input.seek_event();
        assert!(input.observe(Some(1.0), true, true).is_empty());
        input.seek_event(); // A second seek from the player during the managed seek.
        assert_eq!(input.observe(Some(12.0), true, false).position, Some(12.0));
        input.managed_seek(8.0);
        input.seek_event();
        // Even a coalesced seek is native when it lands somewhere other than the sync target.
        assert_eq!(input.observe(Some(5.0), true, false).position, Some(5.0));
    }

    #[test]
    fn stale_no_op_seek_expectation_does_not_swallow_later_native_input() {
        let mut input = ready();
        input.managed_seek(10.0);
        input.managed_seek.as_mut().unwrap().issued -= Duration::from_secs(3);
        assert!(input.observe(Some(10.0), true, false).is_empty());
        input.seek_event();
        assert_eq!(input.observe(Some(10.1), true, false).position, Some(10.1));
    }

    #[test]
    fn loading_quality_switching_and_playing_managed_seeks_do_not_echo() {
        let mut input = ready();
        input.loading();
        input.seek_event();
        assert!(input.observe(Some(0.0), true, false).is_empty());
        input.managed_seek(10.0);
        input.managed_pause(false);
        input.arm();
        input.seek_event();
        input.managed_seek.as_mut().unwrap().issued -= Duration::from_secs(1);
        assert!(input.observe(Some(10.8), false, false).is_empty());
    }
}
