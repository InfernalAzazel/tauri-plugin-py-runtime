use std::{
  collections::HashMap,
  sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
  },
  time::Duration,
};

use serde_json::{json, Map, Value};
use tokio::sync::oneshot;

use crate::{
  bridge::{
    protocol::{Frame, FrameKind, Payload},
    transport::Writer,
  },
  error::{codes, Error, Result},
};

pub type ChannelSink = Arc<dyn Fn(Payload) + Send + Sync>;

pub trait EventSink: Send + Sync + 'static {
  fn emit(&self, event: &str, target: Option<&str>, payload: Payload);
  fn exited(&self, code: Option<i32>);
}

#[derive(Debug, Clone, Default)]
pub struct CallOptions {
  pub client_id: Option<String>,
  pub timeout: Option<Duration>,
  pub window: Option<String>,
}

struct Pending {
  tx: oneshot::Sender<Result<Payload>>,
  channels: Vec<u64>,
  client_id: Option<String>,
}

#[derive(Default)]
pub struct Bridge {
  next_id: AtomicU64,
  next_channel: AtomicU64,
  pending: Mutex<HashMap<u64, Pending>>,
  clients: Mutex<HashMap<String, u64>>,
  channels: Mutex<HashMap<u64, ChannelSink>>,
}

fn cancel_frame(id: u64, reason: &str) -> Frame {
  Frame::new(FrameKind::Cancel, json!({ "id": id, "reason": reason }))
}

impl Bridge {
  pub fn new() -> Self {
    Self::default()
  }

  pub async fn call(
    &self,
    writer: &Writer,
    cmd: &str,
    args: Payload,
    channels: Vec<(String, ChannelSink)>,
    opts: CallOptions,
    default_timeout: Duration,
  ) -> Result<Payload> {
    let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
    let mut channel_ids = Map::new();
    let mut registered = Vec::with_capacity(channels.len());
    {
      let mut map = self.channels.lock().unwrap();
      for (name, sink) in channels {
        let channel_id = self.next_channel.fetch_add(1, Ordering::Relaxed) + 1;
        map.insert(channel_id, sink);
        channel_ids.insert(name, json!(channel_id));
        registered.push(channel_id);
      }
    }

    let (tx, rx) = oneshot::channel();
    self.pending.lock().unwrap().insert(
      id,
      Pending { tx, channels: registered, client_id: opts.client_id.clone() },
    );
    if let Some(client_id) = &opts.client_id {
      self.clients.lock().unwrap().insert(client_id.clone(), id);
    }

    let mut header = json!({ "id": id, "cmd": cmd });
    if !channel_ids.is_empty() {
      header["channels"] = Value::Object(channel_ids);
    }
    if let Some(window) = &opts.window {
      header["window"] = json!(window);
    }
    if let Err(e) = writer.send(Frame::with_payload(FrameKind::Request, header, args)) {
      self.finish(id);
      return Err(e);
    }

    let timeout = opts.timeout.unwrap_or(default_timeout);
    match tokio::time::timeout(timeout, rx).await {
      Ok(Ok(result)) => result,
      Ok(Err(_)) => Err(Error::new(codes::RUNTIME_EXITED, "python runtime exited")),
      Err(_) => {
        if self.finish(id).is_some() {
          let _ = writer.send(cancel_frame(id, "timeout"));
        }
        Err(Error::new(
          codes::TIMEOUT,
          format!("command '{cmd}' timed out after {}ms", timeout.as_millis()),
        ))
      }
    }
  }

  fn finish(&self, id: u64) -> Option<Pending> {
    let pending = self.pending.lock().unwrap().remove(&id)?;
    if let Some(client_id) = &pending.client_id {
      self.clients.lock().unwrap().remove(client_id);
    }
    let mut channels = self.channels.lock().unwrap();
    for channel_id in &pending.channels {
      channels.remove(channel_id);
    }
    Some(pending)
  }

  pub fn cancel(&self, writer: Option<&Writer>, client_id: &str) -> bool {
    let id = { self.clients.lock().unwrap().get(client_id).copied() };
    let Some(id) = id else { return false };
    let Some(pending) = self.finish(id) else { return false };
    if let Some(writer) = writer {
      let _ = writer.send(cancel_frame(id, "client"));
    }
    let _ = pending.tx.send(Err(Error::new(codes::CANCELLED, "call was cancelled")));
    true
  }

  pub fn handle(&self, frame: Frame, sink: &dyn EventSink) {
    match frame.kind {
      FrameKind::Response => {
        let Some(id) = frame.header_u64("id") else { return };
        if let Some(pending) = self.finish(id) {
          let _ = pending.tx.send(frame.into_payload().map_err(Error::from));
        }
      }
      FrameKind::Error => {
        let Some(id) = frame.header_u64("id") else { return };
        if let Some(pending) = self.finish(id) {
          let error = Error {
            code: frame.header_str("code").unwrap_or(codes::INTERNAL).to_string(),
            message: frame.header_str("message").unwrap_or_default().to_string(),
            data: frame.header.get("data").cloned(),
          };
          let _ = pending.tx.send(Err(error));
        }
      }
      FrameKind::Event => {
        let event = frame.header_str("event").unwrap_or_default().to_string();
        let target = frame.header_str("target").map(str::to_owned);
        match frame.into_payload() {
          Ok(payload) => sink.emit(&event, target.as_deref(), payload),
          Err(e) => log::warn!(target: "py-runtime", "dropped event {event}: {e}"),
        }
      }
      FrameKind::ChannelData => {
        let Some(channel_id) = frame.header_u64("channel") else { return };
        let channel = self.channels.lock().unwrap().get(&channel_id).cloned();
        if let Some(channel) = channel {
          match frame.into_payload() {
            Ok(payload) => channel(payload),
            Err(e) => log::warn!(target: "py-runtime", "dropped channel message: {e}"),
          }
        }
      }
      FrameKind::ChannelClose => {
        if let Some(channel_id) = frame.header_u64("channel") {
          self.channels.lock().unwrap().remove(&channel_id);
        }
      }
      other => log::warn!(target: "py-runtime", "unexpected frame from python: {other:?}"),
    }
  }

  pub fn fail_all(&self, code: &str, message: &str) {
    let drained: Vec<Pending> = self.pending.lock().unwrap().drain().map(|(_, p)| p).collect();
    self.clients.lock().unwrap().clear();
    self.channels.lock().unwrap().clear();
    for pending in drained {
      let _ = pending.tx.send(Err(Error::new(code, message)));
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::bridge::protocol::DEFAULT_MAX_FRAME_SIZE;
  use tokio::sync::mpsc::UnboundedReceiver;

  struct NullSink;
  impl EventSink for NullSink {
    fn emit(&self, _: &str, _: Option<&str>, _: Payload) {}
    fn exited(&self, _: Option<i32>) {}
  }

  fn setup() -> (Arc<Bridge>, Writer, UnboundedReceiver<Frame>) {
    let (writer, rx) = Writer::channel(DEFAULT_MAX_FRAME_SIZE);
    (Arc::new(Bridge::new()), writer, rx)
  }

  fn spawn_call(
    bridge: &Arc<Bridge>,
    writer: &Writer,
    cmd: &str,
    opts: CallOptions,
    channels: Vec<(String, ChannelSink)>,
  ) -> tokio::task::JoinHandle<Result<Payload>> {
    let (bridge, writer, cmd) = (bridge.clone(), writer.clone(), cmd.to_string());
    tokio::spawn(async move {
      bridge.call(&writer, &cmd, Payload::None, channels, opts, Duration::from_secs(5)).await
    })
  }

  fn response(id: u64, payload: Payload) -> Frame {
    Frame::with_payload(FrameKind::Response, json!({ "id": id }), payload)
  }

  #[tokio::test]
  async fn response_resolves_call() {
    let (bridge, writer, mut rx) = setup();
    let call = spawn_call(&bridge, &writer, "hello", CallOptions { window: Some("main".into()), ..Default::default() }, vec![]);
    let req = rx.recv().await.unwrap();
    assert_eq!(req.kind, FrameKind::Request);
    assert_eq!(req.header_str("cmd"), Some("hello"));
    assert_eq!(req.header_str("window"), Some("main"));
    bridge.handle(response(req.header_u64("id").unwrap(), Payload::Json("\"hi\"".into())), &NullSink);
    assert_eq!(call.await.unwrap().unwrap(), Payload::Json("\"hi\"".into()));
  }

  #[tokio::test]
  async fn out_of_order_responses() {
    let (bridge, writer, mut rx) = setup();
    let first = spawn_call(&bridge, &writer, "a", CallOptions::default(), vec![]);
    let id_a = rx.recv().await.unwrap().header_u64("id").unwrap();
    let second = spawn_call(&bridge, &writer, "b", CallOptions::default(), vec![]);
    let id_b = rx.recv().await.unwrap().header_u64("id").unwrap();
    bridge.handle(response(id_b, Payload::Json("2".into())), &NullSink);
    assert_eq!(second.await.unwrap().unwrap(), Payload::Json("2".into()));
    bridge.handle(response(id_a, Payload::Json("1".into())), &NullSink);
    assert_eq!(first.await.unwrap().unwrap(), Payload::Json("1".into()));
  }

  #[tokio::test]
  async fn error_frame_maps_to_error() {
    let (bridge, writer, mut rx) = setup();
    let call = spawn_call(&bridge, &writer, "x", CallOptions::default(), vec![]);
    let id = rx.recv().await.unwrap().header_u64("id").unwrap();
    bridge.handle(
      Frame::new(FrameKind::Error, json!({"id": id, "code": "custom", "message": "bad", "data": {"k": 1}})),
      &NullSink,
    );
    let err = call.await.unwrap().unwrap_err();
    assert_eq!(err, Error::new("custom", "bad").with_data(json!({"k": 1})));
  }

  #[tokio::test]
  async fn timeout_sends_cancel() {
    let (bridge, writer, mut rx) = setup();
    let opts = CallOptions { timeout: Some(Duration::from_millis(50)), ..Default::default() };
    let call = spawn_call(&bridge, &writer, "slow", opts, vec![]);
    let id = rx.recv().await.unwrap().header_u64("id").unwrap();
    assert_eq!(call.await.unwrap().unwrap_err().code, codes::TIMEOUT);
    let cancel = rx.recv().await.unwrap();
    assert_eq!(cancel.kind, FrameKind::Cancel);
    assert_eq!(cancel.header_u64("id"), Some(id));
    assert_eq!(cancel.header_str("reason"), Some("timeout"));
  }

  #[tokio::test]
  async fn client_cancel() {
    let (bridge, writer, mut rx) = setup();
    let opts = CallOptions { client_id: Some("c1".into()), ..Default::default() };
    let call = spawn_call(&bridge, &writer, "slow", opts, vec![]);
    rx.recv().await.unwrap();
    assert!(bridge.cancel(Some(&writer), "c1"));
    assert!(!bridge.cancel(Some(&writer), "c1"));
    assert_eq!(call.await.unwrap().unwrap_err().code, codes::CANCELLED);
    assert_eq!(rx.recv().await.unwrap().header_str("reason"), Some("client"));
  }

  #[tokio::test]
  async fn channel_data_is_routed_until_response() {
    let (bridge, writer, mut rx) = setup();
    let received = Arc::new(Mutex::new(Vec::new()));
    let sink_store = received.clone();
    let sink: ChannelSink = Arc::new(move |p| sink_store.lock().unwrap().push(p));
    let call = spawn_call(&bridge, &writer, "count", CallOptions::default(), vec![("onItem".into(), sink)]);
    let req = rx.recv().await.unwrap();
    let id = req.header_u64("id").unwrap();
    let channel_id = req.header["channels"]["onItem"].as_u64().unwrap();
    let data = |n: &str| Frame::with_payload(FrameKind::ChannelData, json!({"channel": channel_id}), Payload::Json(n.into()));
    bridge.handle(data("0"), &NullSink);
    bridge.handle(data("1"), &NullSink);
    bridge.handle(response(id, Payload::None), &NullSink);
    call.await.unwrap().unwrap();
    bridge.handle(data("2"), &NullSink);
    assert_eq!(*received.lock().unwrap(), vec![Payload::Json("0".into()), Payload::Json("1".into())]);
  }

  #[tokio::test]
  async fn fail_all_rejects_pending_calls() {
    let (bridge, writer, mut rx) = setup();
    let call = spawn_call(&bridge, &writer, "x", CallOptions::default(), vec![]);
    rx.recv().await.unwrap();
    bridge.fail_all(codes::RUNTIME_EXITED, "gone");
    assert_eq!(call.await.unwrap().unwrap_err().code, codes::RUNTIME_EXITED);
  }
}
