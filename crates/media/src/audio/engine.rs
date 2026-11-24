//! Voice engine: capture → RNNoise → gate → Opus → send, and
//! receive → jitter buffer → Opus → mix → playback.
//!
//! Threads:
//! * `voice-io`: owns the cpal streams (not `Send` on all platforms).
//! * `voice-tx`: resampling, noise suppression, gating and encoding.
//! * `voice-rx`: per-speaker jitter buffers, decoding and mixing; paced by
//!   the playback device draining the output ring.

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use bytes::Bytes;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use parking_lot::Mutex;

use super::{
    DENOISE_FRAME, FRAME_SAMPLES, SAMPLE_RATE,
    codec::{VoiceDecoder, VoiceEncoder},
    denoise::Denoiser,
    device,
    jitter::{JitterBuffer, Playout},
    level_dbfs, mixer,
    resample::Resampler,
    vad::{Gate, GateMode},
};
use crate::{MediaError, MediaResult};

/// Playback buffering target (device side), in ms.
const OUTPUT_TARGET_MS: u32 = 40;
/// Drop queued playback audio beyond this to bound latency.
const OUTPUT_MAX_MS: u32 = 200;
/// Forget speakers that have been silent this long.
const SPEAKER_IDLE: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq)]
pub struct VoiceConfig {
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub noise_suppression: bool,
    pub gate: GateMode,
    pub muted: bool,
    pub deafened: bool,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            input_device: None,
            output_device: None,
            noise_suppression: true,
            gate: GateMode::VoiceActivity {
                threshold_db: -50.0,
            },
            muted: false,
            deafened: false,
        }
    }
}

/// Sends one Opus packet: `(seq, timestamp, payload)`.
pub type SendPacket = Box<dyn Fn(u16, u32, &[u8]) + Send>;

pub struct VoiceCallbacks {
    pub send: SendPacket,
    /// Local transmit state changed (for the speaking indicator).
    pub local_speaking: Box<dyn Fn(bool) + Send>,
}

enum RxMsg {
    Packet {
        sender: u32,
        seq: u16,
        timestamp: u32,
        payload: Bytes,
    },
    Remove(u32),
}

pub struct VoiceEngine {
    config: Arc<Mutex<VoiceConfig>>,
    stop: Arc<AtomicBool>,
    rx_tx: Sender<RxMsg>,
}

impl Drop for VoiceEngine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl VoiceEngine {
    pub fn start(config: VoiceConfig, cb: VoiceCallbacks) -> MediaResult<Self> {
        let config = Arc::new(Mutex::new(config));
        let stop = Arc::new(AtomicBool::new(false));
        let out_ring: Arc<Mutex<VecDeque<f32>>> = Arc::default();
        let (in_tx, in_rx) = crossbeam_channel::bounded::<Vec<f32>>(64);
        let (rx_tx, rx_rx) = crossbeam_channel::bounded::<RxMsg>(1024);

        // Device thread: open streams and report the negotiated rates.
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<MediaResult<(u32, u32)>>();
        {
            let (input_dev, output_dev) = {
                let c = config.lock();
                (c.input_device.clone(), c.output_device.clone())
            };
            let stop = stop.clone();
            let out_ring = out_ring.clone();
            spawn("voice-io", move || {
                let input = device::open_input(input_dev.as_deref(), move |samples| {
                    let _ = in_tx.try_send(samples.to_vec());
                });
                let output = device::open_output(output_dev.as_deref(), move |out| {
                    let mut ring = out_ring.lock();
                    for s in out.iter_mut() {
                        *s = ring.pop_front().unwrap_or(0.0);
                    }
                });
                let (input, output) = match (input, output) {
                    (Ok(i), Ok(o)) => (i, o),
                    (Err(e), _) | (_, Err(e)) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                use cpal::traits::StreamTrait;
                if let Err(e) = input.stream.play().and(output.stream.play()) {
                    let _ = ready_tx.send(Err(MediaError::Device(e.to_string())));
                    return;
                }
                let _ = ready_tx.send(Ok((input.sample_rate, output.sample_rate)));
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(100));
                }
                drop(input);
                drop(output);
            })?;
        }
        let (in_rate, out_rate) = ready_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| MediaError::Device("audio devices did not start".into()))??;
        tracing::info!(in_rate, out_rate, "voice engine started");

        {
            let config = config.clone();
            let stop = stop.clone();
            spawn("voice-tx", move || {
                tx_loop(&in_rx, in_rate, &config, &cb, &stop)
            })?;
        }
        {
            let config = config.clone();
            let stop = stop.clone();
            spawn("voice-rx", move || {
                rx_loop(&rx_rx, out_rate, &out_ring, &config, &stop)
            })?;
        }
        Ok(Self {
            config,
            stop,
            rx_tx,
        })
    }

    pub fn config(&self) -> VoiceConfig {
        self.config.lock().clone()
    }

    /// Updates gate/mute/deafen/noise suppression live. Device changes require
    /// a restart (the caller compares configs).
    pub fn set_config(&self, c: VoiceConfig) {
        *self.config.lock() = c;
    }

    /// Feeds a received voice packet.
    pub fn push_packet(&self, sender: u32, seq: u16, timestamp: u32, payload: Bytes) {
        let _ = self.rx_tx.try_send(RxMsg::Packet {
            sender,
            seq,
            timestamp,
            payload,
        });
    }

    pub fn remove_speaker(&self, sender: u32) {
        let _ = self.rx_tx.try_send(RxMsg::Remove(sender));
    }
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> MediaResult<()> {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(f)
        .map(|_| ())
        .map_err(|e| MediaError::Device(e.to_string()))
}

fn tx_loop(
    input: &Receiver<Vec<f32>>,
    in_rate: u32,
    config: &Mutex<VoiceConfig>,
    cb: &VoiceCallbacks,
    stop: &AtomicBool,
) {
    let mut encoder = match VoiceEncoder::new() {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(error = %e, "opus encoder");
            return;
        }
    };
    let mut resampler = Resampler::new(in_rate, SAMPLE_RATE);
    let mut denoiser = Denoiser::new();
    let mut gate = Gate::new(GateMode::VoiceActivity {
        threshold_db: -50.0,
    });
    let mut pending: Vec<f32> = Vec::with_capacity(FRAME_SAMPLES * 4);
    let mut frame = vec![0.0f32; FRAME_SAMPLES];
    let mut seq: u16 = 0;
    let mut timestamp: u32 = 0;
    let mut speaking = false;

    while !stop.load(Ordering::Relaxed) {
        let chunk = match input.recv_timeout(Duration::from_millis(100)) {
            Ok(c) => c,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        resampler.process(&chunk, &mut pending);
        while pending.len() >= FRAME_SAMPLES {
            frame.copy_from_slice(&pending[..FRAME_SAMPLES]);
            pending.drain(..FRAME_SAMPLES);

            let cfg = config.lock().clone();
            gate.mode = cfg.gate;
            gate.muted = cfg.muted || cfg.deafened;
            let voice_prob = if cfg.noise_suppression {
                let (a, b) = frame.split_at_mut(DENOISE_FRAME);
                denoiser.process(a).max(denoiser.process(b))
            } else {
                // Without RNNoise, rely on level only.
                1.0
            };
            let transmit = gate.transmit(level_dbfs(&frame), voice_prob);
            if transmit {
                match encoder.encode(&frame) {
                    Ok(pkt) => (cb.send)(seq, timestamp, pkt),
                    Err(e) => tracing::warn!(error = %e, "opus encode"),
                }
                seq = seq.wrapping_add(1);
            }
            timestamp = timestamp.wrapping_add(FRAME_SAMPLES as u32);
            if transmit != speaking {
                speaking = transmit;
                (cb.local_speaking)(speaking);
            }
        }
    }
    if speaking {
        (cb.local_speaking)(false);
    }
}

struct Speaker {
    jitter: JitterBuffer,
    decoder: VoiceDecoder,
    last_packet: Instant,
}

fn rx_loop(
    msgs: &Receiver<RxMsg>,
    out_rate: u32,
    out_ring: &Mutex<VecDeque<f32>>,
    config: &Mutex<VoiceConfig>,
    stop: &AtomicBool,
) {
    let mut speakers: HashMap<u32, Speaker> = HashMap::new();
    let mut resampler = Resampler::new(SAMPLE_RATE, out_rate);
    let mut mix = vec![0.0f32; FRAME_SAMPLES];
    let mut tmp = vec![0.0f32; FRAME_SAMPLES];
    let mut resampled = Vec::with_capacity(FRAME_SAMPLES * 2);
    let target = (out_rate * OUTPUT_TARGET_MS / 1000) as usize;
    let max = (out_rate * OUTPUT_MAX_MS / 1000) as usize;

    while !stop.load(Ordering::Relaxed) {
        match msgs.recv_timeout(Duration::from_millis(5)) {
            Ok(m) => handle_rx(&mut speakers, m),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        while let Ok(m) = msgs.try_recv() {
            handle_rx(&mut speakers, m);
        }

        // Produce 20 ms frames until the device has enough queued.
        while out_ring.lock().len() < target {
            mix.fill(0.0);
            for sp in speakers.values_mut() {
                let ok = match sp.jitter.pop() {
                    Playout::Decode(p) => sp.decoder.decode(&p, &mut tmp).is_ok(),
                    Playout::Fec(p) => sp.decoder.decode_fec(&p, &mut tmp).is_ok(),
                    Playout::Conceal => sp.decoder.conceal(&mut tmp).is_ok(),
                    Playout::Silence => false,
                };
                if ok {
                    mixer::accumulate(&mut mix, &tmp);
                }
            }
            if config.lock().deafened {
                mix.fill(0.0);
            }
            mixer::soft_clip(&mut mix);
            resampled.clear();
            resampler.process(&mix, &mut resampled);
            let mut ring = out_ring.lock();
            ring.extend(resampled.iter().copied());
            let excess = ring.len().saturating_sub(max);
            ring.drain(..excess);
        }
        speakers.retain(|_, s| s.last_packet.elapsed() < SPEAKER_IDLE);
    }
}

fn handle_rx(speakers: &mut HashMap<u32, Speaker>, m: RxMsg) {
    match m {
        RxMsg::Packet {
            sender,
            seq,
            timestamp,
            payload,
        } => {
            let sp = match speakers.entry(sender) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(v) => match VoiceDecoder::new() {
                    Ok(decoder) => v.insert(Speaker {
                        jitter: JitterBuffer::new(),
                        decoder,
                        last_packet: Instant::now(),
                    }),
                    Err(e) => {
                        tracing::warn!(error = %e, "opus decoder");
                        return;
                    }
                },
            };
            sp.last_packet = Instant::now();
            sp.jitter.push(seq, timestamp, payload);
        }
        RxMsg::Remove(id) => {
            speakers.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    /// Needs real audio devices: `cargo test -p qchat-media -- --ignored engine`.
    /// Starts the engine, loops our own packets back as a remote speaker, and
    /// checks the pipeline runs without errors.
    #[test]
    #[ignore = "requires audio hardware"]
    fn engine_starts_on_real_devices() {
        eprintln!("inputs: {:?}", device::input_device_names());
        eprintln!("outputs: {:?}", device::output_device_names());
        let sent = Arc::new(AtomicUsize::new(0));
        let s2 = sent.clone();
        let (tx, rx) = crossbeam_channel::unbounded::<(u16, u32, Vec<u8>)>();
        let engine = VoiceEngine::start(
            VoiceConfig {
                // Always transmit so packets flow even in a silent room.
                gate: GateMode::PushToTalk { pressed: true },
                ..VoiceConfig::default()
            },
            VoiceCallbacks {
                send: Box::new(move |seq, ts, p| {
                    s2.fetch_add(1, Ordering::Relaxed);
                    let _ = tx.send((seq, ts, p.to_vec()));
                }),
                local_speaking: Box::new(|_| {}),
            },
        )
        .expect("engine start");
        let until = Instant::now() + Duration::from_secs(2);
        while Instant::now() < until {
            if let Ok((seq, ts, p)) = rx.recv_timeout(Duration::from_millis(50)) {
                engine.push_packet(42, seq, ts, Bytes::from(p));
            }
        }
        let n = sent.load(Ordering::Relaxed);
        eprintln!("encoded {n} packets in 2 s");
        assert!(n >= 80, "expected ~100 packets (50/s), got {n}");
    }
}
