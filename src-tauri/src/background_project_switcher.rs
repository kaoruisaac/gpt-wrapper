//! Tray-only Explicit Submit switcher; standby navigation is scheduled separately.
use std::{
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};
use tauri::{
    webview::{PageLoadEvent, WebviewWindowBuilder},
    AppHandle, Manager, PhysicalPosition, WebviewUrl,
};
use uuid::Uuid;
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongPtrW, IsWindow, IsWindowVisible, SetWindowLongPtrW, ShowWindow, GWL_EXSTYLE,
    SW_HIDE, SW_SHOWNOACTIVATE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};

use crate::{
    project_settings::{ProjectSettings, ProjectSettingsState},
    update_project_settings,
};

const TOAST_LABEL: &str = "default-project-toast";
const TOAST_DURATION: Duration = Duration::from_secs(3);
const WIDTH: f64 = 320.0;
const HEIGHT: f64 = 88.0;
const MARGIN: f64 = 18.0;
const HIDE_RETRY_DELAY: Duration = Duration::from_millis(250);
const MAX_HIDE_RETRIES: usize = 6;

#[derive(Default)]
struct Session {
    deadline: Option<Instant>,
    generation: u64,
    label: Option<String>,
    hide_pending: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum TimerAction {
    Stale,
    Wait(Duration),
    Hide,
}

impl Session {
    fn active(&self, now: Instant) -> bool {
        !self.hide_pending && self.deadline.is_some_and(|deadline| now < deadline)
    }

    fn update(&mut self, now: Instant, label: String) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.deadline = Some(now + TOAST_DURATION);
        self.label = Some(label);
        self.hide_pending = false;
        self.generation
    }

    fn dismiss(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.deadline = None;
        self.label = None;
        self.hide_pending = true;
        self.generation
    }

    fn timer_action(&self, generation: u64, now: Instant) -> TimerAction {
        if generation != self.generation {
            return TimerAction::Stale;
        }
        if self.hide_pending {
            return TimerAction::Hide;
        }
        match self.deadline {
            Some(deadline) if now < deadline => TimerAction::Wait(deadline - now),
            Some(_) => TimerAction::Hide,
            None => TimerAction::Stale,
        }
    }

    fn hide_complete(&mut self, generation: u64) {
        if self.generation == generation {
            self.deadline = None;
            self.label = None;
            self.hide_pending = false;
        }
    }
}

// hide() is a visibility adapter: it must return Ok only after native visibility
// has been checked. State is cleared only when the adapter has confirmed success.
fn attempt_hide(
    session: &mut Session,
    generation: u64,
    now: Instant,
    hide: impl FnOnce() -> Result<(), String>,
) -> Result<TimerAction, String> {
    let action = session.timer_action(generation, now);
    if action == TimerAction::Hide {
        hide()?;
        session.hide_complete(generation);
    }
    Ok(action)
}

pub struct BackgroundProjectSwitcherState {
    session: Mutex<Session>,
    press_gate: Mutex<()>,
}

impl BackgroundProjectSwitcherState {
    pub fn new() -> Self {
        Self {
            session: Mutex::new(Session::default()),
            press_gate: Mutex::new(()),
        }
    }
}

fn resolved_id(settings: &ProjectSettings) -> Option<Uuid> {
    settings
        .default_project_id
        .filter(|id| settings.projects.iter().any(|project| project.id == *id))
}

fn next_id(settings: &ProjectSettings) -> Option<Uuid> {
    let current = resolved_id(settings);
    match current {
        None => settings.projects.first().map(|project| project.id),
        Some(id) => settings
            .projects
            .iter()
            .position(|project| project.id == id)
            .and_then(|index| settings.projects.get(index + 1))
            .map(|project| project.id),
    }
}

fn label_for(settings: &ProjectSettings) -> String {
    resolved_id(settings)
        .and_then(|id| settings.projects.iter().find(|p| p.id == id))
        .map(|p| p.name.clone())
        .unwrap_or_else(|| "ChatGPT.com".to_string())
}

fn update_script(label: &str) -> String {
    // JSON string encoding ensures a bookmark name is never inserted as HTML/JS source.
    let encoded = serde_json::to_string(label).expect("serializable project name");
    format!("window.__GPT_PROJECT_TOAST__?.update({encoded});")
}

fn place_toast(window: &tauri::WebviewWindow) -> Result<(), String> {
    let monitor = window
        .app_handle()
        .primary_monitor()
        .map_err(|error| error.to_string())?
        .ok_or("primary monitor unavailable")?;
    let area = monitor.work_area();
    let scale = monitor.scale_factor();
    let x = area.position.x + area.size.width as i32 - ((WIDTH + MARGIN) * scale).round() as i32;
    let y = area.position.y + area.size.height as i32 - ((HEIGHT + MARGIN) * scale).round() as i32;
    window
        .set_position(PhysicalPosition::new(
            x.max(area.position.x),
            y.max(area.position.y),
        ))
        .map_err(|error| error.to_string())
}

// All HWND visibility operations run on the Tauri UI thread. Checking the
// actual HWND matters: the toast is shown via Win32, not Window::show(), so
// Tao/Tauri's cached VISIBLE flag can disagree with the native window.
fn native_visible(window: &tauri::WebviewWindow) -> Result<bool, String> {
    let hwnd = window.hwnd().map_err(|error| error.to_string())?;
    Ok(unsafe { IsWindow(Some(hwnd)).as_bool() && IsWindowVisible(hwnd).as_bool() })
}

fn hide_native_confirmed(window: &tauri::WebviewWindow) -> Result<(), String> {
    let hwnd = window.hwnd().map_err(|error| error.to_string())?;
    let before = native_visible(window)?;
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
    if native_visible(window)? {
        // An additional Tauri hide is a fallback, not our visibility proof.
        if let Err(error) = window.hide() {
            eprintln!("[gpt-wrapper] toast Tauri hide fallback failed: {error}");
        }
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
    let after = native_visible(window)?;
    #[cfg(debug_assertions)]
    eprintln!("[gpt-wrapper] toast native hide: visible_before={before} visible_after={after}");
    if after {
        Err("toast HWND remains visible after native hide".to_string())
    } else {
        Ok(())
    }
}

fn show_without_activation(window: &tauri::WebviewWindow) -> Result<(), String> {
    let hwnd = window.hwnd().map_err(|error| error.to_string())?;
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(
            hwnd,
            GWL_EXSTYLE,
            style | (WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0) as isize,
        );
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
    let visible = native_visible(window)?;
    #[cfg(debug_assertions)]
    eprintln!("[gpt-wrapper] toast native show: visible_after={visible}");
    if !visible {
        Err("toast HWND is not visible after native show".to_string())
    } else {
        Ok(())
    }
}

fn show_toast(app: &AppHandle, label: &str, generation: u64) -> Result<(), String> {
    if app.get_webview_window(TOAST_LABEL).is_none() {
        WebviewWindowBuilder::new(
            app,
            TOAST_LABEL,
            WebviewUrl::App("settings/project-toast.html".into()),
        )
        .title("Default Project")
        .inner_size(WIDTH, HEIGHT)
        .decorations(false)
        .resizable(false)
        .shadow(false)
        .focused(false)
        .visible(false)
        .skip_taskbar(true)
        .always_on_top(true)
        .on_page_load(|window, payload| {
            if payload.event() != PageLoadEvent::Finished {
                return;
            }
            let app = window.app_handle();
            let state = app.state::<BackgroundProjectSwitcherState>();
            let Ok(mut session) = state.session.lock() else {
                eprintln!("[gpt-wrapper] toast page-load: session lock unavailable");
                return;
            };
            if session.active(Instant::now()) {
                // Keep the lock through native show so a late PageLoad cannot
                // resurrect a toast after timeout or tray restoration.
                if let Some(label) = &session.label {
                    if let Err(error) = window
                        .eval(update_script(label))
                        .map_err(|error| error.to_string())
                        .and_then(|_| place_toast(&window))
                        .and_then(|_| show_without_activation(&window))
                    {
                        eprintln!("[gpt-wrapper] toast page-load show failed: {error}");
                    }
                }
            } else {
                if !session.hide_pending {
                    session.dismiss();
                }
                let generation = session.generation;
                #[cfg(debug_assertions)]
                eprintln!(
                    "[gpt-wrapper] toast page-load: no active session, generation={generation}"
                );
                let result = attempt_hide(&mut session, generation, Instant::now(), || {
                    hide_native_confirmed(&window)
                });
                if let Err(error) = result {
                    eprintln!("[gpt-wrapper] toast page-load hide failed: {error}");
                    schedule_timer(app.clone(), generation, HIDE_RETRY_DELAY, 1);
                }
            }
        })
        .build()
        .map_err(|error| error.to_string())?;
    }

    let window = app
        .get_webview_window(TOAST_LABEL)
        .ok_or("toast window unavailable")?;
    // A tray Open/PTT or newer keypress may have invalidated this press while
    // the popup WebView was being created; never display an obsolete label.
    let state = app.state::<BackgroundProjectSwitcherState>();
    let session = state
        .session
        .lock()
        .map_err(|_| "toast session unavailable")?;
    if session.generation != generation || !session.active(Instant::now()) {
        return Ok(());
    }
    window
        .set_ignore_cursor_events(true)
        .map_err(|error| error.to_string())?;
    window
        .eval(update_script(label))
        .map_err(|error| error.to_string())?;
    place_toast(&window)?;
    show_without_activation(&window)
}

// Each timer only handles its own generation. A retry is scheduled by a
// sleeping worker, but native HWND access always remains on Tauri's UI thread.
fn schedule_timer(app: AppHandle, generation: u64, delay: Duration, attempt: usize) {
    thread::spawn(move || {
        thread::sleep(delay);
        #[cfg(debug_assertions)]
        eprintln!("[gpt-wrapper] toast timer woke: generation={generation}, retry={attempt}");
        let callback_app = app.clone();
        let enqueue_result = app.run_on_main_thread(move || {
            #[cfg(debug_assertions)]
            eprintln!("[gpt-wrapper] toast timer UI callback: generation={generation}, retry={attempt}");
            let state = callback_app.state::<BackgroundProjectSwitcherState>();
            let Ok(mut session) = state.session.lock() else {
                eprintln!("[gpt-wrapper] toast timer: session lock unavailable");
                return;
            };
            let result = attempt_hide(&mut session, generation, Instant::now(), || {
                match callback_app.get_webview_window(TOAST_LABEL) {
                    Some(window) => hide_native_confirmed(&window),
                    None => Ok(()), // already destroyed; confirmed not visible
                }
            });
            drop(session);
            match result {
                Ok(TimerAction::Stale) => {
                    #[cfg(debug_assertions)]
                    eprintln!("[gpt-wrapper] toast timer stale: generation={generation}");
                }
                Ok(TimerAction::Wait(remaining)) => {
                    #[cfg(debug_assertions)]
                    eprintln!("[gpt-wrapper] toast timer early: generation={generation}, remaining_ms={}", remaining.as_millis());
                    schedule_timer(callback_app, generation, remaining, 0);
                }
                Ok(TimerAction::Hide) => {
                    #[cfg(debug_assertions)]
                    eprintln!("[gpt-wrapper] toast hidden and verified: generation={generation}");
                }
                Err(error) => {
                    eprintln!("[gpt-wrapper] toast hide failed: generation={generation}, retry={attempt}: {error}");
                    if attempt < MAX_HIDE_RETRIES {
                        schedule_timer(callback_app, generation, HIDE_RETRY_DELAY, attempt + 1);
                    } else {
                        eprintln!("[gpt-wrapper] toast hide retries exhausted; retained cleanup state");
                    }
                }
            }
        });
        if let Err(error) = enqueue_result {
            eprintln!("[gpt-wrapper] toast timer UI dispatch failed: generation={generation}, retry={attempt}: {error}");
            if attempt < MAX_HIDE_RETRIES {
                schedule_timer(app, generation, HIDE_RETRY_DELAY, attempt + 1);
            }
        } else {
            #[cfg(debug_assertions)]
            eprintln!("[gpt-wrapper] toast timer UI dispatch queued: generation={generation}");
        }
    });
}

pub fn dismiss(app: &AppHandle) {
    let state = app.state::<BackgroundProjectSwitcherState>();
    let Ok(mut session) = state.session.lock() else {
        eprintln!("[gpt-wrapper] toast dismiss: session lock unavailable");
        return;
    };
    let generation = session.dismiss();
    #[cfg(debug_assertions)]
    eprintln!("[gpt-wrapper] toast dismiss: generation={generation}");
    let result = attempt_hide(&mut session, generation, Instant::now(), || {
        match app.get_webview_window(TOAST_LABEL) {
            Some(window) => hide_native_confirmed(&window),
            None => Ok(()),
        }
    });
    drop(session);
    if let Err(error) = result {
        eprintln!("[gpt-wrapper] toast dismiss hide failed: {error}");
        schedule_timer(app.clone(), generation, HIDE_RETRY_DELAY, 1);
    }
}

pub fn press(app: &AppHandle) {
    let state = app.state::<BackgroundProjectSwitcherState>();
    let (generation, label) = {
        // Serialize input selection transactions without holding the Toast
        // session lock during disk I/O. This gate is released before any UI work.
        let Ok(_press) = state.press_gate.lock() else {
            return;
        };
        let now = Instant::now();
        let (continuing_session, previous_generation) = {
            let Ok(session) = state.session.lock() else {
                return;
            };
            (session.active(now), session.generation)
        };
        let result = if continuing_session {
            update_project_settings(app, |settings| {
                // Compute the next selection inside the settings transaction,
                // so concurrent Settings edits cannot supply a stale next ID.
                let next = next_id(settings);
                crate::project_settings::set_default(settings, next)
            })
        } else {
            app.state::<ProjectSettingsState>().snapshot()
        };
        let label = match result {
            Ok(settings) => label_for(&settings),
            Err(error) => {
                eprintln!("[gpt-wrapper] failed to save Default Project: {error}");
                // Keep the selection in its last persisted state; a subsequent press may retry.
                "Unable to switch project".to_string()
            }
        };
        let Ok(mut session) = state.session.lock() else {
            return;
        };
        // Tray Open/PTT may have dismissed the session during persistence.
        if session.generation != previous_generation {
            return;
        }
        let generation = session.update(now, label.clone());
        #[cfg(debug_assertions)]
        eprintln!(
            "[gpt-wrapper] toast press: generation={generation}, continuing_session={continuing_session}, deadline_reset_ms=3000"
        );
        (generation, label)
    };

    if let Err(error) = show_toast(app, &label, generation) {
        eprintln!("[gpt-wrapper] failed to show Project toast: {error}");
    }
    // The countdown is anchored to the last press, not to WebView creation
    // or page-load completion.
    let remaining = state
        .session
        .lock()
        .ok()
        .filter(|session| session.generation == generation)
        .and_then(|session| session.deadline)
        .map(|deadline| deadline.saturating_duration_since(Instant::now()))
        .unwrap_or(Duration::ZERO);
    schedule_timer(app.clone(), generation, remaining, 0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_settings::ProjectBookmark;

    fn sample() -> ProjectSettings {
        ProjectSettings {
            default_project_id: None,
            projects: vec![
                ProjectBookmark {
                    id: Uuid::new_v4(),
                    name: "Same".into(),
                    url: "https://chatgpt.com".into(),
                    message_prefix: "a".into(),
                },
                ProjectBookmark {
                    id: Uuid::new_v4(),
                    name: "Same".into(),
                    url: "https://chatgpt.com".into(),
                    message_prefix: "b".into(),
                },
            ],
        }
    }

    #[test]
    fn cycle_uses_ids_and_current_order_including_builtin() {
        let mut s = sample();
        assert_eq!(next_id(&s), Some(s.projects[0].id));
        s.default_project_id = next_id(&s);
        assert_eq!(next_id(&s), Some(s.projects[1].id));
        s.default_project_id = next_id(&s);
        assert_eq!(next_id(&s), None);
        s.projects.swap(0, 1);
        assert_eq!(next_id(&s), Some(s.projects[1].id));
        s.projects.clear();
        assert_eq!(next_id(&s), None);
        assert_eq!(label_for(&s), "ChatGPT.com");
    }

    #[test]
    fn missing_default_falls_back_and_zero_projects_does_not_change() {
        let mut s = sample();
        s.default_project_id = Some(Uuid::new_v4());
        assert_eq!(next_id(&s), Some(s.projects[0].id));
        assert_eq!(label_for(&s), "ChatGPT.com");
        s.projects.clear();
        assert_eq!(next_id(&s), None);
    }

    #[test]
    fn session_resets_deadline_and_ignores_stale_timer() {
        let mut session = Session::default();
        let start = Instant::now();
        assert!(!session.active(start));
        let first = session.update(start, "ChatGPT.com".into());
        assert_eq!(
            session.timer_action(first, start + Duration::from_millis(2900)),
            TimerAction::Wait(Duration::from_millis(100))
        );
        let second = session.update(start + Duration::from_millis(2900), "Next".into());
        assert_eq!(
            session.timer_action(first, start + TOAST_DURATION),
            TimerAction::Stale
        );
        assert!(session.active(start + Duration::from_millis(3100)));
        assert_eq!(
            session.timer_action(second, start + Duration::from_millis(5900)),
            TimerAction::Hide
        );
        assert_eq!(session.label.as_deref(), Some("Next")); // not cleared until HWND hide
        let mut visible = true;
        assert_eq!(
            attempt_hide(
                &mut session,
                second,
                start + Duration::from_millis(5900),
                || {
                    visible = false;
                    Ok(())
                }
            )
            .unwrap(),
            TimerAction::Hide
        );
        assert!(!visible);
        assert!(!session.active(start + Duration::from_millis(5900)));
        assert_eq!(session.label, None);
    }

    #[test]
    fn failed_hide_retains_cleanup_state_and_can_retry() {
        let mut session = Session::default();
        let start = Instant::now();
        let generation = session.update(start, "A".into());
        let mut visible = true;
        assert!(
            attempt_hide(&mut session, generation, start + TOAST_DURATION, || {
                Err("simulated HWND still visible".into())
            })
            .is_err()
        );
        assert_eq!(
            session.timer_action(generation, start + TOAST_DURATION),
            TimerAction::Hide
        );
        assert!(visible);
        assert_eq!(session.label.as_deref(), Some("A"));
        assert_eq!(
            attempt_hide(&mut session, generation, start + TOAST_DURATION, || {
                visible = false;
                Ok(())
            })
            .unwrap(),
            TimerAction::Hide
        );
        assert!(!visible);
        assert!(session.deadline.is_none());
    }

    #[test]
    fn session_dismiss_invalidates_all_pending_timers() {
        let mut session = Session::default();
        let now = Instant::now();
        let old = session.update(now, "A".into());
        let dismiss = session.dismiss();
        assert_eq!(
            session.timer_action(old, now + TOAST_DURATION),
            TimerAction::Stale
        );
        assert_eq!(session.timer_action(dismiss, now), TimerAction::Hide);
        assert!(attempt_hide(&mut session, dismiss, now, || Ok(())).is_ok());
        let new = session.update(now + Duration::from_secs(4), "B".into());
        assert_ne!(old, new);
        assert_eq!(
            session.timer_action(dismiss, now + Duration::from_secs(4)),
            TimerAction::Stale
        );
    }

    #[test]
    fn late_page_load_cannot_reveal_expired_or_dismissed_session() {
        let mut session = Session::default();
        let start = Instant::now();
        let generation = session.update(start, "A".into());
        assert!(!session.active(start + TOAST_DURATION));
        assert_eq!(
            session.timer_action(generation, start + TOAST_DURATION),
            TimerAction::Hide
        );
        let _ = session.dismiss();
        assert!(!session.active(start + Duration::from_secs(1)));
    }

    #[test]
    fn a_stale_timer_cannot_hide_a_new_toast_window() {
        let mut session = Session::default();
        let now = Instant::now();
        let old = session.update(now, "One".into());
        let new = session.update(now + Duration::from_secs(1), "Two".into());
        let mut hide_called = false;
        assert_eq!(
            attempt_hide(&mut session, old, now + TOAST_DURATION, || {
                hide_called = true;
                Ok(())
            })
            .unwrap(),
            TimerAction::Stale
        );
        assert!(!hide_called);
        assert_eq!(
            session.timer_action(new, now + TOAST_DURATION),
            TimerAction::Wait(Duration::from_secs(1))
        );
    }

    #[test]
    fn labels_are_encoded_as_javascript_string_literals() {
        let s = update_script("</script>\";window.alert(1);//");
        assert!(s.contains(r#"\u003c"#) || s.contains(r#"</script>"#));
        assert!(s.contains(r#"\";window.alert"#));
    }
}
