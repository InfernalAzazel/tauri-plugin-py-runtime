"""命令式注册（`App.add_*`）与分组路由（`Router` / `App.include`）的测试。"""

import pytest

from tauri_runtime import App, Event, Router
from tauri_runtime import protocol as wire
from tauri_runtime.protocol import PROTOCOL_VERSION, Frame


def test_add_command_and_add_commands(harness):
    app = App(stdio=False)

    def ping() -> str:
        return "pong"

    async def echo(text: str) -> str:
        return text

    app.add_command(ping, "health.ping")
    app.add_commands(echo)
    h = harness(app)
    assert h.hello.message == wire.Hello(protocol=PROTOCOL_VERSION, commands=["echo", "health.ping"])
    h.request(1, "health.ping")
    assert h.recv() == Frame(wire.Response(id=1, value="pong"))
    h.request(2, "echo", {"text": "hi"})
    assert h.recv() == Frame(wire.Response(id=2, value="hi"))


def test_router_prefix_nesting_and_names(harness):
    session = Router(prefix="session")

    @session.command
    async def login(user_name: str) -> str:
        return f"welcome {user_name}"

    @session.command("logout")
    def sign_out() -> None:
        return None

    user = Router(prefix="user")
    user.add_commands(lambda: "me")
    user.include(session)

    root = Router()
    root.add_command(lambda: 1, "version")

    app = App(stdio=False)
    app.include(user)
    app.include(root)
    h = harness(app)
    assert h.hello.message.commands == ["user.<lambda>", "user.session.login", "user.session.logout", "version"]
    h.request(1, "user.session.login", {"userName": "Ky"})
    assert h.recv() == Frame(wire.Response(id=1, value="welcome Ky"))


def test_router_listeners_and_hooks(harness):
    router = Router(prefix="ignored.for.events")
    calls = []
    app = App(stdio=False)

    @router.on("startup")
    def boot() -> None:
        calls.append("startup")

    @router.listen("user:login")
    async def on_login(event: Event) -> None:
        await app.emit("welcome", event.payload, target=event.window)

    app.include(router)
    h = harness(app)
    assert calls == ["startup"]
    h.send(wire.Event(event="user:login", window="main", value={"name": "Ky"}))
    assert h.recv() == Frame(wire.Event(event="welcome", target="main", value={"name": "Ky"}))


def test_include_takes_snapshot():
    router = Router(prefix="a")
    router.add_command(lambda: 1, "one")
    app = App(stdio=False)
    app.include(router)
    router.add_command(lambda: 2, "two")
    assert app._router.names() == ["a.one"]


def test_duplicate_command_across_routers_is_rejected():
    first, second = Router(prefix="user"), Router(prefix="user")
    first.add_command(lambda: 1, "get")
    second.add_command(lambda: 2, "get")
    app = App(stdio=False)
    app.include(first)
    with pytest.raises(ValueError, match="user.get"):
        app.include(second)


def test_invalid_prefix_and_unknown_hook():
    with pytest.raises(ValueError):
        Router(prefix=".user")
    with pytest.raises(ValueError):
        Router(prefix="user.")
    with pytest.raises(ValueError):
        Router().on("boot")
    with pytest.raises(ValueError):
        Router().add_hook("boot", lambda: None)
    with pytest.raises(ValueError):
        App(stdio=False).add_hook("boot", lambda: None)
