import asyncio
import os

from tauri_runtime import App, Channel, Event, Raw, TauriError

app = App()


@app.command
async def hello(name: str) -> str:
    return f"Hello {name}"


@app.command
def reverse(data: Raw) -> Raw:
    return Raw(data.bytes[::-1])


@app.command
async def sleep(ms: int) -> int:
    await asyncio.sleep(ms / 1000)
    return ms


@app.command
async def fail() -> None:
    raise TauriError("custom", "custom failure", {"x": 1})


@app.command
async def count(n: int, on_item: Channel[int]) -> int:
    for i in range(n):
        await on_item.send(i)
    return n


@app.command
async def crash() -> None:
    os._exit(3)


@app.listen("ping")
async def on_ping(event: Event):
    await app.emit("pong", event.payload, target=event.window)


app.run()
