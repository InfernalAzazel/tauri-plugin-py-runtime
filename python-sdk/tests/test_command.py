import asyncio
import io
import json

import pytest

from tauri_runtime.channel import Channel
from tauri_runtime.command import Command, TauriError, bind_arguments, build_handler, to_snake
from tauri_runtime import protocol as wire
from tauri_runtime.protocol import Frame, Raw, Writer, read_frame
from tauri_runtime.router import Router


class FakeApp:
    pass


@pytest.mark.parametrize(
    "name,expected",
    [("onItem", "on_item"), ("first_name", "first_name"), ("userID", "user_id"), ("HTTPServer", "http_server")],
)
def test_to_snake(name, expected):
    assert to_snake(name) == expected


def test_injects_special_parameters():
    async def handler(ctx: Command, app: FakeApp, data: Raw, on_item: Channel[int], name: str, count: int = 3):
        return None

    h = build_handler("h", handler, FakeApp)
    ctx = Command(id=1, cmd="h")
    app = FakeApp()
    raw = Raw(b"x")
    channel = Channel(5, Writer(io.BytesIO()))
    kwargs = bind_arguments(h, {"name": "Ky"}, raw, {"on_item": channel}, ctx, app)
    assert kwargs == {"ctx": ctx, "app": app, "data": raw, "on_item": channel, "name": "Ky"}
    assert h.is_async


def test_camel_case_arguments_are_mapped():
    def handler(first_name: str):
        return first_name

    h = build_handler("h", handler, FakeApp)
    assert bind_arguments(h, {"firstName": "Ky"}, None, {}, Command(1, "h"), FakeApp()) == {"first_name": "Ky"}
    assert not h.is_async


def test_missing_argument_is_invalid_args():
    def handler(name: str):
        return name

    h = build_handler("h", handler, FakeApp)
    with pytest.raises(TauriError) as e:
        bind_arguments(h, {}, None, {}, Command(1, "h"), FakeApp())
    assert e.value.code == "invalid_args"


def test_non_object_arguments_are_invalid():
    h = build_handler("h", lambda: None, FakeApp)
    with pytest.raises(TauriError) as e:
        bind_arguments(h, [1, 2], None, {}, Command(1, "h"), FakeApp())
    assert e.value.code == "invalid_args"


def test_pydantic_validates_and_coerces():
    def add(a: int, b: int) -> int:
        return a + b

    h = build_handler("add", add, FakeApp)
    assert bind_arguments(h, {"a": "2", "b": 1}, None, {}, Command(1, "add"), FakeApp()) == {"a": 2, "b": 1}
    with pytest.raises(TauriError) as e:
        bind_arguments(h, {"a": "x", "b": 1}, None, {}, Command(1, "add"), FakeApp())
    assert e.value.code == "invalid_args"
    json.dumps(e.value.data)


def test_raw_parameter_requires_raw_body():
    def handler(data: Raw):
        return data

    h = build_handler("h", handler, FakeApp)
    with pytest.raises(TauriError) as e:
        bind_arguments(h, {}, None, {}, Command(1, "h"), FakeApp())
    assert e.value.code == "invalid_args"


def test_channel_sends_frames_and_rejects_after_close():
    asyncio.run(_channel_scenario())


async def _channel_scenario():
    out = io.BytesIO()
    channel: Channel[int] = Channel(9, Writer(out))
    await channel.send(1)
    channel.send_sync(Raw(b"\x01"))
    await channel.close()
    with pytest.raises(RuntimeError):
        await channel.send(2)
    out.seek(0)
    assert [read_frame(out) for _ in range(3)] == [
        Frame(wire.ChannelData(channel=9, value=1)),
        Frame(wire.ChannelData(channel=9, raw=True), b"\x01"),
        Frame(wire.ChannelClose(channel=9)),
    ]


def test_router_rejects_duplicates_and_sorts_names():
    router = Router()
    router.add(build_handler("b", lambda: None, FakeApp))
    router.add(build_handler("a", lambda: None, FakeApp))
    assert router.names() == ["a", "b"]
    assert router.get("missing") is None
    with pytest.raises(ValueError):
        router.add(build_handler("a", lambda: None, FakeApp))
