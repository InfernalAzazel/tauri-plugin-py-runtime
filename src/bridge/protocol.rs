//! v2 帧协议：消息类型、帧编解码与载荷表示。
//!
//! 架构位置：前端 -> Tauri 命令 -> Bridge -> Writer/**FrameCodec** -> Python 进程。
//! 本模块只描述线上格式，不涉及调用状态。Python 端对应 `tauri_runtime.protocol` 中的 pydantic
//! 模型，两端以 `python-sdk/tests/fixtures/golden_frames.json` 逐字节对齐。
//!
//! 线上格式：`[u32 BE 内层长度][u32 BE 头长度][头 JSON][body]`。
//!
//! - 外层长度只计其后的字节，上限为 `max_frame_size`（默认 [`DEFAULT_MAX_FRAME_SIZE`]）；
//! - 头是以 `type` 字段区分的 JSON 对象（见 [`Message`]），未知字段被忽略；
//! - JSON 载荷内联在头的 `value` 字段，二进制载荷放在 body 并在头中标记 `raw: true`；
//!   未设置 `raw` 时 body 必须为空（见 [`Inline::join`]）。
//!
//! 不变量：头无法解码的帧只影响该帧本身，可以跳过；超限帧意味着对端异常，按致命错误处理
//! （见 [`FrameCodec`]）。

use std::collections::BTreeMap;

use bytes::{BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::codec::{Decoder, LengthDelimitedCodec, LengthDelimitedCodecError};

use crate::error::{codes, Error};

/// 当前协议版本，握手时 `hello.protocol` 必须与之相等，否则启动失败并返回 `protocol_mismatch`。
pub const PROTOCOL_VERSION: u64 = 2;
/// 默认的单帧大小上限（64 MiB），按外层长度前缀之后的字节数计算。
pub const DEFAULT_MAX_FRAME_SIZE: usize = 64 * 1024 * 1024;

/// 帧编解码过程中的错误。
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
  /// 底层 I/O 错误，包括 EOF 时帧不完整；对连接而言是致命的。
  #[error("io error: {0}")]
  Io(#[from] std::io::Error),
  /// 长度前缀超过上限（携带该上限）；对连接而言是致命的。
  #[error("frame exceeds limit of {0} bytes")]
  TooLarge(usize),
  /// 帧完整但内容非法（头 JSON 错误、未知 `type`、字段缺失或类型错误、载荷不一致），可跳过。
  #[error("malformed frame: {0}")]
  Malformed(String),
}

impl From<ProtocolError> for Error {
  fn from(e: ProtocolError) -> Self {
    match e {
      ProtocolError::TooLarge(_) => Error::new(codes::FRAME_TOO_LARGE, e.to_string()),
      _ => Error::new(codes::INTERNAL, e.to_string()),
    }
  }
}

/// `ready` 消息中携带的宿主应用信息，取自 Tauri 的 `PackageInfo`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppInfo {
  /// 应用名称。
  pub name: String,
  /// 应用版本号。
  pub version: String,
}

/// 消息头中的载荷字段：JSON 载荷内联为 `value`，二进制载荷放在帧 body 中并以 `raw: true` 标记。
///
/// 两个字段均为默认值时不会出现在序列化结果中；与 body 组合成 [`Payload`] 见 [`Inline::join`]。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Inline {
  /// 内联的 JSON 载荷；缺省表示无载荷（`null`）。
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub value: Option<Value>,
  /// 为 `true` 时载荷是帧 body 中的原始字节，此时 `value` 必须缺省。
  #[serde(default, skip_serializing_if = "std::ops::Not::not")]
  pub raw: bool,
}

/// 帧头，即以 `type` 字段区分的 JSON 对象。
///
/// 字段声明顺序即线上顺序，与 Python 模型一致（golden frames 依赖于此）；反序列化时忽略未知字段。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, strum::IntoStaticStr)]
#[serde(tag = "type", rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Message {
  /// Python → Rust：握手首帧，声明协议版本、SDK 版本与已注册的命令。
  Hello {
    protocol: u64,
    #[serde(default)]
    sdk: String,
    #[serde(default)]
    commands: Vec<String>,
  },
  /// Rust → Python：确认握手，Python 收到后执行 `ready` 钩子。
  Ready {
    protocol: u64,
    app: AppInfo,
  },
  /// Rust → Python：请求退出，`timeoutMs` 是 Python 取消调用并执行 `shutdown` 钩子的总预算。
  Shutdown {
    #[serde(rename = "timeoutMs")]
    timeout_ms: u64,
  },
  /// Rust → Python：命令调用请求。
  Request {
    /// Rust 侧分配的调用 id，在单个 [`crate::bridge::Bridge`] 内唯一。
    id: u64,
    cmd: String,
    /// 参数名 → channel id。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    channels: BTreeMap<String, u64>,
    /// 发起调用的 webview 标签。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    window: Option<String>,
    #[serde(flatten)]
    payload: Inline,
  },
  /// Python → Rust：调用成功的结果。
  Response {
    id: u64,
    #[serde(flatten)]
    payload: Inline,
  },
  /// Python → Rust：调用失败；缺省的 `code` 按 `internal` 处理。
  Error {
    id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
  },
  /// Rust → Python：取消调用，`reason` 为 `"client"` 或 `"timeout"`。
  Cancel {
    id: u64,
    reason: String,
  },
  /// 双向事件。Python → Rust 的事件携带 `target`，Rust → Python 的事件携带 `window`。
  Event {
    event: String,
    /// 目标 webview 标签；缺省时广播。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    /// 发送事件的 webview 标签。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    window: Option<String>,
    #[serde(flatten)]
    payload: Inline,
  },
  /// Python → Rust：向前端 `Channel` 推送一条数据。
  ChannelData {
    channel: u64,
    #[serde(flatten)]
    payload: Inline,
  },
  /// Python → Rust：关闭 channel，此后该 channel 的数据被丢弃。
  ChannelClose {
    channel: u64,
  },
}

/// 与线上表示无关的载荷值。
#[derive(Debug, Clone, PartialEq)]
pub enum Payload {
  /// 无载荷，对前端表现为 `null`。
  None,
  /// JSON 值。
  Json(Value),
  /// 原始字节，前端收到 `Uint8Array` / `ArrayBuffer`。
  Raw(Vec<u8>),
}

impl Payload {
  /// 拆分为消息头中的载荷字段和帧 body。
  pub fn split(self) -> (Inline, Vec<u8>) {
    match self {
      Self::None => (Inline::default(), Vec::new()),
      Self::Json(value) => (Inline { value: Some(value), raw: false }, Vec::new()),
      Self::Raw(bytes) => (Inline { value: None, raw: true }, bytes),
    }
  }
}

impl Inline {
  /// 将载荷字段与帧 body 组合为 [`Payload`]，是 [`Payload::split`] 的逆操作。
  ///
  /// # Errors
  ///
  /// 以下情况返回 [`ProtocolError::Malformed`]（转换为 [`Error`] 后错误码为 `internal`）：
  /// `raw` 为 `true` 却同时带有 `value`，或未设置 `raw` 而 body 非空。
  pub fn join(self, body: Vec<u8>) -> Result<Payload, ProtocolError> {
    match (self.raw, self.value) {
      (true, None) => Ok(Payload::Raw(body)),
      (false, value) if body.is_empty() => Ok(value.map_or(Payload::None, Payload::Json)),
      (true, Some(_)) => Err(ProtocolError::Malformed("raw payload with an inline value".into())),
      (false, _) => Err(ProtocolError::Malformed("frame body without the raw flag".into())),
    }
  }
}

/// 一个完整的协议帧：消息头加上可选的二进制 body。
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
  /// 帧头。
  pub message: Message,
  /// 帧 body，仅在载荷 `raw: true` 时非空。
  pub body: Vec<u8>,
}

impl Frame {
  /// 构造 body 为空的帧。
  pub fn new(message: Message) -> Self {
    Self { message, body: Vec::new() }
  }

  /// 编码为 `[u32 BE 头长度][头 JSON][body]`，不含外层长度前缀（由 [`length_codec`] 添加）。
  ///
  /// # Panics
  ///
  /// 仅当 [`Message`] 序列化失败时 panic；所有消息都可序列化为 JSON，实际不会发生。
  pub fn encode(&self) -> Bytes {
    let header = serde_json::to_vec(&self.message).expect("messages always serialize");
    let mut buf = BytesMut::with_capacity(4 + header.len() + self.body.len());
    buf.put_u32(header.len() as u32);
    buf.put_slice(&header);
    buf.put_slice(&self.body);
    buf.freeze()
  }

  /// 从去掉外层长度前缀的帧数据解码。
  ///
  /// # Errors
  ///
  /// 数据不足 4 字节、头长度超出帧长度或头 JSON 无法解码为 [`Message`] 时返回
  /// [`ProtocolError::Malformed`]。
  pub fn decode(buf: &[u8]) -> Result<Self, ProtocolError> {
    let (len, rest) = buf
      .split_first_chunk::<4>()
      .ok_or_else(|| ProtocolError::Malformed("frame shorter than 4 bytes".into()))?;
    let header_len = u32::from_be_bytes(*len) as usize;
    if header_len > rest.len() {
      return Err(ProtocolError::Malformed("header length exceeds frame".into()));
    }
    let (header, body) = rest.split_at(header_len);
    let message =
      serde_json::from_slice(header).map_err(|e| ProtocolError::Malformed(format!("invalid header: {e}")))?;
    Ok(Self { message, body: body.to_vec() })
  }
}

/// 外层分帧编解码器：u32 大端长度前缀，帧长上限为 `max_frame_size`。
pub fn length_codec(max_frame_size: usize) -> LengthDelimitedCodec {
  LengthDelimitedCodec::builder()
    .big_endian()
    .length_field_type::<u32>()
    .max_frame_length(max_frame_size)
    .new_codec()
}

/// 基于 [`length_codec`] 的帧解码器，用于读取 Python 的 stdout。
///
/// 对于完整但非法的帧（JSON 错误、未知消息类型、字段缺失），解码结果为 `Ok(Err(_))`：
/// 此时数据流仍对齐在帧边界上，可以继续读取。`Err(_)` 是致命错误，其中包括长度前缀超限时的
/// [`ProtocolError::TooLarge`]：双方在发送前都会检查大小，出现超限帧说明对端已损坏，因此不跳过。
pub struct FrameCodec {
  inner: LengthDelimitedCodec,
  max_frame_size: usize,
}

impl FrameCodec {
  /// 创建帧长上限为 `max_frame_size` 的解码器。
  pub fn new(max_frame_size: usize) -> Self {
    Self { inner: length_codec(max_frame_size), max_frame_size }
  }
}

impl Decoder for FrameCodec {
  type Item = Result<Frame, ProtocolError>;
  type Error = ProtocolError;

  fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, ProtocolError> {
    let chunk = self.inner.decode(src).map_err(|e| {
      if e.get_ref().is_some_and(|inner| inner.is::<LengthDelimitedCodecError>()) {
        ProtocolError::TooLarge(self.max_frame_size)
      } else {
        ProtocolError::Io(e)
      }
    })?;
    Ok(chunk.map(|buf| Frame::decode(&buf)))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;
  use tokio_util::codec::Encoder;

  fn wire(frame: &Frame) -> Vec<u8> {
    let mut buf = BytesMut::new();
    length_codec(DEFAULT_MAX_FRAME_SIZE).encode(frame.encode(), &mut buf).unwrap();
    buf.to_vec()
  }

  fn raw_wire(header: &[u8]) -> Vec<u8> {
    [&((4 + header.len()) as u32).to_be_bytes()[..], &(header.len() as u32).to_be_bytes(), header].concat()
  }

  fn decode_all(data: &[u8], max: usize) -> Vec<Result<Frame, ProtocolError>> {
    let mut codec = FrameCodec::new(max);
    let mut buf = BytesMut::from(data);
    std::iter::from_fn(|| codec.decode(&mut buf).unwrap()).collect()
  }

  fn event(name: &str) -> Frame {
    Frame::new(Message::Event { event: name.into(), target: None, window: None, payload: Inline::default() })
  }

  fn raw_response(id: u64, body: &[u8]) -> Frame {
    let (payload, body) = Payload::Raw(body.to_vec()).split();
    Frame { message: Message::Response { id, payload }, body }
  }

  /// 必须与 `python-sdk/tests/test_protocol.py` 中的 `GOLDEN_FRAMES` 保持一致。
  fn golden_frames() -> Vec<(&'static str, Frame)> {
    let json = |value: Value| Inline { value: Some(value), raw: false };
    vec![
      ("hello", Frame::new(Message::Hello { protocol: 2, sdk: "0.1.0".into(), commands: vec!["greet".into()] })),
      (
        "ready",
        Frame::new(Message::Ready { protocol: 2, app: AppInfo { name: "app".into(), version: "1.0.0".into() } }),
      ),
      ("shutdown", Frame::new(Message::Shutdown { timeout_ms: 3000 })),
      (
        "request",
        Frame::new(Message::Request {
          id: 7,
          cmd: "greet".into(),
          channels: [("onItem".into(), 3)].into(),
          window: Some("main".into()),
          payload: json(json!({"n": [1, 2.5, null], "name": "中文"})),
        }),
      ),
      ("response_raw", raw_response(7, &[0, 255])),
      (
        "error",
        Frame::new(Message::Error {
          id: 7,
          code: Some("custom".into()),
          message: Some("bad".into()),
          data: Some(json!({"x": 1})),
        }),
      ),
      ("cancel", Frame::new(Message::Cancel { id: 7, reason: "timeout".into() })),
      (
        "event",
        Frame::new(Message::Event {
          event: "tick".into(),
          target: Some("main".into()),
          window: None,
          payload: Inline::default(),
        }),
      ),
      ("channel_data", Frame::new(Message::ChannelData { channel: 3, payload: json(json!(1)) })),
      ("channel_close", Frame::new(Message::ChannelClose { channel: 3 })),
    ]
  }

  #[test]
  fn golden_frames_match_python() {
    let fixture: BTreeMap<String, String> = serde_json::from_str(include_str!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/python-sdk/tests/fixtures/golden_frames.json"
    )))
    .unwrap();
    let frames = golden_frames();
    assert_eq!(fixture.keys().collect::<Vec<_>>(), {
      let mut names: Vec<_> = frames.iter().map(|(name, _)| *name).collect();
      names.sort();
      names
    });
    for (name, frame) in frames {
      let hex = &fixture[name];
      let data: Vec<u8> = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect();
      assert_eq!(wire(&frame), data, "{name}: encoding differs");
      let decoded = decode_all(&data, DEFAULT_MAX_FRAME_SIZE).pop().unwrap().unwrap();
      assert_eq!(decoded, frame, "{name}: decoding differs");
    }
  }

  #[test]
  fn layout() {
    let data = wire(&raw_response(2, b"xy"));
    let header = br#"{"type":"response","id":2,"raw":true}"#;
    assert_eq!(&data[..4], &((4 + header.len() + 2) as u32).to_be_bytes());
    assert_eq!(&data[4..8], &(header.len() as u32).to_be_bytes());
    assert_eq!(&data[8..], [&header[..], b"xy"].concat().as_slice());
  }

  #[test]
  fn partial_input_waits_and_truncated_eof_is_fatal() {
    let data = wire(&raw_response(1, &[0, 255]));
    let mut codec = FrameCodec::new(DEFAULT_MAX_FRAME_SIZE);
    let mut buf = BytesMut::from(&data[..data.len() - 1]);
    assert!(codec.decode(&mut buf).unwrap().is_none());
    assert!(codec.decode_eof(&mut buf).is_err());
    assert!(codec.decode_eof(&mut BytesMut::new()).unwrap().is_none());
  }

  #[test]
  fn oversized_frame_is_fatal() {
    let mut buf = BytesMut::from(&wire(&raw_response(1, &[1; 100]))[..]);
    let err = FrameCodec::new(64).decode(&mut buf).unwrap_err();
    assert!(matches!(err, ProtocolError::TooLarge(64)), "{err:?}");
    assert_eq!(Error::from(err).code, codes::FRAME_TOO_LARGE);
  }

  #[test]
  fn malformed_frames_are_skipped() {
    let bad: [&[u8]; 6] = [
      br#"{"type":"bogus"}"#,
      br#"{"type":"response"}"#,
      br#"{"type":"response","id":"1"}"#,
      br#"{"id":1}"#,
      b"[1]",
      b"{not json",
    ];
    let mut data: Vec<u8> = bad.iter().flat_map(|h| raw_wire(h)).collect();
    data.extend([0, 0, 0, 4, 0, 0, 0, 9]);
    data.extend(wire(&event("ok")));
    let frames = decode_all(&data, 1024);
    assert_eq!(frames.len(), bad.len() + 2);
    for frame in &frames[..bad.len() + 1] {
      assert!(matches!(frame, Err(ProtocolError::Malformed(_))), "{frame:?}");
    }
    assert_eq!(frames.last().unwrap().as_ref().unwrap(), &event("ok"));
  }

  #[test]
  fn payload_roundtrip_and_validation() {
    for payload in [Payload::None, Payload::Json(json!({"a": [1, null]})), Payload::Raw(vec![1, 2])] {
      let (inline, body) = payload.clone().split();
      assert_eq!(inline.join(body).unwrap(), payload);
    }
    assert!(Inline { value: Some(json!(1)), raw: false }.join(vec![1]).is_err());
    assert!(Inline { value: Some(json!(1)), raw: true }.join(vec![1]).is_err());
  }

  #[test]
  fn absent_fields_are_omitted() {
    let frame = Frame::new(Message::Request {
      id: 1,
      cmd: "hi".into(),
      channels: BTreeMap::new(),
      window: None,
      payload: Inline::default(),
    });
    assert_eq!(&wire(&frame)[8..], br#"{"type":"request","id":1,"cmd":"hi"}"#);
    let message: Message = serde_json::from_str(r#"{"type":"error","id":1,"extra":true}"#).unwrap();
    assert_eq!(message, Message::Error { id: 1, code: None, message: None, data: None });
  }
}
