import asyncio

from tauri_runtime import App, Channel, Event, Raw, TauriError

app = App()


@app.command
async def hello(name: str) -> str:
    return f"Hello {name}, from Python!"


@app.command
def reverse(data: Raw) -> Raw:
    return Raw(data.bytes[::-1])


@app.command
async def sleep(ms: int) -> int:
    await asyncio.sleep(ms / 1000)
    return ms


@app.command
async def fail(message: str) -> None:
    raise TauriError("demo_error", message, {"hint": "raised on purpose"})


@app.command
async def progress(steps: int, on_progress: Channel[int]) -> str:
    for i in range(1, steps + 1):
        await asyncio.sleep(0.2)
        await on_progress.send(i)
    return "done"


@app.listen("ping")
async def on_ping(event: Event):
    await app.emit("pong", {"received": event.payload}, target=event.window)


@app.on("ready")
async def on_ready():
    print("Application started")


app.run()
