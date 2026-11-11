//! 调用路由：把前端调用转换为 `request` 帧，并把 Python 返回的帧分发回对应的等待者。
//!
//! 架构位置：前端 -> Tauri 命令 -> **Bridge** -> Writer/FrameCodec -> Python 进程。
//! [`Bridge`] 不持有连接：发送通过调用方传入的 [`Writer`]，接收由
//! [`crate::runtime::Supervisor`] 的读循环调用 [`Bridge::handle`]。
//!
//! 不变量：
//! - 每个调用 id 只会被完成一次（响应、错误、超时、取消或 [`Bridge::fail_all`] 中先到者生效），
//!   之后迟到的帧被静默丢弃；
//! - 调用完成时同时注销其 channel 与客户端 id 映射，因此调用返回后到达的 `channel_data` 不会再投递；
//! - 客户端调用 id 按窗口隔离，见 [`CallOptions::client_id`]。

use std::{
  collections::{BTreeMap, HashMap},
  sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
  },
  time::Duration,
};

use tokio::sync::oneshot;

use crate::{
  bridge::{
    protocol::{Frame, Message, Payload, ProtocolError},
    transport::Writer,
  },
  error::{codes, Error, Result},
};

/// 接收某个 channel 数据的回调，通常把载荷转发给前端的 `Channel`。
///
/// 在 Python 读循环中同步调用，不应阻塞。
pub type ChannelSink = Arc<dyn Fn(Payload) + Send + Sync>;

/// 接收 Python 发往前端的事件与进程退出通知。
///
/// 插件内的实现把事件转发为 Tauri 事件 `py://<event>`；测试中可替换为记录器。
pub trait EventSink: Send + Sync + 'static {
  /// 转发 Python 发出的事件；`target` 为目标 webview 标签，`None` 表示广播。
  fn emit(&self, event: &str, target: Option<&str>, payload: Payload);
  /// 通知 Python 进程意外退出，`code` 为退出码（被信号终止时为 `None`）。
  fn exited(&self, code: Option<i32>);
}

/// 单次调用的可选参数。
#[derive(Debug, Clone, Default)]
pub struct CallOptions {
  /// 前端生成的调用 id，用于 [`Bridge::cancel`]。
  ///
  /// 以 `(窗口标签, client_id)` 为键，因此不同窗口可以使用相同 id，且一个 webview 只能取消
  /// 自己发起的调用；同一窗口内仍在进行的 id 不能重复使用。
  pub client_id: Option<String>,
  /// 本次调用的超时时间；为 `None` 时使用默认超时，指定值最多为默认超时的 [`MAX_TIMEOUT_FACTOR`] 倍。
  pub timeout: Option<Duration>,
  /// 发起调用的 webview 标签，会随 `request` 帧传给 Python（`Command.window`）。
  pub window: Option<String>,
}

/// 前端指定的单次超时最多为默认超时的此倍数。
pub const MAX_TIMEOUT_FACTOR: u32 = 10;

/// 客户端调用 id 按窗口隔离：`(窗口标签, 客户端 id)`。
type ClientKey = (String, String);

struct Pending {
  tx: oneshot::Sender<Result<Payload>>,
  channels: Vec<u64>,
  client: Option<ClientKey>,
}

/// 进行中调用、客户端 id 与 channel 的注册表，在多次进程重启之间复用。
///
/// 内部使用多把互斥锁，任何时刻最多持有其中一把，且锁内不会调用外部回调。
#[derive(Default)]
pub struct Bridge {
  next_id: AtomicU64,
  next_channel: AtomicU64,
  pending: Mutex<HashMap<u64, Pending>>,
  clients: Mutex<HashMap<ClientKey, u64>>,
  channels: Mutex<HashMap<u64, ChannelSink>>,
}

fn cancel_frame(id: u64, reason: &str) -> Frame {
  Frame::new(Message::Cancel { id, reason: reason.into() })
}

impl Bridge {
  /// 创建空的注册表。
  pub fn new() -> Self {
    Self::default()
  }

  /// 通过 `writer` 发送一次命令调用，并等待 Python 的响应。
  ///
  /// `channels` 中的每个回调会分配一个新的 channel id，随请求以“参数名 → channel id”发送，
  /// 调用结束后即注销。超时时间为 `opts.timeout`（上限为 `default_timeout` 的
  /// [`MAX_TIMEOUT_FACTOR`] 倍）或 `default_timeout`；超时后会向 Python 发送
  /// `reason: "timeout"` 的 `cancel` 帧。
  ///
  /// # Errors
  ///
  /// - `invalid_args`：同一窗口内该客户端调用 id 仍在进行中；
  /// - `frame_too_large` / `runtime_unavailable`：请求帧无法发送（见 [`Writer::send`]）；
  /// - `timeout`：超过超时时间；
  /// - `cancelled`：被 [`Bridge::cancel`] 取消；
  /// - `runtime_exited` 等：被 [`Bridge::fail_all`] 以指定错误码终止；
  /// - Python 返回的错误（如 `command_not_found`、`invalid_args`、`internal` 或自定义错误码），
  ///   以及响应载荷不一致时的 `internal`。
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
    let client = opts
      .client_id
      .as_ref()
      .map(|client_id| (opts.window.clone().unwrap_or_default(), client_id.clone()));
    if let Some(key) = &client {
      let mut clients = self.clients.lock().unwrap();
      if clients.contains_key(key) {
        return Err(Error::new(
          codes::INVALID_ARGS,
          format!("call id '{}' is already pending in this window", key.1),
        ));
      }
      clients.insert(key.clone(), id);
    }
    let mut channel_ids = BTreeMap::new();
    let mut registered = Vec::with_capacity(channels.len());
    {
      let mut map = self.channels.lock().unwrap();
      for (name, sink) in channels {
        let channel_id = self.next_channel.fetch_add(1, Ordering::Relaxed) + 1;
        map.insert(channel_id, sink);
        channel_ids.insert(name, channel_id);
        registered.push(channel_id);
      }
    }

    let (tx, rx) = oneshot::channel();
    self.pending.lock().unwrap().insert(id, Pending { tx, channels: registered, client });

    let (payload, body) = args.split();
    let message = Message::Request { id, cmd: cmd.into(), channels: channel_ids, window: opts.window.clone(), payload };
    if let Err(e) = writer.send(Frame { message, body }) {
      self.finish(id);
      return Err(e);
    }

    let timeout = opts
      .timeout
      .map_or(default_timeout, |t| t.min(default_timeout.saturating_mul(MAX_TIMEOUT_FACTOR)));
    match tokio::time::timeout(timeout, rx).await {
      Ok(Ok(result)) => result,
      Ok(Err(_)) => Err(Error::new(codes::RUNTIME_EXITED, "python runtime exited")),
      Err(_) => {
        // 超时与响应可能同时到达：只有成功注销调用的一方才发送 cancel，避免取消已完成的调用。
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

  /// 注销调用及其 channel 与客户端 id 映射；返回 `None` 表示该调用已被其他路径完成。
  fn finish(&self, id: u64) -> Option<Pending> {
    let pending = self.pending.lock().unwrap().remove(&id)?;
    if let Some(key) = &pending.client {
      let mut clients = self.clients.lock().unwrap();
      // 该客户端 id 可能已被同窗口的新调用重新占用，只移除仍指向本调用的映射。
      if clients.get(key) == Some(&id) {
        clients.remove(key);
      }
    }
    let mut channels = self.channels.lock().unwrap();
    for channel_id in &pending.channels {
      channels.remove(channel_id);
    }
    Some(pending)
  }

  /// 取消 `window` 窗口中客户端 id 为 `client_id` 的进行中调用。
  ///
  /// 等待者立即以 `cancelled` 失败，不等待 Python 确认；若提供了 `writer`，还会向 Python 发送
  /// `reason: "client"` 的 `cancel` 帧（发送失败被忽略）。返回是否找到并取消了该调用。
  pub fn cancel(&self, writer: Option<&Writer>, window: &str, client_id: &str) -> bool {
    let key = (window.to_string(), client_id.to_string());
    let id = { self.clients.lock().unwrap().get(&key).copied() };
    let Some(id) = id else { return false };
    let Some(pending) = self.finish(id) else { return false };
    if let Some(writer) = writer {
      let _ = writer.send(cancel_frame(id, "client"));
    }
    let _ = pending.tx.send(Err(Error::new(codes::CANCELLED, "call was cancelled")));
    true
  }

  /// 处理一帧来自 Python 的消息：完成对应调用、转发事件或投递 channel 数据。
  ///
  /// 未知调用 id 或 channel 的帧被静默丢弃；载荷不一致或方向错误的帧记录警告后丢弃，
  /// 不影响连接。
  pub fn handle(&self, frame: Frame, sink: &dyn EventSink) {
    if let Err(e) = self.dispatch(frame, sink) {
      log::warn!(target: "py-runtime", "dropped frame from python: {e}");
    }
  }

  fn dispatch(&self, frame: Frame, sink: &dyn EventSink) -> std::result::Result<(), ProtocolError> {
    let Frame { message, body } = frame;
    match message {
      Message::Response { id, payload } => {
        if let Some(pending) = self.finish(id) {
          let _ = pending.tx.send(payload.join(body).map_err(Error::from));
        }
      }
      Message::Error { id, code, message, data } => {
        if let Some(pending) = self.finish(id) {
          let code = code.unwrap_or_else(|| codes::INTERNAL.into());
          let _ = pending.tx.send(Err(Error { code, message: message.unwrap_or_default(), data }));
        }
      }
      Message::Event { event, target, payload, .. } => sink.emit(&event, target.as_deref(), payload.join(body)?),
      Message::ChannelData { channel, payload } => {
        let channel = self.channels.lock().unwrap().get(&channel).cloned();
        if let Some(channel) = channel {
          channel(payload.join(body)?);
        }
      }
      Message::ChannelClose { channel } => {
        self.channels.lock().unwrap().remove(&channel);
      }
      other => log::warn!(target: "py-runtime", "unexpected {} frame from python", <&str>::from(&other)),
    }
    Ok(())
  }

  /// 以给定错误码终止所有进行中的调用，并清空客户端 id 与 channel 注册表。
  ///
  /// 在 Python 进程退出、停止或启动失败时调用。
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
  use crate::bridge::protocol::{Inline, DEFAULT_MAX_FRAME_SIZE};
  use bytes::Bytes;
  use serde_json::{json, Value};
  use tokio::sync::mpsc::UnboundedReceiver;

  struct NullSink;
  impl EventSink for NullSink {
    fn emit(&self, _: &str, _: Option<&str>, _: Payload) {}
    fn exited(&self, _: Option<i32>) {}
  }

  fn setup() -> (Arc<Bridge>, Writer, UnboundedReceiver<Bytes>) {
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

  /// 读取下一帧发往 Python 的消息，并转为 JSON。
  async fn recv(rx: &mut UnboundedReceiver<Bytes>) -> Value {
    let frame = Frame::decode(&rx.recv().await.unwrap()).unwrap();
    serde_json::to_value(frame.message).unwrap()
  }

  fn response(id: u64, payload: Payload) -> Frame {
    let (payload, body) = payload.split();
    Frame { message: Message::Response { id, payload }, body }
  }

  fn message(value: Value) -> Frame {
    Frame::new(serde_json::from_value(value).unwrap())
  }

  #[tokio::test]
  async fn response_resolves_call() {
    let (bridge, writer, mut rx) = setup();
    let call = spawn_call(&bridge, &writer, "hello", CallOptions { window: Some("main".into()), ..Default::default() }, vec![]);
    let req = recv(&mut rx).await;
    assert_eq!(req, json!({"type": "request", "id": 1, "cmd": "hello", "window": "main"}));
    bridge.handle(response(1, Payload::Json(json!("hi"))), &NullSink);
    assert_eq!(call.await.unwrap().unwrap(), Payload::Json(json!("hi")));
  }

  #[tokio::test]
  async fn out_of_order_responses() {
    let (bridge, writer, mut rx) = setup();
    let first = spawn_call(&bridge, &writer, "a", CallOptions::default(), vec![]);
    let id_a = recv(&mut rx).await["id"].as_u64().unwrap();
    let second = spawn_call(&bridge, &writer, "b", CallOptions::default(), vec![]);
    let id_b = recv(&mut rx).await["id"].as_u64().unwrap();
    bridge.handle(response(id_b, Payload::Json(json!(2))), &NullSink);
    assert_eq!(second.await.unwrap().unwrap(), Payload::Json(json!(2)));
    bridge.handle(response(id_a, Payload::Json(json!(1))), &NullSink);
    assert_eq!(first.await.unwrap().unwrap(), Payload::Json(json!(1)));
  }

  #[tokio::test]
  async fn response_with_inconsistent_payload_fails_call() {
    let (bridge, writer, mut rx) = setup();
    let call = spawn_call(&bridge, &writer, "x", CallOptions::default(), vec![]);
    let id = recv(&mut rx).await["id"].as_u64().unwrap();
    let mut bad = response(id, Payload::Json(json!(1)));
    bad.body = vec![1];
    bridge.handle(bad, &NullSink);
    assert_eq!(call.await.unwrap().unwrap_err().code, codes::INTERNAL);
  }

  #[tokio::test]
  async fn error_frame_maps_to_error() {
    let (bridge, writer, mut rx) = setup();
    let call = spawn_call(&bridge, &writer, "x", CallOptions::default(), vec![]);
    let id = recv(&mut rx).await["id"].as_u64().unwrap();
    bridge.handle(
      message(json!({"type": "error", "id": id, "code": "custom", "message": "bad", "data": {"k": 1}})),
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
    let id = recv(&mut rx).await["id"].as_u64().unwrap();
    assert_eq!(call.await.unwrap().unwrap_err().code, codes::TIMEOUT);
    assert_eq!(recv(&mut rx).await, json!({"type": "cancel", "id": id, "reason": "timeout"}));
  }

  fn client_opts(window: &str, client_id: &str) -> CallOptions {
    CallOptions { client_id: Some(client_id.into()), window: Some(window.into()), ..Default::default() }
  }

  #[tokio::test]
  async fn client_cancel() {
    let (bridge, writer, mut rx) = setup();
    let call = spawn_call(&bridge, &writer, "slow", client_opts("main", "c1"), vec![]);
    rx.recv().await.unwrap();
    assert!(!bridge.cancel(Some(&writer), "other", "c1"));
    assert!(bridge.cancel(Some(&writer), "main", "c1"));
    assert!(!bridge.cancel(Some(&writer), "main", "c1"));
    assert_eq!(call.await.unwrap().unwrap_err().code, codes::CANCELLED);
    assert_eq!(recv(&mut rx).await["reason"], "client");
  }

  #[tokio::test]
  async fn duplicate_pending_client_id_is_rejected() {
    let (bridge, writer, mut rx) = setup();
    let first = spawn_call(&bridge, &writer, "slow", client_opts("main", "c1"), vec![]);
    let id = recv(&mut rx).await["id"].as_u64().unwrap();
    let dup = spawn_call(&bridge, &writer, "slow", client_opts("main", "c1"), vec![]);
    assert_eq!(dup.await.unwrap().unwrap_err().code, codes::INVALID_ARGS);
    assert!(rx.try_recv().is_err(), "duplicate call must not reach python");
    bridge.handle(response(id, Payload::None), &NullSink);
    first.await.unwrap().unwrap();
    let again = spawn_call(&bridge, &writer, "slow", client_opts("main", "c1"), vec![]);
    rx.recv().await.unwrap();
    assert!(bridge.cancel(Some(&writer), "main", "c1"));
    assert_eq!(again.await.unwrap().unwrap_err().code, codes::CANCELLED);
  }

  #[tokio::test]
  async fn same_client_id_in_different_windows_is_independent() {
    let (bridge, writer, mut rx) = setup();
    let a = spawn_call(&bridge, &writer, "slow", client_opts("a", "c1"), vec![]);
    let id_a = recv(&mut rx).await["id"].as_u64().unwrap();
    let b = spawn_call(&bridge, &writer, "slow", client_opts("b", "c1"), vec![]);
    let id_b = recv(&mut rx).await["id"].as_u64().unwrap();
    assert!(bridge.cancel(Some(&writer), "b", "c1"));
    assert_eq!(b.await.unwrap().unwrap_err().code, codes::CANCELLED);
    assert_eq!(recv(&mut rx).await["id"], id_b);
    assert!(bridge.cancel(Some(&writer), "a", "c1"));
    assert_eq!(a.await.unwrap().unwrap_err().code, codes::CANCELLED);
    assert_eq!(recv(&mut rx).await["id"], id_a);
  }

  #[test]
  fn finish_keeps_newer_client_mapping() {
    let bridge = Bridge::new();
    let key = ("main".to_string(), "c1".to_string());
    let (tx, _rx) = oneshot::channel();
    bridge.pending.lock().unwrap().insert(1, Pending { tx, channels: vec![], client: Some(key.clone()) });
    bridge.clients.lock().unwrap().insert(key.clone(), 2);
    assert!(bridge.finish(1).is_some());
    assert_eq!(bridge.clients.lock().unwrap().get(&key), Some(&2));
  }

  #[tokio::test]
  async fn per_call_timeout_is_capped() {
    let (bridge, writer, mut rx) = setup();
    let opts = CallOptions { timeout: Some(Duration::from_secs(60)), ..Default::default() };
    let call = {
      let (bridge, writer) = (bridge.clone(), writer.clone());
      tokio::spawn(async move {
        bridge.call(&writer, "slow", Payload::None, vec![], opts, Duration::from_millis(10)).await
      })
    };
    rx.recv().await.unwrap();
    let err = tokio::time::timeout(Duration::from_secs(2), call).await.expect("cap not applied").unwrap().unwrap_err();
    assert_eq!(err.code, codes::TIMEOUT);
    assert!(err.message.contains("100ms"), "{}", err.message);
  }

  #[tokio::test]
  async fn channel_data_is_routed_until_response() {
    let (bridge, writer, mut rx) = setup();
    let received = Arc::new(Mutex::new(Vec::new()));
    let sink_store = received.clone();
    let sink: ChannelSink = Arc::new(move |p| sink_store.lock().unwrap().push(p));
    let call = spawn_call(&bridge, &writer, "count", CallOptions::default(), vec![("onItem".into(), sink)]);
    let req = recv(&mut rx).await;
    let id = req["id"].as_u64().unwrap();
    let channel = req["channels"]["onItem"].as_u64().unwrap();
    let data = |n: u64| {
      Frame::new(Message::ChannelData { channel, payload: Inline { value: Some(json!(n)), raw: false } })
    };
    bridge.handle(data(0), &NullSink);
    bridge.handle(data(1), &NullSink);
    bridge.handle(response(id, Payload::None), &NullSink);
    call.await.unwrap().unwrap();
    bridge.handle(data(2), &NullSink);
    assert_eq!(*received.lock().unwrap(), vec![Payload::Json(json!(0)), Payload::Json(json!(1))]);
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
