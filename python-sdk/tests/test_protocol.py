import io
import threading
from dataclasses import dataclass

import pytest

from tauri_runtime.protocol import (
    Frame,
    FrameTooLarge,
    Kind,
    MalformedFrame,
    ProtocolError,
    Raw,
    Writer,
    decode_payload,
    encode,
    encode_payload,
    read_frame,
)


def test_roundtrip():
    frame = Frame(Kind.REQUEST, {"id": 1, "cmd": "hello"}, b"\x00\xff")
    assert read_frame(io.BytesIO(encode(frame))) == frame


def test_layout_is_big_endian_with_header_length():
    data = encode(Frame(Kind.CANCEL, {"id": 2}, b"xy"))
    header = b'{"id":2}'
    assert data[:4] == (1 + 4 + len(header) + 2).to_bytes(4, "big")
    assert data[4] == 0x13
    assert data[5:9] == len(header).to_bytes(4, "big")
    assert data[9:] == header + b"xy"


def test_eof_returns_none():
    assert read_frame(io.BytesIO(b"")) is None


def test_truncated_frame_raises():
    data = encode(Frame(Kind.HELLO, {"protocol": 1}))
    with pytest.raises(ProtocolError):
        read_frame(io.BytesIO(data[:-1]))


def test_too_large_frame_is_skipped():
    big = Frame(Kind.EVENT, {"event": "x"}, b"a" * 100)
    small = Frame(Kind.EVENT, {"event": "y"})
    stream = io.BytesIO(encode(big) + encode(small))
    with pytest.raises(FrameTooLarge):
        read_frame(stream, max_size=64)
    assert read_frame(stream, max_size=64) == small


def test_unknown_kind_is_malformed_and_stream_stays_aligned():
    bad = bytearray(encode(Frame(Kind.EVENT, {"event": "x"})))
    bad[4] = 0x7F
    good = Frame(Kind.EVENT, {"event": "y"})
    stream = io.BytesIO(bytes(bad) + encode(good))
    with pytest.raises(MalformedFrame):
        read_frame(stream)
    assert read_frame(stream) == good


@dataclass
class Point:
    x: int
    y: int


def test_encode_payload():
    assert encode_payload(None) == ("none", b"")
    assert encode_payload(Raw(b"\x01")) == ("raw", b"\x01")
    assert encode_payload(b"\x02") == ("raw", b"\x02")
    assert encode_payload({"a": "中"}) == ("json", '{"a":"中"}'.encode())
    assert encode_payload(Point(1, 2)) == ("json", b'{"x":1,"y":2}')


@pytest.mark.parametrize("value", [float("nan"), float("inf"), {"x": [float("-inf")]}])
def test_encode_payload_rejects_non_finite_floats(value):
    with pytest.raises(ValueError):
        encode_payload(value)


def test_decode_payload():
    assert decode_payload("json", b'{"a":1}') == {"a": 1}
    assert decode_payload("raw", b"\x01") == Raw(b"\x01")
    assert decode_payload("none", b"") is None
    with pytest.raises(ProtocolError):
        decode_payload("yaml", b"")


def test_writer_is_thread_safe_and_rejects_oversized():
    out = io.BytesIO()
    writer = Writer(out)
    frames = [Frame(Kind.EVENT, {"event": str(i)}, b"x" * 1000) for i in range(50)]
    threads = [threading.Thread(target=writer.write, args=(f,)) for f in frames]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    out.seek(0)
    got = [read_frame(out) for _ in frames]
    assert sorted(f.header["event"] for f in got) == sorted(str(i) for i in range(50))

    writer.max_size = 16
    with pytest.raises(FrameTooLarge):
        writer.write(Frame(Kind.EVENT, {"event": "big"}, b"x" * 100))
