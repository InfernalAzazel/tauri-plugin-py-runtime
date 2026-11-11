"""线协议（v2）：消息模型、帧编解码与 stdio 接管。

每一帧的布局为::

    [u32 BE inner_len][u32 BE header_len][header JSON][body]

`inner_len` 计入其后的全部字节（不含自身），上限为 `MAX_FRAME_SIZE`。header 是一个以
`type` 字段区分的 JSON 对象，在 Python 侧由 pydantic 模型组成的可辨识联合 `Message` 表示，
与 Rust 侧的 `bridge::protocol::Message` 枚举一一对应。

载荷有两种形式：JSON 值内联在 header 的 `value` 字段中；二进制数据放在帧体 `body` 中，
并在 header 中设置 `raw: true`。未设置 `raw` 时帧体必须为空。
"""

from __future__ import annotations

import json
import os
import struct
import sys
import threading
from dataclasses import dataclass
from typing import Annotated, Any, BinaryIO, Literal, NoReturn, Union

from pydantic import BaseModel, ConfigDict, Field, TypeAdapter, ValidationError

PROTOCOL_VERSION = 2
SDK_VERSION = "0.1.1"
MAX_FRAME_SIZE = 64 * 1024 * 1024

_U32 = struct.Struct(">I")


class ProtocolError(Exception):
    """协议层错误的基类。

    直接抛出该类型（而非子类）表示字节流已损坏或意外结束（如帧内 EOF），无法继续读取。
    """


class MalformedFrame(ProtocolError):
    """已完整读取一帧但无法解码；字节流仍保持帧对齐，可以跳过该帧继续读取。"""


class FrameTooLarge(MalformedFrame):
    """帧大小超过上限。

    读取时超限帧的内容会被完整丢弃以保持对齐；写入时则拒绝发送。

    Attributes:
        size: 超限帧的字节数（不含 4 字节外层长度前缀）。
    """

    def __init__(self, size: int) -> None:
        super().__init__(f"frame of {size} bytes exceeds limit")
        self.size = size


class Raw:
    """二进制载荷。

    作为命令参数注解时，接收前端传入的 `Uint8Array` / `ArrayBuffer`；作为返回值或经
    `Channel` / `App.emit` 发送时，数据放入帧体并设置 `raw: true`，前端收到 `Uint8Array`。

    Args:
        data: 原始字节，会被复制为不可变的 `bytes`。

    Attributes:
        bytes: 载荷内容。

    Example:
        ```python
        @app.command
        def reverse(data: Raw) -> Raw:
            return Raw(data.bytes[::-1])
        ```
    """

    __slots__ = ("bytes",)

    def __init__(self, data: bytes | bytearray | memoryview) -> None:
        self.bytes = bytes(data)

    def __eq__(self, other: object) -> bool:
        return isinstance(other, Raw) and other.bytes == self.bytes

    def __repr__(self) -> str:
        return f"Raw({len(self.bytes)} bytes)"


# 字段声明顺序即线上 JSON 的字段顺序，须与 Rust `Message` 枚举保持一致（golden frames 按字节比对）。


class _Message(BaseModel):
    """所有协议消息的基类：严格类型校验，允许按字段名或别名填充。"""

    model_config = ConfigDict(strict=True, populate_by_name=True)


class Hello(_Message):
    """握手消息（Python → Rust），启动后发送的第一帧。

    Attributes:
        protocol: Python 侧的协议版本，须与 Rust 侧一致，否则启动失败（`protocol_mismatch`）。
        sdk: SDK 版本号。
        commands: 已注册的命令名列表（已排序）。
    """

    type: Literal["hello"] = "hello"
    protocol: int
    sdk: str = SDK_VERSION
    commands: list[str] = []


class AppInfo(_Message):
    """宿主 Tauri 应用的基本信息，随 `Ready` 下发。

    Attributes:
        name: 应用名称。
        version: 应用版本。
    """

    name: str
    version: str


class Ready(_Message):
    """握手确认（Rust → Python）；收到后运行 `ready` 钩子。

    Attributes:
        protocol: Rust 侧的协议版本。
        app: 宿主应用信息。
    """

    type: Literal["ready"] = "ready"
    protocol: int
    app: AppInfo


class Shutdown(_Message):
    """关闭通知（Rust → Python）。

    Attributes:
        timeout_ms: 关闭预算（毫秒），线上字段名为 `timeoutMs`。类型刻意放宽为 `Any`，
            以便非法值不会导致整帧被丢弃，而是由运行时回退到默认的 3 秒。
    """

    type: Literal["shutdown"] = "shutdown"
    timeout_ms: Any = Field(None, alias="timeoutMs")


class Request(_Message):
    """命令调用请求（Rust → Python）。

    Attributes:
        id: 调用 id，后续的 `Response` / `Error` / `Cancel` 均以此关联。
        cmd: 命令名。
        channels: 参数名（camelCase）到前端 `Channel` id 的映射。
        window: 发起调用的 webview 标签。
        value: 内联的 JSON 参数对象。
        raw: 为 `True` 时参数为帧体中的二进制数据。
    """

    type: Literal["request"] = "request"
    id: int
    cmd: str
    channels: dict[str, int] = {}
    window: str | None = None
    value: Any = None
    raw: bool | None = None


class Response(_Message):
    """命令成功返回（Python → Rust）。

    Attributes:
        id: 对应请求的调用 id。
        value: 内联的 JSON 返回值。
        raw: 为 `True` 时返回值为帧体中的二进制数据。
    """

    type: Literal["response"] = "response"
    id: int
    value: Any = None
    raw: bool | None = None


class Error(_Message):
    """命令失败返回（Python → Rust）。

    Attributes:
        id: 对应请求的调用 id。
        code: 错误码，原样透传给前端（如 `invalid_args`、`internal` 或 `TauriError` 的自定义码）。
        message: 可读的错误描述。
        data: 附加的 JSON 数据，可选。
    """

    type: Literal["error"] = "error"
    id: int
    code: str | None = None
    message: str | None = None
    data: Any = None


class Cancel(_Message):
    """取消调用（Rust → Python）。

    Attributes:
        id: 要取消的调用 id。
        reason: 取消原因，`"client"`（前端主动中止）或 `"timeout"`（调用超时）。
    """

    type: Literal["cancel"] = "cancel"
    id: int
    reason: str | None = None


class Event(_Message):
    """事件消息（双向）。

    Python → Rust 方向使用 `target`，Rust → Python 方向使用 `window`。

    Attributes:
        event: 事件名。
        target: 目标 webview 标签；为空时广播。
        window: 发出事件的 webview 标签。
        value: 内联的 JSON 载荷。
        raw: 为 `True` 时载荷为帧体中的二进制数据。
    """

    type: Literal["event"] = "event"
    event: str
    target: str | None = None
    window: str | None = None
    value: Any = None
    raw: bool | None = None


class ChannelData(_Message):
    """向前端 `Channel` 推送一条数据（Python → Rust）。

    Attributes:
        channel: 前端 `Channel` 的 id。
        value: 内联的 JSON 载荷。
        raw: 为 `True` 时载荷为帧体中的二进制数据。
    """

    type: Literal["channel_data"] = "channel_data"
    channel: int
    value: Any = None
    raw: bool | None = None


class ChannelClose(_Message):
    """关闭前端 `Channel`（Python → Rust）。

    Attributes:
        channel: 前端 `Channel` 的 id。
    """

    type: Literal["channel_close"] = "channel_close"
    channel: int


Message = Annotated[
    Union[Hello, Ready, Shutdown, Request, Response, Error, Cancel, Event, ChannelData, ChannelClose],
    Field(discriminator="type"),
]
# NaN / ±Infinity 不是合法 JSON。让 pydantic 将其序列化为 JSON 常量（嵌套的用户模型同样适用），
# encode() 才能检测并拒绝；否则 pydantic 默认会静默写成 null。
MESSAGE: TypeAdapter[Message] = TypeAdapter(Message, config=ConfigDict(ser_json_inf_nan="constants"))


@dataclass
class Frame:
    """一个完整的协议帧。

    Attributes:
        message: 帧头中的协议消息。
        body: 帧体；仅当消息设置了 `raw: true` 时非空。
    """

    message: Message
    body: bytes = b""


def split_payload(value: Any) -> tuple[dict[str, Any], bytes]:
    """将载荷拆分为消息字段与帧体。

    `Raw` / `bytes` / `bytearray` / `memoryview` 走帧体并设置 `raw: True`，其余值内联为 `value`。

    Args:
        value: 待发送的载荷。

    Returns:
        二元组 `(fields, body)`：`fields` 可直接展开为消息构造参数，`body` 为帧体字节。
    """
    if isinstance(value, Raw):
        return {"raw": True}, value.bytes
    if isinstance(value, (bytes, bytearray, memoryview)):
        return {"raw": True}, bytes(value)
    return {"value": value}, b""


def join_payload(message: Request | Response | Event | ChannelData, body: bytes) -> Any:
    """`split_payload` 的逆操作：由消息与帧体还原载荷。

    Args:
        message: 携带载荷的消息。
        body: 帧体。

    Returns:
        `raw` 为真时返回 `Raw`，否则返回内联的 `value`（可能为 `None`）。

    Raises:
        MalformedFrame: `raw` 与内联 `value` 同时出现，或未设置 `raw` 却带有帧体。
    """
    if message.raw:
        if message.value is not None:
            raise MalformedFrame("raw payload with an inline value")
        return Raw(body)
    if body:
        raise MalformedFrame("frame body without the raw flag")
    return message.value


def _reject_constant(name: str) -> NoReturn:
    raise ValueError(f"{name} is not valid JSON")


def encode(frame: Frame) -> bytes:
    """将帧编码为线上字节（含 4 字节外层长度前缀）。

    值为 `None` 的字段不会写出；字段名使用别名（如 `timeoutMs`）。

    Args:
        frame: 待编码的帧。

    Returns:
        完整的帧字节。

    Raises:
        ValueError: 载荷包含 `NaN` / `±Infinity`，或无法序列化为 JSON。
    """
    message = frame.message
    header = MESSAGE.dump_json(message, by_alias=True, exclude={k for k, v in message if v is None})
    if b"NaN" in header or b"Infinity" in header:
        json.loads(header, parse_constant=_reject_constant)
    inner_len = 4 + len(header) + len(frame.body)
    return b"".join((_U32.pack(inner_len), _U32.pack(len(header)), header, frame.body))


def decode(buf: bytes) -> Frame:
    """解码一帧的内部字节（即外层长度前缀之后的部分）。

    Args:
        buf: `[u32 BE header_len][header JSON][body]`。

    Returns:
        解码后的帧。

    Raises:
        MalformedFrame: 长度不足、header 长度越界，或 header 无法校验为已知消息。
    """
    if len(buf) < 4:
        raise MalformedFrame("frame shorter than 4 bytes")
    (header_len,) = _U32.unpack_from(buf)
    end = 4 + header_len
    if end > len(buf):
        raise MalformedFrame("header length exceeds frame")
    try:
        message = MESSAGE.validate_json(buf[4:end])
    except ValidationError as e:
        raise MalformedFrame(f"invalid header: {e}") from None
    return Frame(message, bytes(buf[end:]))


def _read_exact(stream: BinaryIO, n: int) -> bytes | None:
    """精确读取 `n` 字节；在读到任何字节前遇到 EOF 返回 `None`，读到一半遇到 EOF 抛 `ProtocolError`。"""
    chunks: list[bytes] = []
    remaining = n
    while remaining:
        chunk = stream.read(remaining)
        if not chunk:
            if remaining == n:
                return None
            raise ProtocolError("unexpected EOF inside frame")
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def read_frame(stream: BinaryIO, max_size: int = MAX_FRAME_SIZE) -> Frame | None:
    """从阻塞的二进制流中读取一帧。

    Args:
        stream: 输入流。
        max_size: 单帧内部字节数上限。

    Returns:
        解码后的帧；若流在帧边界处正常结束则返回 `None`。

    Raises:
        FrameTooLarge: 帧超过 `max_size`；其内容已被读出丢弃，流仍对齐。
        MalformedFrame: 帧读取完整但无法解码；流仍对齐。
        ProtocolError: 帧内出现 EOF，流已不可用。
    """
    prefix = _read_exact(stream, 4)
    if prefix is None:
        return None
    (size,) = _U32.unpack(prefix)
    if size > max_size:
        # 分块丢弃而不是一次性读入，避免为超限帧分配大块内存，同时保持流对齐。
        remaining = size
        while remaining:
            chunk = stream.read(min(remaining, 1 << 16))
            if not chunk:
                raise ProtocolError("unexpected EOF inside frame")
            remaining -= len(chunk)
        raise FrameTooLarge(size)
    body = _read_exact(stream, size)
    if body is None:
        raise ProtocolError("unexpected EOF inside frame")
    return decode(body)


class Writer:
    """线程安全的帧写入器。

    事件循环与线程池中的同步处理器会并发写出，整帧写入与 flush 在同一把锁内完成，
    保证帧不会交错。

    Args:
        stream: 协议输出流。

    Attributes:
        max_size: 单帧内部字节数上限，默认 `MAX_FRAME_SIZE`。
    """

    def __init__(self, stream: BinaryIO) -> None:
        self._stream = stream
        self._lock = threading.Lock()
        self.max_size = MAX_FRAME_SIZE

    def write(self, message: Message, body: bytes = b"") -> None:
        """编码并写出一帧，随即 flush。

        Args:
            message: 协议消息。
            body: 帧体。

        Raises:
            FrameTooLarge: 编码后超过 `max_size`，不会写出任何字节。
            ValueError: 载荷无法编码为合法 JSON（见 `encode`）。
        """
        data = encode(Frame(message, body))
        if len(data) - 4 > self.max_size:
            raise FrameTooLarge(len(data) - 4)
        with self._lock:
            self._stream.write(data)
            self._stream.flush()


def take_over_stdio() -> tuple[BinaryIO, BinaryIO]:
    """接管 stdio：fd 0/1 专供协议使用，其余输出改道到 stderr / devnull。

    复制原 fd 0/1 作为协议流后，fd 0 指向 devnull，fd 1 指向 stderr，`sys.stdin` 换成
    devnull，`sys.stdout` 换成 `sys.stderr`。这样 `print()` 及子进程的 stdout 都不会污染
    协议流，而是进入 stderr，由宿主按行转发到日志。

    Returns:
        二元组 `(proto_in, proto_out)`：协议输入流与输出流。
    """
    sys.stdout.flush()
    proto_in = os.fdopen(os.dup(0), "rb")
    proto_out = os.fdopen(os.dup(1), "wb")
    null = os.open(os.devnull, os.O_RDONLY)
    os.dup2(null, 0)
    os.close(null)
    os.dup2(2, 1)
    sys.stdin = open(os.devnull, encoding="utf-8")
    sys.stdout = sys.stderr
    return proto_in, proto_out
