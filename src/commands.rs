use std::{collections::HashMap, str::FromStr, sync::Arc, time::Duration};

use tauri::{
  command,
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

fn parse_channels<R: Runtime>(raw: &str, webview: &Webview<R>) -> Result<Vec<(String, ChannelSink)>> {
  let ids: HashMap<String, String> = serde_json::from_str(raw)
    .map_err(|e| Error::new(codes::INVALID_ARGS, format!("invalid {HEADER_CHANNELS} header: {e}")))?;
  ids
    .into_iter()
    .map(|(name, id)| {
      let id = JavaScriptChannelId::from_str(&id)
        .map_err(|e| Error::new(codes::INVALID_ARGS, format!("invalid channel '{name}': {e}")))?;
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
  let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);

  let cmd = header(HEADER_CMD)
    .ok_or_else(|| Error::new(codes::INVALID_ARGS, format!("missing {HEADER_CMD} header")))?;
  let timeout = header(HEADER_TIMEOUT)
    .map(|v| {
      v.parse::<u64>()
        .map(Duration::from_millis)
        .map_err(|_| Error::new(codes::INVALID_ARGS, format!("invalid {HEADER_TIMEOUT} header")))
    })
    .transpose()?;
  let channels = match header(HEADER_CHANNELS) {
    Some(raw) => parse_channels(&raw, &webview)?,
    None => Vec::new(),
  };
  let args = match request.body() {
    InvokeBody::Json(value) if value.is_null() => Payload::None,
    InvokeBody::Json(value) => Payload::json(value),
    InvokeBody::Raw(bytes) => Payload::Raw(bytes.clone()),
  };
  let opts = CallOptions {
    client_id: header(HEADER_CALL_ID),
    timeout,
    window: Some(webview.label().to_string()),
  };

  let result = state.supervisor().call(&cmd, args, channels, opts).await?;
  Ok(Response::new(to_response_body(result)))
}

#[command]
pub(crate) fn cancel(state: State<'_, PyRuntime>, id: String) -> bool {
  state.supervisor().cancel(&id)
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
