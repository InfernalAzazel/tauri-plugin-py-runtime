//! 插件统一的错误类型与错误码。
//!
//! 职责：定义在 Rust、Python 与前端之间共用的错误形状 `{ code, message, data? }`。
//! [`Error`] 会被序列化后原样返回给前端（前端包装为 `PyRuntimeError`），Python 端
//! `error` 帧中的 `code` / `message` / `data` 也会直接映射为 [`Error`]。
//!
//! 不变量：`code` 是稳定的机器可读字符串。插件自身只产生 [`codes`] 中列出的错误码，
//! Python 通过 `TauriError` 抛出的自定义错误码会原样透传。

use serde::Serialize;
use serde_json::Value;

/// 插件内置的错误码常量，与 README 中的错误码表一一对应。
pub mod codes {
  /// Python 端没有注册该名称的命令。
  pub const COMMAND_NOT_FOUND: &str = "command_not_found";
  /// 参数非法：缺少参数、pydantic 校验失败、请求头格式错误或客户端调用 id 重复等。
  pub const INVALID_ARGS: &str = "invalid_args";
  /// Python 中未处理的异常，或插件内部错误（I/O 失败、载荷格式不一致等）。
  pub const INTERNAL: &str = "internal";
  /// 调用被前端主动取消。
  pub const CANCELLED: &str = "cancelled";
  /// 调用超过了超时时间。
  pub const TIMEOUT: &str = "timeout";
  /// Python 运行时不可用：启动失败、握手失败或超时、已停止，或写通道已关闭。
  pub const RUNTIME_UNAVAILABLE: &str = "runtime_unavailable";
  /// 调用进行中时 Python 进程意外退出。
  pub const RUNTIME_EXITED: &str = "runtime_exited";
  /// Python SDK 使用的协议版本与本插件不一致，或首帧无法解码。
  pub const PROTOCOL_MISMATCH: &str = "protocol_mismatch";
  /// 帧大小超过上限（默认 64 MiB，见 [`crate::bridge::DEFAULT_MAX_FRAME_SIZE`]）。
  pub const FRAME_TOO_LARGE: &str = "frame_too_large";
}

/// 结构化错误，序列化为 `{ code, message, data? }` 返回给前端。
#[derive(Debug, Clone, PartialEq, Serialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct Error {
  /// 机器可读的错误码，通常取自 [`codes`]，也可能是 Python 自定义的错误码。
  pub code: String,
  /// 面向开发者的错误描述。
  pub message: String,
  /// 可选的附加数据（任意 JSON），为 `None` 时序列化结果中不包含该字段。
  #[serde(skip_serializing_if = "Option::is_none")]
  pub data: Option<Value>,
}

impl Error {
  /// 以错误码和描述构造不带 `data` 的错误。
  pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
    Self { code: code.into(), message: message.into(), data: None }
  }

  /// 附加 `data` 字段并返回自身，便于链式构造。
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

/// 以 [`Error`] 为错误类型的 `Result` 别名。
pub type Result<T> = std::result::Result<T, Error>;
