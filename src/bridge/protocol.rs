use serde_json::{Map, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

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
  #[error("frame of {0} bytes exceeds limit")]
  TooLarge(usize),
  #[error("malformed frame: {0}")]
  Malformed(String),
}

impl ProtocolError {
  /// The stream is still aligned on a frame boundary and reading can continue.
  pub fn is_recoverable(&self) -> bool {
    !matches!(self, Self::Io(_))
  }
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

pub fn encode(frame: &Frame) -> Vec<u8> {
  let header = header_bytes(frame);
  let inner = 1 + 4 + header.len() + frame.body.len();
  let mut out = Vec::with_capacity(4 + inner);
  out.extend_from_slice(&(inner as u32).to_be_bytes());
  out.push(frame.kind as u8);
  out.extend_from_slice(&(header.len() as u32).to_be_bytes());
  out.extend_from_slice(&header);
  out.extend_from_slice(&frame.body);
  out
}

pub fn decode(buf: &[u8]) -> Result<Frame, ProtocolError> {
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

pub async fn read_frame<R: AsyncRead + Unpin>(
  reader: &mut R,
  max_size: usize,
) -> Result<Option<Frame>, ProtocolError> {
  let mut prefix = [0u8; 4];
  match reader.read_exact(&mut prefix).await {
    Ok(_) => {}
    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
    Err(e) => return Err(e.into()),
  }
  let size = u32::from_be_bytes(prefix) as usize;
  if size > max_size {
    let mut limited = (&mut *reader).take(size as u64);
    let skipped = tokio::io::copy(&mut limited, &mut tokio::io::sink()).await?;
    if skipped as usize != size {
      return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
    }
    return Err(ProtocolError::TooLarge(size));
  }
  let mut buf = vec![0u8; size];
  reader.read_exact(&mut buf).await?;
  decode(&buf).map(Some)
}

pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, frame: &Frame) -> std::io::Result<()> {
  writer.write_all(&encode(frame)).await?;
  writer.flush().await
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  fn sample() -> Frame {
    Frame::with_payload(FrameKind::Request, json!({"id": 1, "cmd": "hello"}), Payload::Raw(vec![0, 255]))
  }

  #[tokio::test]
  async fn roundtrip() {
    let frame = sample();
    let data = encode(&frame);
    let got = read_frame(&mut &data[..], DEFAULT_MAX_FRAME_SIZE).await.unwrap().unwrap();
    assert_eq!(got, frame);
    assert_eq!(got.header_str("encoding"), Some("raw"));
    assert_eq!(encoded_len(&frame), data.len() - 4);
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

  #[tokio::test]
  async fn eof_returns_none() {
    let empty: &[u8] = &[];
    assert!(read_frame(&mut &empty[..], 1024).await.unwrap().is_none());
  }

  #[tokio::test]
  async fn too_large_frame_is_skipped() {
    let big = Frame::with_payload(FrameKind::Event, json!({"event": "x"}), Payload::Raw(vec![1; 100]));
    let small = Frame::new(FrameKind::Event, json!({"event": "y"}));
    let data = [encode(&big), encode(&small)].concat();
    let mut reader = &data[..];
    let err = read_frame(&mut reader, 64).await.unwrap_err();
    assert!(matches!(err, ProtocolError::TooLarge(_)));
    assert!(err.is_recoverable());
    assert_eq!(read_frame(&mut reader, 64).await.unwrap().unwrap(), small);
  }

  #[tokio::test]
  async fn unknown_kind_is_recoverable() {
    let mut bad = encode(&Frame::new(FrameKind::Event, json!({})));
    bad[4] = 0x7f;
    let good = Frame::new(FrameKind::Event, json!({"event": "ok"}));
    let data = [bad, encode(&good)].concat();
    let mut reader = &data[..];
    assert!(read_frame(&mut reader, 1024).await.unwrap_err().is_recoverable());
    assert_eq!(read_frame(&mut reader, 1024).await.unwrap().unwrap(), good);
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
