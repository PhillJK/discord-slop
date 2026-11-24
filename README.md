# qchat

A self-hosted, Discord-style chat app in Rust where everything runs over **QUIC**:

* **Text channels**, persisted in PostgreSQL
* **Voice channels**: Opus over QUIC datagrams, RNNoise noise suppression, voice activity or push-to-talk
* **Screen sharing** up to **1440p @ 30 fps** (H.264, one QUIC stream per frame with relay-side congestion handling)
* On-demand file streaming: planned

One relay server = one community. Connect with the relay address, the shared password and a display name.

## Quick start

```sh
# 1. Database
podman compose -f deploy/compose.yaml up -d

# 2. Relay
cargo run --release -p qchat-server -- init-config --password 'relay-secret'
cargo run --release -p qchat-server
#   → logs "qchat relay listening … fingerprint=…"

# 3. Desktop client (needs the system packages listed in AGENT.md)
cargo run --release -p qchat-client
```

On first connect the client shows the relay's certificate fingerprint. Compare it with
the one in the server log before trusting it.

See [AGENT.md](AGENT.md) for architecture, development commands and platform prerequisites.
