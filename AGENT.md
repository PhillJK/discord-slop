# AGENT.md — qchat

Guidance for AI agents (and humans) working in this repository.

## What this is

**qchat** is a self-hosted, Discord-style chat app written in Rust, with all
traffic over **QUIC** (`quinn`). One relay server is one "community". Users
connect with the relay **URL + shared relay password + a display name**. There
are no accounts.

| Feature | Status |
|---|---|
| Text channels (persisted in PostgreSQL, history paging, rate limits) | ✅ |
| Voice channels (Opus over QUIC datagrams, RNNoise noise suppression, VAD / push-to-talk, mute/deafen) | ✅ |
| Desktop streaming up to **1440p @ 30 fps** (H.264 via openh264, one QUIC stream per frame) | ✅ |
| On-demand file streaming (event-driven, served on demand) | ⏳ **deferred**. Protocol slot reserved (`StreamKind::File`), nothing implemented. Don't build it unless asked. |

## Architecture

```
            ┌───────────────────── one QUIC connection per client ─────────────────────┐
 client ──► │ bidi stream #0  [0x01 Control]  length-prefixed postcard ClientMsg/ServerMsg│ ──► relay
            │ datagrams       [0x01 Voice]    VoiceHeader(11B) + Opus 20 ms           │     (SFU: forwards,
            │ uni streams     [0x02 Video]    VideoHeader(17B) + H.264 AU, 1 per frame│      never mixes or
            │ (reserved)      [0x03 File]                                             │      transcodes)
            └──────────────────────────────────────────────────────────────────────────┘
```

* **Control**: the client opens one bidirectional stream: `Hello{version,password,username}`
  → `Welcome{…snapshot…}` or `Rejected`. After that it's `Request{id,…}` → `Response{id,…}`,
  plus server-pushed `Event`s.
* **Voice**: datagrams. The relay **overwrites the sender id** with the authenticated
  session (no spoofing) and fans out to the others in the same voice channel. Muted or
  deafened senders are dropped and deafened listeners are skipped.
* **Video**: each encoded frame goes on its own uni stream (Media-over-QUIC style).
  Priority: newer GOP first, earlier frame within a GOP first (`proto::media::frame_priority`).
  For each viewer the relay runs a `ViewerPipe`. When the viewer congests, the pipe skips
  delta frames until the next keyframe, resets stale streams, and asks the publisher for a
  keyframe (`Event::KeyframeNeeded`, rate limited). Publishers skip frames *before*
  encoding while their uplink is congested, so the H.264 chain stays intact.
* **TLS trust**: WebPKI roots first, then trust-on-first-use pinning of the SHA-256 cert
  fingerprint (`known_hosts` in the platform config dir). The server generates a
  self-signed cert into `data_dir` on first run and logs its fingerprint.

## Crate map

```
crates/
  proto/        qchat-proto        wire types, framing, voice/video headers (no I/O except tokio io-util)
  server/       qchat-server       relay binary + lib: quinn endpoint, Hub state, sessions, voice/video fan-out, sqlx store
  media/        qchat-media        RNNoise, jitter buffer, VAD, resampler, mixer, Opus, cpal I/O, xcap capture, openh264, pipelines
  client-core/  qchat-client-core  UI-agnostic client: Session (async API), runtime thread + shared AppState, TOFU
  client/       qchat-client       eframe/egui desktop app (binary `qchat`)
  cli/          qchat-cli          headless bot for scripting and smoke tests
migrations/     SQL migrations (embedded with sqlx::migrate!)
deploy/compose.yaml   dev PostgreSQL
```

Dependency direction (keep it acyclic):
`proto ← media ← client-core ← client / cli`, and `proto ← server`.
**The server must never depend on `media` or `client-core`** (client-core is only a
dev-dependency for integration tests).

### Where things live
* Protocol messages: `crates/proto/src/messages.rs`. Binary headers: `crates/proto/src/media.rs`.
* Server state and fan-out rules: `crates/server/src/hub.rs`. Request handling: `session.rs`.
  Video backpressure: `video.rs`.
* Persistence: `crates/server/src/store.rs` (`Store::Postgres` | `Store::Memory`).
* Client network API: `crates/client-core/src/session.rs`. GUI-facing loop: `runtime.rs`.
  State model: `state.rs`.
* Screen capture: `crates/media/src/capture/`. On Wayland it's our own portal + PipeWire backend
  (`pipewire_backend.rs`); X11, Windows and macOS use xcap (`xcap_backend.rs`). xcap's Wayland
  recorder is deliberately **not** used: it pins PipeWire to 24 fps, omits the cursor, and
  never closes the portal session.
* H.264: `video/h264.rs` (encoder via `openh264_ffi.rs` with one slice per thread, since
  openh264 only parallelizes across slices; decoder via the safe `openh264` crate).
  `video/pipeline.rs` publish path: capture → scale thread → encode thread.
* Media wiring: `crates/client-core/src/media_glue/real.rs`. With the `media` feature off,
  `stub.rs` has the same API.
* GUI: `crates/client/src/{app,views,dialogs}.rs`. Icons come from the vendored Phosphor font
  (`crates/client/assets/Phosphor.ttf`, constants in `icons.rs`). Don't use emoji in UI strings:
  egui's default fonts don't have them and they render as squares.

## Rules

### Protocol
* **Bump `PROTOCOL_VERSION`** (`crates/proto/src/lib.rs`) on any wire-incompatible change.
* `StreamKind` tags, the datagram kind byte and error codes are stable. Add new values,
  never renumber.
* The server **never trusts client-sent ids** (sender, publisher). It derives them from
  the authenticated session.
* Every limit (message length, frame size, history page) lives in `proto` as a constant and
  is enforced server-side.

### Code conventions
* Edition 2024, `resolver = "3"`, workspace-managed dependencies (`[workspace.dependencies]`).
* `thiserror` in libraries, `anyhow` in binaries.
* `tracing`, never `println!`. The CLI's user-facing output is the exception.
* No `unwrap()`/`expect()` outside tests. `unsafe_code` is denied workspace-wide. The **only**
  exception is `crates/media/src/video/openh264_ffi.rs` (direct openh264 encoder FFI, every
  block has a `// SAFETY:` comment). Don't add unsafe elsewhere.
* No blocking in async code. Media work runs on dedicated `std::thread`s (cpal streams
  are `!Send` and must stay on their creating thread). Anything that can pop a system
  dialog (Wayland portal, macOS permission) must run off the network thread.
* UI ↔ runtime: the UI sends `Command`s and reads `Shared` (`Arc<RwLock<AppState>>`). The
  runtime mutates state and calls the waker. Keep lock hold times short. Video frames go
  through a latest-value `VideoSlot`, never a queue.
* sqlx: **runtime queries** (`sqlx::query(...)`), no compile-time `query!` macros, so builds
  never need a database. Schema changes go in a new file in `migrations/`.
* Keep the server buildable and testable without media system libraries.

### Testing
* Every protocol or server behaviour change needs an integration test in
  `crates/server/tests/relay.rs` (in-process relay on 127.0.0.1 + real QUIC clients).
* The Postgres test runs only when `DATABASE_URL` is set. Everything else uses `Store::memory()`.
* Pure media logic (jitter buffer, VAD, resampler, denoise, reorder) has unit tests that
  must stay runnable with `--no-default-features`.
* `crates/client-core/tests/media_e2e.rs` drives the real publish/view pipelines and Opus
  through an in-process relay. Keep it passing when you touch media or video forwarding.

### Cargo features
* `qchat-media`: `audio-io` (cpal), `opus`, `h264` (openh264 from source), `capture` (xcap);
  `full` = all.
* `qchat-client-core` / `qchat-client`: `media` (voice + video codecs) and `capture` (real
  screen capture, on by default). Building with `--no-default-features --features media`
  skips xcap and its EGL/Wayland/PipeWire deps; only the synthetic source is available then.

## Commands

```sh
# Dev database
podman compose -f deploy/compose.yaml up -d          # or docker compose
export DATABASE_URL=postgres://qchat:qchat@localhost:5432/qchat

# Server
cargo run -p qchat-server -- init-config --password 'relay-secret'   # writes server.toml
cargo run -p qchat-server                                              # uses server.toml
cargo run -p qchat-server -- --ephemeral                               # in-memory, no Postgres

# Clients
cargo run --release -p qchat-client                     # GUI (binary: qchat)
cargo run -p qchat-cli -- -a localhost -p relay-secret --accept-cert info
cargo run -p qchat-cli -- -a localhost -p relay-secret -u bot send general "hi"

# Checks (no media system libs needed)
cargo test -p qchat-proto -p qchat-server -p qchat-cli
cargo test -p qchat-client-core --no-default-features
cargo test -p qchat-media --no-default-features
DATABASE_URL=… cargo test -p qchat-server --test relay postgres

# Full workspace (needs the system packages below)
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all

# H.264 performance: 1440p30 needs < 33 ms/frame
cargo run --release -p qchat-media --example encode_bench   # test pattern, all presets
cargo run --release -p qchat-media --example stage_bench    # scale / YUV / encode split
cargo run --release -p qchat-media --example capture_probe  # REAL screen → full publish pipeline
                                                            # (Wayland: pick a screen in the dialog)

# Media end-to-end over a real relay (synthetic 1440p share, Opus + FEC)
cargo test --release -p qchat-client-core --test media_e2e
# Voice engine on real audio hardware (ignored by default)
cargo test --release -p qchat-media -- --ignored engine_starts_on_real_devices
```

Set `QCHAT_SYNTHETIC_SOURCE=1` on the client to offer a moving test pattern as a
screen-share source (useful for testing streaming without a real screen).

## Platform build prerequisites (client / media only)

* **Fedora**: `sudo dnf install gcc-c++ cmake clang-devel alsa-lib-devel pipewire-devel wayland-devel mesa-libgbm-devel libdrm-devel libxcb-devel libglvnd-devel nasm`
  (**nasm is essential**: without it openh264 silently builds without SIMD and video is 2-3x
  slower; `crates/media/build.rs` warns when it is missing)
* **Debian/Ubuntu**: `sudo apt install g++ cmake libclang-dev libasound2-dev libpipewire-0.3-dev libwayland-dev libgbm-dev libdrm-dev libegl-dev nasm libxcb1-dev libxcb-randr0-dev libxcb-shm0-dev pkg-config`
* **Windows** (native build): Rust via rustup (MSVC toolchain), Visual Studio Build Tools with
  the "Desktop development with C++" workload, CMake and NASM (`winget install Kitware.CMake NASM.NASM`). Then
  `cargo build --release -p qchat-client` produces `target\release\qchat.exe`. Capture uses
  Windows Graphics Capture / DXGI via xcap. Release builds hide the console window.
* **Windows (cross-compile from Fedora)**: `sudo dnf install mingw64-gcc mingw64-gcc-c++
  mingw64-winpthreads-static`, `rustup target add x86_64-pc-windows-gnu`, then
  `cargo build --release --target x86_64-pc-windows-gnu -p qchat-client` produces
  `target/x86_64-pc-windows-gnu/release/qchat.exe`. `.cargo/config.toml` links libstdc++,
  libgcc and winpthreads statically, so the .exe imports only system DLLs. Check with
  `x86_64-w64-mingw32-objdump -p qchat.exe | grep "DLL Name"`.
* **macOS**: Xcode command line tools + CMake. Grant *Screen Recording* permission to the
  terminal or app on first share.
* Wayland: screen sharing goes through xdg-desktop-portal, so the system picker chooses the
  monitor or window. A portal backend (GNOME/KDE/wlroots) must be running.

## Known limitations / roadmap
* Push-to-talk works only while the qchat window is focused (no global hotkeys yet).
* Software H.264 (openh264). On the dev laptop real 1440p desktop content encodes at about
  22 ms/frame (≈30 fps), peaking near 32 ms on busy content, which is close to the limit.
  Hardware encoders (VAAPI/NVENC/MF/VideoToolbox) are the next step for headroom, as an
  optional backend behind `video::h264`.
* Wayland compositors send frames only when the screen changes, so a static screen streams at
  low fps. That's expected, not a stall.
* No system-audio capture with screen share yet.
* No session resume: a dropped connection means reconnecting (voice/watch state is re-joined manually).
* File streaming: deferred (see top).
