//! Typed frame headers. Field names are the wire names; unknown fields are ignored.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
  pub protocol: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppInfo {
  pub name: String,
  pub version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ready {
  pub protocol: u64,
  pub app: AppInfo,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Shutdown {
  pub timeout_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
  pub id: u64,
  pub cmd: String,
  /// Argument name → channel id.
  #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
  pub channels: BTreeMap<String, u64>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub window: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
  pub id: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Error {
  pub id: u64,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub code: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub message: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub data: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cancel {
  pub id: u64,
  pub reason: String,
}

/// Python → Rust events carry `target`, Rust → Python events carry `window`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
  pub event: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub target: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub window: Option<String>,
}

/// Header of both `ChannelData` and `ChannelClose`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Channel {
  pub channel: u64,
}
