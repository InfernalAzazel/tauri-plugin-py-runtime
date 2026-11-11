//! 插件暴露给前端的 Tauri 命令：`invoke`、`cancel`、`emit`、`status`。
//!
//! 架构位置：前端 -> **Tauri 命令** -> Bridge -> Writer/FrameCodec -> Python 进程。
//! 本模块只负责把 IPC 请求转换为 [`crate::runtime::Supervisor`] 调用，不保存状态。
//!
//! `invoke` 使用原始 IPC 请求：参数体原样作为载荷（JSON 或二进制），调用元数据放在 `x-py-*`
//! 请求头中，以便二进制参数无需 base64 编码。所有命令都以调用方 webview 的标签作为窗口标识。

use std::{collections::HashMap, str::FromStr, sync::Arc, time::Duration};

use tauri::{
  command,
  http::HeaderMap,
  ipc::{Channel, InvokeBody, InvokeResponseBody, JavaScriptChannelId, Request, Response},
  Runtime, State, Webview,
};

use crate::{
  bridge::{CallOptions, ChannelSink, Payload},
  error::{codes, Error, Result},
  runtime::Status,
  PyRuntime,
};

/// Python 命令名，必填且非空。
const HEADER_CMD: &str = "x-py-cmd";
/// 前端生成的调用 id，可选，用于取消。
const HEADER_CALL_ID: &str = "x-py-call-id";
/// 本次调用的超时毫秒数，可选。
const HEADER_TIMEOUT: &str = "x-py-timeout-ms";
/// 参数名 → `JavaScriptChannelId` 的 JSON 对象，可选。
const HEADER_CHANNELS: &str = "x-py-channels";

/// 把载荷转换为 IPC 响应体：无载荷为 JSON `null`，二进制载荷原样返回。
pub(crate) fn to_response_body(payload: Payload) -> InvokeResponseBody {
  match payload {
    Payload::None => InvokeResponseBody::Json("null".into()),
    Payload::Json(value) => InvokeResponseBody::Json(value.to_string()),
    Payload::Raw(bytes) => InvokeResponseBody::Raw(bytes),
  }
}

/// 读取可选的请求头；区分“缺失”（`Ok(None)`）与“不是可见 ASCII”（`invalid_args`）。
fn header(headers: &HeaderMap, name: &str) -> Result<Option<String>> {
  headers
    .get(name)
    .map(|v| {
      v.to_str()
        .map(str::to_owned)
        .map_err(|_| Error::new(codes::INVALID_ARGS, format!("{name} header is not valid visible ASCII")))
    })
    .transpose()
}

/// 解析 `x-py-channels` 请求头；JSON 或任一 channel id 非法时返回 `invalid_args`。
fn parse_channel_ids(raw: &str) -> Result<Vec<(String, JavaScriptChannelId)>> {
  let ids: HashMap<String, String> = serde_json::from_str(raw)
    .map_err(|e| Error::new(codes::INVALID_ARGS, format!("invalid {HEADER_CHANNELS} header: {e}")))?;
  ids
    .into_iter()
    .map(|(name, id)| {
      let id = JavaScriptChannelId::from_str(&id)
        .map_err(|e| Error::new(codes::INVALID_ARGS, format!("invalid channel '{name}': {e}")))?;
      Ok((name, id))
    })
    .collect()
}

/// 把前端 channel id 绑定到调用方 webview，得到转发载荷的 [`ChannelSink`]。
///
/// 投递失败只记录警告：channel 数据是尽力而为的，不应让 Python 侧的调用失败。
fn parse_channels<R: Runtime>(raw: &str, webview: &Webview<R>) -> Result<Vec<(String, ChannelSink)>> {
  parse_channel_ids(raw)?
    .into_iter()
    .map(|(name, id)| {
      let channel: Channel<InvokeResponseBody> = id.channel_on(webview.clone());
      let sink: ChannelSink = Arc::new(move |payload| {
        if let Err(e) = channel.send(to_response_body(payload)) {
          log::warn!(target: "py-runtime", "failed to deliver channel message: {e}");
        }
      });
      Ok((name, sink))
    })
    .collect()
}

/// 前端 `pyInvoke` 对应的命令：调用 Python 命令并返回其结果。
///
/// 请求体为 JSON 时作为 JSON 参数（`null` 视为无参数），为二进制时作为原始字节参数。
///
/// # Errors
///
/// 请求头缺失或非法时返回 `invalid_args`，其余错误见 [`crate::runtime::Supervisor::call`]。
#[command]
pub(crate) async fn invoke<R: Runtime>(
  webview: Webview<R>,
  state: State<'_, PyRuntime>,
  request: Request<'_>,
) -> Result<Response> {
  let headers = request.headers();

  let cmd = header(headers, HEADER_CMD)?
    .ok_or_else(|| Error::new(codes::INVALID_ARGS, format!("missing {HEADER_CMD} header")))?;
  if cmd.is_empty() {
    return Err(Error::new(codes::INVALID_ARGS, format!("{HEADER_CMD} header is empty")));
  }
  let timeout = header(headers, HEADER_TIMEOUT)?
    .map(|v| {
      v.parse::<u64>()
        .map(Duration::from_millis)
        .map_err(|_| Error::new(codes::INVALID_ARGS, format!("invalid {HEADER_TIMEOUT} header: '{v}' is not milliseconds")))
    })
    .transpose()?;
  let channels = match header(headers, HEADER_CHANNELS)? {
    Some(raw) => parse_channels(&raw, &webview)?,
    None => Vec::new(),
  };
  let args = match request.body() {
    InvokeBody::Json(value) if value.is_null() => Payload::None,
    InvokeBody::Json(value) => Payload::Json(value.clone()),
    InvokeBody::Raw(bytes) => Payload::Raw(bytes.clone()),
  };
  let opts = CallOptions {
    client_id: header(headers, HEADER_CALL_ID)?,
    timeout,
    window: Some(webview.label().to_string()),
  };

  let result = state.supervisor().call(&cmd, args, channels, opts).await?;
  Ok(Response::new(to_response_body(result)))
}

/// 取消调用方 webview 中客户端 id 为 `id` 的调用，返回是否找到该调用。
#[command]
pub(crate) fn cancel<R: Runtime>(webview: Webview<R>, state: State<'_, PyRuntime>, id: String) -> bool {
  state.supervisor().cancel(webview.label(), &id)
}

/// 前端 `pyEmit` 对应的命令：向 Python 发送事件，`window` 为调用方 webview 标签。
///
/// # Errors
///
/// Python 未运行时返回 `runtime_unavailable`，事件帧过大时返回 `frame_too_large`。
#[command]
pub(crate) fn emit<R: Runtime>(
  webview: Webview<R>,
  state: State<'_, PyRuntime>,
  event: String,
  payload: Option<serde_json::Value>,
) -> Result<()> {
  let payload = payload.map_or(Payload::None, Payload::Json);
  state.supervisor().emit(&event, payload, Some(webview.label()))
}

/// 前端 `pyStatus` 对应的命令：返回运行时当前状态。
#[command]
pub(crate) fn status(state: State<'_, PyRuntime>) -> Status {
  state.supervisor().status()
}

#[cfg(test)]
mod tests {
  use super::*;
  use tauri::http::HeaderValue;

  #[test]
  fn parses_valid_channel_ids() {
    let Ok(mut ids) = parse_channel_ids(r#"{"progress":"__CHANNEL__:12","log":"__CHANNEL__:7"}"#) else {
      panic!("valid channel ids should parse");
    };
    ids.sort_by(|a, b| a.0.cmp(&b.0));
    let names: Vec<_> = ids.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["log", "progress"]);
  }

  #[test]
  fn rejects_invalid_channel_json() {
    let err = parse_channel_ids("not json").err().expect("should fail");
    assert_eq!(err.code, codes::INVALID_ARGS);
    assert!(err.message.contains(HEADER_CHANNELS), "{}", err.message);
  }

  #[test]
  fn rejects_channel_id_without_prefix() {
    let err = parse_channel_ids(r#"{"progress":"12"}"#).err().expect("should fail");
    assert_eq!(err.code, codes::INVALID_ARGS);
    assert!(err.message.contains("'progress'"), "{}", err.message);
  }

  #[test]
  fn header_distinguishes_missing_from_invalid() {
    let mut headers = HeaderMap::new();
    assert_eq!(header(&headers, HEADER_CMD).unwrap(), None);
    headers.insert(HEADER_CMD, HeaderValue::from_static("greet"));
    assert_eq!(header(&headers, HEADER_CMD).unwrap().as_deref(), Some("greet"));
    headers.insert(HEADER_CMD, HeaderValue::from_bytes(b"gr\xffeet").unwrap());
    let err = header(&headers, HEADER_CMD).unwrap_err();
    assert_eq!(err.code, codes::INVALID_ARGS);
    assert!(err.message.contains(HEADER_CMD), "{}", err.message);
  }
}
