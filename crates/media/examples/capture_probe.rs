//! Captures the real screen for a few seconds and encodes it at 1440p30.
//!
//!     cargo run --release -p qchat-media --example capture_probe
//!
//! On Wayland the system screen-share picker appears; choose a monitor/window.

use std::time::{Duration, Instant};

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use qchat_media::{
    capture,
    video::{
        EncodeSettings,
        pipeline::{PublishCallbacks, Publisher},
    },
};

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
        .init();
    let sources = capture::sources();
    println!("sources:");
    for s in &sources {
        println!("  {} — {}", s.id, s.name);
    }
    let id = sources
        .iter()
        .find(|s| s.id != capture::SYNTHETIC_ID)
        .map(|s| s.id.clone())
        .unwrap_or_else(|| capture::SYNTHETIC_ID.into());
    println!("starting capture of {id} (pick a screen in the system dialog if asked)…");
    let t0 = Instant::now();
    let stream = capture::start(&id, 30).expect("start capture");
    let first = stream
        .frames
        .recv_timeout(Duration::from_secs(90))
        .expect("no frame within 90 s");
    println!(
        "first frame after {:.1}s: {}x{}",
        t0.elapsed().as_secs_f32(),
        first.width,
        first.height
    );

    // Phase 1: raw capture rate (GNOME only sends frames when the screen changes).
    let (mut n, t1) = (0u32, Instant::now());
    while t1.elapsed() < Duration::from_secs(4) {
        if stream
            .frames
            .recv_timeout(Duration::from_millis(500))
            .is_ok()
        {
            n += 1;
        }
    }
    println!(
        "capture only: {:.1} fps",
        n as f32 / t1.elapsed().as_secs_f32()
    );

    // Phase 2: the real publish pipeline (scale thread + multithreaded encoder).
    let frames = Arc::new(AtomicUsize::new(0));
    let bytes = Arc::new(AtomicUsize::new(0));
    let (f2, b2) = (frames.clone(), bytes.clone());
    let publisher = Publisher::start(
        stream,
        EncodeSettings::P1440,
        PublishCallbacks {
            send: Box::new(move |meta, payload| {
                f2.fetch_add(1, Ordering::Relaxed);
                b2.fetch_add(payload.len(), Ordering::Relaxed);
                let _ = meta;
            }),
            in_flight: Box::new(|| 0),
            stats: Box::new(|s| {
                println!(
                    "  pipeline: {}x{} {:.1} fps, encode {:.1} ms/frame, {:.2} Mb/s, skipped {}{}",
                    s.width,
                    s.height,
                    s.encode_fps,
                    s.avg_encode_ms,
                    s.kbps / 1000.0,
                    s.skipped,
                    s.warning.map(|w| format!(" — {w}")).unwrap_or_default()
                );
            }),
        },
    )
    .expect("publisher");
    let start = Instant::now();
    std::thread::sleep(Duration::from_secs(6));
    let secs = start.elapsed().as_secs_f32();
    println!(
        "published {} frames in {secs:.1}s = {:.1} fps, {:.2} Mb/s",
        frames.load(Ordering::Relaxed),
        frames.load(Ordering::Relaxed) as f32 / secs,
        bytes.load(Ordering::Relaxed) as f32 * 8.0 / secs / 1e6
    );
    drop(publisher);
    std::thread::sleep(Duration::from_millis(800));
    println!("stopped (the system's screen-sharing indicator should be gone)");
}
