//! Tray-only Explicit Submit switcher. No ChatGPT tab or PTT target is touched here.
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
    GetWindowLongPtrW, SetWindowLongPtrW, ShowWindow, GWL_EXSTYLE, SW_SHOWNOACTIVATE,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
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

#[derive(Default)]
struct Session {
    deadline: Option<Instant>,
    generation: u64,
    label: Option<String>,
}

impl Session {
    fn active(&self, now: Instant) -> bool {
        self.deadline.is_some_and(|deadline| now < deadline)
    }

    fn update(&mut self, now: Instant, label: String) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.deadline = Some(now + TOAST_DURATION);
        self.label = Some(label);
        self.generation
    }

    fn end(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.deadline = None;
        self.label = None;
    }

    fn expire(&mut self, generation: u64, now: Instant) -> bool {
        if self.generation == generation && !self.active(now) {
            self.end();
            return true;
        }
        false
    }
}

pub struct BackgroundProjectSwitcherState {
    session: Mutex<Session>,
}

impl BackgroundProjectSwitcherState {
    pub fn new() -> Self {
        Self {
            session: Mutex::new(Session::default()),
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
    Ok(())
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
            let label = app
                .state::<BackgroundProjectSwitcherState>()
                .session
                .lock()
                .ok()
                .and_then(|session| {
                    session
                        .active(Instant::now())
                        .then(|| session.label.clone())
                        .flatten()
                });
            if let Some(label) = label {
                let _ = window.eval(update_script(&label));
                if let Err(error) =
                    place_toast(&window).and_then(|_| show_without_activation(&window))
                {
                    eprintln!("[gpt-wrapper] toast display failed: {error}");
                }
            } else {
                let _ = window.hide();
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

pub fn dismiss(app: &AppHandle) {
    if let Ok(mut session) = app.state::<BackgroundProjectSwitcherState>().session.lock() {
        session.end();
    }
    if let Some(window) = app.get_webview_window(TOAST_LABEL) {
        if let Err(error) = window.hide() {
            eprintln!("[gpt-wrapper] failed to hide Project toast: {error}");
        }
    }
}

pub fn press(app: &AppHandle) {
    let state = app.state::<BackgroundProjectSwitcherState>();
    let (generation, label) = {
        let Ok(mut session) = state.session.lock() else {
            eprintln!("[gpt-wrapper] Project switch session unavailable");
            return;
        };
        let settings = match app.state::<ProjectSettingsState>().snapshot() {
            Ok(settings) => settings,
            Err(error) => {
                eprintln!("[gpt-wrapper] failed to read Project settings: {error}");
                return;
            }
        };
        let now = Instant::now();
        let result = if session.active(now) {
            let next = next_id(&settings);
            if next != settings.default_project_id {
                update_project_settings(app, |settings| {
                    crate::project_settings::set_default(settings, next)
                })
            } else {
                Ok(settings)
            }
        } else {
            Ok(settings)
        };
        let label = match result {
            Ok(settings) => label_for(&settings),
            Err(error) => {
                eprintln!("[gpt-wrapper] failed to save Default Project: {error}");
                // Keep the selection in its last persisted state; a subsequent press may retry.
                "Unable to switch project".to_string()
            }
        };
        (session.update(Instant::now(), label.clone()), label)
    };

    if let Err(error) = show_toast(app, &label, generation) {
        eprintln!("[gpt-wrapper] failed to show Project toast: {error}");
    }
    let app = app.clone();
    thread::spawn(move || {
        thread::sleep(TOAST_DURATION);
        let on_main = app.clone();
        let _ = app.run_on_main_thread(move || {
            let should_hide = on_main
                .state::<BackgroundProjectSwitcherState>()
                .session
                .lock()
                .map(|mut session| session.expire(generation, Instant::now()))
                .unwrap_or(false);
            if should_hide {
                if let Some(window) = on_main.get_webview_window(TOAST_LABEL) {
                    let _ = window.hide();
                }
            }
        });
    });
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
        assert!(session.active(start + Duration::from_millis(2900)));
        let second = session.update(start + Duration::from_millis(2900), "Next".into());
        assert!(!session.expire(first, start + TOAST_DURATION));
        assert!(session.active(start + Duration::from_millis(3100)));
        assert!(session.expire(second, start + Duration::from_millis(5900)));
        assert!(!session.active(start + Duration::from_millis(5900)));
        assert_eq!(session.label, None);
    }

    #[test]
    fn session_dismiss_invalidates_all_pending_timers() {
        let mut session = Session::default();
        let now = Instant::now();
        let old = session.update(now, "A".into());
        session.end();
        assert!(!session.expire(old, now + TOAST_DURATION));
        let new = session.update(now + Duration::from_secs(4), "B".into());
        assert_ne!(old, new);
    }

    #[test]
    fn labels_are_encoded_as_javascript_string_literals() {
        let s = update_script("</script>\";window.alert(1);//");
        assert!(s.contains(r#"\u003c"#) || s.contains(r#"</script>"#));
        assert!(s.contains(r#"\";window.alert"#));
    }
}
