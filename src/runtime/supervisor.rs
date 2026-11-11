use std::{
  sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
  },
  time::Duration,
};

use serde::Serialize;
use serde_json::json;
use tokio::{
  io::BufReader,
  sync::{watch, Notify},
};

use crate::{
  bridge::{
    protocol::{read_frame, PROTOCOL_VERSION},
    Bridge, CallOptions, ChannelSink, EventSink, Frame, FrameKind, Payload, Writer,
  },
  error::{codes, Error, Result},
  runtime::process::{self, ProcessConfig, Spawned},
};

const RESTART_DELAY: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
  Starting,
  Running,
  Crashed,
  Stopped,
  Failed,
}

impl Status {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Starting => "starting",
      Self::Running => "running",
      Self::Crashed => "crashed",
      Self::Stopped => "stopped",
      Self::Failed => "failed",
    }
  }
}

#[derive(Debug, Clone)]
pub struct SupervisorConfig {
  pub process: ProcessConfig,
  pub handshake_timeout: Duration,
  pub invoke_timeout: Duration,
  pub shutdown_timeout: Duration,
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
  stopping: AtomicBool,
  kill: Notify,
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
      stopping: AtomicBool::new(false),
      kill: Notify::new(),
    })
  }

  pub fn status(&self) -> Status {
    *self.status.borrow()
  }

  pub fn start(self: &Arc<Self>) {
    self.status.send_replace(Status::Starting);
    let this = self.clone();
    tauri::async_runtime::spawn(async move { this.run().await });
  }

  async fn run(self: Arc<Self>) {
    let Spawned { mut child, stdin, stdout, stderr } = match process::spawn(&self.config.process) {
      Ok(spawned) => spawned,
      Err(e) => {
        self.fail(codes::RUNTIME_UNAVAILABLE, &format!("failed to start python: {e}"));
        return;
      }
    };
    process::spawn_stderr_logger(stderr);
    let writer = Writer::spawn(stdin, self.config.max_frame_size);
    let mut reader = BufReader::new(stdout);
    let max = self.config.max_frame_size;

    let handshake = tokio::time::timeout(self.config.handshake_timeout, read_frame(&mut reader, max)).await;
    let failure = match handshake {
      Ok(Ok(Some(hello))) if hello.kind == FrameKind::Hello => match hello.header_u64("protocol") {
        Some(PROTOCOL_VERSION) => None,
        other => Some((
          codes::PROTOCOL_MISMATCH,
          format!("python sdk speaks protocol {other:?}, expected {PROTOCOL_VERSION}"),
        )),
      },
      Ok(Ok(Some(frame))) => Some((codes::RUNTIME_UNAVAILABLE, format!("expected Hello, got {:?}", frame.kind))),
      Ok(Ok(None)) => Some((codes::RUNTIME_UNAVAILABLE, "python exited before handshake".into())),
      Ok(Err(e)) => Some((codes::RUNTIME_UNAVAILABLE, format!("handshake failed: {e}"))),
      Err(_) => Some((codes::RUNTIME_UNAVAILABLE, "handshake timed out".into())),
    };
    if let Some((code, message)) = failure {
      let _ = child.kill().await;
      self.fail(code, &message);
      return;
    }

    let ready = json!({
      "protocol": PROTOCOL_VERSION,
      "app": { "name": self.config.app_name, "version": self.config.app_version },
    });
    let _ = writer.send(Frame::new(FrameKind::Ready, ready));
    *self.writer.lock().unwrap() = Some(writer);
    self.status.send_replace(Status::Running);
    log::info!(target: "py-runtime", "python runtime is running");

    loop {
      tokio::select! {
        result = read_frame(&mut reader, max) => match result {
          Ok(Some(frame)) => self.bridge.handle(frame, self.sink.as_ref()),
          Ok(None) => break,
          Err(e) if e.is_recoverable() => log::warn!(target: "py-runtime", "dropped frame from python: {e}"),
          Err(e) => {
            log::error!(target: "py-runtime", "failed to read from python: {e}");
            break;
          }
        },
        _ = self.kill.notified() => {
          let _ = child.kill().await;
          break;
        }
      }
    }

    *self.writer.lock().unwrap() = None;
    let exit = tokio::select! {
      status = child.wait() => status.ok(),
      _ = self.kill.notified() => {
        let _ = child.kill().await;
        child.wait().await.ok()
      }
    };
    let code = exit.and_then(|s| s.code());

    if self.stopping.load(Ordering::SeqCst) {
      self.bridge.fail_all(codes::RUNTIME_UNAVAILABLE, "python runtime stopped");
      self.status.send_replace(Status::Stopped);
      log::info!(target: "py-runtime", "python runtime stopped (code {code:?})");
      return;
    }

    log::error!(target: "py-runtime", "python runtime exited unexpectedly (code {code:?})");
    self.bridge.fail_all(codes::RUNTIME_EXITED, "python runtime exited");
    self.status.send_replace(Status::Crashed);
    self.sink.exited(code);

    if self.config.restart_on_crash {
      tokio::time::sleep(RESTART_DELAY).await;
      if !self.stopping.load(Ordering::SeqCst) {
        self.start();
      }
    }
  }

  fn fail(&self, code: &str, message: &str) {
    log::error!(target: "py-runtime", "{message}");
    *self.writer.lock().unwrap() = None;
    self.bridge.fail_all(code, message);
    self.status.send_replace(Status::Failed);
  }

  async fn wait_running(&self) -> Result<Writer> {
    let mut rx = self.status.subscribe();
    let ready = async {
      loop {
        let status = *rx.borrow_and_update();
        match status {
          Status::Running => return true,
          Status::Starting => {
            if rx.changed().await.is_err() {
              return false;
            }
          }
          _ => return false,
        }
      }
    };
    let running = tokio::time::timeout(self.config.handshake_timeout, ready).await.unwrap_or(false);
    let writer = if running { self.writer.lock().unwrap().clone() } else { None };
    writer.ok_or_else(|| {
      Error::new(
        codes::RUNTIME_UNAVAILABLE,
        format!("python runtime is {}", self.status().as_str()),
      )
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

  pub fn cancel(&self, client_id: &str) -> bool {
    let writer = self.writer.lock().unwrap().clone();
    self.bridge.cancel(writer.as_ref(), client_id)
  }

  pub fn emit(&self, event: &str, payload: Payload, window: Option<&str>) -> Result<()> {
    let writer = self
      .writer
      .lock()
      .unwrap()
      .clone()
      .ok_or_else(|| Error::new(codes::RUNTIME_UNAVAILABLE, "python runtime is not running"))?;
    let mut header = json!({ "event": event });
    if let Some(window) = window {
      header["window"] = json!(window);
    }
    writer.send(Frame::with_payload(FrameKind::Event, header, payload))
  }

  pub async fn shutdown(&self) {
    self.stopping.store(true, Ordering::SeqCst);
    let writer = self.writer.lock().unwrap().take();
    let Some(writer) = writer else { return };
    let timeout_ms = self.config.shutdown_timeout.as_millis() as u64;
    let _ = writer.send(Frame::new(FrameKind::Shutdown, json!({ "timeoutMs": timeout_ms })));
    drop(writer);

    if !self.wait_stopped(self.config.shutdown_timeout).await {
      log::warn!(target: "py-runtime", "python runtime did not exit in time, killing it");
      self.kill.notify_one();
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
