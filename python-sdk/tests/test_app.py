import asyncio
import time

import pytest

from conftest import raw_frame

from tauri_runtime import App, Channel, Command, Event, Raw, TauriError
from tauri_runtime import protocol as wire
from tauri_runtime.protocol import PROTOCOL_VERSION, Frame, encode


def base_app():
    app = App(stdio=False)

    @app.command
    async def hello(name: str) -> str:
        return f"Hello {name}"

    return app


def test_hello_frame_lists_commands(harness):
    h = harness(base_app())
    assert h.hello.message == wire.Hello(protocol=PROTOCOL_VERSION, commands=["hello"])


def test_json_call(harness):
    h = harness(base_app())
    h.request(1, "hello", {"name": "Kylin"})
    assert h.recv() == Frame(wire.Response(id=1, value="Hello Kylin"))


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
    assert h.recv() == Frame(wire.Response(id=1, raw=True), b"\x02\x01\x00")
    h.request(2, "noop")
    assert h.recv() == Frame(wire.Response(id=2))


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
    assert [h.recv().message.id, h.recv().message.id] == [2, 1]


def test_command_not_found(harness):
    h = harness(base_app())
    h.request(1, "missing")
    assert h.recv().message.code == "command_not_found"


def test_invalid_args(harness):
    h = harness(base_app())
    h.request(1, "hello", {})
    assert h.recv().message.code == "invalid_args"


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
    assert h.recv().message == wire.Error(id=1, code="not_allowed", message="nope", data={"k": 1})
    h.request(2, "boom")
    error = wire.Error(id=2, code="internal", message="ValueError: boom", data={"type": "ValueError"})
    assert h.recv().message == error


def test_cancel_suppresses_response(harness):
    app = base_app()

    @app.command
    async def wait(ctx: Command) -> str:
        await asyncio.sleep(10)
        return "done"

    h = harness(app)
    h.request(1, "wait")
    h.send(wire.Cancel(id=1, reason="client"))
    h.request(2, "hello", {"name": "x"})
    assert h.recv().message.id == 2


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
    assert [f.message for f in frames] == [
        wire.ChannelData(channel=7, value=0),
        wire.ChannelData(channel=7, value=1),
        wire.ChannelData(channel=7, value=2),
        wire.Response(id=1, value=3),
    ]


def test_events_roundtrip(harness):
    app = App(stdio=False)

    @app.listen("user:login")
    async def on_login(event: Event):
        await app.emit("welcome", {"user": event.payload["name"]}, target=event.window)

    h = harness(app)
    h.send(wire.Event(event="user:login", window="main", value={"name": "Ky"}))
    assert h.recv() == Frame(wire.Event(event="welcome", target="main", value={"user": "Ky"}))


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
    h.send(wire.Ready(protocol=PROTOCOL_VERSION, app=wire.AppInfo(name="app", version="1.0.0")))
    h.request(1, "hello", {"name": "x"})
    h.recv()
    h.send(wire.Shutdown(timeout_ms=1000))
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


def test_malformed_event_dropped_hello_still_works(harness):
    app = base_app()

    @app.listen("e")
    async def on_e(event: Event):
        raise AssertionError("listener should not run")

    h = harness(app)
    h.send(wire.Event(event="e", value=1), b"unexpected body")
    h.send_bytes(raw_frame(b'{"type":"event","event":"e","value":{not json}'))
    h.request(1, "hello", {"name": "Ky"})
    assert h.recv() == Frame(wire.Response(id=1, value="Hello Ky"))


@pytest.mark.parametrize(
    "header",
    [
        b'{"type":"request","cmd":"hello","window":"main","value":{"name":"x"}}',
        b'{"type":"request","id":"1","cmd":"hello"}',
        b'{"type":"teleport","id":1}',
    ],
)
def test_malformed_request_dropped_hello_still_works(harness, header):
    h = harness(base_app())
    h.send_bytes(raw_frame(header))
    h.request(1, "hello", {"name": "Ky"})
    assert h.recv() == Frame(wire.Response(id=1, value="Hello Ky"))


def test_body_without_raw_flag_is_invalid_args(harness):
    h = harness(base_app())
    h.send(wire.Request(id=1, cmd="hello", window="main", value={"name": "x"}), b"{bad")
    f = h.recv()
    assert f.message.code == "invalid_args"
    assert "invalid request body" in f.message.message


def test_handler_cancelled_error_returns_cancelled(harness):
    app = App(stdio=False)

    @app.command
    async def bail() -> None:
        raise asyncio.CancelledError()

    h = harness(app)
    h.request(1, "bail")
    assert h.recv().message == wire.Error(id=1, code="cancelled", message="command was cancelled")


def test_duplicate_call_id(harness):
    app = base_app()

    @app.command
    async def linger() -> str:
        await asyncio.sleep(10)
        return "done"

    h = harness(app)
    h.request(1, "linger")
    h.request(1, "hello", {"name": "x"})
    f = h.recv().message
    assert (f.type, f.id, f.code) == ("error", 1, "invalid_args")
    assert "duplicate call id" in f.message
    h.send(wire.Cancel(id=1, reason="client"))
    h.request(2, "hello", {"name": "ok"})
    assert h.recv() == Frame(wire.Response(id=2, value="Hello ok"))


def stubborn_app():
    app = base_app()

    @app.command
    async def stubborn() -> str:
        while True:
            try:
                await asyncio.sleep(10)
            except asyncio.CancelledError:
                pass

    return app


def test_handler_swallowing_cancel_does_not_block_shutdown(harness):
    h = harness(stubborn_app())
    h.request(1, "stubborn")
    h.request(2, "hello", {"name": "x"})
    h.recv()
    started = time.monotonic()
    h.send(wire.Shutdown(timeout_ms=300))
    h.thread.join(5)
    assert not h.thread.is_alive()
    assert time.monotonic() - started < 2
    assert h.exit_code == 0


def test_stdin_eof_with_stubborn_handler_uses_default_budget(harness):
    h = harness(stubborn_app())
    h.request(1, "stubborn")
    h.request(2, "hello", {"name": "x"})
    h.recv()
    started = time.monotonic()
    h._to_app.close()
    h.thread.join(8)
    assert not h.thread.is_alive()
    assert time.monotonic() - started < 5


@pytest.mark.parametrize("timeout_ms", ["soon", -5, None, True, [1]])
def test_invalid_shutdown_timeout_is_tolerated(harness, timeout_ms):
    app = base_app()
    calls = []

    @app.on("shutdown")
    def on_shutdown():
        calls.append("shutdown")

    h = harness(app)
    h.send(wire.Shutdown(timeout_ms=timeout_ms))
    h.thread.join(5)
    assert h.exit_code == 0
    assert calls == ["shutdown"]


def test_shutdown_budget_parsing():
    from tauri_runtime.app import shutdown_budget

    assert shutdown_budget(1500) == 1.5
    assert shutdown_budget(0) == 0.0
    for bad in ("soon", -5, None, True, float("inf"), float("nan")):
        assert shutdown_budget(bad) == 3.0


def test_nan_result_is_internal_error(harness):
    app = App(stdio=False)

    @app.command
    async def nan() -> float:
        return float("nan")

    h = harness(app)
    h.request(1, "nan")
    assert h.recv().message.code == "internal"


def test_unserializable_error_data_falls_back(harness, caplog):
    app = App(stdio=False)

    @app.command
    async def odd() -> None:
        raise TauriError("odd", "bad data", {"obj": object()})

    h = harness(app)
    h.request(1, "odd")
    assert h.recv().message == wire.Error(id=1, code="odd", message="bad data")
    assert any(r.levelname == "WARNING" and "dropped error data" in r.message for r in caplog.records)


def test_exited_event_name_is_reserved(harness):
    app = App(stdio=False)
    errors = []

    @app.command
    async def bad() -> None:
        try:
            await app.emit("exited", {})
        except ValueError as e:
            errors.append(str(e))

    h = harness(app)
    h.request(1, "bad")
    h.recv()
    assert len(errors) == 1 and "exited" in errors[0]


def test_truncated_frame_exit_code_2(harness):
    h = harness(base_app())
    partial = encode(Frame(wire.Request(id=1, cmd="hello")))[:6]
    h._to_app.write(partial)
    h._to_app.flush()
    h._to_app.close()
    h.thread.join(5)
    assert h.exit_code == 2
