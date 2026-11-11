//! Python 子进程的创建与 stderr 日志转发。
//!
//! 架构位置：由 [`crate::runtime::Supervisor`] 调用，产出的 stdin / stdout 分别交给
//! [`crate::bridge::Writer`] 与 [`crate::bridge::protocol::FrameCodec`]。
//!
//! 不变量：stdout 专用于协议帧，日志只能走 stderr（Python SDK 会把 `sys.stdout` 重定向到 stderr）。

use std::{path::PathBuf, process::Stdio};

use tokio::{
  io::{AsyncBufReadExt, AsyncReadExt, BufReader},
  process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
};

/// 单条日志的最大字节数；超长的行会被拆成多条，避免无换行的输出无限占用内存。
const MAX_LOG_LINE: u64 = 64 * 1024;

/// 启动 Python 进程所需的参数。
#[derive(Debug, Clone)]
pub struct ProcessConfig {
  /// Python 解释器路径或命令名。
  pub program: PathBuf,
  /// 命令行参数，插件构造时第一项为解析后的入口脚本路径。
  pub args: Vec<String>,
  /// 额外的环境变量；`PYTHONUNBUFFERED` 与 `PYTHONIOENCODING` 总会被强制设置。
  pub envs: Vec<(String, String)>,
  /// 工作目录；`None` 时继承当前进程的工作目录。
  pub cwd: Option<PathBuf>,
}

/// 已启动的子进程及其三个标准流。
pub struct Spawned {
  /// 子进程句柄，被丢弃时进程会被杀死（`kill_on_drop`）。
  pub child: Child,
  /// 协议写入端。
  pub stdin: ChildStdin,
  /// 协议读取端。
  pub stdout: ChildStdout,
  /// 日志输出，交给 [`spawn_stderr_logger`]。
  pub stderr: ChildStderr,
}

/// 按 `config` 启动 Python 进程，三个标准流均为管道。
///
/// Windows 上以 `CREATE_NO_WINDOW` 启动，避免弹出控制台窗口。
///
/// # Errors
///
/// 进程无法创建时（如解释器不存在）返回底层 I/O 错误。
pub fn spawn(config: &ProcessConfig) -> std::io::Result<Spawned> {
  let mut command = Command::new(&config.program);
  command
    .args(&config.args)
    .envs(config.envs.iter().map(|(k, v)| (k, v)))
    .env("PYTHONUNBUFFERED", "1")
    .env("PYTHONIOENCODING", "utf-8")
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
  if let Some(cwd) = &config.cwd {
    command.current_dir(cwd);
  }
  #[cfg(windows)]
  command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW

  let mut child = command.spawn()?;
  let stdin = child.stdin.take().expect("stdin is piped");
  let stdout = child.stdout.take().expect("stdout is piped");
  let stderr = child.stderr.take().expect("stderr is piped");
  Ok(Spawned { child, stdin, stdout, stderr })
}

/// 启动后台任务，把 stderr 逐行转发到 `log`（target 为 `py-runtime`）。
///
/// 行首的 `[DEBUG]` / `[INFO]` / `[WARN]` / `[ERROR]` 决定日志级别并被去掉，其余行按 `INFO` 记录。
///
/// 非 UTF-8 字节按有损方式解码；stderr 关闭或读取出错时任务结束。
pub fn spawn_stderr_logger(stderr: ChildStderr) {
  tauri::async_runtime::spawn(async move {
    let mut reader = BufReader::new(stderr);
    let mut buf = Vec::new();
    while let Ok(n) = (&mut reader).take(MAX_LOG_LINE).read_until(b'\n', &mut buf).await {
      if n == 0 {
        break;
      }
      let line = String::from_utf8_lossy(&buf);
      let (level, message) = parse_level(line.trim_end_matches(['\r', '\n']));
      log::log!(target: "py-runtime", level, "{message}");
      buf.clear();
    }
  });
}

/// 解析行首的 `[DEBUG]` / `[INFO]` / `[WARN]` / `[ERROR]` 前缀，返回日志级别与去掉前缀后的内容；
/// 无前缀时为 `INFO` 级别。
pub(crate) fn parse_level(line: &str) -> (log::Level, &str) {
  const PREFIXES: [(&str, log::Level); 4] = [
    ("[DEBUG]", log::Level::Debug),
    ("[INFO]", log::Level::Info),
    ("[WARN]", log::Level::Warn),
    ("[ERROR]", log::Level::Error),
  ];
  for (prefix, level) in PREFIXES {
    if let Some(rest) = line.strip_prefix(prefix) {
      return (level, rest.trim_start());
    }
  }
  (log::Level::Info, line)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_log_prefixes() {
    assert_eq!(parse_level("[WARN] disk low"), (log::Level::Warn, "disk low"));
    assert_eq!(parse_level("[ERROR]boom"), (log::Level::Error, "boom"));
    assert_eq!(parse_level("plain"), (log::Level::Info, "plain"));
  }
}
