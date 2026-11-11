use futures_util::SinkExt;
use tokio::{io::AsyncWrite, sync::mpsc};
use tokio_util::codec::FramedWrite;

use crate::{
  bridge::protocol::{encoded_len, Frame, FrameCodec},
  error::{codes, Error, Result},
};

/// Single-writer handle: every frame goes through one task so frames never interleave.
#[derive(Clone)]
pub struct Writer {
  tx: mpsc::UnboundedSender<Frame>,
  max_frame_size: usize,
}

impl Writer {
  pub fn channel(max_frame_size: usize) -> (Self, mpsc::UnboundedReceiver<Frame>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (Self { tx, max_frame_size }, rx)
  }

  pub fn spawn<W: AsyncWrite + Unpin + Send + 'static>(stream: W, max_frame_size: usize) -> Self {
    let (writer, mut rx) = Self::channel(max_frame_size);
    tauri::async_runtime::spawn(async move {
      let mut sink = FramedWrite::new(stream, FrameCodec::new(max_frame_size));
      while let Some(frame) = rx.recv().await {
        if let Err(e) = sink.send(frame).await {
          log::error!(target: "py-runtime", "failed to write frame: {e}");
          break;
        }
      }
    });
    writer
  }

  pub fn send(&self, frame: Frame) -> Result<()> {
    let size = encoded_len(&frame);
    if size > self.max_frame_size {
      return Err(Error::new(
        codes::FRAME_TOO_LARGE,
        format!("frame of {size} bytes exceeds limit of {}", self.max_frame_size),
      ));
    }
    self
      .tx
      .send(frame)
      .map_err(|_| Error::new(codes::RUNTIME_UNAVAILABLE, "python runtime is not running"))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::bridge::protocol::{FrameKind, Payload, DEFAULT_MAX_FRAME_SIZE};
  use futures_util::StreamExt;
  use serde_json::json;
  use tokio_util::codec::FramedRead;

  #[tokio::test]
  async fn spawned_writer_writes_frames_in_order() {
    let (a, b) = tokio::io::duplex(1024);
    let writer = Writer::spawn(a, DEFAULT_MAX_FRAME_SIZE);
    let mut frames = FramedRead::new(b, FrameCodec::new(DEFAULT_MAX_FRAME_SIZE));
    for i in 0..3u64 {
      writer.send(Frame::new(FrameKind::Event, &json!({"i": i}))).unwrap();
    }
    for i in 0..3u64 {
      let frame = frames.next().await.unwrap().unwrap().unwrap();
      assert_eq!(frame.header["i"], i);
    }
  }

  #[test]
  fn rejects_oversized_frames() {
    let (writer, _rx) = Writer::channel(16);
    let err = writer
      .send(Frame::with_payload(FrameKind::Event, &json!({}), Payload::Raw(vec![0; 100])))
      .unwrap_err();
    assert_eq!(err.code, "frame_too_large");
  }

  #[test]
  fn send_after_close_is_unavailable() {
    let (writer, rx) = Writer::channel(1024);
    drop(rx);
    assert_eq!(writer.send(Frame::new(FrameKind::Event, &json!({}))).unwrap_err().code, "runtime_unavailable");
  }
}
