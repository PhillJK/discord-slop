//! Wayland screen capture: xdg-desktop-portal ScreenCast + PipeWire.
//!
//! Used instead of xcap's Wayland recorder because that one asks PipeWire for a
//! preferred 24 fps (GNOME then caps the stream there), doesn't embed the
//! cursor, and never tears down its PipeWire loop or portal session.
//!
//! The system picker (GNOME/KDE/wlroots portal) chooses the monitor or window.

use std::{
    os::fd::OwnedFd,
    sync::mpsc,
    time::{Duration, Instant},
};

use ashpd::desktop::{
    Session,
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
};
use crossbeam_channel::Sender;
use pipewire::{
    context::ContextRc,
    keys::{MEDIA_CATEGORY, MEDIA_ROLE, MEDIA_TYPE},
    main_loop::MainLoopRc,
    properties,
    spa::{
        param::{
            ParamType,
            format::{FormatProperties, MediaSubtype, MediaType},
            format_utils,
            video::{VideoFormat, VideoInfoRaw},
        },
        pod::{self, Pod, serialize::PodSerializer},
        utils::{Direction, Fraction, Rectangle, SpaTypes},
    },
    stream::{StreamFlags, StreamRc},
};

use super::{CaptureSource, CaptureStream, ScreenCapturer};
use crate::{MediaError, MediaResult, video::RawFrame};

/// Source id of the portal-picked screen/window.
pub const PORTAL_ID: &str = "portal";

/// True when running in a Wayland session (where only the portal works).
pub fn is_wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|v| v.eq_ignore_ascii_case("wayland"))
}

fn cap_err(e: impl std::fmt::Display) -> MediaError {
    MediaError::Capture(e.to_string())
}

pub struct PortalCapturer;

struct Guard {
    quit: Option<pipewire::channel::Sender<()>>,
    session: Option<(Session<Screencast>, Screencast)>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(q) = self.quit.take() {
            let _ = q.send(());
        }
        // Closing the portal session removes the "screen is being shared"
        // indicator. It's a D-Bus call, so do it off the caller's thread.
        if let Some((session, proxy)) = self.session.take() {
            let _ = std::thread::Builder::new()
                .name("portal-close".into())
                .spawn(move || {
                    let _ = pollster::block_on(session.close());
                    drop(proxy);
                });
        }
    }
}

impl ScreenCapturer for PortalCapturer {
    fn sources(&self) -> Vec<CaptureSource> {
        vec![CaptureSource {
            id: PORTAL_ID.into(),
            name: "Screen or window (choose in the system dialog)".into(),
        }]
    }

    fn start(&self, _source_id: &str, fps: u32) -> MediaResult<CaptureStream> {
        let (node_id, fd, session, proxy) = pollster::block_on(open_portal())?;
        let (tx, rx) = crossbeam_channel::bounded(2);
        let (quit_tx, quit_rx) = pipewire::channel::channel::<()>();
        let (ready_tx, ready_rx) = mpsc::channel::<MediaResult<()>>();
        std::thread::Builder::new()
            .name("capture-pipewire".into())
            .spawn(move || {
                if let Err(e) = run_pipewire(fd, node_id, fps.max(1), tx, quit_rx, &ready_tx) {
                    let _ = ready_tx.send(Err(cap_err(e)));
                }
            })
            .map_err(cap_err)?;
        let guard = Guard {
            quit: Some(quit_tx),
            session: Some((session, proxy)),
        };
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| MediaError::Capture("PipeWire stream did not start".into()))??;
        Ok(CaptureStream::new(rx, Box::new(guard)))
    }
}

async fn open_portal() -> MediaResult<(u32, OwnedFd, Session<Screencast>, Screencast)> {
    let proxy = Screencast::new().await.map_err(cap_err)?;
    let session = proxy
        .create_session(Default::default())
        .await
        .map_err(cap_err)?;
    let mut options = SelectSourcesOptions::default()
        .set_sources(SourceType::MONITOR | SourceType::WINDOW)
        .set_multiple(false);
    if proxy
        .available_cursor_modes()
        .await
        .is_ok_and(|m| m.contains(CursorMode::EMBEDDED))
    {
        options = options.set_cursor_mode(CursorMode::EMBEDDED);
    }
    proxy
        .select_sources(&session, options)
        .await
        .map_err(cap_err)?
        .response()
        .map_err(|e| {
            MediaError::Capture(format!("source selection failed or was cancelled: {e}"))
        })?;
    let streams = proxy
        .start(&session, None, Default::default())
        .await
        .map_err(cap_err)?
        .response()
        .map_err(|e| MediaError::Capture(format!("screen share was cancelled: {e}")))?;
    let node_id = streams
        .streams()
        .first()
        .ok_or_else(|| MediaError::Capture("no stream selected".into()))?
        .pipe_wire_node_id();
    let fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await
        .map_err(cap_err)?;
    Ok((node_id, fd, session, proxy))
}

fn format_params(fps: u32) -> Result<Vec<u8>, String> {
    let obj = pod::object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA,
        ),
        pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle {
                width: 1920,
                height: 1080
            },
            Rectangle {
                width: 1,
                height: 1
            },
            Rectangle {
                width: 8192,
                height: 8192
            }
        ),
        // Preferred = target fps (PipeWire fixates on the preferred value).
        pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: fps, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction { num: fps, denom: 1 }
        ),
    );
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &pod::Value::Object(obj))
        .map(|(c, _)| c.into_inner())
        .map_err(|e| format!("{e:?}"))
}

fn run_pipewire(
    fd: OwnedFd,
    node_id: u32,
    fps: u32,
    tx: Sender<RawFrame>,
    quit_rx: pipewire::channel::Receiver<()>,
    ready: &mpsc::Sender<MediaResult<()>>,
) -> Result<(), String> {
    pipewire::init();
    let main_loop = MainLoopRc::new(None).map_err(|e| e.to_string())?;
    let context = ContextRc::new(&main_loop, None).map_err(|e| e.to_string())?;
    let core = context.connect_fd_rc(fd, None).map_err(|e| e.to_string())?;
    let stream = StreamRc::new(
        core,
        "qchat-screen",
        properties::properties! {
            *MEDIA_TYPE => "Video",
            *MEDIA_CATEGORY => "Capture",
            *MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|e| e.to_string())?;

    let _listener = stream
        .add_local_listener_with_user_data(VideoInfoRaw::default())
        .param_changed(|_, info, id, param| {
            let Some(param) = param else { return };
            if id != ParamType::Format.as_raw() {
                return;
            }
            match format_utils::parse_format(param) {
                Ok((MediaType::Video, MediaSubtype::Raw)) => {}
                _ => return,
            }
            if let Err(e) = info.parse(param) {
                tracing::warn!(?e, "cannot parse PipeWire video format");
            } else {
                tracing::info!(
                    format = ?info.format(),
                    width = info.size().width,
                    height = info.size().height,
                    fps = info.framerate().num,
                    "PipeWire screen stream negotiated"
                );
            }
        })
        .process(move |stream, info| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            if tx.is_full() {
                return; // consumer is behind; skip the conversion work
            }
            let datas = buffer.datas_mut();
            let Some(d) = datas.first_mut() else { return };
            let (w, h) = (info.size().width as usize, info.size().height as usize);
            let chunk = d.chunk();
            let (offset, size) = (chunk.offset() as usize, chunk.size() as usize);
            let stride = match chunk.stride() {
                s if s > 0 => s as usize,
                _ => w * 4,
            };
            let format = info.format();
            let Some(bytes) = d.data() else { return };
            if w == 0 || h == 0 || size == 0 || offset >= bytes.len() {
                return;
            }
            let bytes = &bytes[offset..];
            let swap_rb = matches!(format, VideoFormat::BGRx | VideoFormat::BGRA);
            let mut rgba = Vec::with_capacity(w * h * 4);
            for y in 0..h {
                let Some(row) = bytes.get(y * stride..y * stride + w * 4) else {
                    return;
                };
                rgba.extend_from_slice(row);
            }
            for px in rgba.as_chunks_mut::<4>().0 {
                if swap_rb {
                    px.swap(0, 2);
                }
                px[3] = 255;
            }
            let _ = tx.try_send(RawFrame {
                width: w as u32,
                height: h as u32,
                rgba,
                captured_at: Instant::now(),
            });
        })
        .register()
        .map_err(|e| e.to_string())?;

    let values = format_params(fps)?;
    let pod = Pod::from_bytes(&values).ok_or("invalid format pod")?;
    stream
        .connect(
            Direction::Input,
            Some(node_id),
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut [pod],
        )
        .map_err(|e| e.to_string())?;

    let ml = main_loop.clone();
    let _quit = quit_rx.attach(main_loop.loop_(), move |()| ml.quit());
    let _ = ready.send(Ok(()));
    main_loop.run();
    let _ = stream.disconnect();
    Ok(())
}
