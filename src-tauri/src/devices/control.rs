//! Authenticated X1 snapshots. Transport authenticates before calling this module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub seq: u32,
    pub action: String,
    pub ptt: bool,
    pub submit: bool,
    pub mode: String,
    pub press: u32,
    pub segment: u32,
    pub combo: u32,
    pub suppress: bool,
}
pub fn number(s: &str) -> Option<u32> {
    let n = s.parse::<u32>().ok()?;
    (n < 0xffff_fff0 && n.to_string() == s).then_some(n)
}
fn bit(s: &str) -> Option<bool> {
    match s {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    }
}
impl Frame {
    pub fn parse(f: &[&str]) -> Option<Self> {
        if f.len() != 14 || f[0] != "X1" || f[1] != "E" {
            return None;
        }
        let s = Self {
            seq: number(f[5])?,
            action: f[6].into(),
            ptt: bit(f[7])?,
            submit: bit(f[8])?,
            mode: f[9].into(),
            press: number(f[10])?,
            segment: number(f[11])?,
            combo: number(f[12])?,
            suppress: bit(f[13])?,
        };
        if s.seq == 0 || s.press > s.segment || s.segment > s.seq || s.combo > s.seq {
            return None;
        }
        let state = match s.mode.as_str() {
            "IDLE" => !s.ptt,
            "TALK" => s.ptt && !s.submit && s.press > 0 && s.segment > 0,
            "COMBO" => s.ptt && s.submit && s.combo > 0 && s.suppress,
            "FINISH_NO_SEND" => !s.ptt && s.suppress && s.combo > 0,
            "RESETTING" => s.suppress,
            _ => false,
        };
        if !state || (s.combo > 0 && !s.suppress) || (s.suppress && s.combo == 0) {
            return None;
        }
        let action = match s.action.as_str() {
            "DOWN" => {
                s.mode == "TALK"
                    && s.press == s.seq
                    && s.segment == s.seq
                    && s.combo == 0
                    && !s.suppress
            }
            "UP" => !s.ptt && s.press > 0 && matches!(s.mode.as_str(), "IDLE" | "FINISH_NO_SEND"),
            "SUBMIT" => s.mode == "IDLE" && s.submit && !s.suppress,
            "COMBO_BEGIN" => s.mode == "COMBO" && s.combo == s.seq,
            "COMBO_RESUME" => s.mode == "TALK" && s.combo > 0 && s.segment == s.seq && s.suppress,
            "COMBO_END" => s.mode == "FINISH_NO_SEND",
            "RESET" => s.mode == "RESETTING",
            "STATE" => true,
            _ => false,
        };
        action.then_some(s)
    }
    pub fn idle_management(&self) -> bool {
        self.action == "STATE"
            && self.mode == "IDLE"
            && !self.ptt
            && !self.submit
            && self.press == 0
            && self.segment == 0
            && self.combo == 0
            && !self.suppress
    }
}
#[derive(Default)]
pub struct Replay {
    high: Option<u64>,
    bits: u128,
}
impl Replay {
    pub fn accept(&mut self, seq: u64) -> bool {
        let Some(high) = self.high else {
            self.high = Some(seq);
            self.bits = 1;
            return true;
        };
        if seq > high {
            let d = seq - high;
            self.bits = if d >= 128 { 1 } else { (self.bits << d) | 1 };
            self.high = Some(seq);
            return true;
        }
        let d = high - seq;
        if d >= 128 || self.bits & (1u128 << d) != 0 {
            return false;
        }
        self.bits |= 1u128 << d;
        true
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Down,
    Up,
    Submit,
    Pause,
    Resume,
    FinishNoSend,
    Cancel,
}
#[derive(Default)]
pub struct Controls {
    pub replay: Replay,
    pub latest: Option<Frame>,
    pub armed: bool,
    pub active: u32,
    pub barrier: u32,
    pub consumed_press: u32,
}
impl Controls {
    // Only a strictly newer snapshot can alter PTT. Submit has a separate replay
    // window, but cannot cross a later press/Combo/cancellation boundary.
    pub fn apply(&mut self, f: &Frame, dictation: bool, submit: bool) -> Vec<Kind> {
        if !self.replay.accept(f.seq as u64) {
            return vec![];
        }
        if f.action == "SUBMIT" {
            let allowed = submit
                && self.armed
                && self.active == 0
                && f.seq > self.barrier
                && self
                    .latest
                    .as_ref()
                    .is_none_or(|s| !s.ptt && !s.submit && s.press == f.press);
            self.barrier = self.barrier.max(f.seq);
            return if allowed { vec![Kind::Submit] } else { vec![] };
        }
        if self.latest.as_ref().is_some_and(|s| f.seq <= s.seq) {
            return vec![];
        }
        if self
            .latest
            .as_ref()
            .is_some_and(|s| s.press == f.press && s.suppress && !f.suppress)
        {
            return vec![];
        }
        let mut out = vec![];
        let previous = self.latest.replace(f.clone());
        if f.suppress {
            self.barrier = self.barrier.max(f.combo);
        }
        if !f.ptt && !f.submit {
            self.armed = true;
        }
        if self.active != 0 && f.press != self.active {
            out.push(Kind::Cancel);
            self.active = 0;
        }
        if f.mode == "RESETTING" {
            self.barrier = f.seq;
            self.armed = false;
            if self.active != 0 {
                out.push(Kind::Cancel);
                self.active = 0;
            }
        } else if !f.ptt {
            self.consumed_press = self.consumed_press.max(f.press);
            if self.active != 0 {
                out.push(if f.suppress {
                    Kind::FinishNoSend
                } else {
                    Kind::Up
                });
                self.active = 0;
            }
        } else if f.mode == "COMBO" {
            if self.active != 0 {
                out.push(Kind::Pause);
            }
        } else if f.mode == "TALK" && f.combo > 0 {
            if self.active == f.press
                && self.active != 0
                && previous
                    .as_ref()
                    .is_some_and(|p| p.segment != f.segment || p.mode == "COMBO")
            {
                out.push(Kind::Resume);
            }
        } else if f.mode == "TALK" && f.press > self.consumed_press {
            self.consumed_press = f.press;
            if self.armed && dictation && f.seq > self.barrier {
                self.active = f.press;
                self.armed = false;
                out.push(Kind::Down);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn frame(
        seq: u32,
        action: &str,
        p: bool,
        s: bool,
        mode: &str,
        press: u32,
        segment: u32,
        combo: u32,
        suppress: bool,
    ) -> Frame {
        Frame {
            seq,
            action: action.into(),
            ptt: p,
            submit: s,
            mode: mode.into(),
            press,
            segment,
            combo,
            suppress,
        }
    }
    #[test]
    fn replay_window_never_reopens_evicted_sequences() {
        let mut r = Replay::default();
        assert!(r.accept(500));
        assert!(r.accept(400));
        assert!(!r.accept(400));
        assert!(!r.accept(372));
        assert!(r.accept(1000));
        assert!(!r.accept(500));
    }
    #[test]
    fn combo_repairs_pause_without_restarting_or_allowing_send() {
        let mut c = Controls::default();
        c.apply(
            &frame(1, "STATE", false, false, "IDLE", 0, 0, 0, false),
            true,
            true,
        );
        assert_eq!(
            c.apply(
                &frame(2, "DOWN", true, false, "TALK", 2, 2, 0, false),
                true,
                true
            ),
            vec![Kind::Down]
        );
        assert_eq!(
            c.apply(
                &frame(5, "STATE", true, false, "TALK", 2, 4, 3, true),
                true,
                true
            ),
            vec![Kind::Resume]
        );
        assert!(c
            .apply(
                &frame(3, "COMBO_BEGIN", true, true, "COMBO", 2, 2, 3, true),
                true,
                true
            )
            .is_empty());
        assert_eq!(
            c.apply(
                &frame(6, "STATE", false, false, "FINISH_NO_SEND", 2, 4, 3, true),
                true,
                true
            ),
            vec![Kind::FinishNoSend]
        );
        assert!(c
            .apply(
                &frame(7, "STATE", true, false, "TALK", 2, 4, 3, true),
                true,
                true
            )
            .is_empty());
    }
    #[test]
    fn unavailable_press_cannot_start_when_audio_recovers_and_submit_is_independent() {
        let mut c = Controls::default();
        c.apply(
            &frame(1, "STATE", false, false, "IDLE", 0, 0, 0, false),
            false,
            true,
        );
        assert_eq!(
            c.apply(
                &frame(2, "SUBMIT", false, true, "IDLE", 0, 0, 0, false),
                false,
                true
            ),
            vec![Kind::Submit]
        );
        assert!(c
            .apply(
                &frame(3, "DOWN", true, false, "TALK", 3, 3, 0, false),
                false,
                true
            )
            .is_empty());
        assert!(c
            .apply(
                &frame(4, "STATE", true, false, "TALK", 3, 3, 0, false),
                true,
                true
            )
            .is_empty());
    }
    #[test]
    fn reconnect_held_and_orphan_resume_never_start() {
        let mut c = Controls::default();
        assert!(c
            .apply(
                &frame(2, "DOWN", true, false, "TALK", 2, 2, 0, false),
                true,
                true
            )
            .is_empty());
        c.apply(
            &frame(3, "STATE", false, false, "IDLE", 2, 2, 0, false),
            true,
            true,
        );
        assert!(c
            .apply(
                &frame(6, "COMBO_RESUME", true, false, "TALK", 4, 6, 5, true),
                true,
                true
            )
            .is_empty());
    }
    #[test]
    fn terminal_combo_allows_only_a_new_independent_submit_and_never_late_down() {
        let mut c = Controls::default();
        c.apply(
            &frame(1, "STATE", false, false, "IDLE", 0, 0, 0, false),
            true,
            true,
        );
        c.apply(
            &frame(2, "DOWN", true, false, "TALK", 2, 2, 0, false),
            true,
            true,
        );
        c.apply(
            &frame(3, "COMBO_BEGIN", true, true, "COMBO", 2, 2, 3, true),
            true,
            true,
        );
        assert_eq!(
            c.apply(
                &frame(
                    4,
                    "COMBO_END",
                    false,
                    false,
                    "FINISH_NO_SEND",
                    2,
                    2,
                    3,
                    true
                ),
                true,
                true
            ),
            vec![Kind::FinishNoSend]
        );
        assert_eq!(
            c.apply(
                &frame(5, "SUBMIT", false, true, "IDLE", 2, 2, 0, false),
                false,
                true
            ),
            vec![Kind::Submit]
        );
        assert!(c
            .apply(
                &frame(5, "SUBMIT", false, true, "IDLE", 2, 2, 0, false),
                false,
                true
            )
            .is_empty());
        assert!(c
            .apply(
                &frame(6, "STATE", true, false, "TALK", 2, 2, 0, false),
                true,
                true
            )
            .is_empty());
    }
    #[test]
    fn parser_rejects_noncanonical_and_illegal_combo_fields() {
        let f = [
            "X1",
            "E",
            "AABBCCDDEEFF",
            "dn",
            "hn",
            "2",
            "DOWN",
            "1",
            "0",
            "TALK",
            "2",
            "2",
            "0",
            "0",
        ];
        assert!(Frame::parse(&f).is_some());
        let mut bad = f;
        bad[5] = "02";
        assert!(Frame::parse(&bad).is_none());
        let mut bad = f;
        bad[12] = "3";
        assert!(Frame::parse(&bad).is_none());
        let mut bad = f;
        bad[13] = "1";
        assert!(Frame::parse(&bad).is_none());
        let mut bad = f;
        bad[8] = "1";
        assert!(Frame::parse(&bad).is_none());
    }
}
