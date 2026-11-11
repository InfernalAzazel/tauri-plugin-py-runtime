import asyncio
import io
import json
from typing import Annotated

import pytest
from pydantic import BaseModel, Field
from pydantic.alias_generators import to_camel, to_snake

from tauri_runtime import protocol as wire
from tauri_runtime.channel import Channel
from tauri_runtime.command import Command, TauriError, bind_arguments, build_handler
from tauri_runtime.protocol import Frame, Raw, Writer, read_frame
from tauri_runtime.router import CommandTable


class FakeApp:
    pass


def bind(h, args, channels=None):
    return bind_arguments(h, args, None, channels or {}, Command(1, h.name), FakeApp())


@pytest.mark.parametrize(
    "name,expected",
    [("onItem", "on_item"), ("first_name", "first_name"), ("userID", "user_id"), ("HTTPServer", "http_server")],
)
def test_to_snake(name, expected):
    assert to_snake(name) == expected


def test_acronym_keys():
    # 参数键按 to_camel(形参名) 匹配：连续大写的 `fileURLPath` 不是别名，视为未知键；通道键经 to_snake 映射可识别。
    assert to_camel("file_url_path") == "fileUrlPath"
    assert to_snake("fileURLPath") == "file_url_path"

    def handler(file_url_path: str):
        return file_url_path

    h = build_handler("h", handler, FakeApp)
    assert bind(h, {"fileUrlPath": "a"}) == {"file_url_path": "a"}
    with pytest.raises(TauriError) as e:
        bind(h, {"fileURLPath": "a"})
    assert e.value.data[0]["type"] == "missing"


def test_camel_and_snake_keys_both_accepted():
    def handler(first_name: str, last_name: str = "x"):
        return first_name

    h = build_handler("h", handler, FakeApp)
    assert bind(h, {"firstName": "a", "last_name": "b"}) == {"first_name": "a", "last_name": "b"}
    assert bind(h, {"first_name": "a", "extra": 1}) == {"first_name": "a"}


def test_unsupported_annotation_fails_at_registration():
    class Opaque:
        pass

    def handler(thing: Opaque):
        return thing

    with pytest.raises(TypeError, match=r"command 'h'.*parameter 'thing'"):
        build_handler("h", handler, FakeApp)


def test_underscore_parameter_fails_at_registration():
    def handler(_hidden: int):
        return _hidden

    with pytest.raises(TypeError, match="'_hidden'"):
        build_handler("h", handler, FakeApp)


def test_multiple_invalid_fields_in_one_error():
    def handler(a: int, b: int, c: str):
        return a

    h = build_handler("h", handler, FakeApp)
    with pytest.raises(TauriError) as e:
        bind(h, {"a": "x", "b": "y"})
    assert e.value.code == "invalid_args"
    assert [(err["loc"], err["type"]) for err in e.value.data] == [
        (["a"], "int_parsing"),
        (["b"], "int_parsing"),
        (["c"], "missing"),
    ]
    assert e.value.message == "invalid arguments: a, b, c"
    json.dumps(e.value.data)


def test_defaults_and_pydantic_model_params():
    class Point(BaseModel):
        x: int
        y: int

    def handler(p: Point, scale: int = 2, tags: list[str] | None = None):
        return p

    h = build_handler("h", handler, FakeApp)
    kwargs = bind(h, {"p": {"x": "1", "y": 2}})
    assert kwargs == {"p": Point(x=1, y=2)}
    assert bind(h, {"p": {"x": 1, "y": 2}, "scale": "3"})["scale"] == 3


def test_annotated_constraints_are_enforced():
    def handler(
        query: Annotated[str, Field(min_length=1)],
        on_item: Annotated[Channel[int], "doc"],
        page_size: Annotated[int, Field(ge=1, le=100)] = 20,
    ):
        return query

    h = build_handler("h", handler, FakeApp)
    channels = {"on_item": Channel(5, Writer(io.BytesIO()))}
    assert bind(h, {"query": "a", "pageSize": 100}, channels)["page_size"] == 100
    with pytest.raises(TauriError) as e:
        bind(h, {"query": "", "pageSize": 0}, channels)
    assert [(err["loc"], err["type"]) for err in e.value.data] == [
        (["query"], "string_too_short"),
        (["pageSize"], "greater_than_equal"),
    ]


def test_missing_channel_is_invalid_args():
    def handler(on_item: Channel[int]):
        return on_item

    h = build_handler("h", handler, FakeApp)
    with pytest.raises(TauriError) as e:
        bind(h, {})
    assert e.value.code == "invalid_args"


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


def test_command_table_rejects_duplicates_and_sorts_names():
    router = CommandTable()
    router.add(build_handler("b", lambda: None, FakeApp))
    router.add(build_handler("a", lambda: None, FakeApp))
    assert router.names() == ["a", "b"]
    assert router.get("missing") is None
    with pytest.raises(ValueError):
        router.add(build_handler("a", lambda: None, FakeApp))
