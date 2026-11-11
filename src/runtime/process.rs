use std::{path::PathBuf, process::Stdio};

use tokio::{
  io::{AsyncBufReadExt, AsyncReadExt, BufReader},
  process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
};

const MAX_LOG_LINE: u64 = 64 * 1024;

#[derive(Debug, Clone)]
pub struct ProcessConfig {
  pub program: PathBuf,
  pub args: Vec<String>,
  pub envs: Vec<(String, String)>,
  pub cwd: Option<PathBuf>,
}

pub struct Spawned {
  pub child: Child,
  pub stdin: ChildStdin,
  pub stdout: ChildStdout,
  pub stderr: ChildStderr,
}

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
