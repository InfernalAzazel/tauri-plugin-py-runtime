/// The python-sdk venv (`cd python-sdk && uv sync`), which has the SDK installed with pydantic.
#[cfg(windows)]
const PYTHON: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../python-sdk/.venv/Scripts/python.exe");
#[cfg(not(windows))]
const PYTHON: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../python-sdk/.venv/bin/python");

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  tauri::Builder::default()
    .plugin(tauri_plugin_log::Builder::new().level(log::LevelFilter::Info).build())
    .plugin(
      tauri_plugin_py_runtime::Builder::new()
        .python(PYTHON)
        .script("../python/main.py")
        .env("TAURI_RUNTIME_DEBUG", "1")
        .build(),
    )
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}
