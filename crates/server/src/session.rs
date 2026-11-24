//! Per-connection session: handshake, control loop, voice datagrams, video streams.

use std::{sync::Arc, time::Duration};

use bytes::{Bytes, BytesMut};
use qchat_proto::{
    ClientMsg, ErrorKind, FrameError, MAX_HISTORY_PAGE, MAX_MESSAGE_CHARS, MAX_VIDEO_FRAME,
    PROTOCOL_VERSION, RejectReason, Request, Response, ServerMsg, SessionId, StreamInfo,
    StreamKind, VideoHeader, VoiceHeader, error_code, read_frame, write_frame,
};
use tokio::io::AsyncReadExt;

use crate::{
    hub::{Hub, Peer},
    ratelimit::TokenBucket,
    video::Frame,
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Delay before answering a bad password, to slow down guessing.
const BAD_PASSWORD_DELAY: Duration = Duration::from_millis(750);

pub async fn handle(hub: Arc<Hub>, incoming: quinn::Incoming) {
    let remote = incoming.remote_address();
    let conn = match incoming.await {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(%remote, error = %e, "connection failed");
            return;
        }
    };
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake(&hub, &conn)).await {
        Ok(Ok(Some((peer, rx, send, recv)))) => {
            tracing::info!(%remote, session = %peer.id, user = %peer.username, "joined");
            run(hub.clone(), peer.clone(), rx, send, recv).await;
            hub.unregister(peer.id);
            tracing::info!(session = %peer.id, user = %peer.username, "left");
        }
        Ok(Ok(None)) => {}
        Ok(Err(e)) => {
            tracing::debug!(%remote, error = %e, "handshake failed");
            conn.close(error_code::PROTOCOL.into(), b"handshake failed");
        }
        Err(_) => {
            tracing::debug!(%remote, "handshake timed out");
            conn.close(error_code::PROTOCOL.into(), b"handshake timeout");
        }
    }
}

type Handshaken = (
    Arc<Peer>,
    tokio::sync::mpsc::Receiver<ServerMsg>,
    quinn::SendStream,
    quinn::RecvStream,
);

async fn handshake(hub: &Arc<Hub>, conn: &quinn::Connection) -> anyhow::Result<Option<Handshaken>> {
    let (mut send, mut recv) = conn.accept_bi().await?;
    let kind = recv.read_u8().await?;
    anyhow::ensure!(
        StreamKind::from_u8(kind) == Some(StreamKind::Control),
        "first stream must be the control stream"
    );
    let hello: ClientMsg = read_frame(&mut recv).await?;
    let ClientMsg::Hello {
        version,
        password,
        username,
    } = hello
    else {
        anyhow::bail!("expected Hello");
    };

    let admitted = admit(hub, conn, version, password, username).await;
    let (peer, rx, welcome) = match admitted {
        Ok(v) => v,
        Err(reason) => {
            tracing::debug!(%reason, "rejecting client");
            let _ = write_frame(&mut send, &ServerMsg::Rejected(reason)).await;
            let _ = send.finish();
            // Give the peer a moment to read the rejection before closing.
            let _ = tokio::time::timeout(Duration::from_secs(2), send.stopped()).await;
            conn.close(error_code::REJECTED.into(), b"rejected");
            return Ok(None);
        }
    };
    if let Err(e) = write_frame(&mut send, &ServerMsg::Welcome(welcome)).await {
        hub.unregister(peer.id);
        return Err(e.into());
    }
    Ok(Some((peer, rx, send, recv)))
}

async fn admit(
    hub: &Arc<Hub>,
    conn: &quinn::Connection,
    version: u32,
    password: String,
    username: String,
) -> Result<
    (
        Arc<Peer>,
        tokio::sync::mpsc::Receiver<ServerMsg>,
        qchat_proto::Welcome,
    ),
    RejectReason,
> {
    if version != PROTOCOL_VERSION {
        return Err(RejectReason::VersionMismatch {
            server: PROTOCOL_VERSION,
        });
    }
    let hash = hub.cfg.password_hash.clone();
    let ok = tokio::task::spawn_blocking(move || crate::config::verify_password(&hash, &password))
        .await
        .unwrap_or(false);
    if !ok {
        tokio::time::sleep(BAD_PASSWORD_DELAY).await;
        return Err(RejectReason::BadPassword);
    }
    let username =
        qchat_proto::validate_username(&username).map_err(RejectReason::InvalidUsername)?;
    hub.register(username, conn.clone())
}

async fn run(
    hub: Arc<Hub>,
    peer: Arc<Peer>,
    mut rx: tokio::sync::mpsc::Receiver<ServerMsg>,
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
) {
    let writer = async {
        while let Some(msg) = rx.recv().await {
            if let Err(e) = write_frame(&mut send, &msg).await {
                tracing::debug!(session = %peer.id, error = %e, "control write failed");
                break;
            }
        }
    };

    let reader = async {
        let mut chat_bucket =
            TokenBucket::new(hub.cfg.limits.chat_burst, hub.cfg.limits.chat_per_sec);
        loop {
            let msg: ClientMsg = match read_frame(&mut recv).await {
                Ok(m) => m,
                Err(FrameError::Closed) => break,
                Err(e) => {
                    tracing::debug!(session = %peer.id, error = %e, "control read failed");
                    peer.conn
                        .close(error_code::PROTOCOL.into(), b"bad control frame");
                    break;
                }
            };
            let ClientMsg::Request { id, body } = msg else {
                peer.conn
                    .close(error_code::PROTOCOL.into(), b"unexpected Hello");
                break;
            };
            let result = handle_request(&hub, &peer, body, &mut chat_bucket).await;
            peer.send(ServerMsg::Response { id, result });
        }
    };

    let datagrams = async {
        let mut recipients = Vec::new();
        loop {
            let Ok(d) = peer.conn.read_datagram().await else {
                break;
            };
            let mut d = BytesMut::from(d);
            if !VoiceHeader::rewrite_sender(&mut d, peer.id) {
                continue;
            }
            let d: Bytes = d.freeze();
            hub.voice_recipients(peer.id, &mut recipients);
            for c in recipients.drain(..) {
                // Datagrams are best-effort; drop on error (e.g. too large/closed).
                let _ = c.send_datagram(d.clone());
            }
        }
    };

    let uni_streams = async {
        loop {
            let Ok(stream) = peer.conn.accept_uni().await else {
                break;
            };
            let hub = hub.clone();
            let id = peer.id;
            tokio::spawn(async move {
                if let Err(e) = handle_uni(&hub, id, stream).await {
                    tracing::trace!(session = %id, error = %e, "uni stream dropped");
                }
            });
        }
    };

    tokio::select! {
        () = writer => {}
        () = reader => {}
        () = datagrams => {}
        () = uni_streams => {}
        _ = peer.conn.closed() => {}
    }
    peer.conn.close(error_code::NORMAL.into(), b"bye");
}

async fn handle_uni(
    hub: &Hub,
    publisher: SessionId,
    mut stream: quinn::RecvStream,
) -> anyhow::Result<()> {
    let kind = stream.read_u8().await?;
    match StreamKind::from_u8(kind) {
        Some(StreamKind::VideoFrame) => {
            let mut hdr = [0u8; VideoHeader::LEN];
            stream.read_exact(&mut hdr).await?;
            let mut header =
                VideoHeader::decode(&hdr).ok_or_else(|| anyhow::anyhow!("bad video header"))?;
            header.publisher = publisher;
            let payload = stream.read_to_end(MAX_VIDEO_FRAME).await?;
            let frame = Frame {
                header,
                payload: Bytes::from(payload),
            };
            if !hub.forward_frame(publisher, &frame) {
                anyhow::bail!("frame from a session that is not sharing");
            }
            Ok(())
        }
        Some(StreamKind::File) => {
            // Reserved for on-demand file streaming (not implemented yet).
            let _ = stream.stop(error_code::PROTOCOL.into());
            Ok(())
        }
        _ => {
            let _ = stream.stop(error_code::PROTOCOL.into());
            anyhow::bail!("unexpected uni stream kind {kind:#x}")
        }
    }
}

async fn handle_request(
    hub: &Arc<Hub>,
    peer: &Peer,
    req: Request,
    chat_bucket: &mut TokenBucket,
) -> Result<Response, ErrorKind> {
    match req {
        Request::Ping => Ok(Response::Pong),
        Request::SendMessage { channel, content } => {
            let content = content.trim();
            if content.is_empty() {
                return Err(ErrorKind::Invalid("message is empty".into()));
            }
            if content.chars().count() > MAX_MESSAGE_CHARS {
                return Err(ErrorKind::Invalid(format!(
                    "message longer than {MAX_MESSAGE_CHARS} characters"
                )));
            }
            match hub.channel(channel) {
                None => return Err(ErrorKind::UnknownChannel),
                Some(c) if c.kind != qchat_proto::ChannelKind::Text => {
                    return Err(ErrorKind::WrongChannelKind);
                }
                Some(_) => {}
            }
            if !chat_bucket.take() {
                return Err(ErrorKind::RateLimited);
            }
            let msg = hub
                .store
                .insert_message(channel, &peer.username, content)
                .await
                .map_err(internal)?;
            hub.broadcast(ServerMsg::Event(qchat_proto::Event::MessageCreated(
                msg.clone(),
            )));
            Ok(Response::Message(msg))
        }
        Request::FetchHistory {
            channel,
            before,
            limit,
        } => {
            if hub.channel(channel).is_none() {
                return Err(ErrorKind::UnknownChannel);
            }
            let msgs = hub
                .store
                .history(channel, before, limit.clamp(1, MAX_HISTORY_PAGE))
                .await
                .map_err(internal)?;
            Ok(Response::History(msgs))
        }
        Request::CreateChannel { name, kind } => {
            let name = qchat_proto::validate_channel_name(&name).map_err(ErrorKind::Invalid)?;
            if !chat_bucket.take() {
                return Err(ErrorKind::RateLimited);
            }
            let ch = hub
                .store
                .create_channel(&name, kind)
                .await
                .map_err(internal)?;
            hub.add_channel(ch.clone());
            Ok(Response::Channel(ch))
        }
        Request::JoinVoice { channel } => hub.join_voice(peer.id, channel).map(|()| Response::Ok),
        Request::LeaveVoice => {
            hub.leave_voice(peer.id);
            Ok(Response::Ok)
        }
        Request::SetVoiceState { muted, deafened } => hub
            .set_voice_state(peer.id, muted, deafened)
            .map(|()| Response::Ok),
        Request::StartScreenShare { width, height, fps } => {
            if width == 0 || height == 0 || fps == 0 || fps > 120 {
                return Err(ErrorKind::Invalid("bad stream parameters".into()));
            }
            hub.start_stream(StreamInfo {
                publisher: peer.id,
                width,
                height,
                fps,
            })
            .map(|()| Response::Ok)
        }
        Request::StopScreenShare => {
            hub.stop_stream(peer.id);
            Ok(Response::Ok)
        }
        Request::WatchStream { publisher } => {
            if publisher == peer.id {
                return Err(ErrorKind::Invalid("cannot watch your own stream".into()));
            }
            hub.watch(peer.id, publisher).map(|()| Response::Ok)
        }
        Request::UnwatchStream { publisher } => {
            hub.unwatch(peer.id, publisher);
            Ok(Response::Ok)
        }
        Request::RequestKeyframe { publisher } => {
            hub.request_keyframe(publisher);
            Ok(Response::Ok)
        }
    }
}

fn internal(e: crate::store::StoreError) -> ErrorKind {
    tracing::error!(error = %e, "store error");
    ErrorKind::Internal
}
