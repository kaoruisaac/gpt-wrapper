//! Windows native adapter (WebView2 COM + Win32).
//!
//! This is the pre-port Windows implementation moved behind the platform
//! facade. Behaviour, COM calls, ordering and error handling are unchanged;
//! shared decisions (trust, navigation identity, PTT state) live in main.rs.
use std::sync::{Arc, Mutex};
use tauri::{window::Window, AppHandle, Manager, Webview, WebviewBuilder, Wry};
use uuid::Uuid;
use webview2_com::{
    CallDevToolsProtocolMethodCompletedHandler, CoTaskMemPWSTR, ContextMenuRequestedEventHandler,
    CustomItemSelectedEventHandler, ExecuteScriptCompletedHandler,
    Microsoft::Web::WebView2::Win32::{
        ICoreWebView2, ICoreWebView2Environment9, ICoreWebView2_11, ICoreWebView2_2,
        COREWEBVIEW2_CONTEXT_MENU_ITEM_KIND_COMMAND,
    },
    NavigationCompletedEventHandler, NavigationStartingEventHandler,
    NewWindowRequestedEventHandler, WebMessageReceivedEventHandler,
};
use windows::{
    core::{Interface, HSTRING, PCWSTR, PWSTR},
    Win32::UI::{
        Input::KeyboardAndMouse::{GetAsyncKeyState, ReleaseCapture, VK_LBUTTON},
        Shell::ShellExecuteW,
        WindowsAndMessaging::{
            AppendMenuW, CreatePopupMenu, DestroyMenu, GetWindowPlacement, IsIconic,
            SetForegroundWindow, ShowWindow, TrackPopupMenuEx, MF_CHECKED, MF_STRING, SW_RESTORE,
            SW_SHOWMAXIMIZED, SW_SHOWNORMAL, TPM_LEFTBUTTON, TPM_NONOTIFY, TPM_RETURNCMD,
            TPM_RIGHTALIGN, WINDOWPLACEMENT, WPF_RESTORETOMAXIMIZED,
        },
    },
};

use crate::{
    background_project_switcher, create_tab_with_spec, external_link_url, link_tab_spec,
    project_settings::ProjectList, provider::Provider, refresh_tab_host, MenuAnchor,
    MAIN_WINDOW_LABEL,
};

/// Native handle used to dispatch trusted input into the WebView that ran a script.
pub type ClickTarget = ICoreWebView2;

/// Runs `script` and returns its JSON-encoded completion value on the UI thread.
/// `on_unavailable` reports a WebView that could not run the script at all.
pub fn eval_json<R, U>(webview: &Webview, script: String, on_result: R, on_unavailable: U)
where
    R: FnOnce(Result<String, String>, ClickTarget) + Send + 'static,
    U: FnOnce(String) + Send + 'static,
{
    let script = HSTRING::from(script);
    let unavailable = Arc::new(Mutex::new(Some(on_unavailable)));
    let unavailable_for_ui = Arc::clone(&unavailable);
    let result = webview.with_webview(move |platform_webview| {
        let fail = |message: String| {
            if let Some(callback) = unavailable_for_ui.lock().ok().and_then(|mut f| f.take()) {
                callback(message);
            }
        };
        let core = match unsafe { platform_webview.controller().CoreWebView2() } {
            Ok(core) => core,
            Err(error) => {
                fail(format!("WebView2 unavailable: {error:?}"));
                return;
            }
        };
        let click_target = core.clone();
        let handler = ExecuteScriptCompletedHandler::create(Box::new(move |result, raw| {
            on_result(
                result.map(|()| raw).map_err(|error| format!("{error:?}")),
                click_target,
            );
            Ok(())
        }));
        if let Err(error) = unsafe { core.ExecuteScript(&script, &handler) } {
            fail(format!("ExecuteScript failed: {error:?}"));
        }
    });
    if let Err(error) = result {
        if let Some(callback) = unavailable.lock().ok().and_then(|mut f| f.take()) {
            callback(format!("with_webview failed: {error:?}"));
        }
    }
}

fn devtools_mouse_event(
    webview: &ICoreWebView2,
    event_type: &str,
    x: f64,
    y: f64,
    button: &str,
    buttons: u32,
) {
    let method = HSTRING::from("Input.dispatchMouseEvent");
    let params = HSTRING::from(
        serde_json::json!({
            "type": event_type,
            "x": x,
            "y": y,
            "button": button,
            "buttons": buttons,
            "clickCount": 1
        })
        .to_string(),
    );

    unsafe {
        let handler = CallDevToolsProtocolMethodCompletedHandler::create(Box::new(
            move |result, _response| {
                if let Err(error) = result {
                    eprintln!("[gpt-wrapper] DevTools mouse event failed: {error:?}");
                }
                Ok(())
            },
        ));

        if let Err(error) = webview.CallDevToolsProtocolMethod(&method, &params, &handler) {
            eprintln!("[gpt-wrapper] CallDevToolsProtocolMethod failed: {error:?}");
        }
    }
}

/// WebView2 trusted input: CDP Input.dispatchMouseEvent in viewport CSS pixels.
pub fn trusted_click(webview: &ClickTarget, x: f64, y: f64) {
    devtools_mouse_event(webview, "mouseMoved", x, y, "none", 0);
    devtools_mouse_event(webview, "mousePressed", x, y, "left", 1);
    devtools_mouse_event(webview, "mouseReleased", x, y, "left", 0);
}

/// Delivers `(source document URL, string payload)` for every
/// `chrome.webview.postMessage` from this WebView. Trust is decided by the caller.
pub fn install_message_listener<F>(webview: &Webview, tab_id: usize, handler: F) -> Result<(), String>
where
    F: Fn(&str, &str) + Send + 'static,
{
    let setup_error = Arc::new(Mutex::new(None));
    let setup_error_for_callback = Arc::clone(&setup_error);
    webview
        .with_webview(move |platform_webview| {
            let Ok(core) = (unsafe { platform_webview.controller().CoreWebView2() }) else {
                *setup_error_for_callback.lock().unwrap() =
                    Some(format!("WebView2 missing on tab {tab_id}"));
                return;
            };
            let message_handler = WebMessageReceivedEventHandler::create(Box::new(move |_sender, args| {
                let Some(args) = args else {
                    #[cfg(debug_assertions)]
                    eprintln!("[gpt-wrapper] status tab {tab_id}: payload rejected (missing event args)");
                    return Ok(());
                };
                let mut source = PWSTR(std::ptr::null_mut());
                if unsafe { args.Source(&mut source) }.is_err() {
                    #[cfg(debug_assertions)]
                    eprintln!("[gpt-wrapper] status tab {tab_id}: source rejected (read failed)");
                    return Ok(());
                }
                let source = CoTaskMemPWSTR::from(source).to_string();
                let mut raw = PWSTR(std::ptr::null_mut());
                if unsafe { args.TryGetWebMessageAsString(&mut raw) }.is_err() {
                    #[cfg(debug_assertions)]
                    eprintln!("[gpt-wrapper] status tab {tab_id}: payload rejected (not a string)");
                    return Ok(());
                }
                let raw = CoTaskMemPWSTR::from(raw).to_string();
                handler(&source, &raw);
                Ok(())
            }));
            let mut token = 0;
            if let Err(error) = unsafe { core.add_WebMessageReceived(&message_handler, &mut token) } {
                *setup_error_for_callback.lock().unwrap() = Some(format!(
                    "failed to install status observer on tab {tab_id}: {error:?}"
                ));
            } else {
                #[cfg(debug_assertions)]
                eprintln!("[gpt-wrapper] status listener installed on tab {tab_id}");
            }
        })
        .map_err(|error| format!("failed to access WebView2 for tab {tab_id}: {error}"))?;
    let result = setup_error.lock().unwrap().take();
    result.map_or(Ok(()), Err)
}

/// WebView2 navigation IDs distinguish even A -> B -> A and duplicate
/// completions. `on_started(id, url)` also fires for redirects of the same ID.
pub fn install_navigation_listener<S, F>(
    webview: &Webview,
    _tab_id: usize,
    on_started: S,
    on_finished: F,
) -> Result<(), String>
where
    S: Fn(u64, url::Url) + Send + 'static,
    F: Fn(u64, url::Url, bool) + Send + 'static,
{
    let setup_error = Arc::new(Mutex::new(None));
    let callback_error = Arc::clone(&setup_error);
    webview.with_webview(move |platform| {
        let result = (|| -> windows::core::Result<()> {
            let core = unsafe { platform.controller().CoreWebView2()? };
            let starting = NavigationStartingEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else { return Ok(()); };
                let mut id = 0;
                let mut uri = PWSTR::null();
                unsafe { args.NavigationId(&mut id)?; args.Uri(&mut uri)?; }
                let Ok(url) = url::Url::parse(&CoTaskMemPWSTR::from(uri).to_string()) else { return Ok(()); };
                on_started(id, url);
                Ok(())
            }));
            let completed = NavigationCompletedEventHandler::create(Box::new(move |sender, args| {
                let (Some(sender), Some(args)) = (sender, args) else { return Ok(()); };
                let mut id = 0;
                let mut success = windows::core::BOOL::default();
                let mut source = PWSTR::null();
                unsafe { args.NavigationId(&mut id)?; args.IsSuccess(&mut success)?; sender.Source(&mut source)?; }
                let Ok(url) = url::Url::parse(&CoTaskMemPWSTR::from(source).to_string()) else { return Ok(()); };
                on_finished(id, url, success.as_bool());
                Ok(())
            }));
            let mut token = 0;
            unsafe {
                core.add_NavigationStarting(&starting, &mut token)?;
                core.add_NavigationCompleted(&completed, &mut token)?;
            }
            Ok(())
        })();
        if let Err(error) = result { *callback_error.lock().unwrap() = Some(error.to_string()); }
    }).map_err(|error| error.to_string())?;
    let error = setup_error.lock().unwrap().take();
    error.map_or(Ok(()), Err)
}

/// Cancels the current document/request, including a request whose
/// NavigationStarting has not arrived yet.
pub fn stop_loading(webview: &Webview) -> Result<(), String> {
    let stop_error = Arc::new(Mutex::new(None));
    let callback_error = Arc::clone(&stop_error);
    webview
        .with_webview(move |platform| {
            let result = unsafe {
                platform
                    .controller()
                    .CoreWebView2()
                    .and_then(|core| core.Stop())
            };
            if let Err(error) = result {
                *callback_error.lock().unwrap() = Some(error.to_string());
            }
        })
        .map_err(|error| error.to_string())?;
    let error = stop_error.lock().unwrap().take();
    error.map_or(Ok(()), Err)
}

// ShellExecuteW uses the user's Windows default browser. Run it off the WebView
// event thread so link activation cannot stall WebView2 or Tauri's UI loop.
pub fn open_in_system_browser(url: url::Url, tab_id: usize) {
    tauri::async_runtime::spawn_blocking(move || {
        let wide_url: Vec<u16> = url
            .as_str()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let result = unsafe {
            ShellExecuteW(
                None,
                windows::core::w!("open"),
                PCWSTR(wide_url.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            )
        };
        if (result.0 as usize) <= 32 {
            eprintln!("[gpt-wrapper] failed to open external link from tab {tab_id}: {url} (ShellExecuteW code {})", result.0 as usize);
        }
    });
}

/// WebView2 popups are intercepted with NewWindowRequested (installed after
/// creation), so the builder needs no Windows-specific configuration.
pub fn configure_tab_builder(
    builder: WebviewBuilder<Wry>,
    _app: &AppHandle,
    _tab_id: usize,
    _provider: Provider,
) -> WebviewBuilder<Wry> {
    builder
}

pub fn install_link_context_menu(
    app: &AppHandle,
    webview: &Webview,
    tab_id: usize,
    provider: Provider,
) -> Result<(), String> {
    let app = app.clone();
    let setup_error = Arc::new(Mutex::new(None));
    let setup_error_for_callback = Arc::clone(&setup_error);
    webview
        .with_webview(move |platform| {
            let setup = (|| -> windows::core::Result<()> {
                let core = unsafe { platform.controller().CoreWebView2()? };
                let core11: ICoreWebView2_11 = core.cast()?;
                let core2: ICoreWebView2_2 = core.cast()?;
                let environment: ICoreWebView2Environment9 =
                    unsafe { core2.Environment()? }.cast()?;

                let handler = ContextMenuRequestedEventHandler::create(Box::new(move |_, args| {
                    // Leave the native menu unchanged unless a valid web link was clicked.
                    let Some(args) = args else {
                        return Ok(());
                    };
                    let result = (|| -> windows::core::Result<()> {
                        let target = unsafe { args.ContextMenuTarget()? };
                        let mut has_link = windows::core::BOOL::default();
                        unsafe { target.HasLinkUri(&mut has_link)? };
                        if !has_link.as_bool() {
                            return Ok(());
                        }

                        let mut raw_link = PWSTR::null();
                        unsafe { target.LinkUri(&mut raw_link)? };
                        let link_uri = CoTaskMemPWSTR::from(raw_link).to_string();
                        let Ok(spec) = link_tab_spec(provider, &link_uri) else {
                            return Ok(());
                        };
                        let url = spec.url.clone();
                        let items = unsafe { args.MenuItems()? };

                        // Remove the native "open in new window" action. WebView2 uses
                        // this unlocalized Name regardless of the UI language.
                        // A native new-tab action would also hit NewWindowRequested,
                        // so replace it with our in-app tab action if it exists.
                        let mut count = 0;
                        unsafe { items.Count(&mut count)? };
                        for index in (0..count).rev() {
                            let existing = unsafe { items.GetValueAtIndex(index)? };
                            let mut raw_name = PWSTR::null();
                            unsafe { existing.Name(&mut raw_name)? };
                            let name = CoTaskMemPWSTR::from(raw_name).to_string();
                            if matches!(name.as_str(), "openLinkInNewWindow" | "openLinkInNewTab") {
                                unsafe { items.RemoveValueAtIndex(index)? };
                            }
                        }

                        let tab_label: Vec<u16> = "在新分頁中開啟連結\0".encode_utf16().collect();
                        let tab_item = unsafe {
                            environment.CreateContextMenuItem(
                                PCWSTR(tab_label.as_ptr()),
                                None,
                                COREWEBVIEW2_CONTEXT_MENU_ITEM_KIND_COMMAND,
                            )?
                        };
                        let tab_app = app.clone();
                        let tab_selected =
                            CustomItemSelectedEventHandler::create(Box::new(move |_, _| {
                                // add_child waits on the UI loop. Never create tabs inside
                                // the WebView2 context-menu callback itself.
                                let worker_app = tab_app.clone();
                                let worker_spec = spec.clone();
                                tauri::async_runtime::spawn_blocking(move || {
                                    // The link stays with the source tab's provider.
                                    match create_tab_with_spec(&worker_app, provider, worker_spec, false) {
                                        Ok(_) => refresh_tab_host(&worker_app),
                                        Err(error) => eprintln!(
                                            "[gpt-wrapper] failed to open link in new tab: {error}"
                                        ),
                                    }
                                });
                                Ok(())
                            }));
                        let mut tab_selected_token = 0;
                        unsafe {
                            tab_item
                                .add_CustomItemSelected(&tab_selected, &mut tab_selected_token)?
                        };
                        unsafe { items.InsertValueAtIndex(0, &tab_item)? };

                        let label: Vec<u16> = "在預設瀏覽器開啟連結\0".encode_utf16().collect();
                        let item = unsafe {
                            environment.CreateContextMenuItem(
                                PCWSTR(label.as_ptr()),
                                None,
                                COREWEBVIEW2_CONTEXT_MENU_ITEM_KIND_COMMAND,
                            )?
                        };

                        let selected =
                            CustomItemSelectedEventHandler::create(Box::new(move |_, _| {
                                open_in_system_browser(url.clone(), tab_id);
                                Ok(())
                            }));
                        let mut selected_token = 0;
                        unsafe { item.add_CustomItemSelected(&selected, &mut selected_token)? };

                        unsafe { items.InsertValueAtIndex(1, &item)? };
                        Ok(())
                    })();
                    if let Err(error) = result {
                        eprintln!(
                            "[gpt-wrapper] failed to extend context menu on tab {tab_id}: {error:?}"
                        );
                    }
                    Ok(())
                }));
                let mut token = 0;
                unsafe { core11.add_ContextMenuRequested(&handler, &mut token)? };
                Ok(())
            })();
            if let Err(error) = setup {
                *setup_error_for_callback.lock().unwrap() = Some(format!(
                    "failed to install link context menu on tab {tab_id}: {error:?}"
                ));
            }
        })
        .map_err(|error| format!("failed to access WebView2 on tab {tab_id}: {error}"))?;
    let result = setup_error.lock().unwrap().take();
    result.map_or(Ok(()), Err)
}

pub fn install_external_new_window_handler(webview: &Webview, tab_id: usize) -> Result<(), String> {
    let setup_error = Arc::new(Mutex::new(None));
    let callback_error = Arc::clone(&setup_error);
    webview
        .with_webview(move |platform| {
            let setup = (|| -> windows::core::Result<()> {
                let core = unsafe { platform.controller().CoreWebView2()? };
                let handler = NewWindowRequestedEventHandler::create(Box::new(move |_, args| {
                    let Some(args) = args else {
                        return Ok(());
                    };
                    // Suppress WebView2's default popup even for unsupported schemes.
                    unsafe { args.SetHandled(true)? };
                    let mut raw_uri = PWSTR::null();
                    unsafe { args.Uri(&mut raw_uri)? };
                    let uri = CoTaskMemPWSTR::from(raw_uri).to_string();
                    match external_link_url(&uri) {
                        Ok(url) => open_in_system_browser(url, tab_id),
                        Err(_) => eprintln!(
                            "[gpt-wrapper] ignored non-web popup URL on tab {tab_id}: {uri}"
                        ),
                    }
                    Ok(())
                }));
                let mut token = 0;
                unsafe { core.add_NewWindowRequested(&handler, &mut token)? };
                Ok(())
            })();
            if let Err(error) = setup {
                *callback_error.lock().unwrap() = Some(format!(
                    "failed to intercept new windows on tab {tab_id}: {error:?}"
                ));
            }
        })
        .map_err(|error| format!("failed to access WebView2 on tab {tab_id}: {error}"))?;
    let result = setup_error.lock().unwrap().take();
    result.map_or(Ok(()), Err)
}

struct PopupMenu(windows::Win32::UI::WindowsAndMessaging::HMENU);

impl Drop for PopupMenu {
    fn drop(&mut self) {
        if let Err(error) = unsafe { DestroyMenu(self.0) } {
            eprintln!("[gpt-wrapper] failed to destroy bookmark menu: {error}");
        }
    }
}

/// Runs on the UI thread. Returns the chosen project (nil UUID = built-in
/// home) or None when the mouse was already released or nothing was chosen.
pub fn track_project_bookmark_menu(
    app: &AppHandle,
    provider: Provider,
    settings: ProjectList,
    anchor: MenuAnchor,
) -> Result<Option<Uuid>, String> {
    // IPC can reach the UI thread after the mouse has already been released.
    if unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } >= 0 {
        return Ok(None);
    }
    let window = app
        .get_window(MAIN_WINDOW_LABEL)
        .ok_or("main window is missing")?;
    let scale = window.scale_factor().map_err(|error| error.to_string())?;
    let origin = window.inner_position().map_err(|error| error.to_string())?;
    let hwnd = window.hwnd().map_err(|error| error.to_string())?;
    let menu = PopupMenu(unsafe { CreatePopupMenu() }.map_err(|error| error.to_string())?);

    let builtin_label: Vec<u16> = format!("{}\0", background_project_switcher::builtin_label(provider))
        .encode_utf16()
        .collect();
    let builtin_flags = if settings.default_project_id.is_none() {
        MF_STRING | MF_CHECKED
    } else {
        MF_STRING
    };
    unsafe { AppendMenuW(menu.0, builtin_flags, 1, PCWSTR(builtin_label.as_ptr())) }
        .map_err(|error| error.to_string())?;
    {
        for (index, project) in settings.projects.iter().enumerate() {
            let label: Vec<u16> =
                format!("{}\0", project.name.replace('&', "&&").replace('\0', ""))
                    .encode_utf16()
                    .collect();
            let mut flags = MF_STRING;
            if settings.default_project_id == Some(project.id) {
                flags |= MF_CHECKED;
            }
            unsafe { AppendMenuW(menu.0, flags, index + 2, PCWSTR(label.as_ptr())) }
                .map_err(|error| error.to_string())?;
        }
    }

    // WebView2 may still own capture from pointerdown; the menu loop needs it.
    let _ = unsafe { ReleaseCapture() };
    if unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } >= 0 {
        return Ok(None);
    }
    let x = origin.x + (anchor.right * scale).round() as i32;
    let y = origin.y + (anchor.bottom * scale).round() as i32;
    let flags = TPM_LEFTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTALIGN;
    let command = unsafe { TrackPopupMenuEx(menu.0, flags.0, x, y, hwnd, None) }.0 as usize;
    Ok(if command == 1 {
        Some(Uuid::nil())
    } else {
        command
            .checked_sub(2)
            .and_then(|index| settings.projects.get(index).map(|p| p.id))
    })
}

// SW_RESTORE must only run for minimized windows: it unmaximizes visible
// maximized windows. Windows also records whether a minimized window should
// return to maximized state in WINDOWPLACEMENT.
pub(crate) fn minimized_restore_command(
    restore_maximized: bool,
) -> windows::Win32::UI::WindowsAndMessaging::SHOW_WINDOW_CMD {
    if restore_maximized {
        SW_SHOWMAXIMIZED
    } else {
        SW_RESTORE
    }
}

pub fn restore_to_foreground(window: &Window) {
    match window.hwnd() {
        Ok(hwnd) => unsafe {
            let was_minimized = IsIconic(hwnd).as_bool();
            let restore_maximized = if was_minimized {
                let mut placement = WINDOWPLACEMENT {
                    length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
                    ..Default::default()
                };
                GetWindowPlacement(hwnd, &mut placement).is_ok()
                    && placement.flags.contains(WPF_RESTORETOMAXIMIZED)
            } else {
                false
            };

            let _ = window.show();
            if was_minimized {
                let _ = ShowWindow(hwnd, minimized_restore_command(restore_maximized));
            }
            let _ = SetForegroundWindow(hwnd);
        },
        Err(error) => {
            eprintln!("[gpt-wrapper] hwnd unavailable: {error:?}");
            let _ = window.show();
            if window.is_minimized().unwrap_or(false) {
                let _ = window.unminimize();
            }
        }
    }

    let _ = window.set_focus();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimized_restore_preserves_maximized_state() {
        assert_eq!(minimized_restore_command(false), SW_RESTORE);
        assert_eq!(minimized_restore_command(true), SW_SHOWMAXIMIZED);
    }
}
