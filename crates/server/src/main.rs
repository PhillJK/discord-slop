use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use qchat_server::{Config, Server, Store, config::hash_password, tls};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "qchat-server", about = "qchat relay server", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// Path to the TOML config file.
    #[arg(short, long, default_value = "server.toml", global = true)]
    config: PathBuf,
    /// Keep all data in memory instead of PostgreSQL (lost on restart).
    #[arg(long)]
    ephemeral: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the server (default).
    Run,
    /// Print an Argon2 hash of a password for `password_hash` in the config.
    HashPassword {
        /// Password to hash; read from stdin if omitted.
        password: Option<String>,
    },
    /// Write a starter config file.
    InitConfig {
        /// Shared relay password.
        #[arg(long)]
        password: String,
        /// PostgreSQL URL.
        #[arg(long, default_value = "postgres://qchat:qchat@localhost:5432/qchat")]
        database_url: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,quinn=warn")),
        )
        .init();
    let cli = Cli::parse();
    match cli.cmd.unwrap_or(Cmd::Run) {
        Cmd::HashPassword { password } => {
            let password = match password {
                Some(p) => p,
                None => {
                    let mut s = String::new();
                    std::io::stdin().read_line(&mut s)?;
                    s.trim_end_matches(['\r', '\n']).to_owned()
                }
            };
            println!("{}", hash_password(&password)?);
            Ok(())
        }
        Cmd::InitConfig {
            password,
            database_url,
        } => {
            anyhow::ensure!(
                !cli.config.exists(),
                "{} already exists, refusing to overwrite",
                cli.config.display()
            );
            let text = format!(
                "server_name = \"qchat\"\n\
                 listen_addr = \"0.0.0.0:{port}\"\n\
                 password_hash = \"{hash}\"\n\
                 database_url = \"{database_url}\"\n\
                 data_dir = \"data\"\n\
                 # Names/IPs for the generated self-signed certificate:\n\
                 cert_names = [\"localhost\", \"127.0.0.1\"]\n\
                 # cert_path = \"/etc/qchat/fullchain.pem\"\n\
                 # key_path = \"/etc/qchat/privkey.pem\"\n",
                port = qchat_proto::DEFAULT_PORT,
                hash = hash_password(&password)?,
            );
            std::fs::write(&cli.config, text)?;
            println!("wrote {}", cli.config.display());
            Ok(())
        }
        Cmd::Run => run(&cli.config, cli.ephemeral).await,
    }
}

async fn run(config: &std::path::Path, ephemeral: bool) -> anyhow::Result<()> {
    let cfg = Config::load(config)?;
    let store = if ephemeral {
        tracing::warn!("ephemeral mode: data is kept in memory only");
        Store::memory()
    } else {
        let url = cfg
            .database_url
            .as_deref()
            .context("database_url is not set (config or DATABASE_URL); use --ephemeral to run without a database")?;
        Store::connect_postgres(url)
            .await
            .context("connecting to PostgreSQL")?
    };
    let certs = tls::load_or_generate(&cfg)?;
    let server = Server::bind(cfg, store, certs).await?;
    tracing::info!(
        addr = %server.local_addr()?,
        fingerprint = %server.fingerprint(),
        "qchat relay listening"
    );
    server
        .run(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await;
    Ok(())
}
