use bytes::{BufMut, BytesMut};
use serde_json::{Map, Value};
use tokio_util::codec::{Decoder, Encoder, LengthDelimitedCodec, LengthDelimitedCodecError};

use crate::error::{codes, Error};

pub const PROTOCOL_VERSION: u64 = 1;
pub const DEFAULT_MAX_FRAME_SIZE: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
  Hello = 0x01,
  Ready = 0x02,
  Shutdown = 0x03,
  Request = 0x10,
  Response = 0x11,
  Error = 0x12,
  Cancel = 0x13,
  Event = 0x20,
  ChannelData = 0x30,
  ChannelClose = 0x31,
}

impl TryFrom<u8> for FrameKind {
  type Error = ProtocolError;

  fn try_from(value: u8) -> Result<Self, ProtocolError> {
    Ok(match value {
      0x01 => Self::Hello,
      0x02 => Self::Ready,
      0x03 => Self::Shutdown,
      0x10 => Self::Request,
      0x11 => Self::Response,
      0x12 => Self::Error,
      0x13 => Self::Cancel,
      0x20 => Self::Event,
      0x30 => Self::ChannelData,
      0x31 => Self::ChannelClose,
      other => return Err(ProtocolError::Malformed(format!("unknown frame kind {other:#x}"))),
    })
  }
}

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

#[derive(Debug, Clone, PartialEq)]
pub enum Payload {
  None,
  Json(String),
  Raw(Vec<u8>),
}

impl Payload {
  pub fn json(value: &Value) -> Self {
    Self::Json(value.to_string())
  }

  pub fn encoding(&self) -> &'static str {
    match self {
      Self::None => "none",
      Self::Json(_) => "json",
      Self::Raw(_) => "raw",
    }
  }

  pub fn into_body(self) -> Vec<u8> {
    match self {
      Self::None => Vec::new(),
      Self::Json(s) => s.into_bytes(),
      Self::Raw(b) => b,
    }
  }

  pub fn from_wire(encoding: Option<&str>, body: Vec<u8>) -> Result<Self, ProtocolError> {
    match encoding.unwrap_or("none") {
      "none" => Ok(Self::None),
      "raw" => Ok(Self::Raw(body)),
      "json" => String::from_utf8(body)
        .map(Self::Json)
        .map_err(|_| ProtocolError::Malformed("json body is not utf-8".into())),
      other => Err(ProtocolError::Malformed(format!("unknown encoding {other:?}"))),
    }
  }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
  pub kind: FrameKind,
  pub header: Map<String, Value>,
  pub body: Vec<u8>,
}

impl Frame {
  pub fn new(kind: FrameKind, header: Value) -> Self {
    let header = match header {
      Value::Object(map) => map,
      _ => Map::new(),
    };
    Self { kind, header, body: Vec::new() }
  }

  pub fn with_payload(kind: FrameKind, header: Value, payload: Payload) -> Self {
    let mut frame = Self::new(kind, header);
    frame.header.insert("encoding".into(), Value::from(payload.encoding()));
    frame.body = payload.into_body();
    frame
  }

  pub fn header_str(&self, key: &str) -> Option<&str> {
    self.header.get(key).and_then(Value::as_str)
  }

  pub fn header_u64(&self, key: &str) -> Option<u64> {
    self.header.get(key).and_then(Value::as_u64)
  }

  pub fn into_payload(self) -> Result<Payload, ProtocolError> {
    let encoding = self.header_str("encoding").map(str::to_owned);
    Payload::from_wire(encoding.as_deref(), self.body)
  }
}

fn header_bytes(frame: &Frame) -> Vec<u8> {
  serde_json::to_vec(&frame.header).expect("a JSON map always serializes")
}

pub fn encoded_len(frame: &Frame) -> usize {
  1 + 4 + header_bytes(frame).len() + frame.body.len()
}

fn parse(buf: &[u8]) -> Result<Frame, ProtocolError> {
  if buf.len() < 5 {
    return Err(ProtocolError::Malformed("frame shorter than 5 bytes".into()));
  }
  let kind = FrameKind::try_from(buf[0])?;
  let header_len = u32::from_be_bytes(buf[1..5].try_into().unwrap()) as usize;
  let end = 5usize
    .checked_add(header_len)
    .filter(|end| *end <= buf.len())
    .ok_or_else(|| ProtocolError::Malformed("header length exceeds frame".into()))?;
  let header = if header_len == 0 {
    Map::new()
  } else {
    match serde_json::from_slice(&buf[5..end]) {
      Ok(Value::Object(map)) => map,
      Ok(_) => return Err(ProtocolError::Malformed("header is not an object".into())),
      Err(e) => return Err(ProtocolError::Malformed(format!("invalid header: {e}"))),
    }
  };
  Ok(Frame { kind, header, body: buf[end..].to_vec() })
}

/// Frames on top of [`LengthDelimitedCodec`] (u32 big-endian length prefix, capped at
/// `max_frame_size`).
///
/// Decoding yields `Ok(Err(_))` for a complete but malformed frame (unknown kind, bad header):
/// the stream is still aligned on a frame boundary and reading can continue. `Err(_)` is fatal,
/// including [`ProtocolError::TooLarge`] for a length prefix above the limit: both sides check
/// sizes before sending, so an oversized frame means a broken peer and is not skipped.
pub struct FrameCodec {
  inner: LengthDelimitedCodec,
  max_frame_size: usize,
}

impl FrameCodec {
  pub fn new(max_frame_size: usize) -> Self {
    let inner = LengthDelimitedCodec::builder()
      .big_endian()
      .length_field_type::<u32>()
      .max_frame_length(max_frame_size)
      .new_codec();
    Self { inner, max_frame_size }
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
    Ok(chunk.map(|buf| parse(&buf)))
  }
}

impl Encoder<Frame> for FrameCodec {
  type Error = std::io::Error;

  fn encode(&mut self, frame: Frame, dst: &mut BytesMut) -> std::io::Result<()> {
    let header = header_bytes(&frame);
    let mut buf = BytesMut::with_capacity(5 + header.len() + frame.body.len());
    buf.put_u8(frame.kind as u8);
    buf.put_u32(header.len() as u32);
    buf.put_slice(&header);
    buf.put_slice(&frame.body);
    self.inner.encode(buf.freeze(), dst)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  fn sample() -> Frame {
    Frame::with_payload(FrameKind::Request, json!({"id": 1, "cmd": "hello"}), Payload::Raw(vec![0, 255]))
  }

  fn encode(frame: &Frame) -> Vec<u8> {
    let mut buf = BytesMut::new();
    FrameCodec::new(DEFAULT_MAX_FRAME_SIZE).encode(frame.clone(), &mut buf).unwrap();
    buf.to_vec()
  }

  #[test]
  fn roundtrip() {
    let frame = sample();
    let data = encode(&frame);
    let mut buf = BytesMut::from(&data[..]);
    let got = FrameCodec::new(DEFAULT_MAX_FRAME_SIZE).decode(&mut buf).unwrap().unwrap().unwrap();
    assert_eq!(got, frame);
    assert_eq!(got.header_str("encoding"), Some("raw"));
    assert_eq!(encoded_len(&frame), data.len() - 4);
    assert!(buf.is_empty());
  }

  #[test]
  fn layout_matches_python() {
    let mut frame = Frame::new(FrameKind::Cancel, json!({"id": 2}));
    frame.body = b"xy".to_vec();
    let data = encode(&frame);
    let header = br#"{"id":2}"#;
    assert_eq!(&data[..4], &((1 + 4 + header.len() + 2) as u32).to_be_bytes());
    assert_eq!(data[4], 0x13);
    assert_eq!(&data[5..9], &(header.len() as u32).to_be_bytes());
    assert_eq!(&data[9..], [&header[..], b"xy"].concat().as_slice());
  }

  #[test]
  fn partial_input_waits_and_truncated_eof_is_fatal() {
    let data = encode(&sample());
    let mut codec = FrameCodec::new(DEFAULT_MAX_FRAME_SIZE);
    let mut buf = BytesMut::from(&data[..data.len() - 1]);
    assert!(codec.decode(&mut buf).unwrap().is_none());
    assert!(codec.decode_eof(&mut buf).is_err());
    assert!(codec.decode_eof(&mut BytesMut::new()).unwrap().is_none());
  }

  #[test]
  fn oversized_frame_is_fatal() {
    let big = Frame::with_payload(FrameKind::Event, json!({"event": "x"}), Payload::Raw(vec![1; 100]));
    let mut buf = BytesMut::from(&encode(&big)[..]);
    let err = FrameCodec::new(64).decode(&mut buf).unwrap_err();
    assert!(matches!(err, ProtocolError::TooLarge(64)), "{err:?}");
    assert_eq!(Error::from(err).code, codes::FRAME_TOO_LARGE);
  }

  #[test]
  fn malformed_frames_are_skipped() {
    let mut unknown_kind = encode(&Frame::new(FrameKind::Event, json!({})));
    unknown_kind[4] = 0x7f;
    let mut bad_json = encode(&Frame::new(FrameKind::Event, json!({"event": "x"})));
    bad_json[9] = b'[';
    let good = Frame::new(FrameKind::Event, json!({"event": "ok"}));
    let mut buf = BytesMut::from(&[unknown_kind, bad_json, encode(&good)].concat()[..]);
    let mut codec = FrameCodec::new(1024);
    assert!(matches!(codec.decode(&mut buf).unwrap(), Some(Err(ProtocolError::Malformed(_)))));
    assert!(matches!(codec.decode(&mut buf).unwrap(), Some(Err(ProtocolError::Malformed(_)))));
    assert_eq!(codec.decode(&mut buf).unwrap().unwrap().unwrap(), good);
  }

  #[test]
  fn payload_from_wire() {
    assert_eq!(Payload::from_wire(None, vec![]).unwrap(), Payload::None);
    assert_eq!(Payload::from_wire(Some("json"), b"1".to_vec()).unwrap(), Payload::Json("1".into()));
    assert_eq!(Payload::from_wire(Some("raw"), vec![1]).unwrap(), Payload::Raw(vec![1]));
    assert!(Payload::from_wire(Some("json"), vec![0xff]).is_err());
    assert!(Payload::from_wire(Some("yaml"), vec![]).is_err());
  }
}
