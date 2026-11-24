//! qchat relay server library. The binary in `main.rs` is a thin CLI wrapper;
//! integration tests drive [`Server`] directly.

pub mod config;
pub mod hub;
mod ratelimit;
mod session;
pub mod store;
pub mod tls;
pub mod video;

use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::Context;
pub use config::Config;
pub use store::Store;
use tls::CertBundle;

pub struct Server {
    endpoint: quinn::Endpoint,
    hub: Arc<hub::Hub>,
    fingerprint: String,
}

impl Server {
    /// Binds the QUIC endpoint. Does not start accepting until [`Server::run`].
    pub async fn bind(cfg: Config, store: Store, certs: CertBundle) -> anyhow::Result<Self> {
        let fingerprint = certs.fingerprint();
        let server_config = quic_server_config(certs)?;
        let endpoint = quinn::Endpoint::server(server_config, cfg.listen_addr)
            .with_context(|| format!("binding {}", cfg.listen_addr))?;
        let hub = hub::Hub::new(cfg, store).await?;
        Ok(Self {
            endpoint,
            hub,
            fingerprint,
        })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.endpoint.local_addr()
    }

    /// SHA-256 fingerprint of the server certificate (hex).
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Accepts connections until `shutdown` resolves, then closes all connections.
    pub async fn run(self, shutdown: impl std::future::Future<Output = ()>) {
        let accept = async {
            while let Some(incoming) = self.endpoint.accept().await {
                tokio::spawn(session::handle(self.hub.clone(), incoming));
            }
        };
        tokio::select! {
            () = accept => {}
            () = shutdown => {
                tracing::info!("shutting down");
            }
        }
        self.endpoint.close(
            qchat_proto::error_code::SHUTDOWN.into(),
            b"server shutting down",
        );
        let _ = tokio::time::timeout(Duration::from_secs(3), self.endpoint.wait_idle()).await;
    }
}

fn quic_server_config(certs: CertBundle) -> anyhow::Result<quinn::ServerConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(certs.chain, certs.key)?;
    tls.alpn_protocols = vec![qchat_proto::ALPN.to_vec()];
    tls.max_early_data_size = 0;

    let quic = quinn::crypto::rustls::QuicServerConfig::try_from(tls)?;
    let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(quic));
    let mut transport = quinn::TransportConfig::default();
    transport
        .max_idle_timeout(Some(Duration::from_secs(30).try_into()?))
        .keep_alive_interval(Some(Duration::from_secs(10)))
        // One stream per video frame: allow plenty of concurrent uni streams.
        .max_concurrent_uni_streams(256u32.into())
        .max_concurrent_bidi_streams(4u32.into())
        .datagram_receive_buffer_size(Some(1024 * 1024))
        .datagram_send_buffer_size(1024 * 1024)
        // Room for 1440p video bursts (keyframes can be several hundred KB).
        .stream_receive_window((16u32 * 1024 * 1024).into())
        .receive_window((64u32 * 1024 * 1024).into())
        .send_window(64 * 1024 * 1024);
    cfg.transport_config(Arc::new(transport));
    Ok(cfg)
}
