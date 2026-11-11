import asyncio
import json
import time

import pytest

from tauri_runtime import App, Channel, Command, Event, Raw, TauriError
from tauri_runtime.protocol import PROTOCOL_VERSION, Kind


def base_app():
    app = App(stdio=False)

    @app.command
    async def hello(name: str) -> str:
        return f"Hello {name}"

    return app


def test_hello_frame_lists_commands(harness):
    h = harness(base_app())
    assert h.hello.kind == Kind.HELLO
    assert h.hello.header["protocol"] == PROTOCOL_VERSION
    assert h.hello.header["commands"] == ["hello"]


def test_json_call(harness):
    h = harness(base_app())
    h.request(1, "hello", {"name": "Kylin"})
    f = h.recv()
    assert f.kind == Kind.RESPONSE
    assert f.header == {"id": 1, "encoding": "json"}
    assert json.loads(f.body) == "Hello Kylin"


def test_raw_call_and_none_result(harness):
    app = App(stdio=False)

    @app.command
    def reverse(data: Raw) -> Raw:
        return Raw(data.bytes[::-1])

    @app.command("noop")
    async def nothing() -> None:
        return None

    h = harness(app)
    h.request(1, "reverse", raw=b"\x00\x01\x02")
    f = h.recv()
    assert f.header == {"id": 1, "encoding": "raw"}
    assert f.body == b"\x02\x01\x00"
    h.request(2, "noop")
    assert h.recv().header == {"id": 2, "encoding": "none"}


def test_sync_handler_does_not_block_async(harness):
    app = App(stdio=False)

    @app.command
    def slow() -> str:
        time.sleep(0.5)
        return "slow"

    @app.command
    async def fast() -> str:
        return "fast"

    h = harness(app)
    h.request(1, "slow")
    h.request(2, "fast")
    assert [h.recv().header["id"], h.recv().header["id"]] == [2, 1]


def test_command_not_found(harness):
    h = harness(base_app())
    h.request(1, "missing")
    f = h.recv()
    assert f.kind == Kind.ERROR
    assert f.header["code"] == "command_not_found"


def test_invalid_args(harness):
    h = harness(base_app())
    h.request(1, "hello", {})
    assert h.recv().header["code"] == "invalid_args"


def test_tauri_error_and_unexpected_exception(harness):
    app = App(stdio=False)

    @app.command
    async def denied() -> None:
        raise TauriError("not_allowed", "nope", {"k": 1})

    @app.command
    async def boom() -> None:
        raise ValueError("boom")

    h = harness(app)
    h.request(1, "denied")
    assert h.recv().header == {"id": 1, "code": "not_allowed", "message": "nope", "data": {"k": 1}}
    h.request(2, "boom")
    f = h.recv()
    assert f.header["code"] == "internal"
    assert f.header["message"] == "ValueError: boom"
    assert f.header["data"] == {"type": "ValueError"}


def test_cancel_suppresses_response(harness):
    app = base_app()

    @app.command
    async def wait(ctx: Command) -> str:
        await asyncio.sleep(10)
        return "done"

    h = harness(app)
    h.request(1, "wait")
    h.send(Kind.CANCEL, {"id": 1, "reason": "client"})
    h.request(2, "hello", {"name": "x"})
    assert h.recv().header["id"] == 2


def test_channel_streams_before_response(harness):
    app = App(stdio=False)

    @app.command
    async def count(n: int, on_item: Channel[int]) -> int:
        for i in range(n):
            await on_item.send(i)
        return n

    h = harness(app)
    h.request(1, "count", {"n": 3}, channels={"onItem": 7})
    frames = [h.recv() for _ in range(4)]
    assert [(f.kind, f.header.get("channel")) for f in frames[:3]] == [(Kind.CHANNEL_DATA, 7)] * 3
    assert [json.loads(f.body) for f in frames[:3]] == [0, 1, 2]
    assert frames[3].kind == Kind.RESPONSE


def test_events_roundtrip(harness):
    app = App(stdio=False)

    @app.listen("user:login")
    async def on_login(event: Event):
        await app.emit("welcome", {"user": event.payload["name"]}, target=event.window)

    h = harness(app)
    h.send(Kind.EVENT, {"event": "user:login", "encoding": "json", "window": "main"}, b'{"name":"Ky"}')
    f = h.recv()
    assert f.kind == Kind.EVENT
    assert f.header == {"event": "welcome", "encoding": "json", "target": "main"}
    assert json.loads(f.body) == {"user": "Ky"}


def test_lifecycle_hooks(harness):
    app = base_app()
    calls = []

    @app.on("startup")
    def startup():
        calls.append("startup")

    @app.on("ready")
    async def ready():
        calls.append("ready")

    @app.on("shutdown")
    async def shutdown():
        calls.append("shutdown")

    h = harness(app)
    assert calls == ["startup"]
    h.send(Kind.READY, {"protocol": PROTOCOL_VERSION})
    h.request(1, "hello", {"name": "x"})
    h.recv()
    h.send(Kind.SHUTDOWN, {"timeoutMs": 1000})
    h.thread.join(5)
    assert calls == ["startup", "ready", "shutdown"]
    assert h.exit_code == 0


def test_startup_failure_exits_non_zero(harness):
    app = base_app()

    @app.on("startup")
    def broken():
        raise RuntimeError("cannot start")

    h = harness(app)
    assert h.hello is None
    h.thread.join(5)
    assert h.exit_code == 1


def test_registration_errors():
    app = base_app()
    with pytest.raises(ValueError):
        app.on("bogus")
    with pytest.raises(ValueError):

        @app.command("hello")
        async def again() -> None: ...
