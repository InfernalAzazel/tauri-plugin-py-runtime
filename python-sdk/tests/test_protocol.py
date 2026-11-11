import io
import json
import threading
from dataclasses import dataclass
from pathlib import Path

import pytest
from conftest import raw_frame
from pydantic import BaseModel

from tauri_runtime import protocol as wire
from tauri_runtime.protocol import (
    Frame,
    FrameTooLarge,
    MalformedFrame,
    ProtocolError,
    Raw,
    Writer,
    encode,
    join_payload,
    read_frame,
    split_payload,
)

GOLDEN = json.loads((Path(__file__).parent / "fixtures" / "golden_frames.json").read_text())

# Must match `golden_frames` in src/bridge/protocol.rs.
GOLDEN_FRAMES = {
    "hello": Frame(wire.Hello(protocol=2, sdk="0.1.0", commands=["greet"])),
    "ready": Frame(wire.Ready(protocol=2, app=wire.AppInfo(name="app", version="1.0.0"))),
    "shutdown": Frame(wire.Shutdown(timeout_ms=3000)),
    "request": Frame(
        wire.Request(
            id=7, cmd="greet", channels={"onItem": 3}, window="main", value={"n": [1, 2.5, None], "name": "中文"}
        )
    ),
    "response_raw": Frame(wire.Response(id=7, raw=True), b"\x00\xff"),
    "error": Frame(wire.Error(id=7, code="custom", message="bad", data={"x": 1})),
    "cancel": Frame(wire.Cancel(id=7, reason="timeout")),
    "event": Frame(wire.Event(event="tick", target="main")),
    "channel_data": Frame(wire.ChannelData(channel=3, value=1)),
    "channel_close": Frame(wire.ChannelClose(channel=3)),
}


@pytest.mark.parametrize("name", sorted(GOLDEN_FRAMES))
def test_golden_frames_match_rust(name):
    data = bytes.fromhex(GOLDEN[name])
    assert encode(GOLDEN_FRAMES[name]) == data
    assert read_frame(io.BytesIO(data)) == GOLDEN_FRAMES[name]


def test_golden_fixture_covers_every_message_type():
    assert set(GOLDEN) == set(GOLDEN_FRAMES)
    types = {f.message.type for f in GOLDEN_FRAMES.values()}
    assert len(types) == 10


def test_layout_is_big_endian_with_header_length():
    data = encode(Frame(wire.Response(id=2, raw=True), b"xy"))
    header = b'{"type":"response","id":2,"raw":true}'
    assert data[:4] == (4 + len(header) + 2).to_bytes(4, "big")
    assert data[4:8] == len(header).to_bytes(4, "big")
    assert data[8:] == header + b"xy"


def test_none_fields_are_omitted_but_nested_nulls_kept():
    data = encode(Frame(wire.Response(id=1, value={"a": None})))
    assert data[8:] == b'{"type":"response","id":1,"value":{"a":null}}'
    assert encode(Frame(wire.Response(id=1)))[8:] == b'{"type":"response","id":1}'


def test_eof_returns_none():
    assert read_frame(io.BytesIO(b"")) is None


def test_truncated_frame_raises():
    data = encode(Frame(wire.Hello(protocol=2)))
    with pytest.raises(ProtocolError):
        read_frame(io.BytesIO(data[:-1]))


def test_too_large_frame_is_skipped():
    big = Frame(wire.Event(event="x", raw=True), b"a" * 100)
    small = Frame(wire.Event(event="y"))
    stream = io.BytesIO(encode(big) + encode(small))
    with pytest.raises(FrameTooLarge):
        read_frame(stream, max_size=64)
    assert read_frame(stream, max_size=64) == small


@pytest.mark.parametrize(
    "header",
    [
        b'{"type":"bogus"}',
        b'{"type":"cancel"}',
        b'{"type":"cancel","id":"1"}',
        b"[1]",
        b"{not json",
    ],
)
def test_malformed_header_is_skipped_and_stream_stays_aligned(header):
    good = Frame(wire.Event(event="y"))
    stream = io.BytesIO(raw_frame(header) + encode(good))
    with pytest.raises(MalformedFrame):
        read_frame(stream)
    assert read_frame(stream) == good


def test_header_length_beyond_frame_is_malformed():
    data = (8).to_bytes(4, "big") + (100).to_bytes(4, "big") + b"{}{}"
    with pytest.raises(MalformedFrame):
        read_frame(io.BytesIO(data))


@dataclass
class Point:
    x: int
    y: int


class Model(BaseModel):
    name: str
    email: str | None = None


class Measurement(BaseModel):
    f: float


def test_split_payload():
    assert split_payload(None) == ({"value": None}, b"")
    assert split_payload(Raw(b"\x01")) == ({"raw": True}, b"\x01")
    assert split_payload(b"\x02") == ({"raw": True}, b"\x02")
    assert split_payload({"a": 1}) == ({"value": {"a": 1}}, b"")


def test_dataclasses_and_models_serialize():
    data = encode(Frame(wire.Response(id=1, value=[Point(1, 2), Model(name="中")])))
    assert json.loads(data[8:])["value"] == [{"x": 1, "y": 2}, {"name": "中", "email": None}]


@pytest.mark.parametrize(
    "value", [float("nan"), float("inf"), {"x": [float("-inf")]}, Point(1, float("nan")), Measurement(f=float("inf"))]
)
def test_non_finite_floats_are_rejected(value):
    with pytest.raises(ValueError):
        encode(Frame(wire.Response(id=1, value=value)))


def test_strings_mentioning_nan_are_fine():
    data = encode(Frame(wire.Response(id=1, value="NaN and Infinity")))
    assert json.loads(data[8:])["value"] == "NaN and Infinity"


def test_join_payload():
    assert join_payload(wire.Response(id=1, value={"a": 1}), b"") == {"a": 1}
    assert join_payload(wire.Response(id=1, raw=True), b"\x01") == Raw(b"\x01")
    assert join_payload(wire.Response(id=1), b"") is None
    with pytest.raises(MalformedFrame):
        join_payload(wire.Response(id=1, value=1), b"x")
    with pytest.raises(MalformedFrame):
        join_payload(wire.Response(id=1, value=1, raw=True), b"x")


def test_shutdown_uses_wire_alias():
    assert encode(Frame(wire.Shutdown(timeout_ms=5)))[8:] == b'{"type":"shutdown","timeoutMs":5}'


def test_writer_is_thread_safe_and_rejects_oversized():
    out = io.BytesIO()
    writer = Writer(out)
    threads = [
        threading.Thread(target=writer.write, args=(wire.Event(event=str(i), raw=True), b"x" * 1000))
        for i in range(50)
    ]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    out.seek(0)
    got = [read_frame(out) for _ in threads]
    assert sorted(f.message.event for f in got) == sorted(str(i) for i in range(50))

    writer.max_size = 16
    with pytest.raises(FrameTooLarge):
        writer.write(wire.Event(event="big", raw=True), b"x" * 100)
