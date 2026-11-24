//! Async client session: one QUIC connection to a relay.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicUsize, Ordering},
    },
    time::Duration,
};

use bytes::Bytes;
use parking_lot::Mutex;
use qchat_proto::{
    ChannelId, ChannelKind, ChatMessage, ClientMsg, ErrorKind, Event, FrameError, MAX_VIDEO_FRAME,
    MessageId, PROTOCOL_VERSION, RejectReason, Request, Response, ServerMsg, SessionId, StreamKind,
    VideoHeader, VoiceHeader, Welcome, read_frame, write_frame,
};
use tokio::{
    io::AsyncReadExt,
    sync::{mpsc, oneshot},
};

use crate::tofu::{CertObservation, KnownHosts, TofuVerifier};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Where and as whom to connect.
#[derive(Debug, Clone)]
pub struct ConnectParams {
    /// `host`, `host:port`, `qchat://host:port` or `[v6]:port`.
    pub address: String,
    pub password: String,
    pub username: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("invalid address: {0}")]
    InvalidAddress(String),
    #[error("could not resolve {0}")]
    Resolve(String),
    #[error("the relay's certificate is not trusted (fingerprint {fingerprint})")]
    UnknownCertificate { host: String, fingerprint: String },
    #[error(
        "the relay's certificate CHANGED (expected {expected}, got {fingerprint}); this may be an attack"
    )]
    CertificateChanged {
        host: String,
        expected: String,
        fingerprint: String,
    },
    #[error("rejected by relay: {0}")]
    Rejected(RejectReason),
    #[error("connection failed: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("timed out")]
    Timeout,
    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum RequestError {
    #[error("{0}")]
    Server(#[from] ErrorKind),
    #[error("disconnected")]
    Disconnected,
    #[error("request timed out")]
    Timeout,
    #[error("unexpected response")]
    Unexpected,
}

/// Everything the relay sends that isn't a response to a request.
#[derive(Debug)]
pub enum SessionEvent {
    Server(Event),
    Voice { header: VoiceHeader, payload: Bytes },
    Video { header: VideoHeader, payload: Bytes },
    Closed(String),
}

/// Parsed relay address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayAddress {
    pub host: String,
    pub port: u16,
}

impl RelayAddress {
    pub fn parse(s: &str) -> Result<Self, ConnectError> {
        let s = s.trim();
        let s = s
            .strip_prefix("qchat://")
            .unwrap_or(s)
            .trim_end_matches('/');
        if s.is_empty() {
            return Err(ConnectError::InvalidAddress("empty".into()));
        }
        let bad = || ConnectError::InvalidAddress(s.to_owned());
        if let Some(rest) = s.strip_prefix('[') {
            let (host, tail) = rest.split_once(']').ok_or_else(bad)?;
            let port = match tail.strip_prefix(':') {
                Some(p) => p.parse().map_err(|_| bad())?,
                None if tail.is_empty() => qchat_proto::DEFAULT_PORT,
                None => return Err(bad()),
            };
            return Ok(Self {
                host: host.to_owned(),
                port,
            });
        }
        match s.rsplit_once(':') {
            Some((host, port)) if !host.contains(':') => Ok(Self {
                host: host.to_owned(),
                port: port.parse().map_err(|_| bad())?,
            }),
            Some(_) => Ok(Self {
                // bare IPv6 without brackets
                host: s.to_owned(),
                port: qchat_proto::DEFAULT_PORT,
            }),
            None => Ok(Self {
                host: s.to_owned(),
                port: qchat_proto::DEFAULT_PORT,
            }),
        }
    }

    /// Key used for certificate pinning.
    pub fn pin_key(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    async fn resolve(&self) -> Result<Vec<SocketAddr>, ConnectError> {
        let mut addrs: Vec<SocketAddr> = tokio::net::lookup_host((self.host.as_str(), self.port))
            .await
            .map_err(|_| ConnectError::Resolve(self.host.clone()))?
            .collect();
        addrs.dedup();
        if addrs.is_empty() {
            return Err(ConnectError::Resolve(self.host.clone()));
        }
        Ok(addrs)
    }
}

type Pending = Arc<Mutex<HashMap<u32, oneshot::Sender<Result<Response, ErrorKind>>>>>;

/// A live session. Cheap to share behind an `Arc`; dropping the last handle
/// closes the connection.
pub struct Session {
    conn: quinn::Connection,
    endpoint: quinn::Endpoint,
    ctl: mpsc::UnboundedSender<ClientMsg>,
    pending: Pending,
    next_id: AtomicU32,
    video_in_flight: Arc<AtomicUsize>,
    pub welcome: Welcome,
    pub address: RelayAddress,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.conn
            .close(qchat_proto::error_code::NORMAL.into(), b"bye");
    }
}

impl Session {
    /// Connects, verifies the certificate, and performs the Hello handshake.
    pub async fn connect(
        params: &ConnectParams,
        known_hosts: &KnownHosts,
    ) -> Result<(Session, mpsc::UnboundedReceiver<SessionEvent>), ConnectError> {
        tokio::time::timeout(CONNECT_TIMEOUT, Self::connect_inner(params, known_hosts))
            .await
            .map_err(|_| ConnectError::Timeout)?
    }

    async fn connect_inner(
        params: &ConnectParams,
        known_hosts: &KnownHosts,
    ) -> Result<(Session, mpsc::UnboundedReceiver<SessionEvent>), ConnectError> {
        let address = RelayAddress::parse(&params.address)?;
        let remotes = address.resolve().await?;
        let pin = known_hosts.get(&address.pin_key());

        // Happy eyeballs: race all resolved addresses (e.g. ::1 and 127.0.0.1),
        // the first completed QUIC handshake wins.
        let mut attempts = tokio::task::JoinSet::new();
        for remote in remotes {
            let address = address.clone();
            let pin = pin.clone();
            attempts.spawn(async move { establish(remote, &address, pin).await });
        }
        let mut first_err = None;
        let (conn, endpoint) = loop {
            match attempts.join_next().await {
                Some(Ok(Ok(v))) => break v,
                Some(Ok(Err(
                    e @ (ConnectError::UnknownCertificate { .. }
                    | ConnectError::CertificateChanged { .. }),
                ))) => return Err(e),
                Some(Ok(Err(e))) => {
                    first_err.get_or_insert(e);
                }
                Some(Err(join)) => {
                    first_err.get_or_insert(ConnectError::Other(anyhow::anyhow!(join)));
                }
                None => {
                    return Err(
                        first_err.unwrap_or_else(|| ConnectError::Resolve(address.host.clone()))
                    );
                }
            }
        };
        attempts.abort_all();

        let (mut send, mut recv) = conn.open_bi().await?;
        send.write_all(&[StreamKind::Control as u8])
            .await
            .map_err(anyhow::Error::from)?;
        write_frame(
            &mut send,
            &ClientMsg::Hello {
                version: PROTOCOL_VERSION,
                password: params.password.clone(),
                username: params.username.clone(),
            },
        )
        .await
        .map_err(anyhow::Error::from)?;

        let welcome = match read_frame::<_, ServerMsg>(&mut recv).await {
            Ok(ServerMsg::Welcome(w)) => w,
            Ok(ServerMsg::Rejected(r)) => return Err(ConnectError::Rejected(r)),
            Ok(other) => {
                return Err(anyhow::anyhow!("unexpected handshake reply: {other:?}").into());
            }
            Err(e) => return Err(anyhow::anyhow!("handshake failed: {e}").into()),
        };

        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
        let pending: Pending = Arc::default();
        spawn_io(conn.clone(), send, recv, ctl_rx, pending.clone(), ev_tx);

        Ok((
            Session {
                conn,
                endpoint,
                ctl: ctl_tx,
                pending,
                next_id: AtomicU32::new(1),
                video_in_flight: Arc::default(),
                welcome,
                address,
            },
            ev_rx,
        ))
    }

    pub fn id(&self) -> SessionId {
        self.welcome.session
    }

    pub fn is_closed(&self) -> bool {
        self.conn.close_reason().is_some()
    }

    pub fn rtt(&self) -> Duration {
        self.conn.rtt()
    }

    /// Max voice datagram size the path currently supports.
    pub fn max_datagram_size(&self) -> Option<usize> {
        self.conn.max_datagram_size()
    }

    /// Closes the connection and waits briefly for the close to be sent.
    pub async fn close(&self) {
        self.conn
            .close(qchat_proto::error_code::NORMAL.into(), b"bye");
        let _ = tokio::time::timeout(Duration::from_secs(1), self.endpoint.wait_idle()).await;
    }

    pub async fn request(&self, body: Request) -> Result<Response, RequestError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(id, tx);
        if self.ctl.send(ClientMsg::Request { id, body }).is_err() {
            self.pending.lock().remove(&id);
            return Err(RequestError::Disconnected);
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(r)) => r.map_err(RequestError::Server),
            Ok(Err(_)) => Err(RequestError::Disconnected),
            Err(_) => {
                self.pending.lock().remove(&id);
                Err(RequestError::Timeout)
            }
        }
    }

    async fn request_ok(&self, body: Request) -> Result<(), RequestError> {
        match self.request(body).await? {
            Response::Ok => Ok(()),
            _ => Err(RequestError::Unexpected),
        }
    }

    pub async fn send_message(
        &self,
        channel: ChannelId,
        content: impl Into<String>,
    ) -> Result<ChatMessage, RequestError> {
        match self
            .request(Request::SendMessage {
                channel,
                content: content.into(),
            })
            .await?
        {
            Response::Message(m) => Ok(m),
            _ => Err(RequestError::Unexpected),
        }
    }

    pub async fn fetch_history(
        &self,
        channel: ChannelId,
        before: Option<MessageId>,
        limit: u16,
    ) -> Result<Vec<ChatMessage>, RequestError> {
        match self
            .request(Request::FetchHistory {
                channel,
                before,
                limit,
            })
            .await?
        {
            Response::History(m) => Ok(m),
            _ => Err(RequestError::Unexpected),
        }
    }

    pub async fn create_channel(
        &self,
        name: impl Into<String>,
        kind: ChannelKind,
    ) -> Result<qchat_proto::Channel, RequestError> {
        match self
            .request(Request::CreateChannel {
                name: name.into(),
                kind,
            })
            .await?
        {
            Response::Channel(c) => Ok(c),
            _ => Err(RequestError::Unexpected),
        }
    }

    pub async fn join_voice(&self, channel: ChannelId) -> Result<(), RequestError> {
        self.request_ok(Request::JoinVoice { channel }).await
    }

    pub async fn leave_voice(&self) -> Result<(), RequestError> {
        self.request_ok(Request::LeaveVoice).await
    }

    pub async fn set_voice_state(&self, muted: bool, deafened: bool) -> Result<(), RequestError> {
        self.request_ok(Request::SetVoiceState { muted, deafened })
            .await
    }

    pub async fn start_screen_share(
        &self,
        width: u16,
        height: u16,
        fps: u8,
    ) -> Result<(), RequestError> {
        self.request_ok(Request::StartScreenShare { width, height, fps })
            .await
    }

    pub async fn stop_screen_share(&self) -> Result<(), RequestError> {
        self.request_ok(Request::StopScreenShare).await
    }

    pub async fn watch_stream(&self, publisher: SessionId) -> Result<(), RequestError> {
        self.request_ok(Request::WatchStream { publisher }).await
    }

    pub async fn unwatch_stream(&self, publisher: SessionId) -> Result<(), RequestError> {
        self.request_ok(Request::UnwatchStream { publisher }).await
    }

    pub async fn request_keyframe(&self, publisher: SessionId) -> Result<(), RequestError> {
        self.request_ok(Request::RequestKeyframe { publisher })
            .await
    }

    /// Sends one Opus packet as a voice datagram (best effort).
    pub fn send_voice(&self, seq: u16, timestamp: u32, opus: &[u8]) {
        let d = VoiceHeader {
            sender: self.id(),
            seq,
            timestamp,
        }
        .to_datagram(opus);
        if let Err(e) = self.conn.send_datagram(Bytes::from(d)) {
            tracing::trace!(error = %e, "voice datagram dropped");
        }
    }

    /// Number of outgoing video frame streams not yet acknowledged by the relay.
    /// Publishers should skip capturing/encoding frames while this is high, so
    /// the encoded chain stays intact.
    pub fn video_in_flight(&self) -> usize {
        self.video_in_flight.load(Ordering::Relaxed)
    }

    /// Sends one encoded video frame on its own uni stream (non-blocking; the
    /// write happens on a background task).
    pub fn send_video_frame(&self, header: VideoHeader, payload: Bytes, priority: i32) {
        let conn = self.conn.clone();
        let in_flight = self.video_in_flight.clone();
        in_flight.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(async move {
            let res = async {
                let mut s = conn.open_uni().await?;
                let _ = s.set_priority(priority);
                s.write_all(&[StreamKind::VideoFrame as u8]).await?;
                s.write_all(&header.encode()).await?;
                s.write_chunk(payload).await?;
                s.finish()?;
                let _ = s.stopped().await;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if let Err(e) = res {
                tracing::trace!(error = %e, "video frame send failed");
            }
            in_flight.fetch_sub(1, Ordering::Relaxed);
        });
    }
}

/// QUIC + TLS handshake with one remote address.
async fn establish(
    remote: SocketAddr,
    address: &RelayAddress,
    pin: Option<String>,
) -> Result<(quinn::Connection, quinn::Endpoint), ConnectError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(TofuVerifier::new(provider.clone(), pin));
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| anyhow::anyhow!(e))?
        .dangerous()
        .with_custom_certificate_verifier(verifier.clone())
        .with_no_client_auth();
    tls.alpn_protocols = vec![qchat_proto::ALPN.to_vec()];
    let quic =
        quinn::crypto::rustls::QuicClientConfig::try_from(tls).map_err(|e| anyhow::anyhow!(e))?;
    let mut cfg = quinn::ClientConfig::new(Arc::new(quic));
    cfg.transport_config(Arc::new(transport_config()));

    let bind = if remote.is_ipv6() {
        SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, 0))
    } else {
        SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, 0))
    };
    let mut endpoint = quinn::Endpoint::client(bind).map_err(anyhow::Error::from)?;
    endpoint.set_default_client_config(cfg);

    match endpoint
        .connect(remote, &address.host)
        .map_err(|e| anyhow::anyhow!(e))?
        .await
    {
        Ok(c) => Ok((c, endpoint)),
        Err(e) => Err(match verifier.observed.lock().take() {
            Some(CertObservation::Unknown { fingerprint }) => ConnectError::UnknownCertificate {
                host: address.pin_key(),
                fingerprint,
            },
            Some(CertObservation::Changed {
                expected,
                fingerprint,
            }) => ConnectError::CertificateChanged {
                host: address.pin_key(),
                expected,
                fingerprint,
            },
            None => ConnectError::Connection(e),
        }),
    }
}

fn transport_config() -> quinn::TransportConfig {
    let mut t = quinn::TransportConfig::default();
    t.max_idle_timeout(quinn::IdleTimeout::try_from(Duration::from_secs(30)).ok())
        .keep_alive_interval(Some(Duration::from_secs(10)))
        .max_concurrent_uni_streams(256u32.into())
        .max_concurrent_bidi_streams(0u32.into())
        .datagram_receive_buffer_size(Some(1024 * 1024))
        .datagram_send_buffer_size(1024 * 1024)
        .stream_receive_window((16u32 * 1024 * 1024).into())
        .receive_window((64u32 * 1024 * 1024).into())
        .send_window(64 * 1024 * 1024);
    t
}

fn spawn_io(
    conn: quinn::Connection,
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    mut ctl_rx: mpsc::UnboundedReceiver<ClientMsg>,
    pending: Pending,
    events: mpsc::UnboundedSender<SessionEvent>,
) {
    // Control writer.
    tokio::spawn(async move {
        while let Some(msg) = ctl_rx.recv().await {
            if write_frame(&mut send, &msg).await.is_err() {
                break;
            }
        }
        let _ = send.finish();
    });

    // Control reader.
    {
        let events = events.clone();
        let conn = conn.clone();
        tokio::spawn(async move {
            let reason = loop {
                match read_frame::<_, ServerMsg>(&mut recv).await {
                    Ok(ServerMsg::Response { id, result }) => {
                        if let Some(tx) = pending.lock().remove(&id) {
                            let _ = tx.send(result);
                        }
                    }
                    Ok(ServerMsg::Event(e)) => {
                        let _ = events.send(SessionEvent::Server(e));
                    }
                    Ok(other) => {
                        tracing::warn!(?other, "unexpected control message");
                    }
                    Err(FrameError::Closed) => break "control stream closed".to_owned(),
                    Err(e) => break format!("control stream error: {e}"),
                }
            };
            // Fail all outstanding requests.
            pending.lock().clear();
            let reason = conn.close_reason().map_or(reason, |r| describe_close(&r));
            let _ = events.send(SessionEvent::Closed(reason));
        });
    }

    // Voice datagrams.
    {
        let events = events.clone();
        let conn = conn.clone();
        tokio::spawn(async move {
            while let Ok(d) = conn.read_datagram().await {
                if let Some((header, payload)) = VoiceHeader::decode(&d) {
                    let payload = d.slice(d.len() - payload.len()..);
                    if events
                        .send(SessionEvent::Voice { header, payload })
                        .is_err()
                    {
                        break;
                    }
                }
            }
        });
    }

    // Video frame streams.
    tokio::spawn(async move {
        while let Ok(mut s) = conn.accept_uni().await {
            let events = events.clone();
            tokio::spawn(async move {
                let res = async {
                    let kind = s.read_u8().await?;
                    anyhow::ensure!(
                        kind == StreamKind::VideoFrame as u8,
                        "unexpected stream kind"
                    );
                    let mut hdr = [0u8; VideoHeader::LEN];
                    s.read_exact(&mut hdr).await?;
                    let header = VideoHeader::decode(&hdr)
                        .ok_or_else(|| anyhow::anyhow!("bad video header"))?;
                    let payload = s.read_to_end(MAX_VIDEO_FRAME).await?;
                    let _ = events.send(SessionEvent::Video {
                        header,
                        payload: Bytes::from(payload),
                    });
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                if let Err(e) = res {
                    // Reset (stale) frames land here; that's expected.
                    tracing::trace!(error = %e, "video stream dropped");
                }
            });
        }
    });
}

fn describe_close(e: &quinn::ConnectionError) -> String {
    match e {
        quinn::ConnectionError::ApplicationClosed(c) => {
            let code = c.error_code.into_inner();
            let reason = String::from_utf8_lossy(&c.reason);
            if code == u64::from(qchat_proto::error_code::SHUTDOWN) {
                "server shut down".into()
            } else {
                format!("closed by relay: {reason}")
            }
        }
        quinn::ConnectionError::TimedOut => "connection timed out".into(),
        quinn::ConnectionError::LocallyClosed => "disconnected".into(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> (String, u16) {
        let a = RelayAddress::parse(s).expect("parse");
        (a.host, a.port)
    }

    #[test]
    fn parses_addresses() {
        let d = qchat_proto::DEFAULT_PORT;
        assert_eq!(p("example.com"), ("example.com".into(), d));
        assert_eq!(p("example.com:99"), ("example.com".into(), 99));
        assert_eq!(p("qchat://1.2.3.4:5/"), ("1.2.3.4".into(), 5));
        assert_eq!(p("[::1]:8"), ("::1".into(), 8));
        assert_eq!(p("[::1]"), ("::1".into(), d));
        assert_eq!(p("::1"), ("::1".into(), d));
        assert!(RelayAddress::parse("").is_err());
        assert!(RelayAddress::parse("host:notaport").is_err());
    }
}
