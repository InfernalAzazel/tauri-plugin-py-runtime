//! 发往 Python 的帧发送端。
//!
//! 架构位置：前端 -> Tauri 命令 -> Bridge -> **Writer**/FrameCodec -> Python 进程（stdin）。
//!
//! 不变量：每个 Python 进程只有一个写任务独占其 stdin，所有帧都经由同一个队列串行写出，
//! 因此并发调用的帧不会交错。帧在入队前完成编码并检查大小，超限帧不会进入队列。

use bytes::Bytes;
use futures_util::SinkExt;
use tokio::{io::AsyncWrite, sync::mpsc};
use tokio_util::codec::FramedWrite;

use crate::{
  bridge::protocol::{length_codec, Frame},
  error::{codes, Error, Result},
};

/// 单写者句柄：可廉价克隆，所有克隆共享同一个写任务。
///
/// 帧由 [`Writer::send`] 编码，因此超限帧在入队前即被拒绝。队列无界、不提供背压；
/// 写任务因 I/O 错误退出或接收端被丢弃后，后续发送返回 `runtime_unavailable`。
#[derive(Clone)]
pub struct Writer {
  tx: mpsc::UnboundedSender<Bytes>,
  max_frame_size: usize,
}

impl Writer {
  /// 创建一个不绑定 I/O 的写句柄及其接收端，接收端得到的是不含外层长度前缀的帧数据。
  ///
  /// 主要用于测试，或由调用方自行驱动写出；生产环境使用 [`Writer::spawn`]。
  pub fn channel(max_frame_size: usize) -> (Self, mpsc::UnboundedReceiver<Bytes>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (Self { tx, max_frame_size }, rx)
  }

  /// 启动后台写任务，把帧加上 u32 大端长度前缀后依次写入 `stream`。
  ///
  /// 写入失败时记录错误并结束写任务，此后所有 [`Writer::send`] 都会失败。
  pub fn spawn<W: AsyncWrite + Unpin + Send + 'static>(stream: W, max_frame_size: usize) -> Self {
    let (writer, mut rx) = Self::channel(max_frame_size);
    tauri::async_runtime::spawn(async move {
      let mut sink = FramedWrite::new(stream, length_codec(max_frame_size));
      while let Some(frame) = rx.recv().await {
        if let Err(e) = sink.send(frame).await {
          log::error!(target: "py-runtime", "failed to write frame: {e}");
          break;
        }
      }
    });
    writer
  }

  /// 编码并排队发送一帧，不等待实际写出。
  ///
  /// # Errors
  ///
  /// - `frame_too_large`：编码后的帧超过 `max_frame_size`；
  /// - `runtime_unavailable`：写任务已结束（Python 进程已退出或连接已关闭）。
  pub fn send(&self, frame: Frame) -> Result<()> {
    let data = frame.encode();
    if data.len() > self.max_frame_size {
      return Err(Error::new(
        codes::FRAME_TOO_LARGE,
        format!("frame of {} bytes exceeds limit of {}", data.len(), self.max_frame_size),
      ));
    }
    self
      .tx
      .send(data)
      .map_err(|_| Error::new(codes::RUNTIME_UNAVAILABLE, "python runtime is not running"))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::bridge::protocol::{FrameCodec, Message, DEFAULT_MAX_FRAME_SIZE};
  use futures_util::StreamExt;
  use tokio_util::codec::FramedRead;

  fn close(channel: u64) -> Frame {
    Frame::new(Message::ChannelClose { channel })
  }

  #[tokio::test]
  async fn spawned_writer_writes_frames_in_order() {
    let (a, b) = tokio::io::duplex(1024);
    let writer = Writer::spawn(a, DEFAULT_MAX_FRAME_SIZE);
    let mut frames = FramedRead::new(b, FrameCodec::new(DEFAULT_MAX_FRAME_SIZE));
    for i in 0..3 {
      writer.send(close(i)).unwrap();
    }
    for i in 0..3 {
      assert_eq!(frames.next().await.unwrap().unwrap().unwrap(), close(i));
    }
  }

  #[test]
  fn rejects_oversized_frames() {
    let (writer, _rx) = Writer::channel(16);
    let mut frame = close(1);
    frame.body = vec![0; 100];
    assert_eq!(writer.send(frame).unwrap_err().code, "frame_too_large");
  }

  #[test]
  fn send_after_close_is_unavailable() {
    let (writer, rx) = Writer::channel(1024);
    drop(rx);
    assert_eq!(writer.send(close(1)).unwrap_err().code, "runtime_unavailable");
  }
}
