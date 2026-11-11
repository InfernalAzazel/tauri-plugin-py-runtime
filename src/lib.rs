use std::{
  path::{Component, Path, PathBuf},
  sync::Arc,
  time::Duration,
};

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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
  pub python: Option<String>,
  /// Python entry script; relative paths are resolved as described on [`Builder::script`].
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
    Payload::Json(json) => serde_json::from_str(&json).unwrap_or_else(|e| {
      log::warn!(target: "py-runtime", "invalid JSON event payload from python: {e}");
      Value::Null
    }),
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
    if let Err(e) = self.0.emit("py://exited", serde_json::json!({ "code": code })) {
      log::warn!(target: "py-runtime", "failed to emit py://exited: {e}");
    }
  }
}

/// Maps `..` to `_up_`, mirroring how the Tauri bundler lays out `bundle.resources` entries.
fn resource_relative(script: &Path) -> PathBuf {
  script
    .components()
    .map(|c| match c {
      Component::ParentDir => Component::Normal("_up_".as_ref()),
      other => other,
    })
    .collect()
}

fn resolve_script(
  script: &Path,
  cwd: Option<&Path>,
  resource_dir: Option<&Path>,
  dev: bool,
) -> std::result::Result<PathBuf, String> {
  let mut candidates = Vec::new();
  if script.is_absolute() {
    candidates.push(script.to_path_buf());
  } else {
    if dev {
      if let Some(cwd) = cwd {
        candidates.push(cwd.join(script));
      }
    }
    if let Some(dir) = resource_dir {
      candidates.push(dir.join(resource_relative(script)));
    }
  }
  if let Some(found) = candidates.iter().find(|p| p.is_file()) {
    return Ok(found.clone());
  }
  let tried = if candidates.is_empty() {
    "no candidate directories available".to_string()
  } else {
    candidates.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
  };
  Err(format!("py-runtime: Python entry script '{}' not found (tried: {tried})", script.display()))
}

struct Environment<'a> {
  cwd: Option<&'a Path>,
  resource_dir: Option<&'a Path>,
  dev: bool,
  app_name: String,
  app_version: String,
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

  /// Python entry script. Absolute paths are used as is. Relative paths are tried, first match
  /// wins: in debug builds against the current directory (`src-tauri` under `tauri dev`), then in
  /// all builds against the resource directory with every `..` replaced by `_up_` (the bundler's
  /// layout), so `"../python/main.py"` works when `bundle.resources` includes `"../python/**/*"`.
  /// Python runs with the script's directory as its working directory.
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
    let cwd = std::env::current_dir().ok();
    let resource_dir = app.path().resource_dir().ok();
    let package = app.package_info();
    self.into_config(
      file,
      &Environment {
        cwd: cwd.as_deref(),
        resource_dir: resource_dir.as_deref(),
        dev: cfg!(debug_assertions),
        app_name: package.name.clone(),
        app_version: package.version.to_string(),
      },
    )
  }

  fn into_config(self, file: Option<&Config>, env: &Environment) -> std::result::Result<SupervisorConfig, String> {
    let empty = Config::default();
    let file = file.unwrap_or(&empty);
    let script = self
      .script
      .or_else(|| file.script.clone())
      .ok_or("py-runtime: no Python entry script configured; call Builder::script or set plugins.py-runtime.script")?;
    let script = resolve_script(&script, env.cwd, env.resource_dir, env.dev)?;
    let mut args = vec![script.to_string_lossy().into_owned()];
    args.extend(self.args.or_else(|| file.args.clone()).unwrap_or_default());
    Ok(SupervisorConfig {
      process: ProcessConfig {
        program: self.python.or_else(|| file.python.clone()).unwrap_or_else(|| default_python().into()).into(),
        args,
        envs: self.envs,
        cwd: script.parent().map(Path::to_path_buf),
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
      app_name: env.app_name.clone(),
      app_version: env.app_version.clone(),
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

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::atomic::{AtomicUsize, Ordering};

  struct TempDir(PathBuf);

  impl TempDir {
    fn new() -> Self {
      static COUNTER: AtomicUsize = AtomicUsize::new(0);
      let n = COUNTER.fetch_add(1, Ordering::SeqCst);
      let dir = std::env::temp_dir().join(format!("py-runtime-test-{}-{n}", std::process::id()));
      let _ = std::fs::remove_dir_all(&dir);
      std::fs::create_dir_all(&dir).unwrap();
      Self(dir)
    }

    fn touch(&self, rel: &str) -> PathBuf {
      let path = self.0.join(rel);
      std::fs::create_dir_all(path.parent().unwrap()).unwrap();
      std::fs::write(&path, "").unwrap();
      path
    }
  }

  impl Drop for TempDir {
    fn drop(&mut self) {
      let _ = std::fs::remove_dir_all(&self.0);
    }
  }

  fn env<'a>(cwd: Option<&'a Path>, resource_dir: Option<&'a Path>, dev: bool) -> Environment<'a> {
    Environment { cwd, resource_dir, dev, app_name: "app".into(), app_version: "1.0.0".into() }
  }

  #[test]
  fn absolute_script_is_used_as_is() {
    let tmp = TempDir::new();
    let script = tmp.touch("main.py");
    assert_eq!(resolve_script(&script, None, None, false).unwrap(), script);
  }

  #[test]
  fn missing_absolute_script_is_an_error() {
    let tmp = TempDir::new();
    let script = tmp.0.join("nope.py");
    let err = resolve_script(&script, None, None, true).unwrap_err();
    assert!(err.contains(&script.display().to_string()), "{err}");
  }

  #[test]
  fn dev_relative_script_found_via_cwd() {
    let tmp = TempDir::new();
    let expected = tmp.touch("python/main.py");
    let cwd = tmp.0.join("src-tauri");
    std::fs::create_dir_all(&cwd).unwrap();
    let resources = tmp.0.join("resources");
    let found = resolve_script(Path::new("../python/main.py"), Some(&cwd), Some(&resources), true).unwrap();
    assert_eq!(found.canonicalize().unwrap(), expected.canonicalize().unwrap());
  }

  #[test]
  fn release_relative_script_found_in_resource_dir_with_up_mapping() {
    let tmp = TempDir::new();
    let cwd = tmp.0.join("src-tauri");
    tmp.touch("python/main.py");
    let expected = tmp.touch("resources/_up_/python/main.py");
    let resources = tmp.0.join("resources");
    let found = resolve_script(Path::new("../python/main.py"), Some(&cwd), Some(&resources), false).unwrap();
    assert_eq!(found, expected);
  }

  #[test]
  fn dev_falls_back_to_resource_dir() {
    let tmp = TempDir::new();
    let expected = tmp.touch("resources/main.py");
    let cwd = tmp.0.join("cwd");
    let resources = tmp.0.join("resources");
    let found = resolve_script(Path::new("main.py"), Some(&cwd), Some(&resources), true).unwrap();
    assert_eq!(found, expected);
  }

  #[test]
  fn missing_relative_script_lists_tried_paths() {
    let tmp = TempDir::new();
    let cwd = tmp.0.join("cwd");
    let resources = tmp.0.join("resources");
    let err = resolve_script(Path::new("../py/main.py"), Some(&cwd), Some(&resources), true).unwrap_err();
    assert!(err.contains(&cwd.join("../py/main.py").display().to_string()), "{err}");
    assert!(err.contains(&resources.join("_up_").join("py").join("main.py").display().to_string()), "{err}");
  }

  #[test]
  fn config_deserializes_camel_case_keys() {
    let config: Config = serde_json::from_value(serde_json::json!({
      "python": "py",
      "script": "main.py",
      "args": ["-v"],
      "invokeTimeoutMs": 5,
      "shutdownTimeoutMs": 6,
      "restartOnCrash": true
    }))
    .unwrap();
    assert_eq!(config.python.as_deref(), Some("py"));
    assert_eq!(config.script, Some(PathBuf::from("main.py")));
    assert_eq!(config.args, Some(vec!["-v".to_string()]));
    assert_eq!(config.invoke_timeout_ms, Some(5));
    assert_eq!(config.shutdown_timeout_ms, Some(6));
    assert_eq!(config.restart_on_crash, Some(true));
  }

  #[test]
  fn config_rejects_unknown_keys() {
    let err = serde_json::from_value::<Config>(serde_json::json!({ "scirpt": "main.py" })).unwrap_err();
    assert!(err.to_string().contains("scirpt"), "{err}");
  }

  #[test]
  fn builder_values_override_file_config() {
    let tmp = TempDir::new();
    let builder_script = tmp.touch("builder.py");
    tmp.touch("file.py");
    let file = Config {
      python: Some("file-python".into()),
      script: Some("file.py".into()),
      args: Some(vec!["file-arg".into()]),
      invoke_timeout_ms: Some(1000),
      shutdown_timeout_ms: Some(2000),
      restart_on_crash: Some(true),
    };
    let config = Builder::new()
      .python("builder-python")
      .script(&builder_script)
      .args(["builder-arg"])
      .invoke_timeout(Duration::from_secs(7))
      .restart_on_crash(false)
      .into_config(Some(&file), &env(Some(&tmp.0), None, true))
      .unwrap();
    assert_eq!(config.process.program, PathBuf::from("builder-python"));
    assert_eq!(config.process.args, vec![builder_script.to_string_lossy().into_owned(), "builder-arg".into()]);
    assert_eq!(config.process.cwd.as_deref(), Some(tmp.0.as_path()));
    assert_eq!(config.invoke_timeout, Duration::from_secs(7));
    assert_eq!(config.shutdown_timeout, Duration::from_millis(2000));
    assert!(!config.restart_on_crash);
    assert_eq!(config.app_name, "app");
  }

  #[test]
  fn file_config_and_defaults_apply_when_builder_is_empty() {
    let tmp = TempDir::new();
    let script = tmp.touch("file.py");
    let file = Config { script: Some("file.py".into()), invoke_timeout_ms: Some(1000), ..Default::default() };
    let config = Builder::new().into_config(Some(&file), &env(Some(&tmp.0), None, true)).unwrap();
    assert_eq!(config.process.program, PathBuf::from(default_python()));
    assert_eq!(config.process.args, vec![tmp.0.join("file.py").to_string_lossy().into_owned()]);
    assert_eq!(config.process.cwd.as_deref(), script.parent());
    assert_eq!(config.invoke_timeout, Duration::from_millis(1000));
    assert_eq!(config.shutdown_timeout, DEFAULT_SHUTDOWN_TIMEOUT);
    assert_eq!(config.handshake_timeout, DEFAULT_HANDSHAKE_TIMEOUT);
    assert!(!config.restart_on_crash);
  }

  #[test]
  fn missing_script_is_a_config_error() {
    let err = Builder::new().into_config(None, &env(None, None, true)).unwrap_err();
    assert!(err.contains("no Python entry script"), "{err}");
  }
}
