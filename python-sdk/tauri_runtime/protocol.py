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
SDK_VERSION = "0.1.0"
MAX_FRAME_SIZE = 64 * 1024 * 1024

_U32 = struct.Struct(">I")


class ProtocolError(Exception):
    pass


class MalformedFrame(ProtocolError):
    """A complete frame was read but could not be decoded; the stream is still aligned."""


class FrameTooLarge(MalformedFrame):
    def __init__(self, size: int) -> None:
        super().__init__(f"frame of {size} bytes exceeds limit")
        self.size = size


class Raw:
    __slots__ = ("bytes",)

    def __init__(self, data: bytes | bytearray | memoryview) -> None:
        self.bytes = bytes(data)

    def __eq__(self, other: object) -> bool:
        return isinstance(other, Raw) and other.bytes == self.bytes

    def __repr__(self) -> str:
        return f"Raw({len(self.bytes)} bytes)"


# Field order is the wire order and matches the Rust `Message` enum. A JSON payload travels
# inline as `value`; a binary payload travels in the frame body, flagged with `raw: true`.


class _Message(BaseModel):
    model_config = ConfigDict(strict=True, populate_by_name=True)


class Hello(_Message):
    type: Literal["hello"] = "hello"
    protocol: int
    sdk: str = SDK_VERSION
    commands: list[str] = []


class AppInfo(_Message):
    name: str
    version: str


class Ready(_Message):
    type: Literal["ready"] = "ready"
    protocol: int
    app: AppInfo


class Shutdown(_Message):
    type: Literal["shutdown"] = "shutdown"
    timeout_ms: Any = Field(None, alias="timeoutMs")


class Request(_Message):
    type: Literal["request"] = "request"
    id: int
    cmd: str
    channels: dict[str, int] = {}
    window: str | None = None
    value: Any = None
    raw: bool | None = None


class Response(_Message):
    type: Literal["response"] = "response"
    id: int
    value: Any = None
    raw: bool | None = None


class Error(_Message):
    type: Literal["error"] = "error"
    id: int
    code: str | None = None
    message: str | None = None
    data: Any = None


class Cancel(_Message):
    type: Literal["cancel"] = "cancel"
    id: int
    reason: str | None = None


class Event(_Message):
    """Python → Rust events carry `target`, Rust → Python events carry `window`."""

    type: Literal["event"] = "event"
    event: str
    target: str | None = None
    window: str | None = None
    value: Any = None
    raw: bool | None = None


class ChannelData(_Message):
    type: Literal["channel_data"] = "channel_data"
    channel: int
    value: Any = None
    raw: bool | None = None


class ChannelClose(_Message):
    type: Literal["channel_close"] = "channel_close"
    channel: int


Message = Annotated[
    Union[Hello, Ready, Shutdown, Request, Response, Error, Cancel, Event, ChannelData, ChannelClose],
    Field(discriminator="type"),
]
# Writing NaN / ±Infinity as JSON constants (for nested user models too) lets encode() reject them.
MESSAGE: TypeAdapter[Message] = TypeAdapter(Message, config=ConfigDict(ser_json_inf_nan="constants"))


@dataclass
class Frame:
    message: Message
    body: bytes = b""


def split_payload(value: Any) -> tuple[dict[str, Any], bytes]:
    """Message fields and frame body for a payload."""
    if isinstance(value, Raw):
        return {"raw": True}, value.bytes
    if isinstance(value, (bytes, bytearray, memoryview)):
        return {"raw": True}, bytes(value)
    return {"value": value}, b""


def join_payload(message: Request | Response | Event | ChannelData, body: bytes) -> Any:
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
    message = frame.message
    header = MESSAGE.dump_json(message, by_alias=True, exclude={k for k, v in message if v is None})
    if b"NaN" in header or b"Infinity" in header:
        json.loads(header, parse_constant=_reject_constant)
    inner_len = 4 + len(header) + len(frame.body)
    return b"".join((_U32.pack(inner_len), _U32.pack(len(header)), header, frame.body))


def decode(buf: bytes) -> Frame:
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
    prefix = _read_exact(stream, 4)
    if prefix is None:
        return None
    (size,) = _U32.unpack(prefix)
    if size > max_size:
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
    def __init__(self, stream: BinaryIO) -> None:
        self._stream = stream
        self._lock = threading.Lock()
        self.max_size = MAX_FRAME_SIZE

    def write(self, message: Message, body: bytes = b"") -> None:
        data = encode(Frame(message, body))
        if len(data) - 4 > self.max_size:
            raise FrameTooLarge(len(data) - 4)
        with self._lock:
            self._stream.write(data)
            self._stream.flush()


def take_over_stdio() -> tuple[BinaryIO, BinaryIO]:
    """Reserve fd 0/1 for the protocol and point everything else at stderr / devnull."""
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
