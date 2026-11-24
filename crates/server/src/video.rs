//! Video frame fan-out with per-viewer backpressure.
//!
//! Every encoded frame travels on its own unidirectional QUIC stream. For each
//! (publisher, viewer) pair the server runs a [`ViewerPipe`] task that opens one
//! uni stream per frame towards the viewer. If the viewer falls behind (too many
//! unfinished frame streams, or the pipe's queue overflows), the pipe skips delta
//! frames until the next keyframe and asks the publisher for one. When a keyframe
//! is sent, all older unfinished streams are reset since they can no longer be
//! useful to the decoder.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use bytes::Bytes;
use qchat_proto::{StreamKind, VideoHeader, error_code, media::frame_priority};
use tokio::sync::{mpsc, watch};

/// Queue depth between the publisher's reader and one viewer pipe.
const PIPE_QUEUE: usize = 16;

/// An encoded frame ready for forwarding. Cheap to clone.
#[derive(Clone)]
pub struct Frame {
    pub header: VideoHeader,
    pub payload: Bytes,
}

pub struct ViewerPipe {
    tx: mpsc::Sender<Frame>,
    lagging: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ViewerPipe {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl ViewerPipe {
    pub fn spawn(
        conn: quinn::Connection,
        max_in_flight: usize,
        request_keyframe: Box<dyn Fn() + Send + Sync>,
    ) -> Self {
        let (tx, rx) = mpsc::channel(PIPE_QUEUE);
        let lagging = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(run(
            conn,
            rx,
            lagging.clone(),
            max_in_flight.max(1),
            request_keyframe,
        ));
        Self { tx, lagging, task }
    }

    /// Offers a frame without blocking; on overflow the viewer is marked lagging.
    pub fn offer(&self, frame: Frame) {
        if self.tx.try_send(frame).is_err() {
            self.lagging.store(true, Ordering::Relaxed);
        }
    }
}

async fn run(
    conn: quinn::Connection,
    mut rx: mpsc::Receiver<Frame>,
    lagging: Arc<AtomicBool>,
    max_in_flight: usize,
    request_keyframe: Box<dyn Fn() + Send + Sync>,
) {
    let in_flight = Arc::new(AtomicUsize::new(0));
    // Streams carrying frames with seq < this value are reset.
    let (cancel_tx, cancel_rx) = watch::channel(0u32);
    let mut waiting_for_keyframe = true;
    let mut gop: u32 = 0;
    let mut index_in_gop: u32 = 0;

    while let Some(frame) = rx.recv().await {
        if lagging.swap(false, Ordering::Relaxed) && !frame.header.keyframe {
            waiting_for_keyframe = true;
            request_keyframe();
        }
        if frame.header.keyframe {
            waiting_for_keyframe = false;
            gop = gop.wrapping_add(1);
            index_in_gop = 0;
            // Everything older is superseded by this keyframe.
            let _ = cancel_tx.send(frame.header.frame_seq);
        } else {
            if waiting_for_keyframe {
                continue;
            }
            if in_flight.load(Ordering::Relaxed) >= max_in_flight {
                tracing::debug!("viewer congested, skipping to next keyframe");
                waiting_for_keyframe = true;
                request_keyframe();
                continue;
            }
            index_in_gop += 1;
        }

        in_flight.fetch_add(1, Ordering::Relaxed);
        let priority = frame_priority(gop, index_in_gop);
        let conn = conn.clone();
        let in_flight = in_flight.clone();
        let mut cancel = cancel_rx.clone();
        tokio::spawn(async move {
            let seq = frame.header.frame_seq;
            let res = async {
                let mut send = conn.open_uni().await?;
                let _ = send.set_priority(priority);
                let write = async {
                    send.write_all(&[StreamKind::VideoFrame as u8]).await?;
                    send.write_all(&frame.header.encode()).await?;
                    send.write_chunk(frame.payload.clone()).await?;
                    send.finish()?;
                    Ok::<_, anyhow::Error>(())
                };
                let cancelled = async {
                    loop {
                        if *cancel.borrow_and_update() > seq {
                            return;
                        }
                        if cancel.changed().await.is_err() {
                            // Pipe gone: never cancel via this path.
                            std::future::pending::<()>().await;
                        }
                    }
                };
                tokio::select! {
                    r = write => r?,
                    () = cancelled => {
                        let _ = send.reset(error_code::STALE_FRAME.into());
                    }
                }
                // Count the stream as in flight until the peer has acked it.
                let _ = send.stopped().await;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if let Err(e) = res {
                tracing::trace!(error = %e, "frame stream failed");
            }
            in_flight.fetch_sub(1, Ordering::Relaxed);
        });
    }
}
