use serde::Serialize;
use serde_json::Value;

#[allow(dead_code)]
pub mod codes {
  pub const COMMAND_NOT_FOUND: &str = "command_not_found";
  pub const INVALID_ARGS: &str = "invalid_args";
  pub const INTERNAL: &str = "internal";
  pub const CANCELLED: &str = "cancelled";
  pub const TIMEOUT: &str = "timeout";
  pub const RUNTIME_UNAVAILABLE: &str = "runtime_unavailable";
  pub const RUNTIME_EXITED: &str = "runtime_exited";
  pub const PROTOCOL_MISMATCH: &str = "protocol_mismatch";
  pub const FRAME_TOO_LARGE: &str = "frame_too_large";
}

#[derive(Debug, Clone, PartialEq, Serialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct Error {
  pub code: String,
  pub message: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub data: Option<Value>,
}

impl Error {
  pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
    Self { code: code.into(), message: message.into(), data: None }
  }

  pub fn with_data(mut self, data: Value) -> Self {
    self.data = Some(data);
    self
  }
}

impl From<std::io::Error> for Error {
  fn from(e: std::io::Error) -> Self {
    Self::new(codes::INTERNAL, e.to_string())
  }
}

impl From<tauri::Error> for Error {
  fn from(e: tauri::Error) -> Self {
    Self::new(codes::INTERNAL, e.to_string())
  }
}

pub type Result<T> = std::result::Result<T, Error>;
