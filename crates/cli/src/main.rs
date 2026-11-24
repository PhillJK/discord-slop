//! Headless qchat client: send/read messages, watch events. Useful for scripted
//! checks against a running relay.

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use qchat_client_core::{
    ConnectError, ConnectParams, KnownHosts, Session, SessionEvent,
    proto::{Channel, ChannelKind, ChatMessage, Event},
};

#[derive(Parser)]
#[command(name = "qchat-cli", about = "Headless qchat client", version)]
struct Cli {
    /// Relay address (host[:port]).
    #[arg(short, long, env = "QCHAT_ADDR", default_value = "localhost")]
    addr: String,
    /// Relay password.
    #[arg(short, long, env = "QCHAT_PASSWORD")]
    password: String,
    /// Display name.
    #[arg(short, long, env = "QCHAT_USER", default_value = "bot")]
    user: String,
    /// Trust (and pin) the relay certificate if it is not yet known.
    #[arg(long)]
    accept_cert: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List channels and online members.
    Info,
    /// Send a message to a text channel.
    Send { channel: String, message: String },
    /// Print recent messages of a text channel.
    History {
        channel: String,
        #[arg(short, long, default_value_t = 50)]
        limit: u16,
    },
    /// Print events as they arrive until Ctrl-C.
    Listen,
    /// Create a channel.
    CreateChannel {
        name: String,
        #[arg(long)]
        voice: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let known = KnownHosts::default_path().map_or_else(KnownHosts::in_memory, KnownHosts::load);
    let params = ConnectParams {
        address: cli.addr.clone(),
        password: cli.password.clone(),
        username: cli.user.clone(),
    };
    let (session, mut events) = match Session::connect(&params, &known).await {
        Ok(v) => v,
        Err(ConnectError::UnknownCertificate { host, fingerprint }) if cli.accept_cert => {
            eprintln!("pinning certificate for {host}: {fingerprint}");
            known.trust(&host, &fingerprint)?;
            Session::connect(&params, &known).await?
        }
        Err(e @ ConnectError::UnknownCertificate { .. }) => {
            bail!("{e}\nre-run with --accept-cert to trust it")
        }
        Err(e) => return Err(e.into()),
    };
    let channels = session.welcome.channels.clone();
    let find_text = |name: &str| -> anyhow::Result<&Channel> {
        let name = name.trim_start_matches('#');
        channels
            .iter()
            .find(|c| c.kind == ChannelKind::Text && c.name.eq_ignore_ascii_case(name))
            .with_context(|| format!("no text channel named #{name}"))
    };

    match cli.cmd {
        Cmd::Info => {
            println!("server: {}", session.welcome.server_name);
            for c in &channels {
                let sigil = if c.kind == ChannelKind::Text {
                    "#"
                } else {
                    "🔊 "
                };
                println!("  {sigil}{}", c.name);
            }
            println!("online:");
            for m in &session.welcome.members {
                println!("  {} ({})", m.username, m.session);
            }
        }
        Cmd::Send { channel, message } => {
            let ch = find_text(&channel)?;
            let m = session.send_message(ch.id, message).await?;
            print_message(&m, &ch.name);
        }
        Cmd::History { channel, limit } => {
            let ch = find_text(&channel)?;
            for m in session.fetch_history(ch.id, None, limit).await? {
                print_message(&m, &ch.name);
            }
        }
        Cmd::CreateChannel { name, voice } => {
            let kind = if voice {
                ChannelKind::Voice
            } else {
                ChannelKind::Text
            };
            let c = session.create_channel(name, kind).await?;
            println!("created {} ({:?})", c.name, c.kind);
        }
        Cmd::Listen => {
            let mut names: std::collections::HashMap<_, _> =
                channels.iter().map(|c| (c.id, c.name.clone())).collect();
            loop {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => break,
                    ev = events.recv() => match ev {
                        None => break,
                        Some(SessionEvent::Closed(r)) => { eprintln!("closed: {r}"); break; }
                        Some(SessionEvent::Server(Event::MessageCreated(m))) => {
                            let ch = names.get(&m.channel).cloned().unwrap_or_default();
                            print_message(&m, &ch);
                        }
                        Some(SessionEvent::Server(Event::ChannelCreated(c))) => {
                            println!("* channel created: {}", c.name);
                            names.insert(c.id, c.name);
                        }
                        Some(SessionEvent::Server(e)) => println!("* {e:?}"),
                        Some(SessionEvent::Voice { .. } | SessionEvent::Video { .. }) => {}
                    }
                }
            }
        }
    }
    session.close().await;
    Ok(())
}

fn print_message(m: &ChatMessage, channel: &str) {
    let ts = chrono::DateTime::from_timestamp_millis(m.sent_at_ms)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_default();
    println!("[{ts}] #{channel} <{}> {}", m.author, m.content);
}
