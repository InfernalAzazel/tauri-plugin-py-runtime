#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  tauri::Builder::default()
    .plugin(tauri_plugin_log::Builder::new().level(log::LevelFilter::Info).build())
    .plugin(
      tauri_plugin_py_runtime::Builder::new()
        .script("../python/main.py")
        .env("PYTHONPATH", concat!(env!("CARGO_MANIFEST_DIR"), "/../../../python-sdk"))
        .env("TAURI_RUNTIME_DEBUG", "1")
        .build(),
    )
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}
