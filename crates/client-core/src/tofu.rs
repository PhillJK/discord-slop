//! Certificate trust: WebPKI roots first, then trust-on-first-use pinning.
//!
//! Relays usually run with a self-signed certificate. On first connect the user
//! is shown the certificate's SHA-256 fingerprint and can choose to trust it; the
//! pin is stored in a `known_hosts` file (`host:port fingerprint` per line). A
//! later mismatch is reported as [`crate::ConnectError::CertificateChanged`].

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use parking_lot::Mutex;
use rustls::{
    DigitallySignedStruct, SignatureScheme,
    client::{
        WebPkiServerVerifier,
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    },
    crypto::CryptoProvider,
};
use rustls_pki_types::{CertificateDer, ServerName, UnixTime};
use sha2::{Digest, Sha256};

/// Persistent `host:port -> fingerprint` pins.
pub struct KnownHosts {
    path: Option<PathBuf>,
    pins: Mutex<HashMap<String, String>>,
}

impl KnownHosts {
    /// In-memory only (tests, bots).
    pub fn in_memory() -> Self {
        Self {
            path: None,
            pins: Mutex::new(HashMap::new()),
        }
    }

    /// Loads pins from `path` (missing file = empty).
    pub fn load(path: PathBuf) -> Self {
        let mut pins = HashMap::new();
        if let Ok(text) = std::fs::read_to_string(&path) {
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some((host, fp)) = line.split_once(char::is_whitespace) {
                    pins.insert(host.to_owned(), fp.trim().to_lowercase());
                }
            }
        }
        Self {
            path: Some(path),
            pins: Mutex::new(pins),
        }
    }

    /// Default location in the platform config dir.
    pub fn default_path() -> Option<PathBuf> {
        directories::ProjectDirs::from("", "", "qchat").map(|d| d.config_dir().join("known_hosts"))
    }

    pub fn get(&self, host: &str) -> Option<String> {
        self.pins.lock().get(host).cloned()
    }

    /// Pins `fingerprint` for `host` and persists the file.
    pub fn trust(&self, host: &str, fingerprint: &str) -> std::io::Result<()> {
        let mut pins = self.pins.lock();
        pins.insert(host.to_owned(), fingerprint.to_lowercase());
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut entries: Vec<_> = pins.iter().collect();
        entries.sort();
        let mut text = String::from("# qchat pinned relay certificates (host:port sha256)\n");
        for (h, f) in entries {
            text.push_str(h);
            text.push(' ');
            text.push_str(f);
            text.push('\n');
        }
        std::fs::write(path, text)
    }
}

pub fn fingerprint(cert: &CertificateDer<'_>) -> String {
    hex::encode(Sha256::digest(cert.as_ref()))
}

/// What the verifier saw, so connection failures can be explained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CertObservation {
    Unknown {
        fingerprint: String,
    },
    Changed {
        expected: String,
        fingerprint: String,
    },
}

#[derive(Debug)]
pub(crate) struct TofuVerifier {
    webpki: Option<Arc<WebPkiServerVerifier>>,
    provider: Arc<CryptoProvider>,
    pinned: Option<String>,
    pub(crate) observed: Mutex<Option<CertObservation>>,
}

impl TofuVerifier {
    pub(crate) fn new(provider: Arc<CryptoProvider>, pinned: Option<String>) -> Self {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let webpki = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
            .build()
            .ok();
        Self {
            webpki,
            provider,
            pinned,
            observed: Mutex::new(None),
        }
    }
}

impl ServerCertVerifier for TofuVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if let Some(v) = &self.webpki
            && v.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
                .is_ok()
        {
            return Ok(ServerCertVerified::assertion());
        }
        let fp = fingerprint(end_entity);
        match &self.pinned {
            Some(pin) if *pin == fp => Ok(ServerCertVerified::assertion()),
            Some(pin) => {
                *self.observed.lock() = Some(CertObservation::Changed {
                    expected: pin.clone(),
                    fingerprint: fp,
                });
                Err(rustls::Error::General("pinned certificate changed".into()))
            }
            None => {
                *self.observed.lock() = Some(CertObservation::Unknown { fingerprint: fp });
                Err(rustls::Error::General("untrusted certificate".into()))
            }
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_hosts_persist() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sub/known_hosts");
        let kh = KnownHosts::load(path.clone());
        assert_eq!(kh.get("a:1"), None);
        kh.trust("a:1", "ABCD").expect("trust");
        kh.trust("b:2", "ef01").expect("trust");
        let again = KnownHosts::load(path);
        assert_eq!(again.get("a:1").as_deref(), Some("abcd"));
        assert_eq!(again.get("b:2").as_deref(), Some("ef01"));
    }
}
