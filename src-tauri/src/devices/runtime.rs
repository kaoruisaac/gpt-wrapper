use super::{audio::Bridge, control::Kind};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Manager};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub device: String,
    pub pair_id: String,
    pub dn: String,
    pub hn: String,
    pub press: u32,
}
impl Identity {
    pub fn token(&self) -> String {
        format!(
            "{}:{}:{}:{}:{}",
            self.device, self.pair_id, self.dn, self.hn, self.press
        )
    }
}
#[derive(Clone)]
pub struct Input {
    pub identity: Identity,
    pub seq: u32,
    pub segment: u32,
    pub kind: Kind,
    pub suppress: bool,
}
pub struct Live {
    pub identity: Identity,
    pub segment: u32,
    pub held: bool,
    pub paused: bool,
    pub suppress: bool,
}
pub struct Lease {
    pub identity: Identity,
    pub tab: usize,
    pub generation: usize,
    pub route: String,
    pub applied: bool,
    pub capture: bool,
    pub attempt: String,
    pub started: bool,
}
#[derive(Default)]
pub struct Runtime {
    pub selected: Option<String>,
    pub live: Option<Live>,
    pub lease: Option<Lease>,
    pub invalidate: u64,
    pub cancelled: bool,
    pub route_error: Option<String>,
}

pub fn dispatch(app: &AppHandle, input: Input) {
    let target = app.clone();
    let _ = app.run_on_main_thread(move || {
        let state = target.state::<super::Devices>();
        let valid = state.host.lock().is_ok_and(|h| {
            if input.kind == Kind::Cancel {
                h.runtime
                    .lease
                    .as_ref()
                    .is_some_and(|l| l.identity == input.identity)
            } else if input.kind == Kind::Submit {
                super::workstation::ready()
                    && h.runtime.selected.as_deref() == Some(&input.identity.device)
                    && h.sessions.get(&input.identity.device)
                        == Some(&(input.identity.dn.clone(), input.identity.hn.clone()))
            } else {
                !h.runtime.cancelled
                    && h.runtime.live.as_ref().is_some_and(|l| {
                        l.identity == input.identity
                            && input.seq >= input.identity.press
                            && input.segment >= input.identity.press
                    })
            }
        });
        if !valid {
            return;
        }
        let tracker = &target.state::<crate::PttState>().0;
        if input.suppress {
            tracker
                .device_suppress_send
                .store(true, std::sync::atomic::Ordering::SeqCst);
            tracker.clear_deferred_submits();
        }
        match input.kind {
            Kind::Down => crate::handle_ptt_event(
                &target,
                crate::PttEvent::Down(crate::PttSource::Device),
                tracker,
            ),
            Kind::Up => crate::handle_ptt_event(
                &target,
                crate::PttEvent::Up(crate::PttSource::Device),
                tracker,
            ),
            Kind::Submit => {
                if target
                    .state::<crate::TrayVisibilityState>()
                    .tray_hidden
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    crate::handle_ptt_event(&target, crate::PttEvent::SubmitDevice, tracker);
                    return;
                }
                let tab = match tracker
                    .capture_independent_device_submit(crate::active_tab_id(&target))
                {
                    crate::DeviceSubmitTarget::Tab(tab) => Some(tab),
                    crate::DeviceSubmitTarget::None => None,
                    _ => return,
                };
                crate::handle_ptt_event(&target, crate::PttEvent::SubmitDeviceAt(tab), tracker);
            }
            Kind::Pause | Kind::Resume => {
                tracker
                    .device_suppress_send
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                tracker.clear_deferred_submits();
                if tracker
                    .clicked_generation
                    .load(std::sync::atomic::Ordering::SeqCst)
                    == 0
                {
                    fault(&target, "combo_before_start");
                }
            }
            Kind::FinishNoSend => {
                tracker
                    .device_suppress_send
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                tracker.clear_deferred_submits();
                crate::handle_ptt_event(
                    &target,
                    crate::PttEvent::Up(crate::PttSource::Device),
                    tracker,
                );
            }
            Kind::Cancel => {
                revoke_route(&target, None);
                tracker.clear_deferred_submits();
                crate::handle_ptt_event(&target, crate::PttEvent::CancelDevice, tracker);
            }
        }
    });
}

// Returns false while Applied is pending. This function never changes the
// persistent preference or another tab's/Voice's microphone policy.
pub fn prepare_route(app: &AppHandle, tab: usize, generation: usize) -> bool {
    let state = app.state::<super::Devices>();
    let Ok(mut h) = state.host.lock() else {
        return false;
    };
    if let Some(l) = &h.runtime.lease {
        return l.tab == tab && l.generation == generation && l.applied;
    }
    let Some(live) = &h.runtime.live else {
        return false;
    };
    if !live.held || live.paused {
        return false;
    }
    let identity = live.identity.clone();
    let route = uuid::Uuid::new_v4().to_string();
    h.runtime.lease = Some(Lease {
        identity,
        tab,
        generation,
        route: route.clone(),
        applied: false,
        capture: false,
        attempt: String::new(),
        started: false,
    });
    drop(h);
    if let Some(w) = crate::get_tab_webview(app, tab) {
        let encoded = serde_json::to_string(&route).unwrap();
        if w.eval(&format!(
            "window.__GPT_WRAPPER_MIC_ROUTE__?.installDictationLease({encoded});"
        ))
        .is_ok()
        {
            let app = app.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(5));
                let state = app.state::<super::Devices>();
                let failed = state.host.lock().is_ok_and(|h| {
                    h.runtime
                        .lease
                        .as_ref()
                        .is_some_and(|l| l.route == route && (!l.capture || !l.started))
                });
                if failed {
                    fault(&app, "route_or_start_timeout");
                }
            });
            return false;
        }
    }
    fault(app, "route_unavailable");
    false
}
pub fn route_ready(app: &AppHandle, tab: usize) -> Option<bool> {
    let state = app.state::<super::Devices>();
    let h = state.host.lock().ok()?;
    h.runtime.lease.as_ref().filter(|l| l.tab == tab).map(|l| {
        l.applied
            && h.runtime
                .live
                .as_ref()
                .is_some_and(|s| s.identity == l.identity && s.held && !s.paused)
    })
}
pub fn started(app: &AppHandle, tab: usize, generation: usize) {
    if let Ok(mut h) = app.state::<super::Devices>().host.lock() {
        if let Some(l) = &mut h.runtime.lease {
            if l.tab == tab && l.generation == generation {
                l.started = true;
            }
        }
    }
}
pub fn route_message(app: &AppHandle, tab: usize, value: &serde_json::Value) -> bool {
    if value["kind"] != "gpt-wrapper-device-route" {
        return false;
    }
    let state = app.state::<super::Devices>();
    let Ok(mut h) = state.host.lock() else {
        return true;
    };
    let Some(lease) = &mut h.runtime.lease else {
        return true;
    };
    if lease.tab != tab || value["lease"].as_str() != Some(&lease.route) {
        return true;
    }
    let tracker = &app.state::<crate::PttState>().0;
    if !tracker.is_current(tab, lease.generation) {
        return true;
    }
    match value["event"].as_str() {
        Some("applied") => {
            lease.applied = true;
            let generation = lease.generation;
            drop(h);
            if let Some(w) = crate::get_tab_webview(app, tab) {
                crate::schedule_ptt_start(w, tracker.clone(), tab, generation, true);
            }
        }
        Some("attempt") => {
            if let Some(attempt) = value["attempt"]
                .as_str()
                .filter(|s| !s.is_empty() && s.len() < 100)
            {
                lease.attempt = attempt.into();
            }
        }
        Some("captureVerified") => {
            if !lease.attempt.is_empty() && value["attempt"].as_str() == Some(&lease.attempt) {
                lease.capture = true;
            }
        }
        Some("failed") | Some("revoked") => {
            drop(h);
            fault(app, "capture_failed");
        }
        _ => {}
    }
    true
}
pub fn revoke_route(app: &AppHandle, finished: Option<(usize, usize)>) {
    let state = app.state::<super::Devices>();
    let lease = state.host.lock().ok().and_then(|mut h| {
        if finished.is_some_and(|(t, g)| {
            h.runtime
                .lease
                .as_ref()
                .is_none_or(|l| l.tab != t || l.generation != g)
        }) {
            return None;
        }
        h.bridge.lock().ok().map(|mut b| b.close());
        h.runtime.lease.take()
    });
    if let Some(l) = lease {
        if let Some(w) = crate::get_tab_webview(app, l.tab) {
            let _ = w.eval(&format!(
                "window.__GPT_WRAPPER_MIC_ROUTE__?.revokeDictationLease({});",
                serde_json::to_string(&l.route).unwrap()
            ));
        }
    }
}
pub fn fault(app: &AppHandle, reason: &str) {
    if let Ok(mut h) = app.state::<super::Devices>().host.lock() {
        if let Some(live) = h.runtime.live.as_ref().filter(|l| l.held) {
            h.interruption = Some(live.identity.device.clone());
        }
        h.runtime.invalidate += 1;
        h.runtime.cancelled = true;
        h.sessions.clear();
        h.runtime.route_error = Some(reason.into());
        h.bridge.lock().ok().map(|mut b| b.close());
    }
}

pub fn send_allowed(app: &AppHandle, tab: usize, generation: usize) -> bool {
    let state = app.state::<super::Devices>();
    let Ok(h) = state.host.lock() else {
        return false;
    };
    !h.runtime.cancelled
        && h.runtime.lease.as_ref().is_some_and(|l| {
            l.tab == tab
                && l.generation == generation
                && l.capture
                && l.started
                && h.runtime
                    .live
                    .as_ref()
                    .is_some_and(|s| s.identity == l.identity && !s.suppress)
                && h.runtime.selected.as_ref() == Some(&l.identity.device)
                && h.sessions.get(&l.identity.device)
                    == Some(&(l.identity.dn.clone(), l.identity.hn.clone()))
                && super::workstation::ready()
                && h.bridge.lock().is_ok_and(|b| {
                    b.ready() && b.control_seen.elapsed() < std::time::Duration::from_secs(2)
                })
        })
}

pub fn tab_invalidated(app: &AppHandle, tab: usize) {
    let active = app
        .state::<super::Devices>()
        .host
        .lock()
        .is_ok_and(|h| h.runtime.lease.as_ref().is_some_and(|l| l.tab == tab));
    if active {
        fault(app, "target_tab_unavailable");
        revoke_route(app, None);
    }
}

// Accessed by host and callback without any WebView/engine lock dependency.
pub fn close_bridge(bridge: &Arc<Mutex<Bridge>>) {
    if let Ok(mut b) = bridge.lock() {
        b.close();
    }
}
