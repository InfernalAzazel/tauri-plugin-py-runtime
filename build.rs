const COMMANDS: &[&str] = &["invoke", "cancel", "emit", "status"];

fn main() {
  tauri_plugin::Builder::new(COMMANDS).build();
}
