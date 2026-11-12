//! Shared short/long press handling for keyboard shortcuts and ESP32.
use std::{
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use tauri::{AppHandle, Manager};
const LONG_PRESS: Duration = Duration::from_millis(400);
const MAX_HOLD: Duration = Duration::from_secs(10);
pub type Validator = Arc<dyn Fn() -> bool + Send + Sync>;
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Keyboard(u32),
    Device {
        device: String,
        dn: String,
        hn: String,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Down,
    Up,
    Cancel,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Cycle,
    Create,
}
struct Active {
    source: Source,
    started: Instant,
    id: u64,
    long_fired: bool,
    valid: Option<Validator>,
}
#[derive(Default)]
struct Machine {
    current: Option<Active>,
    next_id: u64,
}
impl Machine {
    fn edge(
        &mut self,
        source: Source,
        edge: Edge,
        now: Instant,
        allowed: bool,
        valid: Option<Validator>,
    ) -> (Option<Action>, Option<u64>) {
        if edge == Edge::Cancel || !allowed {
            if self.current.as_ref().is_some_and(|a| a.source == source) {
                self.current = None;
            }
            return (None, None);
        }
        match edge {
            Edge::Down if self.current.is_none() => {
                self.next_id = self.next_id.wrapping_add(1);
                let id = self.next_id;
                self.current = Some(Active {
                    source,
                    started: now,
                    id,
                    long_fired: false,
                    valid,
                });
                (None, Some(id))
            }
            Edge::Up if self.current.as_ref().is_some_and(|a| a.source == source) => {
                let active = self.current.take().unwrap();
                (
                    if active.long_fired {
                        None
                    } else if now.saturating_duration_since(active.started) >= LONG_PRESS {
                        Some(Action::Create)
                    } else {
                        Some(Action::Cycle)
                    },
                    None,
                )
            }
            _ => (None, None),
        }
    }
    fn long(&mut self, id: u64, now: Instant, allowed: bool) -> Option<Action> {
        let active = self.current.as_mut()?;
        if active.id != id {
            return None;
        }
        if !allowed {
            self.current = None;
            return None;
        }
        if active.long_fired || now.saturating_duration_since(active.started) < LONG_PRESS {
            return None;
        }
        active.long_fired = true;
        Some(Action::Create)
    }
    fn cancel_device(&mut self) {
        if self
            .current
            .as_ref()
            .is_some_and(|a| matches!(a.source, Source::Device { .. }))
        {
            self.current = None;
        }
    }
}
#[derive(Default)]
pub struct State(Mutex<Machine>);
fn eligible(app: &AppHandle, source: &Source, valid: Option<&Validator>) -> bool {
    if app.state::<crate::PttState>().0.is_held() {
        return false;
    }
    match source {
        Source::Keyboard(_) => !crate::settings_has_focus(app),
        Source::Device { .. } => valid.is_some_and(|v| v()),
    }
}
fn perform(app: &AppHandle, action: Action) {
    let result = match action {
        Action::Cycle => crate::cycle_tab_internal(app),
        Action::Create => crate::create_tab_internal(app).map(|_| crate::refresh_tab_host(app)),
    };
    if let Err(error) = result {
        eprintln!("[gpt-wrapper] Switch Tabs {action:?} failed: {error}");
    }
}
fn timeout(app: AppHandle, id: u64) {
    let state = app.state::<State>();
    let snapshot = state.0.lock().ok().and_then(|s| {
        s.current
            .as_ref()
            .filter(|a| a.id == id)
            .map(|a| (a.source.clone(), a.valid.clone()))
    });
    let Some((source, validator)) = snapshot else {
        return;
    };
    let allowed = eligible(&app, &source, validator.as_ref());
    let action = state
        .0
        .lock()
        .ok()
        .and_then(|mut s| s.long(id, Instant::now(), allowed));
    if let Some(action) = action {
        perform(&app, action);
    }
}
fn schedule(app: &AppHandle, id: u64) {
    let target = app.clone();
    thread::spawn(move || {
        thread::sleep(LONG_PRESS);
        let on_main = target.clone();
        let _ = target.run_on_main_thread(move || timeout(on_main, id));
        thread::sleep(MAX_HOLD);
        // Missing Up cannot leave the shortcut permanently held.
        if let Some(state) = target.try_state::<State>() {
            if let Ok(mut m) = state.0.lock() {
                if m.current.as_ref().is_some_and(|a| a.id == id) {
                    m.current = None;
                }
            }
        }
    });
}
pub fn input(app: &AppHandle, source: Source, edge: Edge, valid: Option<Validator>) {
    let observed = Instant::now();
    let target = app.clone();
    if let Err(error) = app.run_on_main_thread(move || {
        let allowed = edge == Edge::Cancel || eligible(&target, &source, valid.as_ref());
        let state = target.state::<State>();
        let (action, id) = match state.0.lock() {
            Ok(mut m) => m.edge(source, edge, observed, allowed, valid),
            Err(_) => return,
        };
        if let Some(id) = id {
            schedule(&target, id);
        }
        if let Some(action) = action {
            perform(&target, action);
        }
    }) {
        eprintln!("[gpt-wrapper] Switch Tabs edge dispatch failed: {error}");
    }
}
pub fn cancel_all(app: &AppHandle) {
    if let Some(state) = app.try_state::<State>() {
        if let Ok(mut m) = state.0.lock() {
            m.current = None;
        }
    }
}
pub fn cancel_device(app: &AppHandle) {
    if let Some(state) = app.try_state::<State>() {
        if let Ok(mut m) = state.0.lock() {
            m.cancel_device();
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn k() -> Source {
        Source::Keyboard(17)
    }
    #[test]
    fn short_long_and_repeat_are_exclusive() {
        let start = Instant::now();
        let mut m = Machine::default();
        let (_, id) = m.edge(k(), Edge::Down, start, true, None);
        assert!(m
            .edge(
                k(),
                Edge::Down,
                start + Duration::from_millis(10),
                true,
                None
            )
            .1
            .is_none());
        assert_eq!(
            m.edge(
                k(),
                Edge::Up,
                start + Duration::from_millis(399),
                true,
                None
            )
            .0,
            Some(Action::Cycle)
        );
        assert!(m.long(id.unwrap(), start + LONG_PRESS, true).is_none());
        let (_, id) = m.edge(k(), Edge::Down, start + Duration::from_secs(1), true, None);
        assert_eq!(
            m.long(id.unwrap(), start + Duration::from_millis(1400), true),
            Some(Action::Create)
        );
        assert!(m
            .long(id.unwrap(), start + Duration::from_secs(3), true)
            .is_none());
        assert_eq!(
            m.edge(k(), Edge::Up, start + Duration::from_secs(3), true, None)
                .0,
            None
        );
    }
    #[test]
    fn at_boundary_release_creates_once() {
        let t = Instant::now();
        let mut m = Machine::default();
        let (_, id) = m.edge(k(), Edge::Down, t, true, None);
        assert_eq!(
            m.edge(k(), Edge::Up, t + LONG_PRESS, true, None).0,
            Some(Action::Create)
        );
        assert!(m.long(id.unwrap(), t + LONG_PRESS, true).is_none());
    }
    #[test]
    fn cancellation_and_source_mismatch_never_create() {
        let t = Instant::now();
        let mut m = Machine::default();
        let (_, id) = m.edge(k(), Edge::Down, t, true, None);
        let other = Source::Device {
            device: "dev".into(),
            dn: "dn".into(),
            hn: "hn".into(),
        };
        assert!(m
            .edge(
                other.clone(),
                Edge::Up,
                t + Duration::from_millis(5),
                true,
                None
            )
            .0
            .is_none());
        assert!(m
            .edge(other, Edge::Down, t + Duration::from_millis(8), true, None)
            .1
            .is_none());
        m.edge(k(), Edge::Cancel, t + Duration::from_millis(9), false, None);
        assert!(m.long(id.unwrap(), t + LONG_PRESS, true).is_none());
        assert!(m
            .edge(k(), Edge::Up, t + LONG_PRESS, true, None)
            .0
            .is_none());
    }
    #[test]
    fn invalidated_timer_cannot_act_on_new_press() {
        let t = Instant::now();
        let mut m = Machine::default();
        let (_, first) = m.edge(k(), Edge::Down, t, true, None);
        m.edge(k(), Edge::Cancel, t, true, None);
        let (_, second) = m.edge(k(), Edge::Down, t, true, None);
        assert_ne!(first, second);
        assert!(m.long(first.unwrap(), t + LONG_PRESS, true).is_none());
        assert_eq!(m.long(second.unwrap(), t + LONG_PRESS, false), None);
        assert!(m.current.is_none());
    }
}
