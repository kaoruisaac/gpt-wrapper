//! Non-activating completion cards for ChatGPT tabs. Separate from the tray project switcher.
use std::{
    collections::HashMap,
    os::windows::ffi::OsStrExt,
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};
use tauri::{
    path::BaseDirectory,
    webview::{PageLoadEvent, WebviewWindowBuilder},
    AppHandle, LogicalSize, Manager, PhysicalPosition, WebviewUrl,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongPtrW, IsWindow, IsWindowVisible, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    GWL_EXSTYLE, GWL_STYLE, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER, SW_HIDE, SW_SHOWNOACTIVATE, WS_CAPTION, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_SYSMENU, WS_THICKFRAME,
};

use windows::{
    core::PCWSTR,
    Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_FILENAME, SND_NODEFAULT},
};

use crate::{general_settings::GeneralSettingsState, TabState, TabStatus, MAIN_WINDOW_LABEL};

pub const WINDOW_LABEL: &str = "tab-completion-notifications";
const WIDTH: f64 = 342.0;
const CARD_HEIGHT: f64 = 88.0;
const GAP: f64 = 10.0;
const MARGIN: f64 = 18.0;
const FADE_AFTER: Duration = Duration::from_secs(3);
const SETTLE_AFTER: Duration = Duration::from_millis(220);
const SOUND_COOLDOWN: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Transition {
    None,
    Cancel,
    Candidate,
}

fn transition(old: TabStatus, new: TabStatus, visible: bool) -> Transition {
    if new == TabStatus::Working {
        Transition::Cancel
    } else if visible && old == TabStatus::Working {
        Transition::Candidate
    } else {
        Transition::None
    }
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Card {
    device: bool,
    tab_id: usize,
    generation: u64,
    title: String,
    faded: bool,
    #[serde(skip)]
    shown_at: Option<Instant>,
}

#[derive(Default)]
struct Store {
    cards: Vec<Card>, // newest at the top
    pending: HashMap<usize, u64>,
    generation: u64,
    page_ready: bool,
    last_sound_at: Option<Instant>,
}

impl Store {
    // Called with the result of eval, placement, and native show.
    // Consume first-display eligibility even when muted or inside the cooldown.
    fn after_render(
        &mut self,
        now: Instant,
        succeeded: bool,
        allowed: bool,
        enabled: bool,
        play: impl FnOnce() -> Result<(), String>,
    ) -> Vec<(usize, u64)> {
        if !succeeded || !self.page_ready {
            return Vec::new();
        }
        let mut fresh = Vec::new();
        let mut new_completion = false;
        for card in &mut self.cards {
            if card.shown_at.is_none() {
                new_completion |= !card.device;
                card.shown_at = Some(now);
                fresh.push((card.tab_id, card.generation));
            }
        }
        if new_completion
            && allowed
            && enabled
            && self
                .last_sound_at
                .is_none_or(|last| now.duration_since(last) >= SOUND_COOLDOWN)
        {
            match play() {
                Ok(()) => self.last_sound_at = Some(now),
                Err(error) => eprintln!("[gpt-wrapper] completion sound failed: {error}"),
            }
        }
        fresh
    }

    fn next_generation(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    fn cancel_tab(&mut self, tab_id: usize) -> bool {
        let had_pending = self.pending.remove(&tab_id).is_some();
        let old_len = self.cards.len();
        self.cards.retain(|card| card.tab_id != tab_id);
        had_pending || old_len != self.cards.len()
    }

    fn clear(&mut self) -> bool {
        let changed = !self.pending.is_empty() || !self.cards.is_empty();
        self.pending.clear();
        self.cards.retain(|card| card.device);
        changed
    }

    fn prepare(&mut self, tab_id: usize) -> u64 {
        self.cancel_tab(tab_id);
        let generation = self.next_generation();
        self.pending.insert(tab_id, generation);
        generation
    }

    fn finish(&mut self, tab_id: usize, generation: u64, title: String, allowed: bool) -> bool {
        if self.pending.get(&tab_id) != Some(&generation) {
            return false;
        }
        self.pending.remove(&tab_id);
        if !allowed {
            return false;
        }
        self.cards.insert(
            0,
            Card {
                device: false,
                tab_id,
                generation,
                title,
                faded: false,
                shown_at: None,
            },
        );
        true
    }

    fn fade(&mut self, tab_id: usize, generation: u64, now: Instant) -> bool {
        let Some(card) = self.cards.iter_mut().find(|card| card.tab_id == tab_id) else {
            return false;
        };
        if card.generation != generation || card.faded {
            return false;
        }
        let Some(shown_at) = card.shown_at else {
            return false;
        };
        if now.duration_since(shown_at) < FADE_AFTER {
            return false;
        }
        card.faded = true;
        true
    }

    fn dismiss(&mut self, tab_id: usize, generation: u64) -> bool {
        let old_len = self.cards.len();
        self.cards
            .retain(|card| card.tab_id != tab_id || card.generation != generation);
        self.cards.len() != old_len
    }
}

pub struct NotificationState(Mutex<Store>);

impl NotificationState {
    pub fn new() -> Self {
        Self(Mutex::new(Store::default()))
    }
}

// Main window focus, NOT selected tab, Settings focus, or tray_hidden.
fn can_notify(app: &AppHandle) -> bool {
    app.get_window(MAIN_WINDOW_LABEL)
        .and_then(|main| main.is_focused().ok().map(|focused| !focused))
        .unwrap_or(false)
}

fn schedule_render(app: &AppHandle) {
    let handle = app.clone();
    if let Err(error) = app.run_on_main_thread(move || render_on_ui_thread(&handle)) {
        eprintln!("[gpt-wrapper] completion notification UI dispatch failed: {error}");
    }
}

pub fn on_status(
    app: &AppHandle,
    tab_id: usize,
    old: TabStatus,
    new: TabStatus,
    visible_title: Option<String>,
) {
    let action = transition(old, new, visible_title.is_some());
    let mut candidate = None;
    let mut needs_render = false;
    {
        let state = app.state::<NotificationState>();
        let Ok(mut store) = state.0.lock() else {
            return;
        };
        match action {
            Transition::None => {}
            Transition::Cancel => needs_render = store.cancel_tab(tab_id),
            Transition::Candidate => {
                // A foreground completion is discarded, never queued for later.
                if can_notify(app) {
                    let generation = store.prepare(tab_id);
                    candidate = Some((generation, visible_title.unwrap_or_default()));
                }
            }
        }
    }
    if needs_render {
        schedule_render(app);
    }
    if let Some((generation, title)) = candidate {
        let app = app.clone();
        thread::spawn(move || {
            thread::sleep(SETTLE_AFTER);
            let handle = app.clone();
            if let Err(error) = app.run_on_main_thread(move || {
                let still_finished = handle
                    .state::<TabState>()
                    .lock()
                    .ok()
                    .map(|tabs| {
                        tabs.tabs.iter().any(|tab| tab.id == tab_id)
                            && tabs
                                .contexts
                                .get(&tab_id)
                                .is_some_and(|context| context.status != TabStatus::Working)
                    })
                    .unwrap_or(false);
                let allowed = still_finished && can_notify(&handle);
                let state = handle.state::<NotificationState>();
                let changed = state
                    .0
                    .lock()
                    .map(|mut store| store.finish(tab_id, generation, title, allowed))
                    .unwrap_or(false);
                if changed {
                    schedule_render(&handle);
                }
            }) {
                eprintln!("[gpt-wrapper] completion candidate dispatch failed: {error}");
            }
        });
    }
}

// Used for reload/navigation and tab removal, not as a completion transition.
pub fn cancel_tab(app: &AppHandle, tab_id: usize) {
    let changed = app
        .state::<NotificationState>()
        .0
        .lock()
        .map(|mut store| store.cancel_tab(tab_id))
        .unwrap_or(false);
    if changed {
        schedule_render(app);
    }
}

pub fn clear_all(app: &AppHandle) {
    let changed = app
        .state::<NotificationState>()
        .0
        .lock()
        .map(|mut store| store.clear())
        .unwrap_or(false);
    if changed {
        schedule_render(app);
    }
}

// Slot zero is reserved: actual Tab IDs start at one. Device notices remain
// visible in the foreground, but share the existing non-activating card HWND.
pub fn device_notice(app: &AppHandle, interrupted: bool) {
    if let Ok(mut store) = app.state::<NotificationState>().0.lock() {
        let generation = store.prepare(0);
        let title = if interrupted {
            "ESP32 聽寫已中斷；未送出。請檢查 Devices，放開 PTT 後重試。"
        } else {
            "ESP32 已離線。請檢查電源與 Wi-Fi；恢復後會重新驗證。"
        };
        store.finish(0, generation, title.into(), true);
        if let Some(card) = store.cards.first_mut() {
            card.device = true;
        }
    }
    schedule_render(app);
}

pub fn dismiss_card(app: &AppHandle, tab_id: usize, generation: u64) {
    let changed = app
        .state::<NotificationState>()
        .0
        .lock()
        .map(|mut store| store.dismiss(tab_id, generation))
        .unwrap_or(false);
    if changed {
        schedule_render(app);
    }
}

fn update_script(cards: &[Card]) -> String {
    let encoded = serde_json::to_string(cards).expect("notification cards are serializable");
    format!("window.__GPT_TAB_COMPLETIONS__?.render({encoded});")
}

fn native_visible(window: &tauri::WebviewWindow) -> Result<bool, String> {
    let hwnd = window.hwnd().map_err(|error| error.to_string())?;
    Ok(unsafe { IsWindow(Some(hwnd)).as_bool() && IsWindowVisible(hwnd).as_bool() })
}

fn hide_native(window: &tauri::WebviewWindow) -> Result<(), String> {
    let hwnd = window.hwnd().map_err(|error| error.to_string())?;
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
    if native_visible(window)? {
        let _ = window.hide();
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
    if native_visible(window)? {
        Err("completion notification HWND remained visible after hide".into())
    } else {
        Ok(())
    }
}

// If an HWND does not hide on the first attempt, retry only while there are
// still no cards. A new completion can never be hidden by an old retry.
fn schedule_hide_retry(app: AppHandle, remaining: u8) {
    if remaining == 0 {
        return;
    }
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(250));
        let handle = app.clone();
        if let Err(error) = app.run_on_main_thread(move || {
            let state = handle.state::<NotificationState>();
            let Ok(store) = state.0.lock() else {
                return;
            };
            if !store.cards.is_empty() {
                return;
            }
            if let Some(window) = handle.get_webview_window(WINDOW_LABEL) {
                if let Err(error) = hide_native(&window) {
                    eprintln!("[gpt-wrapper] completion hide retry failed: {error}");
                    schedule_hide_retry(handle.clone(), remaining - 1);
                }
            }
        }) {
            eprintln!("[gpt-wrapper] completion hide retry dispatch failed: {error}");
        }
    });
}

fn show_without_activation(window: &tauri::WebviewWindow) -> Result<(), String> {
    let hwnd = window.hwnd().map_err(|error| error.to_string())?;
    unsafe {
        // Windows can restore a native caption when this hidden WebView window is
        // shown through ShowWindow, even though the builder has decorations(false).
        // Strip its non-client frame explicitly, leaving the HTML card unchanged.
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        let borderless = style
            & !(WS_CAPTION.0 | WS_THICKFRAME.0 | WS_SYSMENU.0
                | WS_MINIMIZEBOX.0 | WS_MAXIMIZEBOX.0) as isize;
        if borderless != style {
            SetWindowLongPtrW(hwnd, GWL_STYLE, borderless);
            SetWindowPos(
                hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            )
            .map_err(|error| format!("cannot remove completion notification frame: {error}"))?;
        }
        let exstyle = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(
            hwnd,
            GWL_EXSTYLE,
            exstyle | (WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0) as isize,
        );
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
    if !native_visible(window)? {
        return Err("completion notification HWND not visible after show".into());
    }
    Ok(())
}

fn place(window: &tauri::WebviewWindow, count: usize) -> Result<(), String> {
    let monitor = window
        .app_handle()
        .primary_monitor()
        .map_err(|error| error.to_string())?
        .ok_or("primary monitor unavailable")?;
    let area = monitor.work_area();
    let scale = monitor.scale_factor();
    let available_width = (area.size.width as f64 / scale - 2.0 * MARGIN).max(80.0);
    let available_height = (area.size.height as f64 / scale - 2.0 * MARGIN).max(50.0);
    let width = WIDTH.min(available_width);
    let full_height = count as f64 * CARD_HEIGHT + count.saturating_sub(1) as f64 * GAP;
    let height = full_height.min(available_height);
    window
        .set_size(LogicalSize::new(width, height))
        .map_err(|error| error.to_string())?;
    let x = area.position.x + area.size.width as i32 - ((width + MARGIN) * scale).round() as i32;
    let y = area.position.y + (MARGIN * scale).round() as i32;
    window
        .set_position(PhysicalPosition::new(x.max(area.position.x), y))
        .map_err(|error| error.to_string())
}

fn create_window(app: &AppHandle) -> Result<(), String> {
    WebviewWindowBuilder::new(
        app,
        WINDOW_LABEL,
        WebviewUrl::App("settings/tab-completion-notifications.html".into()),
    )
    .title("Tab completion notifications")
    .inner_size(WIDTH, CARD_HEIGHT)
    .decorations(false)
    .resizable(false)
    .shadow(false)
    .focused(false)
    .visible(false)
    .skip_taskbar(true)
    .always_on_top(true)
    .transparent(true)
    .on_page_load(|window, payload| {
        if payload.event() != PageLoadEvent::Finished {
            return;
        }
        if let Ok(mut store) = window.app_handle().state::<NotificationState>().0.lock() {
            store.page_ready = true;
        }
        schedule_render(window.app_handle());
    })
    .build()
    .map_err(|error| error.to_string())?;
    Ok(())
}

fn schedule_fade(app: AppHandle, tab_id: usize, generation: u64) {
    thread::spawn(move || {
        thread::sleep(FADE_AFTER);
        let handle = app.clone();
        if let Err(error) = app.run_on_main_thread(move || {
            let changed = handle
                .state::<NotificationState>()
                .0
                .lock()
                .map(|mut store| store.fade(tab_id, generation, Instant::now()))
                .unwrap_or(false);
            if changed {
                schedule_render(&handle);
            }
        }) {
            eprintln!("[gpt-wrapper] completion fade dispatch failed: {error}");
        }
    });
}

fn play_sound(app: &AppHandle) -> Result<(), String> {
    let path = app
        .path()
        .resolve("resources/ui-success-chime.wav", BaseDirectory::Resource)
        .map_err(|error| format!("cannot resolve sound resource: {error}"))?;
    if !path.is_file() {
        return Err(format!("sound resource missing: {}", path.display()));
    }
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // No SND_SYSTEM: use this process's audio session and application volume.
    if unsafe {
        PlaySoundW(
            PCWSTR(wide.as_ptr()),
            None,
            SND_FILENAME | SND_ASYNC | SND_NODEFAULT,
        )
    }
    .as_bool()
    {
        Ok(())
    } else {
        Err("PlaySoundW rejected completion sound".into())
    }
}

// All HWND operations and window creation are serialized on the Tauri UI thread.
fn render_on_ui_thread(app: &AppHandle) {
    let state = app.state::<NotificationState>();
    let Ok(mut store) = state.0.lock() else {
        return;
    };
    if !can_notify(app) {
        store.clear();
    }
    if store.cards.is_empty() {
        if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
            if store.page_ready {
                let _ = window.eval(update_script(&[]));
            }
            if let Err(error) = hide_native(&window) {
                eprintln!("[gpt-wrapper] failed to hide completion cards: {error}");
                schedule_hide_retry(app.clone(), 6);
            }
        }
        return;
    }
    if app.get_webview_window(WINDOW_LABEL).is_none() {
        store.page_ready = false;
        drop(store);
        if let Err(error) = create_window(app) {
            eprintln!("[gpt-wrapper] failed to create completion cards: {error}");
        }
        return;
    }
    if !store.page_ready {
        return;
    }
    let Some(window) = app.get_webview_window(WINDOW_LABEL) else {
        return;
    };
    let render_result = window
        .eval(update_script(&store.cards))
        .map_err(|error| error.to_string())
        .and_then(|_| place(&window, store.cards.len()))
        .and_then(|_| {
            // The window could gain focus elsewhere while its WebView was initializing.
            if !can_notify(app) && !store.cards.iter().any(|card| card.device) {
                Err("main window regained focus".into())
            } else {
                show_without_activation(&window)
            }
        });
    let enabled = if render_result.is_ok() {
        app.state::<GeneralSettingsState>()
            .notification_sound_enabled()
            .unwrap_or_else(|error| {
                eprintln!("[gpt-wrapper] cannot read completion sound setting: {error}");
                false
            })
    } else {
        false
    };
    let fresh = store.after_render(
        Instant::now(),
        render_result.is_ok(),
        can_notify(app),
        enabled,
        || play_sound(app),
    );
    if let Err(error) = render_result {
        eprintln!("[gpt-wrapper] completion card rendering failed: {error}");
        if !can_notify(app) {
            store.clear();
        }
        if let Err(error) = hide_native(&window) {
            eprintln!("[gpt-wrapper] failed to hide completion cards after render: {error}");
            if store.cards.is_empty() {
                schedule_hide_retry(app.clone(), 6);
            }
        }
        return;
    }
    for (tab_id, generation) in fresh {
        schedule_fade(app.clone(), tab_id, generation);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add_card(store: &mut Store, id: usize, device: bool) {
        let generation = store.prepare(id);
        assert!(store.finish(id, generation, "test".into(), true));
        store.cards[0].device = device;
    }

    #[test]
    fn sound_only_on_first_successful_display_of_completions() {
        let mut store = Store::default();
        let now = Instant::now();
        add_card(&mut store, 1, false);
        // Page load, failed eval/place/show, and pending candidates are silent.
        assert!(store
            .after_render(now, true, true, true, || panic!("page not ready"))
            .is_empty());
        store.page_ready = true;
        assert!(store
            .after_render(now, false, true, true, || panic!("display failed"))
            .is_empty());
        assert!(store.cards[0].shown_at.is_none());
        let mut calls = 0;
        assert_eq!(
            store
                .after_render(now, true, true, true, || {
                    calls += 1;
                    Ok(())
                })
                .len(),
            1
        );
        assert_eq!(calls, 1);
        assert_eq!(store.cards[0].shown_at, Some(now));
        assert!(store.fade(1, store.cards[0].generation, now + FADE_AFTER));
        store.after_render(now + FADE_AFTER, true, true, true, || panic!("fade replay"));
        store.page_ready = false;
        store.after_render(now + FADE_AFTER, true, true, true, || {
            panic!("window recreation")
        });
        store.page_ready = true;
        store.after_render(now + FADE_AFTER, true, true, true, || {
            panic!("window recreation replay")
        });
        add_card(&mut store, 0, true);
        store.after_render(now + FADE_AFTER, true, true, true, || {
            panic!("device sound")
        });
    }

    #[test]
    fn batch_and_global_cooldown_drop_sounds_without_queueing() {
        let mut store = Store {
            page_ready: true,
            ..Store::default()
        };
        let now = Instant::now();
        add_card(&mut store, 1, false);
        add_card(&mut store, 2, false);
        add_card(&mut store, 0, true);
        let mut calls = 0;
        assert_eq!(
            store
                .after_render(now, true, true, true, || {
                    calls += 1;
                    Ok(())
                })
                .len(),
            3
        );
        assert_eq!(calls, 1);
        add_card(&mut store, 3, false);
        store.after_render(now + Duration::from_millis(999), true, true, true, || {
            panic!("cooldown")
        });
        store.after_render(now + SOUND_COOLDOWN, true, true, true, || {
            panic!("no delayed replay")
        });
        add_card(&mut store, 4, false);
        store.after_render(now + SOUND_COOLDOWN, true, true, true, || {
            calls += 1;
            Ok(())
        });
        assert_eq!(calls, 2);
    }

    #[test]
    fn focus_muting_and_playback_failure_preserve_cards_without_replay() {
        let mut store = Store {
            page_ready: true,
            ..Store::default()
        };
        let now = Instant::now();
        add_card(&mut store, 1, false);
        store.after_render(now, true, false, true, || panic!("foreground"));
        add_card(&mut store, 2, false);
        store.after_render(now, true, true, false, || panic!("muted"));
        assert_eq!(store.cards.len(), 2);
        assert!(store.cards.iter().all(|card| card.shown_at.is_some()));
        store.after_render(now, true, true, true, || panic!("unmute replay"));
        add_card(&mut store, 3, false);
        store.after_render(now, true, true, true, || Err("native failure".into()));
        assert!(store.last_sound_at.is_none());
        assert!(store.cards[0].shown_at.is_some());
        store.after_render(now, true, true, true, || panic!("failed sound replay"));
        add_card(&mut store, 4, false);
        store.after_render(now, true, true, true, || Ok(()));
        assert_eq!(store.last_sound_at, Some(now));
    }

    #[test]
    fn settle_focus_and_cancellation_gates_cannot_create_a_sound() {
        assert_eq!(SETTLE_AFTER, Duration::from_millis(220));
        let now = Instant::now();
        let mut store = Store {
            page_ready: true,
            ..Store::default()
        };
        let generation = store.prepare(1);
        store.after_render(now, true, true, true, || panic!("still settling"));
        assert!(!store.finish(1, generation, "focused or working again".into(), false));
        store.after_render(now, true, true, true, || panic!("rejected candidate"));
        let generation = store.prepare(1);
        store.cancel_tab(1);
        assert!(!store.finish(1, generation, "cancelled".into(), true));
        store.after_render(now, true, true, true, || panic!("cancelled candidate"));
        add_card(&mut store, 1, false);
        store.clear();
        store.after_render(now, true, true, true, || panic!("focus cleared"));
    }

    #[test]
    fn foreground_clear_keeps_device_notice_and_discards_tab_completion() {
        let mut s = Store::default();
        let g = s.prepare(0);
        s.finish(0, g, "device".into(), true);
        s.cards[0].device = true;
        let g = s.prepare(1);
        s.finish(1, g, "tab".into(), true);
        s.clear();
        assert_eq!(s.cards.len(), 1);
        assert!(s.cards[0].device);
        assert!(s.dismiss(0, s.cards[0].generation));
        assert!(s.cards.is_empty());
    }

    #[test]
    fn only_visible_dom_working_exit_is_candidate() {
        assert_eq!(
            transition(TabStatus::Working, TabStatus::Send, true),
            Transition::Candidate
        );
        assert_eq!(
            transition(TabStatus::Working, TabStatus::Voice, true),
            Transition::Candidate
        );
        assert_eq!(
            transition(TabStatus::Working, TabStatus::Fallback, true),
            Transition::Candidate
        );
        assert_eq!(
            transition(TabStatus::Working, TabStatus::Send, false),
            Transition::None
        );
        assert_eq!(
            transition(TabStatus::Send, TabStatus::Voice, true),
            Transition::None
        );
        assert_eq!(
            transition(TabStatus::Fallback, TabStatus::Send, true),
            Transition::None
        );
        assert_eq!(
            transition(TabStatus::Fallback, TabStatus::Working, true),
            Transition::Cancel
        );
    }

    #[test]
    fn candidates_are_discarded_when_cancelled_or_not_allowed() {
        let mut store = Store::default();
        let first = store.prepare(2);
        assert!(!store.finish(2, first, "A".into(), false));
        assert!(store.cards.is_empty());
        let next = store.prepare(2);
        store.cancel_tab(2);
        assert!(!store.finish(2, next, "A".into(), true));
        assert!(store.cards.is_empty());
    }

    #[test]
    fn separate_tabs_newest_first_and_dismiss_by_generation() {
        let mut store = Store::default();
        let first = store.prepare(1);
        assert!(store.finish(1, first, "Same".into(), true));
        let second = store.prepare(2);
        assert!(store.finish(2, second, "Same".into(), true));
        assert_eq!(
            store
                .cards
                .iter()
                .map(|card| card.tab_id)
                .collect::<Vec<_>>(),
            vec![2, 1]
        );
        assert!(!store.dismiss(2, first));
        assert!(store.dismiss(2, second));
        assert_eq!(store.cards[0].tab_id, 1);
        store.cancel_tab(1);
        assert!(store.cards.is_empty());
    }

    #[test]
    fn repeat_working_invalidates_old_card_and_fade() {
        let start = Instant::now();
        let mut store = Store::default();
        let old = store.prepare(9);
        assert!(store.finish(9, old, "Old".into(), true));
        store.cards[0].shown_at = Some(start);
        assert!(!store.fade(9, old, start + Duration::from_millis(2999)));
        assert!(store.fade(9, old, start + FADE_AFTER));
        assert!(store.cards[0].faded);
        store.cancel_tab(9);
        let new = store.prepare(9);
        assert!(store.finish(9, new, "New".into(), true));
        store.cards[0].shown_at = Some(start + Duration::from_secs(4));
        assert!(!store.fade(9, old, start + Duration::from_secs(7)));
        assert!(!store.dismiss(9, old));
        assert!(!store.cards[0].faded);
        store.clear();
        assert!(!store.fade(9, new, start + Duration::from_secs(8)));
    }

    #[test]
    fn title_is_json_encoded_and_never_injected_as_markup() {
        let card = Card {
            device: false,
            tab_id: 1,
            generation: 42,
            title: "</script>\";alert(1);//".into(),
            faded: false,
            shown_at: None,
        };
        let script = update_script(&[card]);
        assert!(script.contains(r#"\";alert(1);"#));
        assert!(script.starts_with("window.__GPT_TAB_COMPLETIONS__?.render("));
    }
}
