//! Native adapter facade.
//!
//! Only real native differences live behind this module: WebView script
//! execution / JS messages / navigation identity / trusted input / popups and
//! context menus, window focus, non-activating overlays, sounds and native
//! popup menus. Tab, Provider, PTT and Settings state machines stay in the
//! shared Rust Core (main.rs and sibling modules) and are never forked per OS.
//!
//! Every adapter exposes the same functions:
//! - `eval_json(webview, script, on_result, on_unavailable)`: run a script and
//!   return its JSON completion value on the UI thread, plus a `ClickTarget`.
//! - `trusted_click(target, x, y)`: real (user-activation) input at viewport
//!   CSS coordinates of the WebView that evaluated the script.
//! - `install_message_listener(webview, tab_id, handler(source, raw))`.
//! - `install_navigation_listener(webview, tab_id, started(id, url),
//!   finished(id, url, success))` with per-navigation identities.
//! - `stop_loading`, `open_in_system_browser`, `configure_tab_builder`,
//!   `install_link_context_menu`, `install_external_new_window_handler`.
//! - `track_project_bookmark_menu`, `restore_to_foreground`.
//! - macOS overlay helpers (`hide_overlay`, `show_overlay_without_activation`,
//!   `make_overlay_transparent`, `play_sound_file`); Windows keeps its Win32
//!   overlay/PlaySoundW code inside the notification modules under cfg(windows).

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use self::windows::*;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use self::macos::*;

#[cfg(not(any(windows, target_os = "macos")))]
compile_error!(
    "GPT Wrapper only supports Windows (WebView2) and macOS (WKWebView). \
     There is no native adapter for this target; refusing to build a shell \
     that pretends the core features work."
);
