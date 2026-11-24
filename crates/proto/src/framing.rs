//! Length-prefixed postcard framing for the control stream.
//!
//! Frame layout: `u32 big-endian length` followed by `length` bytes of postcard.

use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::MAX_CONTROL_FRAME;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame of {0} bytes exceeds the limit")]
    TooLarge(usize),
    #[error("malformed frame: {0}")]
    Decode(#[from] postcard::Error),
    #[error("stream closed")]
    Closed,
}

/// Encodes a value into a complete frame (length prefix included).
pub fn encode_frame<T: Serialize>(value: &T) -> Result<Vec<u8>, FrameError> {
    let body = postcard::to_stdvec(value)?;
    if body.len() > MAX_CONTROL_FRAME {
        return Err(FrameError::TooLarge(body.len()));
    }
    let len = u32::try_from(body.len()).map_err(|_| FrameError::TooLarge(body.len()))?;
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(&body);
    Ok(buf)
}

/// Writes one frame.
pub async fn write_frame<W, T>(w: &mut W, value: &T) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let frame = encode_frame(value)?;
    w.write_all(&frame).await?;
    Ok(())
}

/// Reads one frame. Returns [`FrameError::Closed`] on a clean EOF at a frame boundary.
pub async fn read_frame<R, T>(r: &mut R) -> Result<T, FrameError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(FrameError::Closed),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_CONTROL_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(postcard::from_bytes(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClientMsg, Request};

    #[tokio::test]
    async fn roundtrip_multiple_frames() {
        let (mut a, mut b) = tokio::io::duplex(64);
        let msgs = vec![
            ClientMsg::Hello {
                version: 1,
                password: "pw".into(),
                username: "alice".into(),
            },
            ClientMsg::Request {
                id: 1,
                body: Request::Ping,
            },
            ClientMsg::Request {
                id: 2,
                body: Request::SendMessage {
                    channel: crate::ChannelId(uuid::Uuid::nil()),
                    content: "x".repeat(3000),
                },
            },
        ];
        let expected = msgs.clone();
        let writer = tokio::spawn(async move {
            for m in &msgs {
                write_frame(&mut a, m).await.expect("write");
            }
        });
        for e in expected {
            let got: ClientMsg = read_frame(&mut b).await.expect("read");
            assert_eq!(got, e);
        }
        writer.await.expect("join");
        // Writer dropped → clean EOF.
        assert!(matches!(
            read_frame::<_, ClientMsg>(&mut b).await,
            Err(FrameError::Closed)
        ));
    }

    #[tokio::test]
    async fn rejects_oversized_length() {
        let mut data: &[u8] = &(u32::MAX).to_be_bytes();
        assert!(matches!(
            read_frame::<_, ClientMsg>(&mut data).await,
            Err(FrameError::TooLarge(_))
        ));
    }

    #[tokio::test]
    async fn truncated_body_is_io_error() {
        let mut frame = encode_frame(&ClientMsg::Request {
            id: 1,
            body: Request::Ping,
        })
        .expect("encode");
        frame.pop();
        let mut data: &[u8] = &frame;
        assert!(matches!(
            read_frame::<_, ClientMsg>(&mut data).await,
            Err(FrameError::Io(_))
        ));
    }

    #[tokio::test]
    async fn garbage_body_is_decode_error() {
        let mut data: &[u8] = &[0, 0, 0, 2, 0xff, 0xff];
        assert!(matches!(
            read_frame::<_, ClientMsg>(&mut data).await,
            Err(FrameError::Decode(_))
        ));
    }
}
