//! 用 Python 编写 Tauri 后端的 Tauri 2 插件（插件名 `py-runtime`）。
//!
//! 插件在 setup 阶段启动一个 Python 子进程（sidecar），通过其 stdin / stdout 上的长度前缀帧协议
//! 在前端与 Python 之间转发命令调用、事件和 channel 数据流；Python 的 stderr 按行转发到 `log`。
//! Python 侧使用 `tauri-runtime` SDK（`tauri_runtime`），前端使用 `tauri-plugin-py-runtime-api`。
//!
//! # 架构
//!
//! 前端 -> Tauri 命令（`commands`）-> [`bridge::Bridge`] -> [`bridge::Writer`] /
//! [`bridge::protocol::FrameCodec`] -> Python 进程；[`runtime::Supervisor`] 负责进程生命周期
//! （启动、握手、崩溃重启、关闭）。
//!
//! # 快速开始
//!
//! ```rust,no_run
//! use std::time::Duration;
//!
//! let _app = tauri::Builder::default().plugin(
//!   tauri_plugin_py_runtime::Builder::new()
//!     .script("../src-python/main.py")
//!     .invoke_timeout(Duration::from_secs(60))
//!     .build(),
//! );
//! ```
//!
//! 也可以只通过 `tauri.conf.json` 配置（[`Builder`] 上设置的值优先），此时使用 [`init`]：
//!
//! ```json
//! {
//!   "plugins": {
//!     "py-runtime": {
//!       "python": "python3",
//!       "script": "../src-python/main.py",
//!       "invokeTimeoutMs": 30000,
//!       "restartOnCrash": false
//!     }
//!   }
//! }
//! ```
//!
//! 前端需要在 capability 中加入 `"py-runtime:default"`。Rust 代码可通过 [`PyRuntimeExt`]
//! （`app.py_runtime().supervisor()`）直接调用 Python 命令或查询状态。
//!
//! 帧协议、错误码与进程生命周期的完整说明见仓库 README。

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

/// `tauri.conf.json` 中的 `plugins.py-runtime` 配置，[`Builder`] 上设置的值优先。
///
/// 键名为 camelCase，且拒绝未知键，拼写错误会在启动时报错而不是被静默忽略。
/// `env` 与 `handshake_timeout` 只能通过 [`Builder`] 设置。
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
  /// Python 解释器，见 [`Builder::python`]。
  pub python: Option<String>,
  /// Python 入口脚本；相对路径的解析规则见 [`Builder::script`]。
  pub script: Option<PathBuf>,
  /// Nuitka 打包产物名，见 [`Builder::bundle`]。
  pub bundle: Option<String>,
  /// 追加在脚本路径（或打包产物）之后的命令行参数。
  pub args: Option<Vec<String>>,
  /// 每次调用的默认超时（毫秒），见 [`Builder::invoke_timeout`]。
  pub invoke_timeout_ms: Option<u64>,
  /// 关闭超时（毫秒），见 [`Builder::shutdown_timeout`]。
  pub shutdown_timeout_ms: Option<u64>,
  /// 崩溃后是否自动重启，见 [`Builder::restart_on_crash`]。
  pub restart_on_crash: Option<bool>,
}

/// 由插件注册到 Tauri 状态中的运行时句柄，通过 [`PyRuntimeExt::py_runtime`] 获取。
pub struct PyRuntime {
  supervisor: Arc<Supervisor>,
}

impl PyRuntime {
  /// 返回底层的 [`Supervisor`]，可用于从 Rust 调用 Python 命令、发送事件或查询状态。
  pub fn supervisor(&self) -> &Arc<Supervisor> {
    &self.supervisor
  }
}

/// 为 [`AppHandle`]、`App`、`Window` 等实现了 [`Manager`] 的类型提供 [`PyRuntime`] 访问入口。
pub trait PyRuntimeExt<R: Runtime> {
  /// 返回插件注册的 [`PyRuntime`]。
  ///
  /// # Panics
  ///
  /// 插件尚未完成 setup（未注册 [`PyRuntime`] 状态）时 panic。
  fn py_runtime(&self) -> &PyRuntime;
}

impl<R: Runtime, T: Manager<R>> PyRuntimeExt<R> for T {
  fn py_runtime(&self) -> &PyRuntime {
    self.state::<PyRuntime>().inner()
  }
}

/// 把 Python 事件转发为 Tauri 事件 `py://<event>` 的 [`EventSink`] 实现。
struct TauriSink<R: Runtime>(AppHandle<R>);

/// Tauri 事件载荷只能是 JSON，因此二进制载荷会变成数字数组。
fn payload_to_value(payload: Payload) -> Value {
  match payload {
    Payload::None => Value::Null,
    Payload::Json(value) => value,
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

/// 把 `..` 替换为 `_up_`，与 Tauri 打包器放置 `bundle.resources` 条目的方式一致。
fn resource_relative(script: &Path) -> PathBuf {
  script
    .components()
    .map(|c| match c {
      Component::ParentDir => Component::Normal("_up_".as_ref()),
      other => other,
    })
    .collect()
}

/// 按 [`Builder::script`] 描述的规则查找入口脚本，第一个存在的文件胜出；
/// 找不到时返回列出所有尝试路径的错误信息。
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

/// 打包产物中可执行文件的文件名：Windows 追加 `.exe`。
fn bundle_executable(name: &str) -> String {
  if cfg!(windows) {
    format!("{name}.exe")
  } else {
    name.to_string()
  }
}

/// 在资源目录中查找打包产物 `<name>/<name>[.exe]`；找不到时返回包含尝试路径的错误信息。
fn resolve_bundle(name: &str, resource_dir: Option<&Path>) -> std::result::Result<PathBuf, String> {
  let dir = resource_dir.ok_or("py-runtime: resource directory unavailable, cannot locate the packaged Python backend")?;
  let path = dir.join(name).join(bundle_executable(name));
  if path.is_file() {
    Ok(path)
  } else {
    Err(format!(
      "py-runtime: packaged Python backend not found at {}; run `tauri-runtime build` and add it to bundle.resources",
      path.display()
    ))
  }
}

/// 解析配置所需的宿主环境信息，独立出来以便在不启动 Tauri 应用的情况下测试。
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

/// 插件构建器。未设置的选项回退到 `tauri.conf.json` 中的 [`Config`]，再回退到默认值。
///
/// 唯一必填项是入口脚本（[`Builder::script`] 或 `plugins.py-runtime.script`）。
///
/// # Examples
///
/// ```rust,no_run
/// use std::time::Duration;
///
/// let plugin = tauri_plugin_py_runtime::Builder::new()
///   .script("../src-python/main.py")
///   .python("python3")
///   .args(["--verbose"])
///   .env("APP_MODE", "dev")
///   .shutdown_timeout(Duration::from_secs(5))
///   .restart_on_crash(true)
///   .build::<tauri::Wry>();
/// let _app = tauri::Builder::default().plugin(plugin);
/// ```
#[derive(Debug, Default)]
pub struct Builder {
  python: Option<String>,
  script: Option<PathBuf>,
  bundle: Option<String>,
  args: Option<Vec<String>>,
  envs: Vec<(String, String)>,
  invoke_timeout: Option<Duration>,
  shutdown_timeout: Option<Duration>,
  handshake_timeout: Option<Duration>,
  restart_on_crash: Option<bool>,
}

impl Builder {
  /// 创建所有选项均未设置的构建器。
  pub fn new() -> Self {
    Self::default()
  }

  /// 设置 Python 解释器（路径或命令名），默认 `python3`（Windows 上为 `python`）。
  ///
  /// 该解释器必须能导入 `tauri_runtime`（及其依赖 `pydantic`）。
  pub fn python(mut self, program: impl Into<String>) -> Self {
    self.python = Some(program.into());
    self
  }

  /// 设置 Python 入口脚本。
  ///
  /// 绝对路径原样使用。相对路径按以下顺序尝试，第一个存在的文件胜出：仅在 debug 构建中，
  /// 相对于当前工作目录（`tauri dev` 下为 `src-tauri`）；然后在所有构建中，相对于资源目录，
  /// 并把每个 `..` 替换为 `_up_`（与打包器布局一致）。因此当 `bundle.resources` 包含
  /// `"../src-python/**/*"` 时，`"../src-python/main.py"` 在开发与发布构建中都能找到。
  /// Python 以脚本所在目录作为工作目录运行。
  pub fn script(mut self, path: impl Into<PathBuf>) -> Self {
    self.script = Some(path.into());
    self
  }

  /// 设置 Nuitka 打包产物名（即 `tauri-runtime build` 配置中的 `name`）。
  ///
  /// 仅在 release 构建中生效：运行资源目录下的 `<name>/<name>`（Windows 为 `<name>.exe`），
  /// 以其所在目录为工作目录，并忽略 [`Builder::python`] 与 [`Builder::script`]；debug 构建
  /// 仍以解释器运行脚本。产物需通过 `bundle.resources` 打进应用，找不到时 setup 失败。
  pub fn bundle(mut self, name: impl Into<String>) -> Self {
    self.bundle = Some(name.into());
    self
  }

  /// 设置追加在脚本路径（或打包产物）之后的命令行参数，覆盖配置文件中的 `args`。
  pub fn args<I, S>(mut self, args: I) -> Self
  where
    I: IntoIterator<Item = S>,
    S: Into<String>,
  {
    self.args = Some(args.into_iter().map(Into::into).collect());
    self
  }

  /// 为 Python 进程追加一个环境变量，可多次调用。
  ///
  /// `PYTHONUNBUFFERED=1` 与 `PYTHONIOENCODING=utf-8` 总会被设置，无法覆盖。
  pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
    self.envs.push((key.into(), value.into()));
    self
  }

  /// 设置每次调用的默认超时，默认 30 秒。前端的 `timeoutMs` 最多可将其放宽到该值的 10 倍。
  pub fn invoke_timeout(mut self, timeout: Duration) -> Self {
    self.invoke_timeout = Some(timeout);
    self
  }

  /// 设置 Python 执行 `shutdown` 钩子的时间预算，默认 3 秒；超过该时间再加 1 秒宽限后进程会被杀死。
  pub fn shutdown_timeout(mut self, timeout: Duration) -> Self {
    self.shutdown_timeout = Some(timeout);
    self
  }

  /// 设置 Python 启动并发送握手的时限，默认 10 秒；也是调用等待启动中的运行时就绪的最长时间。
  pub fn handshake_timeout(mut self, timeout: Duration) -> Self {
    self.handshake_timeout = Some(timeout);
    self
  }

  /// 设置 Python 意外退出后是否以指数退避自动重启，默认关闭。
  ///
  /// 详见 [`SupervisorConfig::restart_on_crash`]。
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

  /// 按“Builder > 配置文件 > 默认值”的优先级合并出 [`SupervisorConfig`]。
  fn into_config(self, file: Option<&Config>, env: &Environment) -> std::result::Result<SupervisorConfig, String> {
    let empty = Config::default();
    let file = file.unwrap_or(&empty);
    let extra = self.args.or_else(|| file.args.clone()).unwrap_or_default();
    let (program, args, cwd) = match self.bundle.or_else(|| file.bundle.clone()) {
      // release 构建运行打包产物；debug 构建即使配置了 bundle 也运行脚本，改动即时生效。
      Some(name) if !env.dev => {
        let exe = resolve_bundle(&name, env.resource_dir)?;
        let cwd = exe.parent().map(Path::to_path_buf);
        (exe, extra, cwd)
      }
      _ => {
        let script = self
          .script
          .or_else(|| file.script.clone())
          .ok_or("py-runtime: no Python entry script configured; call Builder::script or set plugins.py-runtime.script")?;
        let script = resolve_script(&script, env.cwd, env.resource_dir, env.dev)?;
        let mut args = vec![script.to_string_lossy().into_owned()];
        args.extend(extra);
        let python = self.python.or_else(|| file.python.clone()).unwrap_or_else(|| default_python().into());
        (PathBuf::from(python), args, script.parent().map(Path::to_path_buf))
      }
    };
    Ok(SupervisorConfig {
      process: ProcessConfig { program, args, envs: self.envs, cwd },
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

  /// 构建 Tauri 插件。
  ///
  /// 插件 setup 时解析配置与入口脚本、注册 [`PyRuntime`] 状态并在后台启动 Python；
  /// 应用退出（`RunEvent::Exit`）时阻塞等待 [`Supervisor::shutdown`] 完成。
  /// 未配置入口脚本或脚本不存在时 setup 失败，错误信息列出所有尝试过的路径。
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
            // 必须同步等待：Exit 事件返回后进程即退出，异步任务没有机会让 Python 执行关闭钩子。
            tauri::async_runtime::block_on(async move { supervisor.shutdown().await });
          }
        }
      })
      .build()
  }
}

/// 仅使用 `tauri.conf.json` 中的 `plugins.py-runtime` 初始化插件，等价于 `Builder::new().build()`。
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
      bundle: None,
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

  fn exe(name: &str) -> String {
    if cfg!(windows) {
      format!("{name}.exe")
    } else {
      name.to_string()
    }
  }

  #[test]
  fn release_with_bundle_runs_packaged_executable() {
    let tmp = TempDir::new();
    let resources = tmp.0.join("resources");
    let expected = tmp.touch(&format!("resources/py-backend/{}", exe("py-backend")));
    let config = Builder::new()
      .bundle("py-backend")
      .args(["--flag"])
      .into_config(None, &env(None, Some(&resources), false))
      .unwrap();
    assert_eq!(config.process.program, expected);
    assert_eq!(config.process.args, vec!["--flag".to_string()]);
    assert_eq!(config.process.cwd.as_deref(), expected.parent());
  }

  #[test]
  fn dev_with_bundle_still_runs_script() {
    let tmp = TempDir::new();
    let script = tmp.touch("main.py");
    let config = Builder::new()
      .bundle("py-backend")
      .script(&script)
      .into_config(None, &env(None, None, true))
      .unwrap();
    assert_eq!(config.process.args, vec![script.to_string_lossy().into_owned()]);
  }

  #[test]
  fn missing_bundle_lists_tried_path() {
    let tmp = TempDir::new();
    let resources = tmp.0.join("resources");
    let err = Builder::new()
      .bundle("py-backend")
      .into_config(None, &env(None, Some(&resources), false))
      .unwrap_err();
    assert!(err.contains(&resources.join("py-backend").join(exe("py-backend")).display().to_string()), "{err}");
  }

  #[test]
  fn bundle_from_file_config() {
    let tmp = TempDir::new();
    let resources = tmp.0.join("resources");
    let expected = tmp.touch(&format!("resources/app/{}", exe("app")));
    let file: Config = serde_json::from_value(serde_json::json!({ "bundle": "app" })).unwrap();
    let config = Builder::new().into_config(Some(&file), &env(None, Some(&resources), false)).unwrap();
    assert_eq!(config.process.program, expected);
  }
}
