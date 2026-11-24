//! TLS certificate loading / self-signed generation.

use std::path::Path;

use anyhow::Context;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use sha2::{Digest, Sha256};

use crate::config::Config;

pub struct CertBundle {
    pub chain: Vec<CertificateDer<'static>>,
    pub key: PrivateKeyDer<'static>,
}

impl CertBundle {
    /// SHA-256 fingerprint of the end-entity certificate, as shown to clients
    /// for trust-on-first-use.
    pub fn fingerprint(&self) -> String {
        self.chain
            .first()
            .map(|c| hex::encode(Sha256::digest(c.as_ref())))
            .unwrap_or_default()
    }
}

/// Loads the configured certificate, or generates (and persists) a self-signed one.
pub fn load_or_generate(cfg: &Config) -> anyhow::Result<CertBundle> {
    if let (Some(cert), Some(key)) = (&cfg.cert_path, &cfg.key_path) {
        return load_pem(cert, key);
    }
    let cert_path = cfg.data_dir.join("cert.pem");
    let key_path = cfg.data_dir.join("key.pem");
    if cert_path.exists() && key_path.exists() {
        return load_pem(&cert_path, &key_path);
    }
    std::fs::create_dir_all(&cfg.data_dir)
        .with_context(|| format!("creating {}", cfg.data_dir.display()))?;
    let generated = rcgen::generate_simple_self_signed(cfg.cert_names.clone())
        .context("generating self-signed certificate")?;
    std::fs::write(&cert_path, generated.cert.pem())?;
    write_private(&key_path, generated.signing_key.serialize_pem().as_bytes())?;
    tracing::info!(path = %cert_path.display(), "generated self-signed certificate");
    load_pem(&cert_path, &key_path)
}

/// Generates an in-memory self-signed certificate (tests).
pub fn self_signed(names: Vec<String>) -> anyhow::Result<CertBundle> {
    let generated = rcgen::generate_simple_self_signed(names)?;
    Ok(CertBundle {
        chain: vec![generated.cert.der().clone()],
        key: PrivateKeyDer::try_from(generated.signing_key.serialize_der())
            .map_err(|e| anyhow::anyhow!("{e}"))?,
    })
}

fn load_pem(cert: &Path, key: &Path) -> anyhow::Result<CertBundle> {
    let chain = CertificateDer::pem_file_iter(cert)
        .with_context(|| format!("opening {}", cert.display()))?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("parsing {}", cert.display()))?;
    anyhow::ensure!(
        !chain.is_empty(),
        "{} contains no certificates",
        cert.display()
    );
    let key = PrivateKeyDer::from_pem_file(key)
        .with_context(|| format!("parsing private key {}", key.display()))?;
    Ok(CertBundle { chain, key })
}

fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(data)
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, data)
    }
}
