//! P8AU and a bounded, fail-closed WASAPI bridge. No PCM is persisted or logged.
use super::{control::Replay, wire::unhex};
use aes_gcm::{
    aead::{Aead, Payload},
    Aes256Gcm, KeyInit, Nonce,
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use hkdf::Hkdf;
use sha2::Sha256;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use zeroize::Zeroize;

pub struct Codec {
    cipher: Aes256Gcm,
    material: [u8; 44],
    device: Vec<u8>,
    replay: Replay,
}
impl Drop for Codec {
    fn drop(&mut self) {
        self.material.zeroize();
    }
}
impl Codec {
    pub fn new(key: &[u8], device: &str, dn: &str, hn: &str) -> Option<Self> {
        let context = unhex(&format!("{}{dn}{hn}", device.to_ascii_lowercase()), 22).ok()?;
        let mut material = [0; 44];
        Hkdf::<Sha256>::new(Some(&context), key)
            .expand(b"GPTW-P8-AUDIO-v1", &mut material)
            .ok()?;
        Some(Self {
            cipher: Aes256Gcm::new_from_slice(&material[..32]).ok()?,
            material,
            device: context[..6].to_vec(),
            replay: Replay::default(),
        })
    }
    pub fn decode(&mut self, packet: &[u8], segment: u32) -> Option<(u64, [f32; 320])> {
        if packet.len() != 688
            || &packet[..6] != b"P8AU\x01\x01"
            || packet[6..12] != self.device
            || packet[12..20] != self.material[36..44]
            || u32::from_be_bytes(packet[28..32].try_into().ok()?) != segment
            || segment == 0
        {
            return None;
        }
        let seq = u64::from_be_bytes(packet[20..28].try_into().ok()?);
        if seq >= 1u64 << 32 {
            return None;
        }
        let mut iv = [0; 12];
        iv[..4].copy_from_slice(&self.material[32..36]);
        iv[4..].copy_from_slice(&packet[20..28]);
        let mut plain = self
            .cipher
            .decrypt(
                Nonce::from_slice(&iv),
                Payload {
                    msg: &packet[32..],
                    aad: &packet[..32],
                },
            )
            .ok()?;
        // Authentication must precede replay mutation, including invalid-tag packets.
        if !self.replay.accept(seq) {
            plain.zeroize();
            return None;
        }
        let mut pcm = [0.; 320];
        for (o, b) in pcm.iter_mut().zip(plain.chunks_exact(2)) {
            *o = i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.;
        }
        plain.zeroize();
        Some((seq, pcm))
    }
}
pub struct Bridge {
    pub status: String,
    pub recheck: bool,
    pub token: String,
    pub segment: u32,
    pub gate: bool,
    pub control_seen: Instant,
    os_epoch: u64,
    gate_opened: Instant,
    queue: BTreeMap<u64, [f32; 320]>,
    samples: VecDeque<f32>,
    expected: Option<u64>,
    last_audio: Instant,
    callback_at: Instant,
    target: usize,
    next_target: usize,
    limiter: f32,
    pub valid: u64,
    pub rejected: u64,
    pub underruns: u64,
    pub max_queue: usize,
    pub peak: f32,
    pub square_sum: f64,
    pub sample_count: u64,
}
impl Default for Bridge {
    fn default() -> Self {
        Self {
            status: "Starting".into(),
            recheck: false,
            token: String::new(),
            segment: 0,
            gate: false,
            control_seen: Instant::now(),
            os_epoch: 0,
            gate_opened: Instant::now(),
            queue: BTreeMap::new(),
            samples: VecDeque::with_capacity(960),
            expected: None,
            last_audio: Instant::now(),
            callback_at: Instant::now(),
            target: 4,
            next_target: 4,
            limiter: 1.,
            valid: 0,
            rejected: 0,
            underruns: 0,
            max_queue: 0,
            peak: 0.,
            square_sum: 0.,
            sample_count: 0,
        }
    }
}
impl Bridge {
    pub fn ready(&self) -> bool {
        self.status == "Ready" && self.callback_at.elapsed() < Duration::from_millis(500)
    }
    pub fn clear(&mut self) {
        self.queue.clear();
        self.samples.clear();
        self.expected = None;
        self.limiter = 1.;
    }
    pub fn close(&mut self) {
        self.gate = false;
        self.clear();
    }
    pub fn permission(&mut self, token: &str, segment: u32, permit: bool, seen: Instant) {
        if self.token != token || self.segment != segment {
            self.close();
            self.target = self.next_target;
        }
        self.token = token.into();
        self.segment = segment;
        self.control_seen = seen;
        if !permit {
            self.close();
        } else {
            if !self.gate {
                self.gate_opened = Instant::now();
                self.os_epoch = super::workstation::generation();
            }
            self.gate = self.ready();
        }
    }
    pub fn audio_stalled(&self) -> bool {
        self.gate
            && self.gate_opened.elapsed() > Duration::from_millis(700)
            && self.last_audio.elapsed() > Duration::from_millis(700)
    }
    pub fn receive(&mut self, codec: &mut Codec, packet: &[u8]) {
        if !self.gate || !self.ready() || self.control_seen.elapsed() > Duration::from_secs(2) {
            self.close();
            return;
        }
        let Some((seq, pcm)) = codec.decode(packet, self.segment) else {
            self.rejected += 1;
            self.clear();
            return;
        };
        if self.expected.is_some_and(|e| seq < e) {
            return;
        }
        if self.expected.is_some_and(|e| seq > e + 12) || self.queue.len() >= 12 {
            self.clear();
            self.next_target = (self.next_target + 1).min(10);
        }
        self.queue.insert(seq, pcm);
        self.max_queue = self.max_queue.max(self.queue.len());
        self.valid += 1;
        let gap = self.last_audio.elapsed();
        if gap > Duration::from_millis(35) && gap < Duration::from_millis(300) {
            self.next_target = (self.next_target + 1).min(10);
        }
        if self.valid % 500 == 0 && self.next_target > 2 {
            self.next_target -= 1;
        }
        self.last_audio = Instant::now();
    }
    fn next(&mut self) -> f32 {
        if !self.gate
            || self.control_seen.elapsed() > Duration::from_secs(2)
            || self.last_audio.elapsed() > Duration::from_millis(300)
        {
            self.clear();
            return 0.;
        }
        if let Some(sample) = self.samples.pop_front() {
            return sample;
        }
        if self.expected.is_none() {
            if self.queue.len() < self.target {
                return 0.;
            }
            self.expected = self.queue.first_key_value().map(|(&seq, _)| seq);
        }
        let seq = self.expected.unwrap();
        self.expected = Some(seq + 1);
        let pcm = self.queue.remove(&seq).unwrap_or_else(|| {
            self.underruns += 1;
            self.next_target = (self.next_target + 1).min(10);
            [0.; 320]
        });
        let peak = pcm.iter().fold(0f32, |p, x| p.max(x.abs() * 3.9810717));
        let wanted = if peak > 0.94 { 0.94 / peak } else { 1. };
        self.limiter = if wanted < self.limiter {
            wanted
        } else {
            wanted.min(self.limiter + 0.015)
        };
        for i in 0..320 {
            let a = pcm[i] * 3.9810717 * self.limiter;
            let b = pcm[(i + 1).min(319)] * 3.9810717 * self.limiter;
            for k in 0..3 {
                let v = (a + (b - a) * k as f32 / 3.).clamp(-0.94, 0.94);
                self.peak = self.peak.max(v.abs());
                self.square_sum += (v as f64).powi(2);
                self.sample_count += 1;
                self.samples.push_back(v);
            }
        }
        self.samples.pop_front().unwrap_or(0.)
    }
}
pub fn start(shared: Arc<Mutex<Bridge>>, stop: Arc<AtomicBool>) {
    thread::spawn(move || {
        let mut stream = None;
        let mut retry = true;
        let mut logged = Instant::now();
        let mut last_valid = 0;
        while !stop.load(Ordering::SeqCst) {
            if let Ok(mut b) = shared.lock() {
                retry |= std::mem::take(&mut b.recheck);
            }
            if retry {
                retry = false;
                stream = None;
                if let Ok(mut b) = shared.lock() {
                    b.close();
                    b.status = "Starting".into();
                }
                match open(shared.clone()) {
                    Ok(s) => stream = Some(s),
                    Err(reason) => {
                        if let Ok(mut b) = shared.lock() {
                            b.close();
                            b.status = format!("Unavailable: {reason}");
                        }
                    }
                }
            }
            if stream.is_some() {
                if let Ok(mut b) = shared.lock() {
                    if b.status == "Ready" && b.callback_at.elapsed() > Duration::from_millis(500) {
                        b.close();
                        b.status = "Unavailable: callback_stalled".into();
                    }
                }
            }
            if logged.elapsed() >= Duration::from_secs(10) {
                logged = Instant::now();
                if let Ok(b) = shared.lock() {
                    if b.valid != last_valid {
                        last_valid = b.valid;
                        let rms = if b.sample_count > 0 {
                            (b.square_sum / b.sample_count as f64).sqrt()
                        } else {
                            0.
                        };
                        eprintln!("[esp32-audio] valid={} rejected={} underruns={} max_queue={} peak={:.4} rms={:.4} limiter={:.4}",b.valid,b.rejected,b.underruns,b.max_queue,b.peak,rms,b.limiter);
                    }
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
        if let Ok(mut b) = shared.lock() {
            b.close();
            b.status = "Unavailable: stopped".into();
        }
        drop(stream);
    });
}
fn open(shared: Arc<Mutex<Bridge>>) -> Result<cpal::Stream, &'static str> {
    let host = cpal::default_host();
    let mut outputs = host
        .output_devices()
        .map_err(|_| "enumeration_failed")?
        .filter(|d| {
            d.name()
                .is_ok_and(|n| n.starts_with("CABLE Input (") && n.contains("VB-Audio"))
        });
    let device = outputs.next().ok_or("cable_input_missing")?;
    if outputs.next().is_some() {
        return Err("cable_input_ambiguous");
    }
    let inputs = host
        .input_devices()
        .map_err(|_| "enumeration_failed")?
        .filter(|d| {
            d.name()
                .is_ok_and(|n| n.starts_with("CABLE Output (") && n.contains("VB-Audio"))
        })
        .count();
    if inputs != 1 {
        return Err("cable_output_missing_or_ambiguous");
    }
    let supported = device
        .supported_output_configs()
        .map_err(|_| "format_failed")?
        .find(|c| {
            c.min_sample_rate().0 <= 48000
                && c.max_sample_rate().0 >= 48000
                && c.channels() >= 1
                && matches!(
                    c.sample_format(),
                    cpal::SampleFormat::F32 | cpal::SampleFormat::I16 | cpal::SampleFormat::U16
                )
        })
        .ok_or("48khz_unavailable")?
        .with_sample_rate(cpal::SampleRate(48000));
    let format = supported.sample_format();
    let mut config = supported.config();
    // WASAPI shared mode chooses its supported block size. DSP is always 960
    // samples per 20ms, regardless of callback slicing by the driver.
    config.buffer_size = cpal::BufferSize::Default;
    let channels = config.channels as usize;
    fn build<T: cpal::SizedSample + cpal::FromSample<f32>>(
        device: &cpal::Device,
        config: &cpal::StreamConfig,
        shared: Arc<Mutex<Bridge>>,
        channels: usize,
    ) -> Result<cpal::Stream, &'static str> {
        let failed = shared.clone();
        device
            .build_output_stream(
                config,
                move |out: &mut [T], _| {
                    for s in out.iter_mut() {
                        *s = T::from_sample(0.);
                    }
                    if let Ok(mut b) = shared.try_lock() {
                        b.callback_at = Instant::now();
                        if b.status == "Starting" {
                            b.status = "Ready".into();
                        }
                        if b.status != "Ready" {
                            b.close();
                            return;
                        }
                        if b.gate
                            && (b.os_epoch != super::workstation::generation()
                                || !super::workstation::ready())
                        {
                            b.close();
                            return;
                        }
                        for frame in out.chunks_mut(channels) {
                            let v = b.next();
                            for sample in frame {
                                *sample = T::from_sample(v);
                            }
                        }
                    }
                },
                move |_| {
                    if let Ok(mut b) = failed.lock() {
                        b.close();
                        b.status = "Unavailable: output_failed".into();
                    }
                },
                None,
            )
            .map_err(|_| "output_open_failed")
    }
    let stream = match format {
        cpal::SampleFormat::F32 => build::<f32>(&device, &config, shared, channels),
        cpal::SampleFormat::I16 => build::<i16>(&device, &config, shared, channels),
        cpal::SampleFormat::U16 => build::<u16>(&device, &config, shared, channels),
        _ => Err("format_unsupported"),
    }?;
    stream.play().map_err(|_| "output_start_failed")?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prototype_packet_and_bad_tag_cannot_poison_replay() {
        let f: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/p8au.json")).unwrap();
        let packet = unhex(f["packet"].as_str().unwrap(), 688).unwrap();
        let mut c = Codec::new(
            &(0u8..32).collect::<Vec<_>>(),
            "AABBCCDDEEFF",
            "0011223344556677",
            "8899aabbccddeeff",
        )
        .unwrap();
        assert_eq!(super::super::wire::hex(&c.material),"6d65cb0acfe465ea34a0e867459ab7c6344954fd7e28def83e183d9a729c50e319d18a4cd8ccd9ef30ecf20c");
        let mut corrupt = packet.clone();
        corrupt[687] ^= 1;
        assert!(c.decode(&corrupt, 1).is_none());
        assert!(c.decode(&packet, 2).is_none());
        let (_, samples) = c.decode(&packet, 1).unwrap();
        let expected = unhex(f["pcm"].as_str().unwrap(), 640).unwrap();
        for (sample, bytes) in samples.iter().zip(expected.chunks_exact(2)) {
            assert_eq!(
                *sample,
                i16::from_le_bytes([bytes[0], bytes[1]]) as f32 / 32768.
            );
        }
        use sha2::Digest;
        assert_eq!(
            super::super::wire::hex(&Sha256::digest(&packet)),
            f["sha256"].as_str().unwrap()
        );
        assert!(c.decode(&packet, 1).is_none());
    }
    #[test]
    #[ignore = "Opens actual Windows CABLE playback with silence; requires installed driver and unlocked workstation"]
    fn real_windows_bridge_probe() {
        let stop = Arc::new(AtomicBool::new(false));
        let bridge = Arc::new(Mutex::new(Bridge::default()));
        super::super::workstation::start(stop.clone());
        start(bridge.clone(), stop.clone());
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            thread::sleep(Duration::from_millis(50));
            let b = bridge.lock().unwrap();
            if b.ready() && super::super::workstation::ready() {
                break;
            }
            if Instant::now() >= deadline {
                let status = b.status.clone();
                stop.store(true, Ordering::SeqCst);
                panic!(
                    "bridge={status}, workstation={}",
                    super::super::workstation::ready()
                );
            }
        }
        stop.store(true, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(100));
        println!("PASS: actual CABLE Input playback callback at 48kHz, CABLE Output enumerated, Windows session listener and input desktop ready. Gate stayed closed; no microphone captured.");
    }
    #[test]
    fn gate_clears_driver_pending_samples_and_limiter_stays_bounded() {
        let mut b = Bridge::default();
        b.status = "Ready".into();
        b.gate = true;
        for i in 0..4 {
            b.queue.insert(i, [1.; 320]);
        }
        for _ in 0..960 {
            assert!(b.next().abs() <= 0.94);
        }
        b.close();
        assert_eq!(b.next(), 0.);
        assert!(b.queue.is_empty());
        assert!(b.samples.is_empty());
    }
}
