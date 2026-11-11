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
    header,
    protocol::{FrameCodec, PROTOCOL_VERSION},
    Bridge, CallOptions, ChannelSink, EventSink, Frame, FrameKind, Payload, Writer,
  },
  error::{codes, Error, Result},
  runtime::process::{self, ProcessConfig, Spawned},
};

const RESTART_DELAY: Duration = Duration::from_secs(1);
const MAX_RESTART_DELAY: Duration = Duration::from_secs(30);
const STABLE_UPTIME: Duration = Duration::from_secs(30);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

enum Exit {
  Stopped,
  Failed,
  Crashed { code: Option<i32>, uptime: Duration },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, strum::IntoStaticStr, strum::AsRefStr)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Status {
  Starting,
  Running,
  Crashed,
  Stopped,
  Failed,
}

#[derive(Debug, Clone)]
pub struct SupervisorConfig {
  pub process: ProcessConfig,
  pub handshake_timeout: Duration,
  pub invoke_timeout: Duration,
  pub shutdown_timeout: Duration,
  /// Restart python after an unexpected exit, with exponential backoff (1 s doubling up to 30 s,
  /// reset after 30 s of uptime). A failed handshake on a restarted process ends auto-restart and
  /// leaves the supervisor in [`Status::Failed`].
  pub restart_on_crash: bool,
  pub max_frame_size: usize,
  pub app_name: String,
  pub app_version: String,
}

pub struct Supervisor {
  config: SupervisorConfig,
  bridge: Bridge,
  sink: Arc<dyn EventSink>,
  status: watch::Sender<Status>,
  writer: Mutex<Option<Writer>>,
  last_error: Mutex<Option<Error>>,
  /// Shutdown requested: python is asked to exit, startup and restart backoff are aborted.
  stop: CancellationToken,
  /// Shutdown budget exhausted: the running child is killed.
  kill: CancellationToken,
}

impl Supervisor {
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

  pub fn status(&self) -> Status {
    *self.status.borrow()
  }

  pub fn start(self: &Arc<Self>) {
    *self.last_error.lock().unwrap() = None;
    self.status.send_replace(Status::Starting);
    let this = self.clone();
    tauri::async_runtime::spawn(async move { this.run().await });
  }

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
      Ok(Some(Ok(Ok(hello)))) if hello.kind == FrameKind::Hello => match hello.header::<header::Hello>().ok() {
        Some(header::Hello { protocol: PROTOCOL_VERSION }) => None,
        other => Some((
          codes::PROTOCOL_MISMATCH,
          format!("python sdk speaks protocol {:?}, expected {PROTOCOL_VERSION}", other.map(|h| h.protocol)),
        )),
      },
      Ok(Some(Ok(Ok(frame)))) => Some((codes::RUNTIME_UNAVAILABLE, format!("expected Hello, got {:?}", frame.kind))),
      Ok(Some(Ok(Err(e)) | Err(e))) => Some((codes::RUNTIME_UNAVAILABLE, format!("handshake failed: {e}"))),
      Ok(None) => Some((codes::RUNTIME_UNAVAILABLE, "python exited before handshake".into())),
      Err(_) => Some((codes::RUNTIME_UNAVAILABLE, "handshake timed out".into())),
    };
    if let Some((code, message)) = failure {
      let _ = child.kill().await;
      self.fail(code, &message);
      return Exit::Failed;
    }

    let app = header::AppInfo { name: self.config.app_name.clone(), version: self.config.app_version.clone() };
    let _ = writer.send(Frame::new(FrameKind::Ready, &header::Ready { protocol: PROTOCOL_VERSION, app }));
    *self.writer.lock().unwrap() = Some(writer);
    // Checked after publishing the writer: a concurrent shutdown either took it (and sent
    // Shutdown) or cancelled `stop` before we got here.
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

  async fn stop_child(&self, mut child: tokio::process::Child) -> Exit {
    let _ = child.kill().await;
    let _ = child.wait().await;
    self.bridge.fail_all(codes::RUNTIME_UNAVAILABLE, "python runtime stopped");
    self.status.send_replace(Status::Stopped);
    log::info!(target: "py-runtime", "python runtime stopped during startup");
    Exit::Stopped
  }

  fn fail(&self, code: &str, message: &str) {
    log::error!(target: "py-runtime", "{message}");
    *self.writer.lock().unwrap() = None;
    *self.last_error.lock().unwrap() = Some(Error::new(code, message));
    self.bridge.fail_all(code, message);
    self.status.send_replace(Status::Failed);
  }

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

  pub fn cancel(&self, window: &str, client_id: &str) -> bool {
    let writer = self.writer.lock().unwrap().clone();
    self.bridge.cancel(writer.as_ref(), window, client_id)
  }

  pub fn emit(&self, event: &str, payload: Payload, window: Option<&str>) -> Result<()> {
    let writer = self
      .writer
      .lock()
      .unwrap()
      .clone()
      .ok_or_else(|| Error::new(codes::RUNTIME_UNAVAILABLE, "python runtime is not running"))?;
    let header = header::Event { event: event.into(), target: None, window: window.map(Into::into) };
    writer.send(Frame::with_payload(FrameKind::Event, &header, payload))
  }

  pub async fn shutdown(&self) {
    self.stop.cancel();
    let budget = self.config.shutdown_timeout + SHUTDOWN_GRACE;
    let writer = self.writer.lock().unwrap().take();
    match writer {
      Some(writer) => {
        let timeout_ms = self.config.shutdown_timeout.as_millis() as u64;
        let _ = writer.send(Frame::new(FrameKind::Shutdown, &header::Shutdown { timeout_ms }));
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

