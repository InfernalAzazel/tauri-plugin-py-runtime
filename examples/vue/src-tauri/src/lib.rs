/// 示例自己的 venv（`cd src-python && uv sync` 创建），以可编辑模式安装仓库内的 SDK。
/// 这是编译期确定的绝对路径，只适用于本机开发。
#[cfg(windows)]
const PYTHON: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../src-python/.venv/Scripts/python.exe");
#[cfg(not(windows))]
const PYTHON: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../src-python/.venv/bin/python");

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  tauri::Builder::default()
    .plugin(tauri_plugin_log::Builder::new().level(log::LevelFilter::Info).build())
    .plugin(
      tauri_plugin_py_runtime::Builder::new()
        .python(PYTHON)
        .script("../src-python/main.py")
        .env("TAURI_RUNTIME_DEBUG", "1")
        .build(),
    )
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}
