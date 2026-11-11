from __future__ import annotations

import json
import os
import struct
import sys
import threading
from dataclasses import asdict, dataclass, field, is_dataclass
from enum import IntEnum
from typing import Any, BinaryIO

PROTOCOL_VERSION = 1
SDK_VERSION = "0.1.0"
MAX_FRAME_SIZE = 64 * 1024 * 1024

_U32 = struct.Struct(">I")


class Kind(IntEnum):
    HELLO = 0x01
    READY = 0x02
    SHUTDOWN = 0x03
    REQUEST = 0x10
    RESPONSE = 0x11
    ERROR = 0x12
    CANCEL = 0x13
    EVENT = 0x20
    CHANNEL_DATA = 0x30
    CHANNEL_CLOSE = 0x31


class ProtocolError(Exception):
    pass


class MalformedFrame(ProtocolError):
    """A complete frame was read but could not be decoded; the stream is still aligned."""


class FrameTooLarge(MalformedFrame):
    def __init__(self, size: int) -> None:
        super().__init__(f"frame of {size} bytes exceeds limit")
        self.size = size


@dataclass
class Frame:
    kind: Kind
    header: dict[str, Any] = field(default_factory=dict)
    body: bytes = b""


class Raw:
    __slots__ = ("bytes",)

    def __init__(self, data: bytes | bytearray | memoryview) -> None:
        self.bytes = bytes(data)

    def __eq__(self, other: object) -> bool:
        return isinstance(other, Raw) and other.bytes == self.bytes

    def __repr__(self) -> str:
        return f"Raw({len(self.bytes)} bytes)"


def _json_default(value: Any) -> Any:
    if is_dataclass(value) and not isinstance(value, type):
        return asdict(value)
    dump = getattr(value, "model_dump", None)
    if callable(dump):
        return dump(mode="json")
    raise TypeError(f"Object of type {type(value).__name__} is not JSON serializable")


def _dumps(value: Any) -> bytes:
    return json.dumps(
        value, default=_json_default, ensure_ascii=False, allow_nan=False, separators=(",", ":")
    ).encode()


def encode(frame: Frame) -> bytes:
    header = _dumps(frame.header)
    inner_len = 1 + 4 + len(header) + len(frame.body)
    return b"".join(
        (_U32.pack(inner_len), bytes((frame.kind,)), _U32.pack(len(header)), header, frame.body)
    )


def decode(buf: bytes) -> Frame:
    if len(buf) < 5:
        raise MalformedFrame("frame shorter than 5 bytes")
    try:
        kind = Kind(buf[0])
    except ValueError:
        raise MalformedFrame(f"unknown frame kind {buf[0]:#x}") from None
    (header_len,) = _U32.unpack_from(buf, 1)
    end = 5 + header_len
    if end > len(buf):
        raise MalformedFrame("header length exceeds frame")
    try:
        header = json.loads(buf[5:end]) if header_len else {}
    except ValueError as e:
        raise MalformedFrame(f"invalid header: {e}") from None
    if not isinstance(header, dict):
        raise MalformedFrame("header is not an object")
    return Frame(kind, header, bytes(buf[end:]))


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


def encode_payload(value: Any) -> tuple[str, bytes]:
    if value is None:
        return "none", b""
    if isinstance(value, Raw):
        return "raw", value.bytes
    if isinstance(value, (bytes, bytearray, memoryview)):
        return "raw", bytes(value)
    return "json", _dumps(value)


def decode_payload(encoding: str, body: bytes) -> Any:
    if encoding == "json":
        return json.loads(body) if body else None
    if encoding == "raw":
        return Raw(body)
    if encoding == "none":
        return None
    raise ProtocolError(f"unknown encoding {encoding!r}")


class Writer:
    def __init__(self, stream: BinaryIO) -> None:
        self._stream = stream
        self._lock = threading.Lock()
        self.max_size = MAX_FRAME_SIZE

    def write(self, frame: Frame) -> None:
        data = encode(frame)
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
