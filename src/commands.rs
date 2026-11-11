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

const HEADER_CMD: &str = "x-py-cmd";
const HEADER_CALL_ID: &str = "x-py-call-id";
const HEADER_TIMEOUT: &str = "x-py-timeout-ms";
const HEADER_CHANNELS: &str = "x-py-channels";

pub(crate) fn to_response_body(payload: Payload) -> InvokeResponseBody {
  match payload {
    Payload::None => InvokeResponseBody::Json("null".into()),
    Payload::Json(json) => InvokeResponseBody::Json(json),
    Payload::Raw(bytes) => InvokeResponseBody::Raw(bytes),
  }
}

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
    InvokeBody::Json(value) => Payload::json(value),
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

#[command]
pub(crate) fn cancel<R: Runtime>(webview: Webview<R>, state: State<'_, PyRuntime>, id: String) -> bool {
  state.supervisor().cancel(webview.label(), &id)
}

#[command]
pub(crate) fn emit<R: Runtime>(
  webview: Webview<R>,
  state: State<'_, PyRuntime>,
  event: String,
  payload: Option<serde_json::Value>,
) -> Result<()> {
  let payload = payload.map(|v| Payload::json(&v)).unwrap_or(Payload::None);
  state.supervisor().emit(&event, payload, Some(webview.label()))
}

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
