//! Python 进程的监督者：启动、握手、读循环、崩溃重启与关闭。
//!
//! 架构位置：前端 -> Tauri 命令 -> Bridge -> Writer/FrameCodec -> Python 进程；
//! [`Supervisor`] 负责进程生命周期，持有唯一的 [`Bridge`]，并在进程就绪后对外提供
//! [`Supervisor::call`]、[`Supervisor::cancel`] 与 [`Supervisor::emit`]。
//!
//! 状态流转（见 [`Status`]）：
//!
//! - `Starting` -> `Running`：Python 发送协议版本匹配的 `hello`，Rust 回复 `ready`；
//! - `Starting` -> `Failed`：进程无法启动、握手失败或超时；
//! - `Running` -> `Crashed`：进程意外退出且未开启 `restart_on_crash`；
//! - `Running` -> `Starting`：意外退出且开启了 `restart_on_crash`，按指数退避重启；
//! - `Starting` / `Running` -> `Stopped`：调用 [`Supervisor::shutdown`]。
//!
//! 不变量：[`Writer`] 仅在握手完成后发布、在进程结束或关闭时撤下；进程结束时所有进行中的调用
//! 都会被 [`Bridge::fail_all`] 终止，不会无限挂起。

use std::{
  sync::{Arc, Mutex},
  time::{Duration, Instant},
};

use futures_util::StreamExt;
use serde::Serialize;
use tokio::sync::watch;
use tokio_util::{codec::FramedRead, sync::CancellationToken};

use crate::{
  bridge::{
    protocol::{AppInfo, FrameCodec, PROTOCOL_VERSION},
    Bridge, CallOptions, ChannelSink, EventSink, Frame, Message, Payload, Writer,
  },
  error::{codes, Error, Result},
  runtime::process::{self, ProcessConfig, Spawned},
};

/// 首次重启前的等待时间，之后每次连续崩溃翻倍。
const RESTART_DELAY: Duration = Duration::from_secs(1);
/// 重启等待时间上限。
const MAX_RESTART_DELAY: Duration = Duration::from_secs(30);
/// 进程运行超过该时长后再崩溃，退避重新从 [`RESTART_DELAY`] 开始。
const STABLE_UPTIME: Duration = Duration::from_secs(30);
/// 在 `shutdown_timeout` 之外额外给 Python 的退出宽限时间，覆盖其关闭钩子之后的清理与进程退出。
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

/// 单次进程运行的结束方式。
enum Exit {
  /// 因关闭请求而停止，不再重启。
  Stopped,
  /// 启动或握手失败，不再重启。
  Failed,
  /// 意外退出，可能触发重启。
  Crashed { code: Option<i32>, uptime: Duration },
}

/// Python 运行时的状态，序列化为小写字符串返回给前端（`pyStatus()`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, strum::IntoStaticStr, strum::AsRefStr)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Status {
  /// 进程启动中、等待握手，或处于崩溃重启的退避等待中；此时的调用会等待就绪。
  Starting,
  /// 握手完成，可以处理调用。
  Running,
  /// 进程意外退出且未开启自动重启。
  Crashed,
  /// 已通过 [`Supervisor::shutdown`] 停止。
  Stopped,
  /// 启动失败、握手失败或超时；失败原因会作为后续调用的错误返回。
  Failed,
}

/// [`Supervisor`] 的完整配置，由插件 `Builder` 与 `tauri.conf.json` 合并得到。
#[derive(Debug, Clone)]
pub struct SupervisorConfig {
  /// 子进程启动参数。
  pub process: ProcessConfig,
  /// 等待 `hello` 的时长，同时也是调用等待运行时就绪的最长时间。
  pub handshake_timeout: Duration,
  /// 每次调用的默认超时。
  pub invoke_timeout: Duration,
  /// 通过 `shutdown` 帧交给 Python 的关闭预算；超过该时长再加 1 秒宽限后进程会被杀死。
  pub shutdown_timeout: Duration,
  /// 进程意外退出后是否重启。采用指数退避（从 1 秒开始翻倍，最多 30 秒，进程稳定运行 30 秒后
  /// 重置）。重启后的进程若握手失败，则停止自动重启，状态变为 [`Status::Failed`]。
  pub restart_on_crash: bool,
  /// 单帧大小上限，同时作用于收发两个方向。
  pub max_frame_size: usize,
  /// 通过 `ready` 帧告知 Python 的应用名称。
  pub app_name: String,
  /// 通过 `ready` 帧告知 Python 的应用版本。
  pub app_version: String,
}

/// Python 进程的监督者，负责其完整生命周期并转发调用、取消与事件。
///
/// 通过 [`Supervisor::new`] 创建（返回 `Arc`），[`Supervisor::start`] 启动，
/// [`Supervisor::shutdown`] 关闭。
pub struct Supervisor {
  config: SupervisorConfig,
  bridge: Bridge,
  sink: Arc<dyn EventSink>,
  status: watch::Sender<Status>,
  /// 握手完成后为 `Some`，进程结束或开始关闭时被取走。
  writer: Mutex<Option<Writer>>,
  /// 最近一次进入 `Failed` 的原因，供后续调用作为错误返回。
  last_error: Mutex<Option<Error>>,
  /// 已请求关闭：通知 Python 退出，并中止启动过程与重启退避。
  stop: CancellationToken,
  /// 关闭预算已耗尽：杀死仍在运行的子进程。
  kill: CancellationToken,
}

impl Supervisor {
  /// 创建处于 [`Status::Starting`] 的监督者，不会启动进程；需随后调用 [`Supervisor::start`]。
  pub fn new(config: SupervisorConfig, sink: Arc<dyn EventSink>) -> Arc<Self> {
    let (status, _) = watch::channel(Status::Starting);
    Arc::new(Self {
      config,
      bridge: Bridge::new(),
      sink,
      status,
      writer: Mutex::new(None),
      last_error: Mutex::new(None),
      stop: CancellationToken::new(),
      kill: CancellationToken::new(),
    })
  }

  /// 返回当前状态。
  pub fn status(&self) -> Status {
    *self.status.borrow()
  }

  /// 在后台启动 Python 进程并开始监督，立即返回。
  ///
  /// 只应调用一次：每次调用都会启动一个独立的监督循环；[`Supervisor::shutdown`] 之后再调用也
  /// 无法重新启动。
  pub fn start(self: &Arc<Self>) {
    *self.last_error.lock().unwrap() = None;
    self.status.send_replace(Status::Starting);
    let this = self.clone();
    // 插件 setup 运行在主线程，通常不处于 tokio 运行时上下文中，直接 tokio::spawn 会 panic，
    // 因此使用 Tauri 的全局运行时。
    tauri::async_runtime::spawn(async move { this.run().await });
  }

  /// 监督循环：运行进程，并在意外退出时按退避策略重启。
  async fn run(self: Arc<Self>) {
    let mut crashes = 0u32;
    loop {
      let (code, uptime) = match self.run_once().await {
        Exit::Stopped | Exit::Failed => return,
        Exit::Crashed { code, uptime } => (code, uptime),
      };
      log::error!(target: "py-runtime", "python runtime exited unexpectedly (code {code:?})");
      self.bridge.fail_all(codes::RUNTIME_EXITED, "python runtime exited");

      if !self.config.restart_on_crash {
        self.status.send_replace(Status::Crashed);
        self.sink.exited(code);
        return;
      }

      self.sink.exited(code);
      self.status.send_replace(Status::Starting);
      // 退避期间状态保持 `Starting`，新调用会等待重启完成（最多 `handshake_timeout`）。
      crashes = if uptime >= STABLE_UPTIME { 1 } else { crashes + 1 };
      let delay = RESTART_DELAY
        .saturating_mul(1 << (crashes - 1).min(5))
        .min(MAX_RESTART_DELAY);
      log::info!(target: "py-runtime", "restarting python runtime in {delay:?}");
      tokio::select! {
        _ = tokio::time::sleep(delay) => {}
        _ = self.stop.cancelled() => {}
      }
      if self.stop.is_cancelled() {
        self.status.send_replace(Status::Stopped);
        return;
      }
    }
  }

  /// 运行一次 Python 进程：启动、握手、读循环，直到进程结束。
  async fn run_once(&self) -> Exit {
    let Spawned { mut child, stdin, stdout, stderr } = match process::spawn(&self.config.process) {
      Ok(spawned) => spawned,
      Err(e) => {
        self.fail(codes::RUNTIME_UNAVAILABLE, &format!("failed to start python: {e}"));
        return Exit::Failed;
      }
    };
    process::spawn_stderr_logger(stderr);
    let writer = Writer::spawn(stdin, self.config.max_frame_size);
    let mut frames = FramedRead::new(stdout, FrameCodec::new(self.config.max_frame_size));

    let handshake = tokio::select! {
      result = tokio::time::timeout(self.config.handshake_timeout, frames.next()) => result,
      _ = self.stop.cancelled() => return self.stop_child(child).await,
    };
    let failure = match handshake {
      Ok(Some(Ok(Ok(Frame { message: Message::Hello { protocol, sdk, commands }, .. })))) => {
        if protocol == PROTOCOL_VERSION {
          log::debug!(target: "py-runtime", "python sdk {sdk} registered {} command(s)", commands.len());
          None
        } else {
          let message = format!("python sdk speaks protocol {protocol}, expected {PROTOCOL_VERSION}");
          Some((codes::PROTOCOL_MISMATCH, message))
        }
      }
      Ok(Some(Ok(Ok(frame)))) => {
        Some((codes::RUNTIME_UNAVAILABLE, format!("expected hello, got {}", <&str>::from(&frame.message))))
      }
      // 首帧无法解码，最可能是对端 SDK 使用了其他协议版本（帧格式不同）。
      Ok(Some(Ok(Err(e)))) => Some((codes::PROTOCOL_MISMATCH, format!("could not decode handshake: {e}"))),
      Ok(Some(Err(e))) => Some((codes::RUNTIME_UNAVAILABLE, format!("handshake failed: {e}"))),
      Ok(None) => Some((codes::RUNTIME_UNAVAILABLE, "python exited before handshake".into())),
      Err(_) => Some((codes::RUNTIME_UNAVAILABLE, "handshake timed out".into())),
    };
    if let Some((code, message)) = failure {
      let _ = child.kill().await;
      self.fail(code, &message);
      return Exit::Failed;
    }

    let app = AppInfo { name: self.config.app_name.clone(), version: self.config.app_version.clone() };
    let _ = writer.send(Frame::new(Message::Ready { protocol: PROTOCOL_VERSION, app }));
    *self.writer.lock().unwrap() = Some(writer);
    // 必须在发布 writer 之后检查：并发的 shutdown 要么已取走 writer（并发送了 `shutdown` 帧），
    // 要么在此之前已取消 `stop`，两种情况都不会遗漏。
    if self.stop.is_cancelled() {
      *self.writer.lock().unwrap() = None;
      return self.stop_child(child).await;
    }
    self.status.send_replace(Status::Running);
    let running_since = Instant::now();
    log::info!(target: "py-runtime", "python runtime is running");

    loop {
      tokio::select! {
        next = frames.next() => match next {
          Some(Ok(Ok(frame))) => self.bridge.handle(frame, self.sink.as_ref()),
          Some(Ok(Err(e))) => log::warn!(target: "py-runtime", "dropped frame from python: {e}"),
          // 读错误（含超限帧）后流已无法对齐，只能结束进程，按崩溃处理。
          Some(Err(e)) => {
            log::error!(target: "py-runtime", "failed to read from python: {e}");
            let _ = child.kill().await;
            break;
          }
          None => break,
        },
        _ = self.kill.cancelled() => break,
      }
    }

    *self.writer.lock().unwrap() = None;
    let exit = tokio::select! {
      status = child.wait() => status.ok(),
      _ = self.kill.cancelled() => {
        let _ = child.kill().await;
        child.wait().await.ok()
      }
    };
    let code = exit.and_then(|s| s.code());

    if self.stop.is_cancelled() {
      self.bridge.fail_all(codes::RUNTIME_UNAVAILABLE, "python runtime stopped");
      self.status.send_replace(Status::Stopped);
      log::info!(target: "py-runtime", "python runtime stopped (code {code:?})");
      return Exit::Stopped;
    }
    Exit::Crashed { code, uptime: running_since.elapsed() }
  }

  /// 启动阶段收到关闭请求：Python 尚未就绪、无法处理 `shutdown` 帧，直接杀死进程。
  async fn stop_child(&self, mut child: tokio::process::Child) -> Exit {
    let _ = child.kill().await;
    let _ = child.wait().await;
    self.bridge.fail_all(codes::RUNTIME_UNAVAILABLE, "python runtime stopped");
    self.status.send_replace(Status::Stopped);
    log::info!(target: "py-runtime", "python runtime stopped during startup");
    Exit::Stopped
  }

  /// 进入 [`Status::Failed`]，记录原因并以同一错误终止所有进行中的调用。
  fn fail(&self, code: &str, message: &str) {
    log::error!(target: "py-runtime", "{message}");
    *self.writer.lock().unwrap() = None;
    *self.last_error.lock().unwrap() = Some(Error::new(code, message));
    self.bridge.fail_all(code, message);
    self.status.send_replace(Status::Failed);
  }

  /// 等待运行时进入 `Running` 并返回其 [`Writer`]，最多等待 `handshake_timeout`。
  ///
  /// 状态离开 `Starting` 却不是 `Running` 时立即失败；`Failed` 时返回记录的失败原因，
  /// 其余情况返回 `runtime_unavailable`。
  async fn wait_running(&self) -> Result<Writer> {
    let mut rx = self.status.subscribe();
    let ready = async {
      loop {
        let status = *rx.borrow_and_update();
        if status == Status::Running {
          if let Some(writer) = self.writer.lock().unwrap().clone() {
            return Some(writer);
          }
        } else if status != Status::Starting {
          return None;
        }
        if rx.changed().await.is_err() {
          return None;
        }
      }
    };
    let writer = tokio::time::timeout(self.config.handshake_timeout, ready).await.ok().flatten();
    writer.ok_or_else(|| {
      let status = self.status();
      if status == Status::Failed {
        if let Some(error) = self.last_error.lock().unwrap().clone() {
          return error;
        }
      }
      Error::new(codes::RUNTIME_UNAVAILABLE, format!("python runtime is {}", status.as_ref()))
    })
  }

  /// 调用 Python 命令 `cmd`，必要时先等待运行时就绪。
  ///
  /// 运行时处于 `Starting` 时最多等待 `handshake_timeout`；调用本身的超时规则见 [`Bridge::call`]。
  ///
  /// # Errors
  ///
  /// - `runtime_unavailable`：运行时未就绪、已停止或已崩溃，或等待就绪超时；
  /// - `protocol_mismatch` 等：运行时处于 [`Status::Failed`] 时返回导致失败的原始错误；
  /// - `runtime_exited`：调用进行中 Python 进程退出；
  /// - 其余错误（`invalid_args`、`timeout`、`cancelled`、`frame_too_large`、Python 返回的错误等）
  ///   见 [`Bridge::call`]。
  pub async fn call(
    &self,
    cmd: &str,
    args: Payload,
    channels: Vec<(String, ChannelSink)>,
    opts: CallOptions,
  ) -> Result<Payload> {
    let writer = self.wait_running().await?;
    self.bridge.call(&writer, cmd, args, channels, opts, self.config.invoke_timeout).await
  }

  /// 取消 `window` 窗口中客户端 id 为 `client_id` 的调用，返回是否找到该调用。
  ///
  /// 运行时未运行时仍会让等待者以 `cancelled` 失败，只是不再通知 Python。详见 [`Bridge::cancel`]。
  pub fn cancel(&self, window: &str, client_id: &str) -> bool {
    let writer = self.writer.lock().unwrap().clone();
    self.bridge.cancel(writer.as_ref(), window, client_id)
  }

  /// 向 Python 发送事件，由其 `@app.listen(event)` 处理；`window` 为发送方 webview 标签。
  ///
  /// 不等待运行时就绪，也不等待 Python 处理。
  ///
  /// # Errors
  ///
  /// - `runtime_unavailable`：运行时当前未运行；
  /// - `frame_too_large`：事件帧超过大小上限。
  pub fn emit(&self, event: &str, payload: Payload, window: Option<&str>) -> Result<()> {
    let writer = self
      .writer
      .lock()
      .unwrap()
      .clone()
      .ok_or_else(|| Error::new(codes::RUNTIME_UNAVAILABLE, "python runtime is not running"))?;
    let (payload, body) = payload.split();
    let message = Message::Event { event: event.into(), target: None, window: window.map(Into::into), payload };
    writer.send(Frame { message, body })
  }

  /// 关闭运行时并等待进程退出，不可逆。
  ///
  /// 向 Python 发送带 `timeoutMs` 的 `shutdown` 帧，由其在该预算内取消进行中的调用并执行
  /// `shutdown` 钩子；若在 `shutdown_timeout` 加 1 秒宽限后仍未退出，则强制杀死进程。启动阶段或
  /// 重启退避中调用时会直接中止启动；已处于 `Crashed` / `Failed` / `Stopped` 时立即返回。
  pub async fn shutdown(&self) {
    self.stop.cancel();
    // kill 只是兜底：Python 自身在 `timeoutMs` 内会放弃未响应的处理器并退出，宽限时间用于覆盖
    // 其退出过程和管道传递的延迟，避免误杀正在正常退出的进程。
    let budget = self.config.shutdown_timeout + SHUTDOWN_GRACE;
    let writer = self.writer.lock().unwrap().take();
    match writer {
      Some(writer) => {
        let timeout_ms = self.config.shutdown_timeout.as_millis() as u64;
        let _ = writer.send(Frame::new(Message::Shutdown { timeout_ms }));
      }
      None if !matches!(self.status(), Status::Starting | Status::Running) => return,
      None => {}
    }

    if !self.wait_stopped(budget).await {
      log::warn!(target: "py-runtime", "python runtime did not exit in time, killing it");
      self.kill.cancel();
      self.wait_stopped(Duration::from_secs(1)).await;
    }
  }

  /// 等待状态进入终态（`Stopped` / `Crashed` / `Failed`），返回是否在 `timeout` 内达到。
  async fn wait_stopped(&self, timeout: Duration) -> bool {
    let mut rx = self.status.subscribe();
    let stopped = async {
      loop {
        if matches!(*rx.borrow_and_update(), Status::Stopped | Status::Crashed | Status::Failed) {
          return;
        }
        if rx.changed().await.is_err() {
          return;
        }
      }
    };
    tokio::time::timeout(timeout, stopped).await.is_ok()
  }
}

