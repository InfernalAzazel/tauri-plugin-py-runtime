use std::{path::PathBuf, sync::Arc, time::Duration};

use serde::Deserialize;
use serde_json::Value;
use tauri::{
  plugin::{Builder as PluginBuilder, TauriPlugin},
  AppHandle, Emitter, Manager, RunEvent, Runtime,
};

pub mod bridge;
pub mod runtime;

mod commands;
mod error;

pub use error::{codes, Error, Result};

use bridge::{EventSink, Payload, DEFAULT_MAX_FRAME_SIZE};
use runtime::{ProcessConfig, Supervisor, SupervisorConfig};

const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_INVOKE_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

/// `plugins.py-runtime` in `tauri.conf.json`. Values set on [`Builder`] take precedence.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
  pub python: Option<String>,
  pub script: Option<PathBuf>,
  pub args: Option<Vec<String>>,
  pub invoke_timeout_ms: Option<u64>,
  pub shutdown_timeout_ms: Option<u64>,
  pub restart_on_crash: Option<bool>,
}

pub struct PyRuntime {
  supervisor: Arc<Supervisor>,
}

impl PyRuntime {
  pub fn supervisor(&self) -> &Arc<Supervisor> {
    &self.supervisor
  }
}

pub trait PyRuntimeExt<R: Runtime> {
  fn py_runtime(&self) -> &PyRuntime;
}

impl<R: Runtime, T: Manager<R>> PyRuntimeExt<R> for T {
  fn py_runtime(&self) -> &PyRuntime {
    self.state::<PyRuntime>().inner()
  }
}

struct TauriSink<R: Runtime>(AppHandle<R>);

fn payload_to_value(payload: Payload) -> Value {
  match payload {
    Payload::None => Value::Null,
    Payload::Json(json) => serde_json::from_str(&json).unwrap_or(Value::Null),
    Payload::Raw(bytes) => Value::from(bytes),
  }
}

impl<R: Runtime> EventSink for TauriSink<R> {
  fn emit(&self, event: &str, target: Option<&str>, payload: Payload) {
    let name = format!("py://{event}");
    let value = payload_to_value(payload);
    let result = match target {
      Some(label) => self.0.emit_to(label, &name, value),
      None => self.0.emit(&name, value),
    };
    if let Err(e) = result {
      log::warn!(target: "py-runtime", "failed to emit {name}: {e}");
    }
  }

  fn exited(&self, code: Option<i32>) {
    let _ = self.0.emit("py://exited", serde_json::json!({ "code": code }));
  }
}

fn default_python() -> &'static str {
  if cfg!(windows) {
    "python"
  } else {
    "python3"
  }
}

#[derive(Debug, Default)]
pub struct Builder {
  python: Option<String>,
  script: Option<PathBuf>,
  args: Option<Vec<String>>,
  envs: Vec<(String, String)>,
  invoke_timeout: Option<Duration>,
  shutdown_timeout: Option<Duration>,
  handshake_timeout: Option<Duration>,
  restart_on_crash: Option<bool>,
}

impl Builder {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn python(mut self, program: impl Into<String>) -> Self {
    self.python = Some(program.into());
    self
  }

  pub fn script(mut self, path: impl Into<PathBuf>) -> Self {
    self.script = Some(path.into());
    self
  }

  pub fn args<I, S>(mut self, args: I) -> Self
  where
    I: IntoIterator<Item = S>,
    S: Into<String>,
  {
    self.args = Some(args.into_iter().map(Into::into).collect());
    self
  }

  pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
    self.envs.push((key.into(), value.into()));
    self
  }

  pub fn invoke_timeout(mut self, timeout: Duration) -> Self {
    self.invoke_timeout = Some(timeout);
    self
  }

  pub fn shutdown_timeout(mut self, timeout: Duration) -> Self {
    self.shutdown_timeout = Some(timeout);
    self
  }

  pub fn handshake_timeout(mut self, timeout: Duration) -> Self {
    self.handshake_timeout = Some(timeout);
    self
  }

  pub fn restart_on_crash(mut self, enabled: bool) -> Self {
    self.restart_on_crash = Some(enabled);
    self
  }

  fn resolve<R: Runtime>(self, file: Option<&Config>, app: &AppHandle<R>) -> std::result::Result<SupervisorConfig, String> {
    let empty = Config::default();
    let file = file.unwrap_or(&empty);
    let script = self
      .script
      .or_else(|| file.script.clone())
      .ok_or("py-runtime: no Python entry script configured; call Builder::script or set plugins.py-runtime.script")?;
    let mut args = vec![script.to_string_lossy().into_owned()];
    args.extend(self.args.or_else(|| file.args.clone()).unwrap_or_default());
    let package = app.package_info();
    Ok(SupervisorConfig {
      process: ProcessConfig {
        program: self.python.or_else(|| file.python.clone()).unwrap_or_else(|| default_python().into()).into(),
        args,
        envs: self.envs,
        cwd: None,
      },
      handshake_timeout: self.handshake_timeout.unwrap_or(DEFAULT_HANDSHAKE_TIMEOUT),
      invoke_timeout: self
        .invoke_timeout
        .or(file.invoke_timeout_ms.map(Duration::from_millis))
        .unwrap_or(DEFAULT_INVOKE_TIMEOUT),
      shutdown_timeout: self
        .shutdown_timeout
        .or(file.shutdown_timeout_ms.map(Duration::from_millis))
        .unwrap_or(DEFAULT_SHUTDOWN_TIMEOUT),
      restart_on_crash: self.restart_on_crash.or(file.restart_on_crash).unwrap_or(false),
      max_frame_size: DEFAULT_MAX_FRAME_SIZE,
      app_name: package.name.clone(),
      app_version: package.version.to_string(),
    })
  }

  pub fn build<R: Runtime>(self) -> TauriPlugin<R, Option<Config>> {
    PluginBuilder::<R, Option<Config>>::new("py-runtime")
      .invoke_handler(tauri::generate_handler![
        commands::invoke,
        commands::cancel,
        commands::emit,
        commands::status
      ])
      .setup(move |app, api| {
        let config = self.resolve(api.config().as_ref(), app)?;
        let supervisor = Supervisor::new(config, Arc::new(TauriSink(app.clone())));
        supervisor.start();
        app.manage(PyRuntime { supervisor });
        Ok(())
      })
      .on_event(|app, event| {
        if let RunEvent::Exit = event {
          if let Some(runtime) = app.try_state::<PyRuntime>() {
            let supervisor = runtime.supervisor.clone();
            tauri::async_runtime::block_on(async move { supervisor.shutdown().await });
          }
        }
      })
      .build()
  }
}

/// Initializes the plugin using only `plugins.py-runtime` from `tauri.conf.json`.
pub fn init<R: Runtime>() -> TauriPlugin<R, Option<Config>> {
  Builder::new().build()
}
