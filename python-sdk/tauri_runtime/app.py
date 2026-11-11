from __future__ import annotations

import asyncio
import functools
import inspect
import os
import sys
import threading
import traceback
from concurrent.futures import ThreadPoolExecutor
from typing import Any, BinaryIO, Callable, NoReturn

from .channel import Channel
from .command import Command, TauriError, bind_arguments, build_handler, to_snake
from .event import Event, EventBus
from .protocol import (
    PROTOCOL_VERSION,
    SDK_VERSION,
    Frame,
    FrameTooLarge,
    Kind,
    MalformedFrame,
    ProtocolError,
    Raw,
    Writer,
    decode_payload,
    encode_payload,
    read_frame,
    take_over_stdio,
)
from .router import Router

LIFECYCLE = ("startup", "ready", "shutdown")


class App:
    """Tauri Python sidecar runtime.

    Cancelled synchronous handlers keep running in their worker thread until they
    return; Python cannot interrupt arbitrary thread work.
    """

    def __init__(self, *, stdio: bool = True, max_workers: int | None = None) -> None:
        self._router = Router()
        self._events = EventBus()
        self._hooks: dict[str, list[Callable[..., Any]]] = {name: [] for name in LIFECYCLE}
        self._max_workers = max_workers or min(32, (os.cpu_count() or 1) + 4)
        self._stdio = take_over_stdio() if stdio else None
        self._writer: Writer | None = None
        self._loop: asyncio.AbstractEventLoop | None = None
        self._executor: ThreadPoolExecutor | None = None
        self._calls: dict[int, tuple[asyncio.Future[Any], Command]] = {}
        self._background: set[asyncio.Future[Any]] = set()
        self._protocol_error = False

    # registration -------------------------------------------------------

    def command(self, name_or_func: Any = None) -> Any:
        def register(func: Callable[..., Any], name: str | None) -> Callable[..., Any]:
            self._router.add(build_handler(name or func.__name__, func, App))
            return func

        if callable(name_or_func):
            return register(name_or_func, None)
        return lambda func: register(func, name_or_func)

    def on(self, hook: str) -> Callable[[Callable[..., Any]], Callable[..., Any]]:
        if hook not in self._hooks:
            raise ValueError(f"unknown lifecycle hook '{hook}', expected one of {LIFECYCLE}")

        def decorator(func: Callable[..., Any]) -> Callable[..., Any]:
            self._hooks[hook].append(func)
            return func

        return decorator

    def listen(self, event: str) -> Callable[[Callable[..., Any]], Callable[..., Any]]:
        def decorator(func: Callable[..., Any]) -> Callable[..., Any]:
            self._events.add(event, func)
            return func

        return decorator

    async def emit(self, event: str, payload: Any = None, *, target: str | None = None) -> None:
        if self._writer is None:
            raise RuntimeError("app is not running")
        encoding, body = encode_payload(payload)
        header: dict[str, Any] = {"event": event, "encoding": encoding}
        if target is not None:
            header["target"] = target
        self._writer.write(Frame(Kind.EVENT, header, body))

    # running ------------------------------------------------------------

    def run(self) -> NoReturn:
        if self._stdio is None:
            raise RuntimeError("App(stdio=False) must be driven with serve()")
        reader, writer = self._stdio
        code = asyncio.run(self.serve(reader, writer))
        sys.stderr.flush()
        writer.flush()
        os._exit(code)

    async def serve(self, reader: BinaryIO, writer: BinaryIO) -> int:
        self._loop = asyncio.get_running_loop()
        self._writer = Writer(writer)
        self._executor = ThreadPoolExecutor(self._max_workers, thread_name_prefix="tauri-runtime")
        self._protocol_error = False
        queue: asyncio.Queue[Frame | None] = asyncio.Queue()
        shutdown_timeout_s = 3.0
        try:
            try:
                await self._run_hooks("startup")
            except Exception:
                traceback.print_exc()
                return 1
            self._writer.write(
                Frame(Kind.HELLO, {"protocol": PROTOCOL_VERSION, "sdk": SDK_VERSION, "commands": self._router.names()})
            )
            threading.Thread(target=self._read_loop, args=(reader, queue), name="tauri-runtime-reader", daemon=True).start()
            while (frame := await queue.get()) is not None:
                if frame.kind == Kind.SHUTDOWN:
                    shutdown_timeout_s = frame.header.get("timeoutMs", 3000) / 1000.0
                    break
                try:
                    self._dispatch(frame)
                except Exception as e:
                    print(f"[WARN] dropped {frame.kind.name} frame: {e}", file=sys.stderr)
            await self._stop_tasks()
            await self._run_hooks_logged("shutdown", timeout=shutdown_timeout_s)
            return 2 if self._protocol_error else 0
        finally:
            self._executor.shutdown(wait=False, cancel_futures=True)
            self._writer = None

    def _read_loop(self, reader: BinaryIO, queue: asyncio.Queue[Frame | None]) -> None:
        loop = self._loop
        assert loop is not None

        def put(item: Frame | None) -> None:
            try:
                loop.call_soon_threadsafe(queue.put_nowait, item)
            except RuntimeError:
                pass  # loop already closed

        try:
            while True:
                try:
                    frame = read_frame(reader)
                except MalformedFrame as e:
                    print(f"[WARN] dropped frame: {e}", file=sys.stderr)
                    continue
                if frame is None:
                    break
                put(frame)
        except ProtocolError as e:
            self._protocol_error = True
            print(f"[WARN] protocol error: {e}", file=sys.stderr)
        except OSError as e:
            self._protocol_error = True
            print(f"[WARN] read error: {e}", file=sys.stderr)
        except Exception:
            traceback.print_exc()
        finally:
            put(None)

    def _dispatch(self, frame: Frame) -> None:
        if frame.kind == Kind.READY:
            self._spawn(self._run_hooks_logged("ready"))
        elif frame.kind == Kind.REQUEST:
            self._start_call(frame)
        elif frame.kind == Kind.CANCEL:
            self._cancel_call(frame.header.get("id"))
        elif frame.kind == Kind.EVENT:
            self._deliver_event(frame)
        else:
            print(f"[WARN] unexpected frame {frame.kind.name}", file=sys.stderr)

    # helpers ------------------------------------------------------------

    def _spawn(self, coro: Any) -> asyncio.Future[Any]:
        task = asyncio.ensure_future(coro)
        self._background.add(task)
        task.add_done_callback(self._background.discard)
        return task

    async def _invoke(self, func: Callable[..., Any], *args: Any, **kwargs: Any) -> Any:
        if inspect.iscoroutinefunction(func):
            return await func(*args, **kwargs)
        assert self._loop is not None
        return await self._loop.run_in_executor(self._executor, functools.partial(func, *args, **kwargs))

    async def _run_hooks(self, hook: str) -> None:
        for func in self._hooks[hook]:
            await self._invoke(func)

    async def _run_hooks_logged(self, hook: str, timeout: float | None = None) -> None:
        try:
            if timeout is not None:
                await asyncio.wait_for(self._run_hooks(hook), timeout=timeout)
            else:
                await self._run_hooks(hook)
        except asyncio.TimeoutError:
            print("[WARN] shutdown hooks timed out", file=sys.stderr)
        except Exception:
            traceback.print_exc()

    async def _stop_tasks(self) -> None:
        for call_id in list(self._calls):
            self._cancel_call(call_id)
        pending = list(self._background)
        for task in pending:
            task.cancel()
        await asyncio.gather(*pending, return_exceptions=True)

    # calls --------------------------------------------------------------

    def _start_call(self, frame: Frame) -> None:
        call_id = frame.header.get("id")
        if not isinstance(call_id, int):
            print("[WARN] dropped REQUEST frame: missing or invalid id", file=sys.stderr)
            return
        if call_id in self._calls:
            ctx = Command(id=call_id, cmd=frame.header.get("cmd", ""), window=frame.header.get("window"))
            self._send_error(ctx, "invalid_args", f"duplicate call id {call_id}")
            return
        ctx = Command(id=call_id, cmd=frame.header.get("cmd", ""), window=frame.header.get("window"))
        task = self._spawn(self._handle_call(ctx, frame))
        self._calls[call_id] = (task, ctx)

        def on_done(t: asyncio.Future[Any]) -> None:
            entry = self._calls.get(call_id)
            if entry is not None and entry[0] is t:
                self._calls.pop(call_id, None)

        task.add_done_callback(on_done)

    def _cancel_call(self, call_id: Any) -> None:
        entry = self._calls.pop(call_id, None)
        if entry is not None:
            task, ctx = entry
            ctx.cancelled = True
            task.cancel()

    async def _handle_call(self, ctx: Command, frame: Frame) -> None:
        channels: dict[str, Channel[Any]] = {}
        try:
            result = await self._execute(ctx, frame, channels)
            if not ctx.cancelled:
                encoding, body = encode_payload(result)
                self._send(Frame(Kind.RESPONSE, {"id": ctx.id, "encoding": encoding}, body))
        except asyncio.CancelledError:
            if not ctx.cancelled:
                self._send_error(ctx, "cancelled", "command was cancelled")
        except TauriError as e:
            self._send_error(ctx, e.code, e.message, e.data)
        except FrameTooLarge as e:
            self._send_error(ctx, "frame_too_large", str(e))
        except Exception as e:
            data: dict[str, Any] = {"type": type(e).__name__}
            if os.environ.get("TAURI_RUNTIME_DEBUG") == "1":
                data["traceback"] = traceback.format_exc()
            self._send_error(ctx, "internal", f"{type(e).__name__}: {e}", data)
        finally:
            for channel in channels.values():
                channel.closed = True

    async def _execute(self, ctx: Command, frame: Frame, channels: dict[str, Channel[Any]]) -> Any:
        handler = self._router.get(ctx.cmd)
        if handler is None:
            raise TauriError("command_not_found", f"command '{ctx.cmd}' not found")
        try:
            payload = decode_payload(frame.header.get("encoding", "none"), frame.body)
        except (ValueError, ProtocolError) as e:
            raise TauriError("invalid_args", f"invalid request body: {e}") from e
        raw = payload if isinstance(payload, Raw) else None
        args = None if raw is not None else payload
        assert self._writer is not None
        for name, channel_id in (frame.header.get("channels") or {}).items():
            channels[to_snake(name)] = Channel(channel_id, self._writer)
        kwargs = bind_arguments(handler, args, raw, channels, ctx, self)
        return await self._invoke(handler.func, **kwargs)

    def _send(self, frame: Frame) -> None:
        if self._writer is not None:
            self._writer.write(frame)

    def _send_error(self, ctx: Command, code: str, message: str, data: Any = None) -> None:
        if ctx.cancelled:
            return
        header: dict[str, Any] = {"id": ctx.id, "code": code, "message": message}
        if data is not None:
            header["data"] = data
        self._send(Frame(Kind.ERROR, header))

    # events -------------------------------------------------------------

    def _deliver_event(self, frame: Frame) -> None:
        try:
            payload = decode_payload(frame.header.get("encoding", "none"), frame.body)
        except (ValueError, ProtocolError) as e:
            print(f"[WARN] dropped event frame: {e}", file=sys.stderr)
            return
        event = Event(
            frame.header.get("event", ""),
            payload,
            frame.header.get("window"),
        )
        for func in self._events.get(event.name):
            self._spawn(self._run_listener(func, event))

    async def _run_listener(self, func: Callable[..., Any], event: Event) -> None:
        try:
            await self._invoke(func, event)
        except Exception:
            traceback.print_exc()
