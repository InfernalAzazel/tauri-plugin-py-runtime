use std::collections::BTreeMap;

use bytes::{BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::codec::{Decoder, LengthDelimitedCodec, LengthDelimitedCodecError};

use crate::error::{codes, Error};

pub const PROTOCOL_VERSION: u64 = 2;
pub const DEFAULT_MAX_FRAME_SIZE: usize = 64 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
  #[error("io error: {0}")]
  Io(#[from] std::io::Error),
  #[error("frame exceeds limit of {0} bytes")]
  TooLarge(usize),
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppInfo {
  pub name: String,
  pub version: String,
}

/// Payload fields of a message: a JSON payload travels inline as `value`, a binary payload in
/// the frame body flagged with `raw: true`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Inline {
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub value: Option<Value>,
  #[serde(default, skip_serializing_if = "std::ops::Not::not")]
  pub raw: bool,
}

/// The frame header. Field order is the wire order and matches the Python models; unknown
/// fields are ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, strum::IntoStaticStr)]
#[serde(tag = "type", rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Message {
  Hello {
    protocol: u64,
    #[serde(default)]
    sdk: String,
    #[serde(default)]
    commands: Vec<String>,
  },
  Ready {
    protocol: u64,
    app: AppInfo,
  },
  Shutdown {
    #[serde(rename = "timeoutMs")]
    timeout_ms: u64,
  },
  Request {
    id: u64,
    cmd: String,
    /// Argument name → channel id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    channels: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    window: Option<String>,
    #[serde(flatten)]
    payload: Inline,
  },
  Response {
    id: u64,
    #[serde(flatten)]
    payload: Inline,
  },
  Error {
    id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
  },
  Cancel {
    id: u64,
    reason: String,
  },
  /// Python → Rust events carry `target`, Rust → Python events carry `window`.
  Event {
    event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    window: Option<String>,
    #[serde(flatten)]
    payload: Inline,
  },
  ChannelData {
    channel: u64,
    #[serde(flatten)]
    payload: Inline,
  },
  ChannelClose {
    channel: u64,
  },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Payload {
  None,
  Json(Value),
  Raw(Vec<u8>),
}

impl Payload {
  /// Message fields and frame body for this payload.
  pub fn split(self) -> (Inline, Vec<u8>) {
    match self {
      Self::None => (Inline::default(), Vec::new()),
      Self::Json(value) => (Inline { value: Some(value), raw: false }, Vec::new()),
      Self::Raw(bytes) => (Inline { value: None, raw: true }, bytes),
    }
  }
}

impl Inline {
  pub fn join(self, body: Vec<u8>) -> Result<Payload, ProtocolError> {
    match (self.raw, self.value) {
      (true, None) => Ok(Payload::Raw(body)),
      (false, value) if body.is_empty() => Ok(value.map_or(Payload::None, Payload::Json)),
      (true, Some(_)) => Err(ProtocolError::Malformed("raw payload with an inline value".into())),
      (false, _) => Err(ProtocolError::Malformed("frame body without the raw flag".into())),
    }
  }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
  pub message: Message,
  pub body: Vec<u8>,
}

impl Frame {
  pub fn new(message: Message) -> Self {
    Self { message, body: Vec::new() }
  }

  /// `[u32 BE header length][header JSON][body]`, without the outer length prefix.
  pub fn encode(&self) -> Bytes {
    let header = serde_json::to_vec(&self.message).expect("messages always serialize");
    let mut buf = BytesMut::with_capacity(4 + header.len() + self.body.len());
    buf.put_u32(header.len() as u32);
    buf.put_slice(&header);
    buf.put_slice(&self.body);
    buf.freeze()
  }

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

/// u32 big-endian length prefix, capped at `max_frame_size`.
pub fn length_codec(max_frame_size: usize) -> LengthDelimitedCodec {
  LengthDelimitedCodec::builder()
    .big_endian()
    .length_field_type::<u32>()
    .max_frame_length(max_frame_size)
    .new_codec()
}

/// Decodes frames on top of [`length_codec`].
///
/// Decoding yields `Ok(Err(_))` for a complete but malformed frame (bad JSON, unknown message
/// type, missing field): the stream is still aligned on a frame boundary and reading can
/// continue. `Err(_)` is fatal, including [`ProtocolError::TooLarge`] for a length prefix above
/// the limit: both sides check sizes before sending, so an oversized frame means a broken peer
/// and is not skipped.
pub struct FrameCodec {
  inner: LengthDelimitedCodec,
  max_frame_size: usize,
}

impl FrameCodec {
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

  /// Must match `GOLDEN_FRAMES` in python-sdk/tests/test_protocol.py.
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
