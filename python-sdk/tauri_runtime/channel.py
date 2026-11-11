from __future__ import annotations

from typing import Generic, TypeVar

from .protocol import Frame, Kind, Writer, encode_payload

T = TypeVar("T")


class Channel(Generic[T]):
    """A stream from Python to a frontend `Channel`. Valid until the command returns."""

    def __init__(self, channel_id: int, writer: Writer) -> None:
        self.id = channel_id
        self.closed = False
        self._writer = writer

    def send_sync(self, value: T) -> None:
        """Thread-safe variant for synchronous command handlers."""
        if self.closed:
            raise RuntimeError(f"channel {self.id} is closed")
        encoding, body = encode_payload(value)
        self._writer.write(Frame(Kind.CHANNEL_DATA, {"channel": self.id, "encoding": encoding}, body))

    async def send(self, value: T) -> None:
        self.send_sync(value)

    def close_sync(self) -> None:
        if self.closed:
            return
        self.closed = True
        self._writer.write(Frame(Kind.CHANNEL_CLOSE, {"channel": self.id}))

    async def close(self) -> None:
        self.close_sync()
