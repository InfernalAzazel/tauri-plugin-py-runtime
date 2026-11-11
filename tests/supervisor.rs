//! [`Supervisor`] 的端到端集成测试：启动真实的 Python 进程（`python-sdk/tests/fixtures/echo_app.py`
//! 或内联脚本），覆盖调用、错误、超时与取消、channel、事件、崩溃重启和关闭等行为。
//!
//! 需要可导入 `tauri_runtime` 的解释器，默认使用 `python-sdk` 下 `uv sync` 创建的虚拟环境。

use std::{
  path::PathBuf,
  sync::{Arc, Mutex},
  time::{Duration, Instant},
};

use serde_json::json;
use tauri_plugin_py_runtime::{
  bridge::{CallOptions, ChannelSink, EventSink, Payload, DEFAULT_MAX_FRAME_SIZE},
  runtime::{ProcessConfig, Status, Supervisor, SupervisorConfig},
};

#[derive(Default)]
struct Recorder {
  events: Mutex<Vec<(String, Option<String>, Payload)>>,
  exits: Mutex<Vec<Option<i32>>>,
}

impl EventSink for Recorder {
  fn emit(&self, event: &str, target: Option<&str>, payload: Payload) {
    self.events.lock().unwrap().push((event.into(), target.map(Into::into), payload));
  }
  fn exited(&self, code: Option<i32>) {
    self.exits.lock().unwrap().push(code);
  }
}

/// 优先使用 `$PYTHON`，否则使用已安装 SDK 依赖的 python-sdk 虚拟环境（`uv sync`），最后回退到系统解释器。
fn python() -> PathBuf {
  if let Ok(python) = std::env::var("PYTHON") {
    return python.into();
  }
  let venv = if cfg!(windows) { ".venv/Scripts/python.exe" } else { ".venv/bin/python" };
  let venv = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("python-sdk").join(venv);
  if venv.exists() {
    venv
  } else {
    (if cfg!(windows) { "python" } else { "python3" }).into()
  }
}

fn config(args: Vec<String>) -> SupervisorConfig {
  let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  SupervisorConfig {
    process: ProcessConfig {
      program: python(),
      args,
      envs: vec![("PYTHONPATH".into(), root.join("python-sdk").to_string_lossy().into_owned())],
      cwd: None,
    },
    handshake_timeout: Duration::from_secs(10),
    invoke_timeout: Duration::from_secs(5),
    shutdown_timeout: Duration::from_secs(3),
    restart_on_crash: false,
    max_frame_size: DEFAULT_MAX_FRAME_SIZE,
    app_name: "test".into(),
    app_version: "0.0.0".into(),
  }
}

fn start(recorder: Arc<Recorder>) -> Arc<Supervisor> {
  let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("python-sdk/tests/fixtures/echo_app.py");
  let supervisor = Supervisor::new(config(vec![fixture.to_string_lossy().into_owned()]), recorder);
  supervisor.start();
  supervisor
}

fn json_payload(value: serde_json::Value) -> Payload {
  Payload::Json(value)
}

async fn wait_for(supervisor: &Supervisor, status: Status) {
  let deadline = Instant::now() + Duration::from_secs(5);
  while supervisor.status() != status {
    assert!(Instant::now() < deadline, "status stayed {:?}", supervisor.status());
    tokio::time::sleep(Duration::from_millis(20)).await;
  }
}

#[tokio::test(flavor = "multi_thread")]
async fn json_and_raw_calls() {
  let sup = start(Default::default());
  let out = sup.call("hello", json_payload(json!({"name": "Kylin"})), vec![], CallOptions::default()).await.unwrap();
  assert_eq!(out, Payload::Json(json!("Hello Kylin")));
  let out = sup.call("reverse", Payload::Raw(vec![1, 2, 3]), vec![], CallOptions::default()).await.unwrap();
  assert_eq!(out, Payload::Raw(vec![3, 2, 1]));
  sup.shutdown().await;
  assert_eq!(sup.status(), Status::Stopped);
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_calls_do_not_block_each_other() {
  let sup = start(Default::default());
  let started = Instant::now();
  let slow = sup.call("sleep", json_payload(json!({"ms": 500})), vec![], CallOptions::default());
  let fast = async {
    let result = sup.call("sleep", json_payload(json!({"ms": 10})), vec![], CallOptions::default()).await;
    (result, started.elapsed())
  };
  let (slow, (fast, fast_elapsed)) = tokio::join!(slow, fast);
  assert_eq!(slow.unwrap(), Payload::Json(json!(500)));
  assert_eq!(fast.unwrap(), Payload::Json(json!(10)));
  assert!(fast_elapsed < Duration::from_millis(400), "fast call took {fast_elapsed:?}");
  sup.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn errors_are_structured() {
  let sup = start(Default::default());
  let err = sup.call("fail", Payload::None, vec![], CallOptions::default()).await.unwrap_err();
  assert_eq!((err.code.as_str(), err.message.as_str()), ("custom", "custom failure"));
  assert_eq!(err.data, Some(json!({"x": 1})));
  let err = sup.call("missing", Payload::None, vec![], CallOptions::default()).await.unwrap_err();
  assert_eq!(err.code, "command_not_found");
  sup.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn timeout_and_cancel() {
  let sup = start(Default::default());
  let opts = CallOptions { timeout: Some(Duration::from_millis(100)), ..Default::default() };
  let err = sup.call("sleep", json_payload(json!({"ms": 2000})), vec![], opts).await.unwrap_err();
  assert_eq!(err.code, "timeout");

  let opts = CallOptions { client_id: Some("c1".into()), window: Some("main".into()), ..Default::default() };
  let call = sup.call("sleep", json_payload(json!({"ms": 2000})), vec![], opts);
  let cancel = async {
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(sup.cancel("main", "c1"));
  };
  let (result, ()) = tokio::join!(call, cancel);
  assert_eq!(result.unwrap_err().code, "cancelled");

  let out = sup.call("hello", json_payload(json!({"name": "again"})), vec![], CallOptions::default()).await.unwrap();
  assert_eq!(out, Payload::Json(json!("Hello again")));
  sup.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn channel_streams_items() {
  let sup = start(Default::default());
  let items = Arc::new(Mutex::new(Vec::new()));
  let store = items.clone();
  let sink: ChannelSink = Arc::new(move |p| store.lock().unwrap().push(p));
  let out = sup
    .call("count", json_payload(json!({"n": 3})), vec![("onItem".into(), sink)], CallOptions::default())
    .await
    .unwrap();
  assert_eq!(out, Payload::Json(json!(3)));
  assert_eq!(
    *items.lock().unwrap(),
    vec![Payload::Json(json!(0)), Payload::Json(json!(1)), Payload::Json(json!(2))]
  );
  sup.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn events_roundtrip() {
  let recorder = Arc::new(Recorder::default());
  let sup = start(recorder.clone());
  wait_for(&sup, Status::Running).await;
  sup.emit("ping", json_payload(json!({"a": 1})), Some("main")).unwrap();
  let deadline = Instant::now() + Duration::from_secs(3);
  while recorder.events.lock().unwrap().is_empty() {
    assert!(Instant::now() < deadline, "no pong received");
    tokio::time::sleep(Duration::from_millis(20)).await;
  }
  assert_eq!(
    recorder.events.lock().unwrap()[0],
    ("pong".to_string(), Some("main".to_string()), Payload::Json(json!({"a":1})))
  );
  sup.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_fails_pending_calls_and_reports_exit() {
  let recorder = Arc::new(Recorder::default());
  let sup = start(recorder.clone());
  let err = sup.call("crash", Payload::None, vec![], CallOptions::default()).await.unwrap_err();
  assert_eq!(err.code, "runtime_exited");
  wait_for(&sup, Status::Crashed).await;
  assert_eq!(*recorder.exits.lock().unwrap(), vec![Some(3)]);
  let err = sup.call("hello", json_payload(json!({"name": "x"})), vec![], CallOptions::default()).await.unwrap_err();
  assert_eq!(err.code, "runtime_unavailable");
}

#[tokio::test(flavor = "multi_thread")]
async fn handshake_failure_marks_failed() {
  let sup = Supervisor::new(
    config(vec!["-c".into(), "import sys; sys.exit(0)".into()]),
    Arc::new(Recorder::default()),
  );
  sup.start();
  let err = sup.call("hello", Payload::None, vec![], CallOptions::default()).await.unwrap_err();
  assert_eq!(err.code, "runtime_unavailable");
  assert!(err.message.contains("exited before handshake"), "message was {:?}", err.message);
  assert_eq!(sup.status(), Status::Failed);
}

#[tokio::test(flavor = "multi_thread")]
async fn protocol_mismatch_is_reported() {
  let script = "import sys\n\
from tauri_runtime.protocol import Frame, Hello, encode\n\
sys.stdout.buffer.write(encode(Frame(Hello(protocol=99))))\n\
sys.stdout.buffer.flush()\n\
sys.stdin.read()\n";
  let sup = Supervisor::new(config(vec!["-c".into(), script.into()]), Arc::new(Recorder::default()));
  sup.start();
  let err = sup.call("hello", Payload::None, vec![], CallOptions::default()).await.unwrap_err();
  assert_eq!(err.code, "protocol_mismatch", "error was {err:?}");
  assert_eq!(sup.status(), Status::Failed);
}

#[tokio::test(flavor = "multi_thread")]
async fn undecodable_handshake_is_protocol_mismatch() {
  // 协议 v1 的 Hello 帧：类型字节 0x01，随后是头长度和头。
  let script = "import sys\n\
header = b'{\"protocol\":1}'\n\
frame = b'\\x01' + len(header).to_bytes(4, 'big') + header\n\
sys.stdout.buffer.write(len(frame).to_bytes(4, 'big') + frame)\n\
sys.stdout.buffer.flush()\n\
sys.stdin.read()\n";
  let sup = Supervisor::new(config(vec!["-c".into(), script.into()]), Arc::new(Recorder::default()));
  sup.start();
  let err = sup.call("hello", Payload::None, vec![], CallOptions::default()).await.unwrap_err();
  assert_eq!(err.code, "protocol_mismatch", "error was {err:?}");
  assert_eq!(sup.status(), Status::Failed);
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_kills_unresponsive_runtime() {
  let script = "import sys, time\n\
from tauri_runtime.protocol import PROTOCOL_VERSION, Frame, Hello, encode\n\
sys.stdout.buffer.write(encode(Frame(Hello(protocol=PROTOCOL_VERSION))))\n\
sys.stdout.buffer.flush()\n\
time.sleep(60)\n";
  let mut cfg = config(vec!["-c".into(), script.into()]);
  cfg.shutdown_timeout = Duration::from_millis(200);
  let sup = Supervisor::new(cfg, Arc::new(Recorder::default()));
  sup.start();
  wait_for(&sup, Status::Running).await;
  let started = Instant::now();
  sup.shutdown().await;
  let elapsed = started.elapsed();
  assert_eq!(sup.status(), Status::Stopped);
  assert!(elapsed >= Duration::from_millis(1200) && elapsed < Duration::from_secs(3), "shutdown took {elapsed:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_during_startup_stops_runtime() {
  let sup = start(Default::default());
  sup.shutdown().await;
  assert_eq!(sup.status(), Status::Stopped);
  let err = sup.call("hello", json_payload(json!({"name": "x"})), vec![], CallOptions::default()).await.unwrap_err();
  assert_eq!(err.code, "runtime_unavailable");
  tokio::time::sleep(Duration::from_millis(500)).await;
  assert_eq!(sup.status(), Status::Stopped);
}

#[tokio::test(flavor = "multi_thread")]
async fn non_utf8_stderr_does_not_block() {
  let sup = start(Default::default());
  let out = sup.call("noisy", Payload::None, vec![], CallOptions::default()).await.unwrap();
  assert_eq!(out, Payload::Json(json!("ok")));
  sup.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_on_crash_recovers() {
  let recorder = Arc::new(Recorder::default());
  let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("python-sdk/tests/fixtures/echo_app.py");
  let mut cfg = config(vec![fixture.to_string_lossy().into_owned()]);
  cfg.restart_on_crash = true;
  let sup = Supervisor::new(cfg, recorder.clone());
  sup.start();
  let err = sup.call("crash", Payload::None, vec![], CallOptions::default()).await.unwrap_err();
  assert_eq!(err.code, "runtime_exited");
  let out = sup.call("hello", json_payload(json!({"name": "back"})), vec![], CallOptions::default()).await.unwrap();
  assert_eq!(out, Payload::Json(json!("Hello back")));
  assert_eq!(*recorder.exits.lock().unwrap(), vec![Some(3)]);
  sup.shutdown().await;
  assert_eq!(sup.status(), Status::Stopped);
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_during_restart_backoff_stops() {
  let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("python-sdk/tests/fixtures/echo_app.py");
  let mut cfg = config(vec![fixture.to_string_lossy().into_owned()]);
  cfg.restart_on_crash = true;
  let sup = Supervisor::new(cfg, Arc::new(Recorder::default()));
  sup.start();
  let err = sup.call("crash", Payload::None, vec![], CallOptions::default()).await.unwrap_err();
  assert_eq!(err.code, "runtime_exited");
  let started = Instant::now();
  sup.shutdown().await;
  let elapsed = started.elapsed();
  assert!(elapsed < Duration::from_secs(2), "shutdown took {elapsed:?}");
  assert_eq!(sup.status(), Status::Stopped);
  tokio::time::sleep(Duration::from_millis(1500)).await;
  assert_eq!(sup.status(), Status::Stopped);
  let err = sup.call("hello", json_payload(json!({"name": "x"})), vec![], CallOptions::default()).await.unwrap_err();
  assert_eq!(err.code, "runtime_unavailable");
}
