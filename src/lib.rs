use tauri::{
  plugin::{Builder, TauriPlugin},
  Manager, Runtime,
};

pub use models::*;

#[cfg(desktop)]
mod desktop;
#[cfg(mobile)]
mod mobile;

pub mod bridge;
mod commands;
mod error;
mod models;

pub use error::{Error, Result};

#[cfg(desktop)]
use desktop::PyRuntime;
#[cfg(mobile)]
use mobile::PyRuntime;

/// Extensions to [`tauri::App`], [`tauri::AppHandle`] and [`tauri::Window`] to access the py-runtime APIs.
pub trait PyRuntimeExt<R: Runtime> {
  fn py_runtime(&self) -> &PyRuntime<R>;
}

impl<R: Runtime, T: Manager<R>> crate::PyRuntimeExt<R> for T {
  fn py_runtime(&self) -> &PyRuntime<R> {
    self.state::<PyRuntime<R>>().inner()
  }
}

/// Initializes the plugin.
pub fn init<R: Runtime>() -> TauriPlugin<R> {
  Builder::new("py-runtime")
    .invoke_handler(tauri::generate_handler![commands::ping])
    .setup(|app, api| {
      #[cfg(mobile)]
      let py_runtime = mobile::init(app, api)?;
      #[cfg(desktop)]
      let py_runtime = desktop::init(app, api)?;
      app.manage(py_runtime);
      Ok(())
    })
    .build()
}
