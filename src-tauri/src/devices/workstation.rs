//! Windows session events plus an input-desktop check; neither alone grants PTT.
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use windows::{
    core::w,
    Win32::{
        Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM},
        System::{RemoteDesktop::*, StationsAndDesktops::*},
        UI::WindowsAndMessaging::*,
    },
};
static ALLOWED: AtomicBool = AtomicBool::new(false);
static GENERATION: AtomicU64 = AtomicU64::new(0);
pub fn generation() -> u64 {
    GENERATION.load(Ordering::SeqCst)
}
fn desktop_ready() -> bool {
    unsafe {
        let Ok(d) = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS) else {
            return false;
        };
        let mut name = [0u16; 128];
        let result = GetUserObjectInformationW(
            HANDLE(d.0),
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            256,
            None,
        )
        .is_ok();
        let _ = CloseDesktop(d);
        result
            && String::from_utf16_lossy(&name[..name.iter().position(|&c| c == 0).unwrap_or(128)])
                .eq_ignore_ascii_case("default")
    }
}
pub fn ready() -> bool {
    ALLOWED.load(Ordering::SeqCst) && desktop_ready()
}
unsafe extern "system" fn window(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_WTSSESSION_CHANGE => {
            if matches!(
                wparam.0 as u32,
                WTS_SESSION_LOCK
                    | WTS_SESSION_LOGOFF
                    | WTS_CONSOLE_DISCONNECT
                    | WTS_REMOTE_DISCONNECT
            ) {
                ALLOWED.store(false, Ordering::SeqCst);
                GENERATION.fetch_add(1, Ordering::SeqCst);
            } else if matches!(
                wparam.0 as u32,
                WTS_SESSION_UNLOCK | WTS_SESSION_LOGON | WTS_CONSOLE_CONNECT | WTS_REMOTE_CONNECT
            ) {
                ALLOWED.store(desktop_ready(), Ordering::SeqCst);
            }
        }
        WM_POWERBROADCAST => {
            if wparam.0 == 4 {
                ALLOWED.store(false, Ordering::SeqCst);
                GENERATION.fetch_add(1, Ordering::SeqCst);
            } else if matches!(wparam.0, 7 | 18) {
                ALLOWED.store(desktop_ready(), Ordering::SeqCst);
                GENERATION.fetch_add(1, Ordering::SeqCst);
            }
        }
        WM_QUERYENDSESSION => {
            ALLOWED.store(false, Ordering::SeqCst);
            GENERATION.fetch_add(1, Ordering::SeqCst);
            return LRESULT(1); // Permit logout after revoking input permission.
        }
        WM_ENDSESSION => {
            ALLOWED.store(wparam.0 == 0 && desktop_ready(), Ordering::SeqCst);
            GENERATION.fetch_add(1, Ordering::SeqCst);
        }
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}
pub fn start(stop: Arc<AtomicBool>) {
    std::thread::spawn(move || unsafe {
        let class = WNDCLASSW {
            lpfnWndProc: Some(window),
            lpszClassName: w!("GPTWDeviceSessionEvents"),
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            return;
        }
        let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class.lpszClassName,
            w!(""),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            None,
            None,
        ) else {
            return;
        };
        if WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION).is_err() {
            let _ = DestroyWindow(hwnd);
            return;
        }
        ALLOWED.store(desktop_ready(), Ordering::SeqCst);
        while !stop.load(Ordering::SeqCst) {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        ALLOWED.store(false, Ordering::SeqCst);
        let _ = WTSUnRegisterSessionNotification(hwnd);
        let _ = DestroyWindow(hwnd);
    });
}
