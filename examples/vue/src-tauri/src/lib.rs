#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  tauri::Builder::default()
    .plugin(tauri_plugin_log::Builder::new().level(log::LevelFilter::Info).build())
    .plugin(
      // debug 构建用 src-python/.venv（`uv sync` 创建）运行脚本；release 构建运行
      // `tauri-runtime build` 打出的 Nuitka 产物（经 bundle.resources 打包到资源目录的 py-backend/）。
      tauri_plugin_py_runtime::Builder::new()
        .script("../src-python/main.py")
        .bundle("py-backend")
        .env("TAURI_RUNTIME_DEBUG", "1")
        .build(),
    )
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}
