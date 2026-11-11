"""Tauri 2 插件 `py-runtime` 的 Python SDK。

Python 进程作为 Tauri 应用的 sidecar 运行，通过 stdio 上的长度前缀帧协议与 Rust 宿主通信。
本包对外导出编写后端所需的全部公共 API：

- `App`：运行时入口，负责注册命令、生命周期钩子与事件监听，并驱动协议循环。
- `Command`：单次调用的上下文（调用 id、命令名、来源窗口、是否已取消）。
- `Channel`：从 Python 向前端 `Channel` 推送数据的流。
- `Event`：前端通过 `pyEmit` 发来的事件。
- `Raw`：二进制载荷，经帧体传输而非内联 JSON。
- `TauriError`：带自定义错误码的结构化错误。

Example:
    ```python
    from tauri_runtime import App

    app = App()

    @app.command
    async def hello(name: str) -> str:
        return f"Hello {name}"

    app.run()
    ```
"""

from .app import App
from .channel import Channel
from .command import Command, TauriError
from .event import Event
from .protocol import PROTOCOL_VERSION, SDK_VERSION, Raw

__version__ = SDK_VERSION

__all__ = ["App", "Channel", "Command", "Event", "Raw", "TauriError", "PROTOCOL_VERSION", "SDK_VERSION"]
