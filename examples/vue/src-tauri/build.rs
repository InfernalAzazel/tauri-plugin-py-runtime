fn main() {
  // tauri-build 要求 bundle.resources 的路径存在；Nuitka 产物由 `tauri-runtime build` 生成。
  std::fs::create_dir_all("../src-python/dist/py-backend").expect("failed to create the Python bundle directory");
  tauri_build::build()
}
