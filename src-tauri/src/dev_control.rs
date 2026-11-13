//! Developer-only verification harness (debug builds only).
//!
//! Enabled solely when a *debug* build runs with
//! `GPT_WRAPPER_DEV_CONTROL=<port>`; release builds do not compile this module.
//! It listens on 127.0.0.1 and lets a local test script observe the real
//! WebViews and drive the same native paths the app uses:
//!   `state`                      -> tab snapshot JSON
//!   `eval <tab_id> <script>`     -> JSON result via platform::eval_json
//!   `click <tab_id> <x> <y>`     -> platform::trusted_click (native input)
//!   `notify <tab_id>`            -> simulated Working -> Send transition
//!   `ptt down|up`, `submit`      -> the same handle_ptt_event calls as the
//!                                   global-shortcut callbacks (keyboard source)
//!   `switch down|up`             -> switch_tabs_gesture keyboard edges
//!   `close-main` / `hidden`      -> real CloseRequested (hide to tray) / state
//! It never reads cookies, credentials or page content on its own.
use std::{
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    sync::mpsc,
    thread,
    time::Duration,
};
use tauri::{AppHandle, Manager};

use crate::{
    active_tab_id, get_tab_webview, handle_ptt_event, platform, set_tab_status, snapshot,
    switch_tabs_gesture, PttEvent, PttSource, PttState, TabStatus, TrayVisibilityState,
};
use std::sync::{atomic::Ordering, Arc};

pub fn start(app: &AppHandle) {
    let Some(port) = std::env::var("GPT_WRAPPER_DEV_CONTROL")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
    else {
        return;
    };
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("[gpt-wrapper] dev control unavailable on 127.0.0.1:{port}: {error}");
            return;
        }
    };
    eprintln!("[gpt-wrapper] DEV CONTROL listening on 127.0.0.1:{port} (debug build only)");
    let app = app.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let app = app.clone();
            thread::spawn(move || {
                let mut writer = match stream.try_clone() {
                    Ok(writer) => writer,
                    Err(_) => return,
                };
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    let reply = handle(&app, line.trim());
                    if writeln!(writer, "{}", reply.replace('\n', " ")).is_err() {
                        return;
                    }
                }
            });
        }
    });
}

fn handle(app: &AppHandle, line: &str) -> String {
    let mut parts = line.splitn(2, ' ');
    let command = parts.next().unwrap_or_default();
    let rest = parts.next().unwrap_or_default();
    match command {
        "state" => snapshot(app)
            .map(|value| serde_json::to_string(&value).unwrap_or_default())
            .unwrap_or_else(|error| format!("ERR {error}")),
        "eval" => {
            let (tab, script) = rest.split_once(' ').unwrap_or((rest, ""));
            let Some(webview) = tab.parse().ok().and_then(|id| get_tab_webview(app, id)) else {
                return "ERR no such tab".into();
            };
            let (sender, receiver) = mpsc::channel();
            let unavailable = sender.clone();
            platform::eval_json(
                &webview,
                script.to_string(),
                move |result, _| {
                    let _ = sender.send(result.unwrap_or_else(|error| format!("ERR {error}")));
                },
                move |error| {
                    let _ = unavailable.send(format!("ERR {error}"));
                },
            );
            receiver
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|_| "ERR timeout".into())
        }
        "click" => {
            let values: Vec<&str> = rest.split(' ').collect();
            let (Some(tab), Some(x), Some(y)) = (
                values.first().and_then(|v| v.parse().ok()),
                values.get(1).and_then(|v| v.parse::<f64>().ok()),
                values.get(2).and_then(|v| v.parse::<f64>().ok()),
            ) else {
                return "ERR usage: click <tab> <x> <y>".into();
            };
            let Some(webview) = get_tab_webview(app, tab) else {
                return "ERR no such tab".into();
            };
            // Obtain the platform ClickTarget the same way production code does.
            platform::eval_json(
                &webview,
                "null".into(),
                move |_, target| platform::trusted_click(&target, x, y),
                |error| eprintln!("[gpt-wrapper] dev click unavailable: {error}"),
            );
            "OK".into()
        }
        "notify" => {
            let Ok(tab) = rest.parse::<usize>() else {
                return "ERR usage: notify <tab>".into();
            };
            let app = app.clone();
            let handle = app.clone();
            let _ = app.run_on_main_thread(move || {
                set_tab_status(&handle, tab, TabStatus::Working);
                set_tab_status(&handle, tab, TabStatus::Send);
            });
            "OK".into()
        }
        "ptt" | "submit" => {
            let event = match (command, rest) {
                ("ptt", "down") => PttEvent::Down(PttSource::Keyboard),
                ("ptt", "up") => PttEvent::Up(PttSource::Keyboard),
                ("submit", _) => {
                    let hidden = app.state::<TrayVisibilityState>().tray_hidden.load(Ordering::SeqCst);
                    PttEvent::SubmitKeyboard(if hidden { None } else { active_tab_id(app) })
                }
                _ => return "ERR usage: ptt down|up | submit".into(),
            };
            let handle = app.clone();
            let _ = app.run_on_main_thread(move || {
                let tracker = Arc::clone(&handle.state::<PttState>().0);
                handle_ptt_event(&handle, event, &tracker);
            });
            "OK".into()
        }
        "switch" => {
            let edge = match rest {
                "down" => switch_tabs_gesture::Edge::Down,
                "up" => switch_tabs_gesture::Edge::Up,
                _ => return "ERR usage: switch down|up".into(),
            };
            let handle = app.clone();
            let _ = app.run_on_main_thread(move || {
                switch_tabs_gesture::input(&handle, switch_tabs_gesture::Source::Keyboard(0xdead), edge, None);
            });
            "OK".into()
        }
        "auto-submit" => app
            .state::<crate::general_settings::GeneralSettingsState>()
            .set_ptt_auto_submit(rest == "true")
            .map(|v| v.to_string())
            .unwrap_or_else(|e| format!("ERR {e}")),
        "provider" => {
            let provider = match rest {
                "chatgpt" => crate::provider::Provider::Chatgpt,
                "claude" => crate::provider::Provider::Claude,
                _ => return "ERR usage: provider chatgpt|claude".into(),
            };
            // Same blocking path as the async Tauri command (off the UI thread).
            crate::switch_provider_internal(app, provider)
                .map(|value| serde_json::to_string(&value).unwrap_or_default())
                .unwrap_or_else(|error| format!("ERR {error}"))
        }
        "evalw" => {
            // Any app WebView by label (e.g. the notification card window).
            let (label, script) = rest.split_once(' ').unwrap_or((rest, ""));
            let Some(webview) = app.get_webview(label) else {
                return "ERR no such webview".into();
            };
            let (sender, receiver) = mpsc::channel();
            let unavailable = sender.clone();
            platform::eval_json(
                &webview,
                script.to_string(),
                move |result, _| {
                    let _ = sender.send(result.unwrap_or_else(|error| format!("ERR {error}")));
                },
                move |error| {
                    let _ = unavailable.send(format!("ERR {error}"));
                },
            );
            receiver
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|_| "ERR timeout".into())
        }
        #[cfg(target_os = "macos")]
        "nswindow" => {
            let Some(window) = app.get_webview_window(rest) else {
                return "ERR no such window".into();
            };
            let (sender, receiver) = mpsc::channel();
            let target = window.clone();
            let _ = app.run_on_main_thread(move || {
                let _ = sender.send(platform::describe_window(&target));
            });
            receiver
                .recv_timeout(Duration::from_secs(5))
                .unwrap_or_else(|_| "ERR timeout".into())
        }
        "close-tab" => match rest.parse::<usize>() {
            // Same path as the tab bar's close button (off the UI thread).
            Ok(tab) => crate::close_tab_internal(app, tab)
                .map(|value| serde_json::to_string(&value).unwrap_or_default())
                .unwrap_or_else(|error| format!("ERR {error}")),
            Err(_) => "ERR usage: close-tab <tab>".into(),
        },
        "settings" => {
            crate::open_settings_window(app);
            "OK".into()
        }
        #[cfg(target_os = "macos")]
        "menu-hook" => match rest.parse::<usize>().ok().and_then(|id| get_tab_webview(app, id)) {
            Some(webview) => platform::exercise_link_menu_hook(&webview),
            None => "ERR no such tab".into(),
        },
        #[cfg(target_os = "macos")]
        "menu-hook-host" => match app.get_webview(crate::HOST_WEBVIEW_LABEL) {
            Some(webview) => platform::exercise_link_menu_hook(&webview),
            None => "ERR no host".into(),
        },
        #[cfg(target_os = "macos")]
        "link-route-newtab" => match rest.parse::<usize>() {
            Ok(tab) => {
                let _ = app.run_on_main_thread(move || platform::set_pending_link_route_for_test(tab, true));
                "OK".into()
            }
            Err(_) => "ERR usage: link-route-newtab <tab>".into(),
        },
        #[cfg(target_os = "macos")]
        "layout" => {
            let Some(window) = app.get_window(crate::MAIN_WINDOW_LABEL) else {
                return "ERR no main window".into();
            };
            let (sender, receiver) = mpsc::channel();
            let _ = app.run_on_main_thread(move || {
                let _ = sender.send(platform::describe_layout(&window));
            });
            receiver.recv_timeout(Duration::from_secs(5)).unwrap_or_else(|_| "ERR timeout".into())
        }
        "close-main" => match app.get_window(crate::MAIN_WINDOW_LABEL) {
            Some(window) => window.close().map(|_| "OK".into()).unwrap_or_else(|e| format!("ERR {e}")),
            None => "ERR no main window".into(),
        },
        "hidden" => app
            .state::<TrayVisibilityState>()
            .tray_hidden
            .load(Ordering::SeqCst)
            .to_string(),
        "visible" => app
            .get_window(crate::MAIN_WINDOW_LABEL)
            .and_then(|w| w.is_visible().ok())
            .map(|v| v.to_string())
            .unwrap_or_else(|| "ERR".into()),
        "focused" => app
            .get_window(crate::MAIN_WINDOW_LABEL)
            .and_then(|w| w.is_focused().ok())
            .map(|f| f.to_string())
            .unwrap_or_else(|| "ERR".into()),
        _ => "ERR unknown command".into(),
    }
}
