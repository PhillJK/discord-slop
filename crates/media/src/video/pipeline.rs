//! Screen-share publish and view pipelines (each on its own OS thread).

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use bytes::Bytes;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError};
use qchat_proto::{VideoHeader, media::frame_priority};

use super::{
    EncodeSettings, fit_within,
    h264::{DecodedFrame, EncodedFrame, H264Decoder, H264Encoder},
    scale::Scaler,
};
use crate::capture::CaptureStream;

/// Skip capturing new frames while this many sent frames are unacknowledged
/// (uplink congestion). Skipping *before* encoding keeps the H.264 chain intact.
const MAX_UPLINK_IN_FLIGHT: usize = 8;

/// Metadata for an encoded frame ready to send.
#[derive(Debug, Clone, Copy)]
pub struct FrameMeta {
    pub header: VideoHeader,
    pub priority: i32,
}

#[derive(Debug, Clone, Default)]
pub struct PublishStats {
    pub width: u32,
    pub height: u32,
    pub encode_fps: f32,
    pub kbps: f32,
    pub avg_encode_ms: f32,
    pub skipped: u64,
    pub warning: Option<String>,
}

pub struct PublishCallbacks {
    pub send: Box<dyn Fn(FrameMeta, Bytes) + Send>,
    pub in_flight: Box<dyn Fn() -> usize + Send>,
    pub stats: Box<dyn Fn(PublishStats) + Send>,
}

/// Running screen share. Dropping it stops capture and encoding.
pub struct Publisher {
    force_keyframe: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl Drop for Publisher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Publisher {
    pub fn start(
        capture: CaptureStream,
        settings: EncodeSettings,
        cb: PublishCallbacks,
    ) -> std::io::Result<Self> {
        let force_keyframe = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let (fk, st) = (force_keyframe.clone(), stop.clone());
        std::thread::Builder::new()
            .name("video-publish".into())
            .spawn(move || publish_loop(capture, settings, cb, &fk, &st))?;
        Ok(Self {
            force_keyframe,
            stop,
        })
    }

    pub fn request_keyframe(&self) {
        self.force_keyframe.store(true, Ordering::Relaxed);
    }
}

fn publish_loop(
    capture: CaptureStream,
    settings: EncodeSettings,
    cb: PublishCallbacks,
    force_keyframe: &AtomicBool,
    stop: &AtomicBool,
) {
    let interval = Duration::from_secs_f64(1.0 / f64::from(settings.fps.max(1)));
    let started = Instant::now();
    let mut encoder: Option<H264Encoder> = None;
    let mut next_due = Instant::now();
    let mut seq: u32 = 0;
    let (mut gop, mut idx_in_gop) = (0u32, 0u32);
    let mut stats = PublishStats::default();
    let (mut win_frames, mut win_bytes, mut win_encode) = (0u32, 0usize, Duration::ZERO);
    let mut win_start = Instant::now();

    // Stage 1 (own thread): take the newest captured frame and scale it into
    // the preset box. Runs in parallel with encoding of the previous frame.
    let scale_skipped = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let (scaled_tx, scaled_rx) = crossbeam_channel::bounded::<super::RawFrame>(1);
    // Stops the scale thread (and with it the capture) however this loop exits,
    // even if no frames arrive (static screen on a damage-driven compositor).
    struct StopOnDrop(Arc<AtomicBool>);
    impl Drop for StopOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }
    let scale_stop = StopOnDrop(Arc::new(AtomicBool::new(false)));
    {
        let skipped = scale_skipped.clone();
        let stop = scale_stop.0.clone();
        let spawned = std::thread::Builder::new()
            .name("video-scale".into())
            .spawn(move || scale_loop(capture, settings, &scaled_tx, &skipped, &stop));
        if let Err(e) = spawned {
            stats.warning = Some(format!("cannot start scaler: {e}"));
            (cb.stats)(stats.clone());
            return;
        }
    }

    // Stage 2 (this thread): pacing, congestion control, encoding, sending.
    while !stop.load(Ordering::Relaxed) {
        let frame = match scaled_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(f) => f,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                stats.warning = Some("capture ended".into());
                (cb.stats)(stats.clone());
                break;
            }
        };
        let now = Instant::now();
        if now + Duration::from_millis(2) < next_due {
            continue; // source faster than target fps
        }
        next_due = (next_due + interval).max(now);
        if (cb.in_flight)() >= MAX_UPLINK_IN_FLIGHT {
            stats.skipped += 1;
            continue;
        }
        let (w, h) = (frame.width, frame.height);
        if encoder.as_ref().is_none_or(|e| e.size() != (w, h)) {
            match H264Encoder::new(w, h, settings.fps, settings.bitrate_bps) {
                Ok(e) => encoder = Some(e),
                Err(e) => {
                    stats.warning = Some(format!("encoder: {e}"));
                    (cb.stats)(stats.clone());
                    return;
                }
            }
            force_keyframe.store(true, Ordering::Relaxed);
        }
        let Some(enc) = encoder.as_mut() else {
            continue;
        };
        if force_keyframe.swap(false, Ordering::Relaxed) {
            enc.force_keyframe();
        }
        let t0 = Instant::now();
        let encoded = match enc.encode(&frame) {
            Ok(Some(e)) => e,
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!(error = %e, "encode failed");
                force_keyframe.store(true, Ordering::Relaxed);
                continue;
            }
        };
        win_encode += t0.elapsed();
        let EncodedFrame {
            data,
            keyframe,
            width,
            height,
        } = encoded;
        if keyframe {
            gop = gop.wrapping_add(1);
            idx_in_gop = 0;
        } else {
            idx_in_gop += 1;
        }
        let header = VideoHeader {
            publisher: qchat_proto::SessionId(0),
            frame_seq: seq,
            timestamp_ms: started.elapsed().as_millis() as u32,
            keyframe,
            width: width as u16,
            height: height as u16,
        };
        seq = seq.wrapping_add(1);
        win_frames += 1;
        win_bytes += data.len();
        (cb.send)(
            FrameMeta {
                header,
                priority: frame_priority(gop, idx_in_gop),
            },
            Bytes::from(data),
        );

        let elapsed = win_start.elapsed();
        if elapsed >= Duration::from_secs(1) {
            let secs = elapsed.as_secs_f32();
            stats.skipped += scale_skipped.swap(0, Ordering::Relaxed);
            stats.width = width;
            stats.height = height;
            stats.encode_fps = win_frames as f32 / secs;
            stats.kbps = win_bytes as f32 * 8.0 / 1000.0 / secs;
            stats.avg_encode_ms = if win_frames > 0 {
                win_encode.as_secs_f32() * 1000.0 / win_frames as f32
            } else {
                0.0
            };
            stats.warning = (stats.avg_encode_ms > 1000.0 / settings.fps as f32).then(|| {
                format!(
                    "encoder too slow ({:.0} ms/frame); try a lower preset",
                    stats.avg_encode_ms
                )
            });
            (cb.stats)(stats.clone());
            (win_frames, win_bytes, win_encode) = (0, 0, Duration::ZERO);
            win_start = Instant::now();
        }
    }
}

/// Scaling stage of the publish pipeline. Exits when capture ends or the
/// encoder side goes away (dropping the capture stops it).
fn scale_loop(
    capture: CaptureStream,
    settings: EncodeSettings,
    out: &Sender<super::RawFrame>,
    skipped: &std::sync::atomic::AtomicU64,
    stop: &AtomicBool,
) {
    let mut scaler = Scaler::new();
    while !stop.load(Ordering::Relaxed) {
        let mut frame = match capture.frames.recv_timeout(Duration::from_millis(200)) {
            Ok(f) => f,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        // Always work on the newest frame.
        while let Ok(newer) = capture.frames.try_recv() {
            frame = newer;
            skipped.fetch_add(1, Ordering::Relaxed);
        }
        if out.is_full() {
            // Encoder still busy with the previous frame.
            skipped.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let (w, h) = fit_within(
            frame.width,
            frame.height,
            settings.max_width,
            settings.max_height,
        );
        match scaler.scale(Scaler::make_even(frame), w, h) {
            Ok(f) => {
                if let Err(TrySendError::Disconnected(_)) = out.try_send(f) {
                    return;
                }
            }
            Err(e) => tracing::warn!(error = %e, "scaling failed"),
        }
    }
}

// ---------------------------------------------------------------- viewing ---

#[derive(Debug, Clone, Default)]
pub struct ViewStats {
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    pub kbps: f32,
    pub dropped: u64,
}

pub struct ViewCallbacks {
    pub frame: Box<dyn Fn(DecodedFrame) + Send>,
    pub request_keyframe: Box<dyn Fn() + Send>,
    pub stats: Box<dyn Fn(ViewStats) + Send>,
}

/// Decodes one remote stream. Dropping it stops the decoder thread.
pub struct Viewer {
    tx: Sender<(VideoHeader, Bytes)>,
}

/// How long to wait for a missing frame before giving up on it and asking the
/// publisher for a keyframe.
const GAP_TIMEOUT: Duration = Duration::from_millis(250);
const KEYFRAME_REQUEST_INTERVAL: Duration = Duration::from_millis(500);

impl Viewer {
    pub fn start(cb: ViewCallbacks) -> std::io::Result<Self> {
        let (tx, rx) = crossbeam_channel::bounded(64);
        std::thread::Builder::new()
            .name("video-view".into())
            .spawn(move || view_loop(&rx, &cb))?;
        Ok(Self { tx })
    }

    /// Queues a received frame (dropped if the decoder is far behind).
    pub fn push(&self, header: VideoHeader, payload: Bytes) -> bool {
        !matches!(
            self.tx.try_send((header, payload)),
            Err(TrySendError::Full(_))
        )
    }
}

struct Reorder {
    pending: BTreeMap<u32, (VideoHeader, Bytes)>,
    next: u32,
    waiting_for_keyframe: bool,
    gap_since: Option<Instant>,
}

impl Reorder {
    fn new() -> Self {
        Self {
            pending: BTreeMap::new(),
            next: 0,
            waiting_for_keyframe: true,
            gap_since: None,
        }
    }

    fn insert(&mut self, h: VideoHeader, p: Bytes) -> bool {
        if !self.waiting_for_keyframe && h.frame_seq < self.next {
            return false; // stale
        }
        self.pending.insert(h.frame_seq, (h, p));
        true
    }

    /// Next frame to decode, if any. Sets `need_keyframe` on unrecoverable gaps.
    fn pop(
        &mut self,
        now: Instant,
        need_keyframe: &mut bool,
        dropped: &mut u64,
    ) -> Option<(VideoHeader, Bytes)> {
        if self.waiting_for_keyframe {
            let kf = self
                .pending
                .iter()
                .find(|(_, (h, _))| h.keyframe)
                .map(|(s, _)| *s);
            match kf {
                Some(seq) => {
                    let before = self.pending.len();
                    self.pending = self.pending.split_off(&seq);
                    *dropped += (before - self.pending.len()) as u64;
                    self.next = seq;
                    self.waiting_for_keyframe = false;
                }
                None => {
                    while self.pending.len() > 60 {
                        self.pending.pop_first();
                        *dropped += 1;
                    }
                    return None;
                }
            }
        }
        if let Some(f) = self.pending.remove(&self.next) {
            self.next = self.next.wrapping_add(1);
            self.gap_since = None;
            return Some(f);
        }
        if self.pending.is_empty() {
            return None;
        }
        // Gap. A later keyframe lets us skip it immediately.
        if let Some(seq) = self
            .pending
            .iter()
            .find(|(s, (h, _))| **s > self.next && h.keyframe)
            .map(|(s, _)| *s)
        {
            let before = self.pending.len();
            self.pending = self.pending.split_off(&seq);
            *dropped += (before - self.pending.len()) as u64;
            self.next = seq;
            self.gap_since = None;
            return self.pop(now, need_keyframe, dropped);
        }
        let since = *self.gap_since.get_or_insert(now);
        if now.duration_since(since) > GAP_TIMEOUT {
            self.waiting_for_keyframe = true;
            self.gap_since = None;
            *need_keyframe = true;
        }
        None
    }
}

fn view_loop(rx: &Receiver<(VideoHeader, Bytes)>, cb: &ViewCallbacks) {
    let mut decoder = match H264Decoder::new() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(error = %e, "cannot create decoder");
            return;
        }
    };
    let mut reorder = Reorder::new();
    let mut stats = ViewStats::default();
    let mut last_kf_request: Option<Instant> = None;
    let (mut win_frames, mut win_bytes) = (0u32, 0usize);
    let mut win_start = Instant::now();
    let request = |last: &mut Option<Instant>| {
        if last.is_none_or(|t| t.elapsed() >= KEYFRAME_REQUEST_INTERVAL) {
            *last = Some(Instant::now());
            (cb.request_keyframe)();
        }
    };

    loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok((h, p)) => {
                win_bytes += p.len();
                if !reorder.insert(h, p) {
                    stats.dropped += 1;
                }
                while let Ok((h, p)) = rx.try_recv() {
                    win_bytes += p.len();
                    if !reorder.insert(h, p) {
                        stats.dropped += 1;
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        let mut need_kf = false;
        while let Some((_, payload)) = reorder.pop(Instant::now(), &mut need_kf, &mut stats.dropped)
        {
            match decoder.decode(&payload) {
                Ok(Some(frame)) => {
                    stats.width = frame.width;
                    stats.height = frame.height;
                    win_frames += 1;
                    (cb.frame)(frame);
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::debug!(error = %e, "decode error; waiting for keyframe");
                    reorder.waiting_for_keyframe = true;
                    need_kf = true;
                    break;
                }
            }
        }
        if need_kf || (reorder.waiting_for_keyframe && last_kf_request.is_none()) {
            request(&mut last_kf_request);
        }
        let elapsed = win_start.elapsed();
        if elapsed >= Duration::from_secs(1) {
            let secs = elapsed.as_secs_f32();
            stats.fps = win_frames as f32 / secs;
            stats.kbps = win_bytes as f32 * 8.0 / 1000.0 / secs;
            (cb.stats)(stats.clone());
            (win_frames, win_bytes) = (0, 0);
            win_start = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(seq: u32, keyframe: bool) -> VideoHeader {
        VideoHeader {
            publisher: qchat_proto::SessionId(1),
            frame_seq: seq,
            timestamp_ms: seq * 33,
            keyframe,
            width: 16,
            height: 16,
        }
    }

    fn seqs(r: &mut Reorder, now: Instant) -> (Vec<u32>, bool) {
        let mut out = vec![];
        let mut need = false;
        let mut dropped = 0;
        while let Some((hh, _)) = r.pop(now, &mut need, &mut dropped) {
            out.push(hh.frame_seq);
        }
        (out, need)
    }

    #[test]
    fn starts_at_keyframe_and_reorders() {
        let mut r = Reorder::new();
        let now = Instant::now();
        r.insert(h(4, false), Bytes::new());
        r.insert(h(6, false), Bytes::new());
        r.insert(h(5, true), Bytes::new());
        assert_eq!(seqs(&mut r, now).0, vec![5, 6]);
        r.insert(h(8, false), Bytes::new());
        r.insert(h(7, false), Bytes::new());
        assert_eq!(seqs(&mut r, now).0, vec![7, 8]);
    }

    #[test]
    fn gap_times_out_and_requests_keyframe() {
        let mut r = Reorder::new();
        let t0 = Instant::now();
        r.insert(h(0, true), Bytes::new());
        r.insert(h(2, false), Bytes::new());
        let (got, need) = seqs(&mut r, t0);
        assert_eq!(got, vec![0]);
        assert!(!need, "wait a bit for the missing frame first");
        let (got, need) = seqs(&mut r, t0 + GAP_TIMEOUT + Duration::from_millis(1));
        assert!(got.is_empty());
        assert!(need);
        r.insert(h(3, false), Bytes::new());
        r.insert(h(4, true), Bytes::new());
        assert_eq!(seqs(&mut r, t0 + Duration::from_secs(1)).0, vec![4]);
    }

    #[test]
    fn later_keyframe_skips_gap_immediately() {
        let mut r = Reorder::new();
        let t0 = Instant::now();
        r.insert(h(0, true), Bytes::new());
        r.insert(h(3, true), Bytes::new());
        r.insert(h(4, false), Bytes::new());
        assert_eq!(seqs(&mut r, t0).0, vec![0, 3, 4]);
    }
}
