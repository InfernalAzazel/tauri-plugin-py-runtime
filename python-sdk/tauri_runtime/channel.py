"""`Channel`：命令执行期间由 Python 向前端 `Channel` 推送数据的流。"""

from __future__ import annotations

from typing import Generic, TypeVar

from .protocol import ChannelClose, ChannelData, Writer, split_payload

T = TypeVar("T")


class Channel(Generic[T]):
    """指向前端 `Channel` 的单向数据流。

    在命令处理器中以 `Channel[T]` 注解参数即可注入，参数名与前端传入的 `Channel` 参数名
    对应（camelCase 自动转为 snake_case，如 `onProgress` → `on_progress`）。通道仅在命令
    返回前有效；命令结束后通道被标记为已关闭，再次发送会抛出 `RuntimeError`。

    `T` 为载荷类型，可以是任意可被 pydantic 序列化为 JSON 的值，也可以是 `Raw` / `bytes`
    （以二进制帧体发送）。

    Attributes:
        id: 前端 `Channel` 的 id。
        closed: 通道是否已关闭。

    Example:
        ```python
        @app.command
        async def count(n: int, on_item: Channel[int]) -> int:
            for i in range(n):
                await on_item.send(i)
            return n
        ```
    """

    def __init__(self, channel_id: int, writer: Writer) -> None:
        self.id = channel_id
        self.closed = False
        self._writer = writer

    def send_sync(self, value: T) -> None:
        """同步发送一条数据，线程安全，供同步（`def`）命令处理器使用。

        Args:
            value: 载荷。

        Raises:
            RuntimeError: 通道已关闭。
            ValueError: 载荷无法编码为合法 JSON（如包含 `NaN` / `±Infinity`）。
            FrameTooLarge: 编码后超过帧大小上限。
        """
        if self.closed:
            raise RuntimeError(f"channel {self.id} is closed")
        fields, body = split_payload(value)
        self._writer.write(ChannelData(channel=self.id, **fields), body)

    async def send(self, value: T) -> None:
        """发送一条数据，供异步（`async def`）命令处理器使用。

        Args:
            value: 载荷。

        Raises:
            RuntimeError: 通道已关闭。
            ValueError: 载荷无法编码为合法 JSON（如包含 `NaN` / `±Infinity`）。
            FrameTooLarge: 编码后超过帧大小上限。
        """
        self.send_sync(value)

    def close_sync(self) -> None:
        """同步关闭通道并通知前端；重复调用无副作用。"""
        if self.closed:
            return
        self.closed = True
        self._writer.write(ChannelClose(channel=self.id))

    async def close(self) -> None:
        """异步关闭通道并通知前端；重复调用无副作用。"""
        self.close_sync()
