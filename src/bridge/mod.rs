//! Rust 与 Python 进程之间的通信层。
//!
//! 架构位置：前端 -> Tauri 命令 -> **Bridge -> Writer/FrameCodec** -> Python 进程。
//! 进程的启动、握手与重启由 [`crate::runtime::Supervisor`] 负责，本模块只处理已建立的连接：
//!
//! - [`protocol`]：v2 帧格式与消息类型（[`Message`]、[`Frame`]、[`protocol::FrameCodec`]）；
//! - [`transport`]：单写者的帧发送端 [`Writer`]；
//! - [`invoke`]：调用路由 [`Bridge`]，负责请求/响应配对、超时、取消以及 channel 与事件分发。

pub mod invoke;
pub mod protocol;
pub mod transport;

pub use invoke::{Bridge, CallOptions, ChannelSink, EventSink};
pub use protocol::{Frame, Message, Payload, DEFAULT_MAX_FRAME_SIZE, PROTOCOL_VERSION};
pub use transport::Writer;
