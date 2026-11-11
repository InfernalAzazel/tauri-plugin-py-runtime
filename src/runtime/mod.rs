//! Python 进程的生命周期管理。
//!
//! 架构位置：前端 -> Tauri 命令 -> Bridge -> Writer/FrameCodec -> Python 进程；
//! 本模块中的 [`Supervisor`] 负责进程生命周期（启动、握手、崩溃重启、关闭），并在进程
//! 就绪后把调用转交给 [`crate::bridge::Bridge`]。
//!
//! - [`process`]：子进程的创建与 stderr 日志转发；
//! - [`supervisor`]：状态机 [`Status`] 与 [`Supervisor`]。

pub mod process;
pub mod supervisor;

pub use process::ProcessConfig;
pub use supervisor::{Status, Supervisor, SupervisorConfig};
