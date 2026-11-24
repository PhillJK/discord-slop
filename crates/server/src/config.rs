//! Server configuration (TOML file + environment overrides).

use std::{net::SocketAddr, path::PathBuf};

use anyhow::{Context, bail};
use argon2::{
    Argon2, PasswordHash, PasswordHasher, PasswordVerifier,
    password_hash::{SaltString, rand_core::OsRng},
};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Display name of this relay ("community").
    #[serde(default = "default_name")]
    pub server_name: String,
    /// UDP address to listen on.
    #[serde(default = "default_listen")]
    pub listen_addr: SocketAddr,
    /// Argon2 PHC hash of the shared relay password (see `qchat-server hash-password`).
    pub password_hash: String,
    /// PostgreSQL URL. Unset (and no `DATABASE_URL`) means `--ephemeral` is required.
    #[serde(default)]
    pub database_url: Option<String>,
    /// PEM certificate chain. If unset, a self-signed cert is generated in `data_dir`.
    #[serde(default)]
    pub cert_path: Option<PathBuf>,
    /// PEM private key matching `cert_path`.
    #[serde(default)]
    pub key_path: Option<PathBuf>,
    /// Where generated files (self-signed cert) are kept.
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    /// DNS names / IPs put in the self-signed certificate.
    #[serde(default = "default_names")]
    pub cert_names: Vec<String>,
    #[serde(default)]
    pub limits: Limits,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub max_sessions: usize,
    /// Chat messages: burst size.
    pub chat_burst: u32,
    /// Chat messages: sustained rate per second.
    pub chat_per_sec: f64,
    /// Max unfinished video frame streams per viewer before frames are skipped.
    pub video_max_in_flight: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_sessions: 500,
            chat_burst: 10,
            chat_per_sec: 2.0,
            video_max_in_flight: 8,
        }
    }
}

fn default_name() -> String {
    "qchat".into()
}
fn default_listen() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], qchat_proto::DEFAULT_PORT))
}
fn default_data_dir() -> PathBuf {
    PathBuf::from("data")
}
fn default_names() -> Vec<String> {
    vec!["localhost".into(), "127.0.0.1".into()]
}

impl Config {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let mut cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if let Ok(url) = std::env::var("DATABASE_URL") {
            cfg.database_url = Some(url);
        }
        if let Ok(hash) = std::env::var("QCHAT_PASSWORD_HASH") {
            cfg.password_hash = hash;
        }
        cfg.validate()?;
        Ok(cfg)
    }

    /// Config with defaults and the given password (hashed). Used by tests and
    /// quick local runs.
    pub fn with_password(password: &str) -> anyhow::Result<Self> {
        Ok(Self {
            server_name: default_name(),
            listen_addr: default_listen(),
            password_hash: hash_password(password)?,
            database_url: None,
            cert_path: None,
            key_path: None,
            data_dir: default_data_dir(),
            cert_names: default_names(),
            limits: Limits::default(),
        })
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        PasswordHash::new(&self.password_hash)
            .map_err(|e| anyhow::anyhow!("password_hash is not a valid PHC string: {e}"))?;
        if self.cert_path.is_some() != self.key_path.is_some() {
            bail!("cert_path and key_path must be set together");
        }
        Ok(())
    }
}

pub fn hash_password(password: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow::anyhow!("hashing password: {e}"))
}

/// Constant-time verification against a PHC hash.
pub fn verify_password(hash: &str, password: &str) -> bool {
    PasswordHash::new(hash)
        .and_then(|h| Argon2::default().verify_password(password.as_bytes(), &h))
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hash_roundtrip() {
        let h = hash_password("hunter2").expect("hash");
        assert!(verify_password(&h, "hunter2"));
        assert!(!verify_password(&h, "hunter3"));
        assert!(!verify_password("garbage", "hunter2"));
    }

    #[test]
    fn parses_minimal_toml() {
        let h = hash_password("x").expect("hash");
        let cfg: Config = toml::from_str(&format!("password_hash = '{h}'")).expect("parse");
        cfg.validate().expect("valid");
        assert_eq!(cfg.listen_addr.port(), qchat_proto::DEFAULT_PORT);
    }
}
