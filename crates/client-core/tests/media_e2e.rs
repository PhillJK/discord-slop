//! Media end-to-end over a real in-process relay (no audio devices or screen
//! needed): synthetic 1440p capture → H.264 → QUIC → relay → H.264 decode, and
//! Opus → datagram → relay → jitter buffer → Opus decode.
#![cfg(feature = "media")]

use std::time::{Duration, Instant};

use qchat_client_core::{
    ConnectParams, KnownHosts, Session, SessionEvent,
    proto::{ChannelKind, Event},
};
use qchat_media::{
    audio::{
        FRAME_SAMPLES,
        codec::{VoiceDecoder, VoiceEncoder},
        jitter::{JitterBuffer, Playout},
    },
    capture::{ScreenCapturer, synthetic::SyntheticCapturer},
    video::{
        EncodeSettings,
        pipeline::{PublishCallbacks, Publisher, ViewCallbacks, Viewer},
    },
};
use qchat_server::{Config, Server, Store, tls};

async fn relay() -> (String, KnownHosts, tokio::sync::oneshot::Sender<()>) {
    let mut cfg = Config::with_password("pw").expect("cfg");
    cfg.listen_addr = "127.0.0.1:0".parse().expect("addr");
    let server = Server::bind(
        cfg,
        Store::memory(),
        tls::self_signed(vec!["localhost".into()]).expect("cert"),
    )
    .await
    .expect("bind");
    let addr = format!("127.0.0.1:{}", server.local_addr().expect("addr").port());
    let kh = KnownHosts::in_memory();
    kh.trust(&addr, server.fingerprint()).expect("trust");
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(server.run(async {
        let _ = rx.await;
    }));
    (addr, kh, tx)
}

async fn join(
    addr: &str,
    kh: &KnownHosts,
    user: &str,
) -> (
    std::sync::Arc<Session>,
    tokio::sync::mpsc::UnboundedReceiver<SessionEvent>,
) {
    let (s, ev) = Session::connect(
        &ConnectParams {
            address: addr.into(),
            password: "pw".into(),
            username: user.into(),
        },
        kh,
    )
    .await
    .expect("connect");
    (std::sync::Arc::new(s), ev)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn screen_share_1440p_end_to_end() {
    let (addr, kh, _stop) = relay().await;
    let (publisher, mut pub_events) = join(&addr, &kh, "streamer").await;
    let (viewer, mut view_events) = join(&addr, &kh, "viewer").await;

    publisher
        .start_screen_share(2560, 1440, 30)
        .await
        .expect("share");
    viewer.watch_stream(publisher.id()).await.expect("watch");

    // Publish pipeline: synthetic 2560x1440 @ 30 fps.
    let capture = SyntheticCapturer::default()
        .start("synthetic", 30)
        .expect("capture");
    let handle = tokio::runtime::Handle::current();
    let (p_send, p_flight) = (publisher.clone(), publisher.clone());
    let pub_pipeline = Publisher::start(
        capture,
        EncodeSettings::P1440,
        PublishCallbacks {
            send: Box::new(move |meta, payload| {
                let _g = handle.enter();
                p_send.send_video_frame(meta.header, payload, meta.priority);
            }),
            in_flight: Box::new(move || p_flight.video_in_flight()),
            stats: Box::new(|s| eprintln!("publish stats: {s:?}")),
        },
    )
    .expect("publisher");

    // View pipeline.
    let (frame_tx, frame_rx) = std::sync::mpsc::channel();
    let view_pipeline = Viewer::start(ViewCallbacks {
        frame: Box::new(move |f| {
            let _ = frame_tx.send((f.width, f.height, f.rgba.len()));
        }),
        request_keyframe: Box::new(|| {}),
        stats: Box::new(|s| eprintln!("view stats: {s:?}")),
    })
    .expect("viewer");

    let start = Instant::now();
    let mut decoded = 0usize;
    let deadline = start + Duration::from_secs(10);
    while decoded < 60 && Instant::now() < deadline {
        tokio::select! {
            Some(ev) = pub_events.recv() => {
                if matches!(ev, SessionEvent::Server(Event::KeyframeNeeded)) {
                    pub_pipeline.request_keyframe();
                }
            }
            Some(ev) = view_events.recv() => {
                if let SessionEvent::Video { header, payload } = ev {
                    assert_eq!(header.publisher, publisher.id());
                    view_pipeline.push(header, payload);
                }
            }
            () = tokio::time::sleep(Duration::from_millis(10)) => {}
        }
        while let Ok((w, h, len)) = frame_rx.try_recv() {
            assert_eq!((w, h), (2560, 1440));
            assert_eq!(len, 2560 * 1440 * 4);
            decoded += 1;
        }
    }
    let secs = start.elapsed().as_secs_f32();
    eprintln!("decoded {decoded} frames of 2560x1440 in {secs:.2}s");
    assert!(decoded >= 60, "only {decoded} frames decoded in {secs:.1}s");
}

#[tokio::test]
async fn voice_opus_over_relay() {
    let (addr, kh, _stop) = relay().await;
    let (a, _a_ev) = join(&addr, &kh, "a").await;
    let (b, mut b_ev) = join(&addr, &kh, "b").await;
    let voice = a
        .welcome
        .channels
        .iter()
        .find(|c| c.kind == ChannelKind::Voice)
        .expect("voice")
        .id;
    a.join_voice(voice).await.expect("join");
    b.join_voice(voice).await.expect("join");

    // A sends 1 s of a 440 Hz tone as 50 Opus packets in real time, dropping
    // every 10th, concurrently with B receiving.
    let sender = a.clone();
    let send_task = tokio::spawn(async move {
        let mut enc = VoiceEncoder::new().expect("enc");
        let mut tick = tokio::time::interval(Duration::from_millis(20));
        for f in 0..50u16 {
            tick.tick().await;
            let pcm: Vec<f32> = (0..FRAME_SAMPLES)
                .map(|i| {
                    let t = (usize::from(f) * FRAME_SAMPLES + i) as f32 / 48_000.0;
                    (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5
                })
                .collect();
            let pkt = enc.encode(&pcm).expect("encode").to_vec();
            if f % 10 != 5 {
                sender.send_voice(f, u32::from(f) * FRAME_SAMPLES as u32, &pkt);
            }
        }
    });

    // B: record arrivals, then replay them against a 20 ms playout clock.
    let mut arrivals = Vec::new();
    while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(300), b_ev.recv()).await {
        if let SessionEvent::Voice { header, payload } = ev {
            assert_eq!(header.sender, a.id());
            arrivals.push((Instant::now(), header, payload));
        }
    }
    send_task.await.expect("sender");
    assert_eq!(arrivals.len(), 45, "45 of 50 packets were sent");

    let mut jb = JitterBuffer::new();
    let mut dec = VoiceDecoder::new().expect("dec");
    let mut out = vec![0.0; FRAME_SAMPLES];
    let (mut decoded, mut recovered, mut energy) = (0, 0, 0.0f32);
    let t0 = arrivals[0].0;
    let mut pending = arrivals.into_iter().peekable();
    for tick in 0..70u32 {
        let now = t0 + Duration::from_millis(20 * u64::from(tick));
        while let Some((t, h, p)) = pending.next_if(|(t, _, _)| *t <= now) {
            jb.push_at(h.seq, h.timestamp, p, t);
        }
        match jb.pop() {
            Playout::Decode(p) => {
                dec.decode(&p, &mut out).expect("decode");
                decoded += 1;
            }
            Playout::Fec(p) => {
                dec.decode_fec(&p, &mut out).expect("fec");
                recovered += 1;
            }
            Playout::Conceal => dec.conceal(&mut out).expect("plc"),
            Playout::Silence => out.fill(0.0),
        }
        energy += out.iter().map(|s| s * s).sum::<f32>();
    }
    eprintln!(
        "voice: decoded {decoded}, FEC-recovered {recovered}, stats {:?}",
        jb.stats
    );
    assert_eq!(decoded, 45, "every received packet is played");
    assert_eq!(recovered, 5, "all 5 lost packets recovered via FEC");
    assert!(energy > 100.0, "decoded audio is silent");
}
