//! Notification timing is independent of (and never delays) the safety gate.
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct Notices {
    selected: Option<String>,
    offline_at: Option<Instant>,
    notified: bool,
    audio_failure: bool,
}
impl Notices {
    pub fn update(
        &mut self,
        selected: Option<String>,
        online: bool,
        audio: bool,
        interrupted: bool,
        now: Instant,
    ) -> Option<bool> {
        if self.selected != selected {
            *self = Self {
                selected,
                ..Self::default()
            };
        }
        self.selected.as_ref()?;
        if online && (!self.audio_failure || audio) && !interrupted {
            self.offline_at = None;
            self.notified = false;
            self.audio_failure = false;
            return None;
        }
        if interrupted {
            self.audio_failure |= !audio;
        }
        if !online || interrupted {
            self.offline_at.get_or_insert(now);
        }
        if !self.notified
            && (interrupted
                || self
                    .offline_at
                    .is_some_and(|at| now.duration_since(at) > Duration::from_secs(10)))
        {
            self.notified = true;
            return Some(interrupted);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn idle_threshold_dedup_recovery_and_selection() {
        let mut n = Notices::default();
        let t = Instant::now();
        let id = Some("one".into());
        assert_eq!(n.update(id.clone(), false, false, false, t), None);
        assert_eq!(
            n.update(id.clone(), false, false, false, t + Duration::from_secs(10)),
            None
        );
        assert_eq!(
            n.update(id.clone(), false, false, false, t + Duration::from_secs(11)),
            Some(false)
        );
        assert_eq!(
            n.update(id.clone(), false, false, true, t + Duration::from_secs(12)),
            None
        );
        n.update(id.clone(), true, true, false, t + Duration::from_secs(13));
        assert_eq!(
            n.update(id.clone(), false, true, true, t + Duration::from_secs(14)),
            Some(true)
        );
        assert_eq!(
            n.update(
                Some("two".into()),
                false,
                true,
                false,
                t + Duration::from_secs(15)
            ),
            None
        );
        assert_eq!(
            n.update(None, false, true, false, t + Duration::from_secs(30)),
            None
        );
    }
    #[test]
    fn audio_interruption_stays_deduplicated_until_repaired() {
        let mut n = Notices::default();
        let t = Instant::now();
        let id = Some("one".into());
        assert_eq!(n.update(id.clone(), true, false, true, t), Some(true));
        assert_eq!(
            n.update(id.clone(), true, false, false, t + Duration::from_secs(20)),
            None
        );
        n.update(id.clone(), true, true, false, t + Duration::from_secs(21));
        assert_eq!(
            n.update(id, true, false, true, t + Duration::from_secs(22)),
            Some(true)
        );
    }
}
