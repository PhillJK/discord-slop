//! End-to-end tests: an in-process relay on 127.0.0.1 with real QUIC clients.

use std::time::Duration;

use bytes::Bytes;
use qchat_client_core::{
    ConnectError, ConnectParams, KnownHosts, Session, SessionEvent,
    proto::{ChannelKind, Event, RejectReason, SessionId, VideoHeader},
};
use qchat_server::{Config, Server, Store, tls};
use tokio::sync::mpsc::UnboundedReceiver;

const PASSWORD: &str = "correct horse";
const WAIT: Duration = Duration::from_secs(5);

struct TestRelay {
    addr: String,
    fingerprint: String,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

async fn start_relay_with(store: Store) -> TestRelay {
    let mut cfg = Config::with_password(PASSWORD).expect("config");
    cfg.listen_addr = "127.0.0.1:0".parse().expect("addr");
    let certs = tls::self_signed(vec!["localhost".into()]).expect("cert");
    let server = Server::bind(cfg, store, certs).await.expect("bind");
    let addr = format!("127.0.0.1:{}", server.local_addr().expect("addr").port());
    let fingerprint = server.fingerprint().to_owned();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(server.run(async {
        let _ = rx.await;
    }));
    TestRelay {
        addr,
        fingerprint,
        _shutdown: tx,
    }
}

async fn start_relay() -> TestRelay {
    start_relay_with(Store::memory()).await
}

impl TestRelay {
    fn trusted(&self) -> KnownHosts {
        let kh = KnownHosts::in_memory();
        kh.trust(&self.addr, &self.fingerprint).expect("trust");
        kh
    }

    async fn try_join(&self, user: &str, password: &str) -> Result<Client, ConnectError> {
        let params = ConnectParams {
            address: self.addr.clone(),
            password: password.into(),
            username: user.into(),
        };
        let (session, events) = Session::connect(&params, &self.trusted()).await?;
        Ok(Client { session, events })
    }

    async fn join(&self, user: &str) -> Client {
        self.try_join(user, PASSWORD).await.expect("join")
    }
}

struct Client {
    session: Session,
    events: UnboundedReceiver<SessionEvent>,
}

impl Client {
    /// Waits for the first event matching `pred`, skipping others.
    async fn expect<T>(&mut self, mut pred: impl FnMut(SessionEvent) -> Option<T>) -> T {
        tokio::time::timeout(WAIT, async {
            loop {
                let ev = self.events.recv().await.expect("event stream ended");
                if let Some(v) = pred(ev) {
                    return v;
                }
            }
        })
        .await
        .expect("timed out waiting for event")
    }

    /// Asserts no event matching `pred` arrives within `dur`.
    async fn expect_none(&mut self, dur: Duration, mut pred: impl FnMut(&SessionEvent) -> bool) {
        let deadline = tokio::time::Instant::now() + dur;
        loop {
            match tokio::time::timeout_at(deadline, self.events.recv()).await {
                Err(_) => return,
                Ok(None) => return,
                Ok(Some(ev)) => assert!(!pred(&ev), "unexpected event: {ev:?}"),
            }
        }
    }

    fn id(&self) -> SessionId {
        self.session.id()
    }
}

#[tokio::test]
async fn wrong_password_is_rejected() {
    let relay = start_relay().await;
    match relay.try_join("alice", "nope").await {
        Err(ConnectError::Rejected(RejectReason::BadPassword)) => {}
        Err(e) => panic!("unexpected error: {e}"),
        Ok(_) => panic!("should have been rejected"),
    }
}

#[tokio::test]
async fn duplicate_username_is_rejected_case_insensitively() {
    let relay = start_relay().await;
    let _alice = relay.join("alice").await;
    match relay.try_join("ALICE", PASSWORD).await {
        Err(ConnectError::Rejected(RejectReason::UsernameTaken)) => {}
        Err(e) => panic!("unexpected error: {e}"),
        Ok(_) => panic!("should have been rejected"),
    }
}

#[tokio::test]
async fn unknown_certificate_reports_fingerprint() {
    let relay = start_relay().await;
    let params = ConnectParams {
        address: relay.addr.clone(),
        password: PASSWORD.into(),
        username: "alice".into(),
    };
    match Session::connect(&params, &KnownHosts::in_memory()).await {
        Err(ConnectError::UnknownCertificate { host, fingerprint }) => {
            assert_eq!(host, relay.addr);
            assert_eq!(fingerprint, relay.fingerprint);
        }
        Err(e) => panic!("unexpected error: {e}"),
        Ok(_) => panic!("untrusted cert must not connect"),
    }

    let wrong = KnownHosts::in_memory();
    wrong.trust(&relay.addr, &"00".repeat(32)).expect("trust");
    assert!(matches!(
        Session::connect(&params, &wrong).await,
        Err(ConnectError::CertificateChanged { .. })
    ));
}

#[tokio::test]
async fn welcome_has_default_channels_and_members() {
    let relay = start_relay().await;
    let alice = relay.join("alice").await;
    let mut bob = relay.join("bob").await;
    let w = bob.session.welcome.clone();
    assert!(
        w.channels
            .iter()
            .any(|c| c.name == "general" && c.kind == ChannelKind::Text)
    );
    assert!(w.channels.iter().any(|c| c.kind == ChannelKind::Voice));
    let names: Vec<_> = w.members.iter().map(|m| m.username.as_str()).collect();
    assert_eq!(names, ["alice", "bob"]);

    drop(alice);
    let left = bob
        .expect(|e| match e {
            SessionEvent::Server(Event::MemberLeft { session }) => Some(session),
            _ => None,
        })
        .await;
    assert_eq!(left, w.members[0].session);
}

#[tokio::test]
async fn messages_broadcast_and_history() {
    let relay = start_relay().await;
    let alice = relay.join("alice").await;
    let mut bob = relay.join("bob").await;
    let general = alice
        .session
        .welcome
        .channels
        .iter()
        .find(|c| c.kind == ChannelKind::Text)
        .expect("text channel")
        .id;

    let sent = alice
        .session
        .send_message(general, "  hello bob  ")
        .await
        .expect("send");
    assert_eq!(sent.content, "hello bob");
    assert_eq!(sent.author, "alice");

    let got = bob
        .expect(|e| match e {
            SessionEvent::Server(Event::MessageCreated(m)) => Some(m),
            _ => None,
        })
        .await;
    assert_eq!(got, sent);

    for i in 0..5 {
        alice
            .session
            .send_message(general, format!("m{i}"))
            .await
            .expect("send");
    }
    let page = bob
        .session
        .fetch_history(general, None, 3)
        .await
        .expect("history");
    let contents: Vec<_> = page.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(contents, ["m2", "m3", "m4"]);
    let older = bob
        .session
        .fetch_history(general, Some(page[0].id), 50)
        .await
        .expect("history");
    let contents: Vec<_> = older.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(contents, ["hello bob", "m0", "m1"]);

    // Validation.
    assert!(alice.session.send_message(general, "   ").await.is_err());
    assert!(
        alice
            .session
            .send_message(general, "x".repeat(4001))
            .await
            .is_err()
    );
    let voice = alice
        .session
        .welcome
        .channels
        .iter()
        .find(|c| c.kind == ChannelKind::Voice)
        .expect("voice")
        .id;
    assert!(alice.session.send_message(voice, "hi").await.is_err());
}

#[tokio::test]
async fn chat_is_rate_limited() {
    let relay = start_relay().await;
    let alice = relay.join("alice").await;
    let general = alice.session.welcome.channels[0].id;
    let mut limited = false;
    for i in 0..30 {
        if alice
            .session
            .send_message(general, format!("{i}"))
            .await
            .is_err()
        {
            limited = true;
            break;
        }
    }
    assert!(limited, "30 rapid messages should trip the rate limit");
}

#[tokio::test]
async fn created_channels_are_broadcast() {
    let relay = start_relay().await;
    let alice = relay.join("alice").await;
    let mut bob = relay.join("bob").await;
    let ch = alice
        .session
        .create_channel("#random", ChannelKind::Text)
        .await
        .expect("create");
    assert_eq!(ch.name, "random");
    let got = bob
        .expect(|e| match e {
            SessionEvent::Server(Event::ChannelCreated(c)) => Some(c),
            _ => None,
        })
        .await;
    assert_eq!(got, ch);
}

#[tokio::test]
async fn voice_fan_out_respects_room_mute_and_deafen() {
    let relay = start_relay().await;
    let mut a = relay.join("a").await;
    let mut b = relay.join("b").await;
    let mut c = relay.join("c").await;
    let mut d = relay.join("d").await; // not in voice
    let voice = a
        .session
        .welcome
        .channels
        .iter()
        .find(|ch| ch.kind == ChannelKind::Voice)
        .expect("voice")
        .id;
    for cl in [&a, &b, &c] {
        cl.session.join_voice(voice).await.expect("join voice");
    }
    c.session.set_voice_state(true, true).await.expect("deafen");

    // Text channels can't be joined as voice.
    let text = a.session.welcome.channels[0].id;
    assert!(a.session.join_voice(text).await.is_err());

    let is_voice = |e: &SessionEvent| matches!(e, SessionEvent::Voice { .. });
    // Drain state events first.
    tokio::time::sleep(Duration::from_millis(100)).await;

    a.session.send_voice(1, 960, b"opus-frame");
    let (from, payload) = b
        .expect(|e| match e {
            SessionEvent::Voice { header, payload } => Some((header.sender, payload)),
            _ => None,
        })
        .await;
    assert_eq!(from, a.id(), "relay must stamp the real sender");
    assert_eq!(&payload[..], b"opus-frame");
    c.expect_none(Duration::from_millis(300), is_voice).await;
    d.expect_none(Duration::from_millis(10), is_voice).await;
    a.expect_none(Duration::from_millis(10), is_voice).await;

    // Muted senders are dropped by the relay.
    a.session.set_voice_state(true, false).await.expect("mute");
    a.session.send_voice(2, 1920, b"x");
    b.expect_none(Duration::from_millis(300), is_voice).await;
}

#[tokio::test]
async fn video_frames_reach_viewers_starting_at_keyframe() {
    let relay = start_relay().await;
    let mut publisher = relay.join("pub").await;
    let mut viewer = relay.join("viewer").await;

    publisher
        .session
        .start_screen_share(2560, 1440, 30)
        .await
        .expect("share");
    assert!(
        publisher
            .session
            .start_screen_share(1280, 720, 30)
            .await
            .is_err()
    );
    let info = viewer
        .expect(|e| match e {
            SessionEvent::Server(Event::StreamStarted(i)) => Some(i),
            _ => None,
        })
        .await;
    assert_eq!(
        (info.publisher, info.width, info.height),
        (publisher.id(), 2560, 1440)
    );

    viewer
        .session
        .watch_stream(publisher.id())
        .await
        .expect("watch");
    publisher
        .expect(|e| matches!(e, SessionEvent::Server(Event::KeyframeNeeded)).then_some(()))
        .await;

    let frame = |seq: u32, keyframe: bool| VideoHeader {
        publisher: SessionId(0),
        frame_seq: seq,
        timestamp_ms: seq * 33,
        keyframe,
        width: 2560,
        height: 1440,
    };
    // A delta frame before any keyframe is useless to a new viewer: dropped.
    publisher
        .session
        .send_video_frame(frame(0, false), Bytes::from_static(b"delta0"), 0);
    tokio::time::sleep(Duration::from_millis(100)).await;
    publisher
        .session
        .send_video_frame(frame(1, true), Bytes::from(vec![7u8; 300_000]), 0);
    tokio::time::sleep(Duration::from_millis(50)).await;
    publisher
        .session
        .send_video_frame(frame(2, false), Bytes::from_static(b"delta2"), 0);

    let (h1, p1) = viewer
        .expect(|e| match e {
            SessionEvent::Video { header, payload } => Some((header, payload)),
            _ => None,
        })
        .await;
    assert!(h1.keyframe);
    assert_eq!(h1.frame_seq, 1);
    assert_eq!(h1.publisher, publisher.id());
    assert_eq!(p1.len(), 300_000);
    let (h2, p2) = viewer
        .expect(|e| match e {
            SessionEvent::Video { header, payload } => Some((header, payload)),
            _ => None,
        })
        .await;
    assert_eq!(h2.frame_seq, 2);
    assert_eq!(&p2[..], b"delta2");

    // Stopping the share notifies everyone.
    publisher.session.stop_screen_share().await.expect("stop");
    viewer
        .expect(|e| match e {
            SessionEvent::Server(Event::StreamEnded { publisher: p }) => Some(p),
            _ => None,
        })
        .await;
}

#[tokio::test]
async fn disconnect_cleans_up_voice_and_streams() {
    let relay = start_relay().await;
    let a = relay.join("a").await;
    let mut b = relay.join("b").await;
    let voice = a
        .session
        .welcome
        .channels
        .iter()
        .find(|ch| ch.kind == ChannelKind::Voice)
        .expect("voice")
        .id;
    a.session.join_voice(voice).await.expect("join");
    a.session
        .start_screen_share(1280, 720, 30)
        .await
        .expect("share");
    let a_id = a.id();
    a.session.close().await;
    drop(a);

    let mut voice_left = false;
    let mut stream_ended = false;
    let mut member_left = false;
    while !(voice_left && stream_ended && member_left) {
        match b
            .expect(|e| match e {
                SessionEvent::Server(ev) => Some(ev),
                _ => None,
            })
            .await
        {
            Event::VoiceStateChanged {
                session,
                state: None,
            } if session == a_id => voice_left = true,
            Event::StreamEnded { publisher } if publisher == a_id => stream_ended = true,
            Event::MemberLeft { session } if session == a_id => member_left = true,
            _ => {}
        }
    }
    // The name is free again.
    let _a2 = relay.join("a").await;
}

/// Runs only when `DATABASE_URL` points at a PostgreSQL instance
/// (e.g. `podman compose -f deploy/compose.yaml up -d`).
#[tokio::test]
async fn postgres_history_survives_restart() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping postgres_history_survives_restart: DATABASE_URL not set");
        return;
    };
    let marker = format!("persist-{}", uuid::Uuid::new_v4());
    let channel = {
        let relay = start_relay_with(Store::connect_postgres(&url).await.expect("pg")).await;
        let a = relay.join(&format!("pg-{}", &marker[8..16])).await;
        let general = a
            .session
            .welcome
            .channels
            .iter()
            .find(|c| c.kind == ChannelKind::Text)
            .expect("text")
            .id;
        a.session
            .send_message(general, marker.clone())
            .await
            .expect("send");
        general
    };
    let relay = start_relay_with(Store::connect_postgres(&url).await.expect("pg")).await;
    let b = relay.join(&format!("pg2-{}", &marker[8..16])).await;
    let hist = b
        .session
        .fetch_history(channel, None, 200)
        .await
        .expect("history");
    assert!(hist.iter().any(|m| m.content == marker));
}
