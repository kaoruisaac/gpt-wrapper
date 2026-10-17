use super::{
    audio::{Bridge, Codec},
    control::{Controls, Frame, Kind},
    runtime::{self, Identity, Input, Live, Runtime},
    store::{PairState, Record},
    wire::*,
};
use std::{
    collections::HashMap,
    net::{SocketAddr, UdpSocket},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use tauri::AppHandle;

#[derive(Clone)]
pub struct Evidence {
    pub pair_id: String,
    pub dn: String,
    pub hn: String,
    pub phase: String,
    pub wifi_only: bool,
    pub seen: Instant,
}
#[derive(Default)]
pub struct HostState {
    pub interruption: Option<String>,
    pub retry: bool,
    pub records: HashMap<String, Record>,
    pub evidence: HashMap<String, Evidence>,
    pub error: Option<String>,
    pub ready: bool,
    pub bridge: Arc<Mutex<Bridge>>,
    pub runtime: Runtime,
    pub sessions: HashMap<String, (String, String)>,
}
struct Peer {
    record: Record,
    endpoint: SocketAddr,
    dn: String,
    hn: String,
    authenticated: bool,
    phase: String,
    wifi_only: bool,
    seq: u32,
    last_state: Instant,
    last_g: Instant,
    hseq: u32,
    began: Instant,
    controls: Controls,
    codec: Option<Codec>,
    invalid_frames: u64,
    bad_tags: u64,
}
pub fn start(shared: Arc<Mutex<HostState>>, stop: Arc<AtomicBool>, app: AppHandle) {
    thread::spawn(move || {
        let mut delay = 5;
        let mut failures = 0;
        while !stop.load(Ordering::SeqCst) {
            let began = Instant::now();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(&shared, &stop, &app)
            }))
            .unwrap_or_else(|_| {
                // A panic while holding a mutex must not permanently disable Retry.
                // Recover only after discarding all transient authorization and audio.
                let mut h = shared.lock().unwrap_or_else(|p| p.into_inner());
                h.ready = false;
                h.runtime.cancelled = true;
                h.runtime.invalidate += 1;
                h.sessions.clear();
                h.evidence.clear();
                let mut b = h.bridge.lock().unwrap_or_else(|p| p.into_inner());
                b.close();
                b.status = "Unavailable: receiver_panicked; recheck audio".into();
                drop(b);
                h.bridge.clear_poison();
                drop(h);
                shared.clear_poison();
                Err("worker_failed".into())
            });
            cancel(&shared, &app, !stop.load(Ordering::SeqCst));
            if let Ok(mut s) = shared.lock() {
                s.ready = false;
                s.evidence.clear();
                s.error = result.err();
            }
            if began.elapsed() > Duration::from_secs(10) {
                delay = 5;
                failures = 0;
            }
            failures += 1;
            let manual = failures >= 3;
            if manual {
                eprintln!("[esp32-receiver] automatic retries paused after three failures; retry in Devices");
                if let Ok(mut h) = shared.lock() {
                    let reason = h.error.as_deref().unwrap_or("worker_failed");
                    h.error = Some(format!(
                        "Receiver Error: {reason}; automatic retries paused, retry manually"
                    ));
                }
            }
            let mut ticks = 0;
            loop {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                thread::sleep(Duration::from_millis(100));
                if shared
                    .lock()
                    .is_ok_and(|mut h| std::mem::take(&mut h.retry))
                {
                    failures = 0;
                    delay = 5;
                    break;
                }
                ticks += 1;
                if !manual && ticks >= delay * 10 {
                    break;
                }
            }
            delay = (delay + 5).min(30);
        }
    });
}
fn bind(port: u16) -> Result<UdpSocket, String> {
    let s = UdpSocket::bind(("0.0.0.0", port)).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            "port_in_use"
        } else {
            "bind_failed"
        }
    })?;
    s.set_nonblocking(true).map_err(|_| "bind_failed")?;
    Ok(s)
}
fn run(shared: &Arc<Mutex<HostState>>, stop: &AtomicBool, app: &AppHandle) -> Result<(), String> {
    let control = bind(3335)?;
    let audio = bind(3333)?;
    shared.lock().map_err(|_| "host_lock")?.ready = true;
    let mut peers = HashMap::<String, Peer>::new();
    let mut buf = [0; 2049];
    let mut revision = shared.lock().map_err(|_| "host_lock")?.runtime.invalidate;
    let mut selected = shared
        .lock()
        .map_err(|_| "host_lock")?
        .runtime
        .selected
        .clone();
    let mut os_generation = super::workstation::generation();
    let mut was_unlocked = super::workstation::ready();
    let mut was_audio = shared
        .lock()
        .map_err(|_| "host_lock")?
        .bridge
        .lock()
        .map_err(|_| "audio_lock")?
        .ready();
    while !stop.load(Ordering::SeqCst) {
        let unlocked = super::workstation::ready();
        let (changed, audio_ready) = {
            let h = shared.lock().map_err(|_| "host_lock")?;
            let ready = h.bridge.lock().map_err(|_| "audio_lock")?.ready();
            (
                super::workstation::generation() != os_generation
                    || h.runtime.invalidate != revision
                    || h.runtime.selected != selected
                    || (was_unlocked && !unlocked)
                    || (was_audio && !ready)
                    || h.bridge.lock().is_ok_and(|b| b.audio_stalled()),
                ready,
            )
        };
        if changed {
            if let Ok(h) = shared.lock() {
                eprintln!("[esp32-receiver] invalidate os={} revision={} selection={} locked={} audio_lost={} audio_stalled={} route_fault={}",
                    super::workstation::generation() != os_generation,
                    h.runtime.invalidate != revision, h.runtime.selected != selected,
                    was_unlocked && !unlocked, was_audio && !audio_ready,
                    h.bridge.lock().is_ok_and(|b| b.audio_stalled()),
                    h.runtime.route_error.as_deref().unwrap_or("none"));
            }
            if let Ok(mut h) = shared.lock() {
                let stalled = h.bridge.lock().is_ok_and(|b| b.audio_stalled());
                if stalled {
                    h.runtime.route_error =
                        Some("audio_stalled: release PTT and retry; check device/PDM/LAN".into());
                }
            }
            revoke_permissions(&control, &mut peers);
            let notify = super::workstation::generation() != os_generation
                || (was_unlocked && !unlocked)
                || (was_audio && !audio_ready)
                || shared.lock().is_ok_and(|h| h.runtime.route_error.is_some());
            cancel(shared, app, notify);
            peers.clear();
            let h = shared.lock().map_err(|_| "host_lock")?;
            revision = h.runtime.invalidate;
            selected = h.runtime.selected.clone();
        }
        os_generation = super::workstation::generation();
        was_unlocked = unlocked;
        was_audio = audio_ready;
        let records = shared.lock().map_err(|_| "host_lock")?.records.clone();
        let active_before = shared
            .lock()
            .map_err(|_| "host_lock")?
            .runtime
            .live
            .as_ref()
            .map(|s| s.identity.device.clone());
        peers.retain(|id, p| {
            let record_matches = records
                .get(id)
                .is_some_and(|r| r.pair_id == p.record.pair_id && r.key == p.record.key);
            let keep = record_matches
                && p.last_state.elapsed() < Duration::from_secs(if p.authenticated { 2 } else { 3 })
                && p.began.elapsed() < Duration::from_secs(86400);
            if !keep {
                eprintln!("[esp32-receiver] peer_expired record_changed={} authenticated={} control_age_ms={} session_age_ms={} seq={} invalid_frames={} bad_tags={}",
                    !record_matches, p.authenticated, p.last_state.elapsed().as_millis(),
                    p.began.elapsed().as_millis(), p.seq, p.invalid_frames, p.bad_tags);
            }
            keep
        });
        if active_before
            .as_ref()
            .is_some_and(|id| !peers.contains_key(id))
        {
            cancel(shared, app, true);
        }
        shared
            .lock()
            .map_err(|_| "host_lock")?
            .sessions
            .retain(|id, nonces| {
                peers
                    .get(id)
                    .is_some_and(|p| p.dn == nonces.0 && p.hn == nonces.1)
            });
        for _ in 0..64 {
            let (n, source) = match control.recv_from(&mut buf) {
                Ok(x) => x,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => return Err("receiver_failed".into()),
            };
            if source.port() != 3334 || !source.is_ipv4() || n > 512 {
                continue;
            }
            let packet = match std::str::from_utf8(&buf[..n]) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let f: Vec<_> = packet.split('|').collect();
            if f.len() == 4 && f[0] == "P6" && f[1] == "B" {
                let Some(record) = records.get(f[2]) else {
                    continue;
                };
                if unhex(f[3], 8).is_err() || record.state == PairState::Quarantined {
                    continue;
                }
                // An untrusted beacon cannot replace any active or in-flight peer.
                if !peers.contains_key(f[2]) {
                    peers.insert(
                        f[2].into(),
                        Peer {
                            record: record.clone(),
                            endpoint: source,
                            dn: f[3].into(),
                            hn: hex(&random::<8>()),
                            authenticated: false,
                            phase: String::new(),
                            wifi_only: false,
                            seq: 0,
                            last_state: Instant::now(),
                            last_g: Instant::now(),
                            hseq: 0,
                            began: Instant::now(),
                            controls: Controls::default(),
                            codec: None,
                            invalid_frames: 0,
                            bad_tags: 0,
                        },
                    );
                }
                if let Some(p) = peers
                    .get(f[2])
                    .filter(|p| p.endpoint == source && p.dn == f[3] && !p.authenticated)
                {
                    let _ = control.send_to(
                        sign(
                            &p.record.key,
                            &format!("P6|H|{}|{}|{}", p.record.device, p.dn, p.hn),
                        )
                        .as_bytes(),
                        source,
                    );
                }
                continue;
            }
            if f.len() < 6 {
                continue;
            }
            let Some(p) = peers.get_mut(f[2]) else {
                continue;
            };
            if source != p.endpoint || f[3] != p.dn || f[4] != p.hn {
                continue;
            }
            let Some(fields) = verified(&p.record.key, packet) else {
                p.bad_tags = p.bad_tags.saturating_add(1);
                continue;
            };
            match (fields[0], fields[1], fields.len()) {
                ("P6", "P", 5) => {
                    if !p.authenticated {
                        p.codec = Codec::new(&p.record.key, &p.record.device, &p.dn, &p.hn);
                    }
                    p.authenticated = true;
                    let _ = control.send_to(
                        sign(
                            &p.record.key,
                            &format!("P6|A|{}|{}|{}", p.record.device, p.dn, p.hn),
                        )
                        .as_bytes(),
                        source,
                    );
                }
                ("P6", "O", 6) if p.authenticated => {
                    if fields[5] == "W" {
                        p.wifi_only = true;
                    } else if fields[5] == "B" {
                        p.wifi_only = false;
                        let mut h = shared.lock().map_err(|_| "host_lock")?;
                        if h.runtime
                            .live
                            .as_ref()
                            .is_some_and(|l| l.identity.device == p.record.device)
                        {
                            h.runtime.invalidate += 1;
                            h.runtime.cancelled = true;
                            h.sessions.clear();
                            runtime::close_bridge(&h.bridge);
                        }
                    } else {
                        continue;
                    }
                }
                ("X1", "V", 11) if p.authenticated => {
                    if fields[5] != p.record.pair_id
                        || fields[6] != "1"
                        || fields[7] != "1"
                        || fields[8] != "3335"
                        || fields[9] != "3333"
                        || !matches!(
                            fields[10],
                            "CANDIDATE" | "PREPARED" | "WAIT_FINAL" | "FINALIZED" | "PAIRED"
                        )
                    {
                        p.phase.clear();
                        let mut h = shared.lock().map_err(|_| "host_lock")?;
                        if h.runtime.selected.as_deref() == Some(&p.record.device) {
                            h.runtime.route_error=Some("E21: incompatible app/firmware protocol; update the incompatible endpoint".into());
                            h.runtime.invalidate += 1;
                            h.runtime.cancelled = true;
                            h.sessions.clear();
                            runtime::close_bridge(&h.bridge);
                        }
                        continue;
                    }
                    if p.record.state == PairState::Paired
                        && !matches!(fields[10], "PAIRED" | "FINALIZED")
                    {
                        continue;
                    }
                    p.phase = fields[10].into();
                    let mut echo = fields.clone();
                    echo[1] = "R";
                    let _ =
                        control.send_to(sign(&p.record.key, &echo.join("|")).as_bytes(), source);
                }
                ("X1", "E", 14) if p.authenticated && !p.phase.is_empty() => {
                    let Some(frame) = Frame::parse(&fields) else {
                        p.invalid_frames = p.invalid_frames.saturating_add(1);
                        continue;
                    };
                    let runtime_allowed = records
                        .get(&p.record.device)
                        .is_some_and(|r| r.state == PairState::Paired)
                        && p.wifi_only
                        && matches!(p.phase.as_str(), "FINALIZED" | "PAIRED");
                    if !runtime_allowed && !frame.idle_management() {
                        continue;
                    }
                    let mut echo = fields.clone();
                    echo[1] = "A";
                    let _ =
                        control.send_to(sign(&p.record.key, &echo.join("|")).as_bytes(), source);
                    if frame.seq > p.seq {
                        p.seq = frame.seq;
                        p.last_state = Instant::now();
                    }
                    let allowed = runtime_allowed
                        && unlocked
                        && selected.as_deref() == Some(&p.record.device);
                    let kinds = p.controls.apply(&frame, allowed && audio_ready, allowed);
                    if runtime_allowed {
                        shared
                            .lock()
                            .map_err(|_| "host_lock")?
                            .sessions
                            .insert(p.record.device.clone(), (p.dn.clone(), p.hn.clone()));
                    }
                    for kind in kinds {
                        let identity = Identity {
                            device: p.record.device.clone(),
                            pair_id: p.record.pair_id.clone(),
                            dn: p.dn.clone(),
                            hn: p.hn.clone(),
                            press: frame.press,
                        };
                        let mut h = shared.lock().map_err(|_| "host_lock")?;
                        if kind == Kind::Down {
                            h.runtime.cancelled = false;
                            h.runtime.route_error = None;
                            h.runtime.live = Some(Live {
                                identity: identity.clone(),
                                segment: frame.segment,
                                held: true,
                                paused: false,
                                suppress: false,
                            });
                        }
                        let mut event_identity = identity;
                        if let Some(l) = h.runtime.live.as_mut() {
                            if l.identity.device == p.record.device {
                                event_identity = l.identity.clone();
                                l.suppress |= frame.suppress;
                                l.segment = frame.segment;
                                if matches!(kind, Kind::Up | Kind::FinishNoSend | Kind::Cancel) {
                                    l.held = false;
                                }
                                l.paused = frame.mode == "COMBO";
                            }
                        }
                        if kind != Kind::Submit {
                            runtime::close_bridge(&h.bridge);
                        }
                        drop(h);
                        runtime::dispatch(
                            app,
                            Input {
                                identity: event_identity,
                                seq: frame.seq,
                                segment: frame.segment,
                                kind,
                                suppress: frame.suppress,
                            },
                        );
                    }
                }
                _ => {}
            }
        }
        let mut evidence = HashMap::new();
        for (id, p) in &mut peers {
            if !p.authenticated || p.phase.is_empty() {
                continue;
            }
            evidence.insert(
                id.clone(),
                Evidence {
                    pair_id: p.record.pair_id.clone(),
                    dn: p.dn.clone(),
                    hn: p.hn.clone(),
                    phase: p.phase.clone(),
                    wifi_only: p.wifi_only,
                    seen: p.last_state,
                },
            );
            if p.last_g.elapsed() >= Duration::from_millis(400) {
                p.hseq += 1;
                p.last_g = Instant::now();
                let h = shared.lock().map_err(|_| "host_lock")?;
                let paired = records
                    .get(id)
                    .is_some_and(|r| r.state == PairState::Paired)
                    && p.wifi_only
                    && matches!(p.phase.as_str(), "PAIRED" | "FINALIZED");
                let submit_ready = paired
                    && unlocked
                    && h.runtime.invalidate == revision
                    && h.runtime.selected.as_deref() == Some(id);
                let dictation_ready = submit_ready && audio_ready;
                let live = h.runtime.live.as_ref().filter(|l| {
                    l.identity.device == *id && l.identity.dn == p.dn && l.identity.hn == p.hn
                });
                let (press, segment) = p
                    .controls
                    .latest
                    .as_ref()
                    .map(|f| (f.press, f.segment))
                    .unwrap_or((0, 0));
                let permit = dictation_ready
                    && live.is_some_and(|l| {
                        l.held && !l.paused && l.segment == segment && l.identity.press == press
                    })
                    && h.runtime.lease.as_ref().is_some_and(|l| {
                        live.is_some_and(|s| s.identity == l.identity)
                            && l.applied
                            && l.capture
                            && l.started
                    });
                let g = format!(
                    "X1|G|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
                    id,
                    p.dn,
                    p.hn,
                    p.hseq,
                    p.seq,
                    press,
                    segment,
                    u8::from(dictation_ready),
                    u8::from(submit_ready),
                    u8::from(permit)
                );
                let _ = control.send_to(sign(&p.record.key, &g).as_bytes(), p.endpoint);
                if selected.as_deref() == Some(id) {
                    let token = live.map(|l| l.identity.token()).unwrap_or_default();
                    h.bridge.lock().map_err(|_| "audio_lock")?.permission(
                        &token,
                        segment,
                        permit,
                        p.last_state,
                    );
                }
            }
        }
        {
            let mut s = shared.lock().map_err(|_| "host_lock")?;
            s.evidence = evidence;
            s.error = None;
        }
        // Both the authenticated receive path and the playback callback check Gate.
        for _ in 0..64 {
            let (n, source) = match audio.recv_from(&mut buf) {
                Ok(v) => v,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => return Err("audio_socket_failed".into()),
            };
            if n != 688 || source.port() != 3336 {
                continue;
            }
            let Some(p) = peers.values_mut().find(|p| {
                p.endpoint.ip() == source.ip() && selected.as_deref() == Some(&p.record.device)
            }) else {
                continue;
            };
            let h = shared.lock().map_err(|_| "host_lock")?;
            if h.runtime.invalidate != revision || h.runtime.selected != selected {
                runtime::close_bridge(&h.bridge);
                continue;
            }
            if !p.authenticated
                || !p.wifi_only
                || !matches!(p.phase.as_str(), "PAIRED" | "FINALIZED")
                || !unlocked
                || !records
                    .get(&p.record.device)
                    .is_some_and(|r| r.state == PairState::Paired)
            {
                runtime::close_bridge(&h.bridge);
                continue;
            }
            if p.last_state.elapsed() >= Duration::from_secs(2) {
                runtime::close_bridge(&h.bridge);
                continue;
            }
            if let Some(codec) = p.codec.as_mut() {
                h.bridge
                    .lock()
                    .map_err(|_| "audio_lock")?
                    .receive(codec, &buf[..n]);
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    revoke_permissions(&control, &mut peers);
    Ok(())
}
fn revoke_permissions(socket: &UdpSocket, peers: &mut HashMap<String, Peer>) {
    for (id, p) in peers {
        if !p.authenticated || p.hseq >= 0xffff_ffef {
            continue;
        }
        p.hseq += 1;
        let (press, segment) = p
            .controls
            .latest
            .as_ref()
            .map(|f| (f.press, f.segment))
            .unwrap_or((0, 0));
        let base = format!(
            "X1|G|{id}|{}|{}|{}|{}|{press}|{segment}|0|0|0",
            p.dn, p.hn, p.hseq, p.seq
        );
        let _ = socket.send_to(sign(&p.record.key, &base).as_bytes(), p.endpoint);
    }
}

fn cancel(shared: &Arc<Mutex<HostState>>, app: &AppHandle, notify: bool) {
    if let Ok(mut h) = shared.lock() {
        if notify {
            if let Some(live) = h.runtime.live.as_ref().filter(|l| l.held) {
                h.interruption = Some(live.identity.device.clone());
            }
        }
        runtime::close_bridge(&h.bridge);
        let identity = h.runtime.lease.as_ref().map(|l| l.identity.clone());
        h.runtime.live = None;
        h.sessions.clear();
        h.evidence.clear();
        h.runtime.cancelled = true;
        drop(h);
        if let Some(identity) = identity {
            runtime::dispatch(
                app,
                Input {
                    identity,
                    seq: 0,
                    segment: 0,
                    kind: Kind::Cancel,
                    suppress: true,
                },
            );
        }
    }
}
