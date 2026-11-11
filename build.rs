//! 构建脚本：为插件命令生成 Tauri 权限清单（`py-runtime:allow-<name>` 等）。

/// 插件暴露给前端的命令名，必须与 `src/lib.rs` 中 `generate_handler!` 注册的命令保持一致。
const COMMANDS: &[&str] = &["invoke", "cancel", "emit", "status"];

fn main() {
  tauri_plugin::Builder::new(COMMANDS).build();
}
