"""运行时核心：`App` 负责注册命令、钩子与事件监听，并驱动协议主循环。

主循环在 asyncio 事件循环上运行：专用线程阻塞读取帧，按消息类型分发为调用任务、取消、
事件投递或生命周期钩子；所有输出经线程安全的 `Writer` 写回宿主。
"""

from __future__ import annotations

import asyncio
import contextlib
import functools
import inspect
import logging
import math
import os
import sys
import traceback
from collections.abc import Callable
from concurrent.futures import ThreadPoolExecutor
from typing import Any, BinaryIO, NoReturn

from pydantic.alias_generators import to_snake

from . import protocol as wire
from .channel import Channel
from .command import Command, TauriError, bind_arguments, build_handler
from .event import Event, EventBus
from .protocol import (
    PROTOCOL_VERSION,
    Frame,
    FrameTooLarge,
    MalformedFrame,
    ProtocolError,
    Raw,
    Writer,
    join_payload,
    read_frame,
    split_payload,
    take_over_stdio,
)
from .router import Router

LIFECYCLE = ("startup", "ready", "shutdown")
RESERVED_EVENTS = ("exited",)
DEFAULT_SHUTDOWN_TIMEOUT_S = 3.0

logger = logging.getLogger(__name__)


def shutdown_budget(value: Any) -> float:
    """将 `Shutdown.timeoutMs` 换算为关闭预算（秒）。

    Args:
        value: 原始 `timeoutMs`；非数值、布尔值、非有限值或负数均视为非法。

    Returns:
        关闭预算（秒）；非法值回退为 `DEFAULT_SHUTDOWN_TIMEOUT_S`（3 秒）。
    """
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return DEFAULT_SHUTDOWN_TIMEOUT_S
    if not math.isfinite(value) or value < 0:
        return DEFAULT_SHUTDOWN_TIMEOUT_S
    return value / 1000.0


def _exit_code(exc: SystemExit) -> int:
    """按解释器的约定将 `SystemExit` 转为退出码：`None` 为 0，整数原样，其他值打印到 stderr 后为 1。"""
    if exc.code is None:
        return 0
    if isinstance(exc.code, int):
        return exc.code
    logger.error("[ERROR] %s", exc.code)
    return 1


class App:
    """Tauri Python sidecar 运行时。

    一个进程通常只创建一个 `App`：用装饰器注册命令、生命周期钩子和事件监听，最后调用
    `run()` 接管进程并进入协议主循环。

    Example:
        ```python
        from tauri_runtime import App, Channel, Event

        app = App()

        @app.command
        async def hello(name: str) -> str:
            return f"Hello {name}"

        @app.on("ready")
        async def on_ready():
            print("[INFO] backend ready")

        @app.listen("ping")
        async def on_ping(event: Event):
            await app.emit("pong", event.payload, target=event.window)

        app.run()
        ```

    生命周期钩子（`@app.on(...)`）：
        - `startup`：握手之前运行；抛出异常会终止运行时（退出码 1），不会发送 `hello`。
        - `ready`：收到 Rust 侧的 `ready` 确认后运行；异常仅记录日志。
        - `shutdown`：收到 `shutdown` 或输入流结束后运行，与取消在途调用共享同一关闭预算，
          超时即放弃；异常仅记录日志。

    同步与异步处理器：
        `async def` 命令、钩子和监听器直接在事件循环上运行；普通 `def` 在线程池中运行
        （默认 `min(32, cpu_count + 4)` 个工作线程），不会阻塞事件循环。

    取消语义：
        前端中止或调用超时时，Rust 发送 `cancel`，运行时将对应 `Command.cancelled` 置为
        `True` 并取消其 asyncio 任务。异步处理器在下一个 `await` 处收到 `CancelledError`；
        同步处理器无法被中断（Python 不能打断任意线程），会在工作线程中继续运行直至返回，
        其结果被丢弃，可轮询 `Command.cancelled` 主动提前退出。已取消的调用不会再发送任何响应。

    参数绑定：
        - 请求中 JSON 参数对象的 camelCase 键转为 snake_case 后与形参名匹配。
        - 以下注解的参数由运行时注入，而非取自 JSON：`Command`（调用上下文）、`App`
          （应用实例）、`Raw`（二进制请求体）、`Channel[T]`（同名的前端通道）。
        - 带注解的普通参数经 pydantic 校验与转换；校验失败或缺少必填参数时以 `invalid_args`
          失败，校验错误放在 `data` 中。
        - 返回值可以是任意可被 pydantic 序列化为 JSON 的值；返回 `Raw` / `bytes` 则以二进制发送。
          抛出 `TauriError` 返回自定义错误码，其他异常转为 `internal`（设置环境变量
          `TAURI_RUNTIME_DEBUG=1` 时 `data` 中附带 traceback）。

    stdio 接管：
        默认（`stdio=True`）构造时即接管 stdio：原 stdin/stdout 专供协议使用，`sys.stdout`
        改指向 stderr，因此 `print()` 是安全的。stderr 由宿主按行转发到 Rust `log`（target 为
        `py-runtime`），行首的 `[DEBUG]` / `[INFO]` / `[WARN]` / `[ERROR]` 前缀决定日志级别并被
        去除，无前缀的行按 `INFO` 记录。

    Args:
        stdio: 是否在构造时接管进程 stdio。设为 `False` 时须自行调用 `serve()` 并提供流（如测试）。
        max_workers: 同步处理器线程池大小；默认 `min(32, cpu_count + 4)`。
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

    # 注册 ---------------------------------------------------------------

    def command(self, name_or_func: Any = None) -> Any:
        """注册命令处理器的装饰器，可直接使用或传入自定义命令名。

        命令须在 `run()` / `serve()` 之前注册，命令列表随 `hello` 握手发送给宿主。

        Args:
            name_or_func: 被装饰的函数（`@app.command`），或自定义命令名
                （`@app.command("name")`）；省略时使用函数名。

        Returns:
            直接装饰时返回原函数；传入命令名时返回装饰器。

        Raises:
            ValueError: 命令名已注册。

        Example:
            ```python
            @app.command
            async def hello(name: str) -> str:
                return f"Hello {name}"

            @app.command("get_version")
            def version() -> str:
                return "1.0.0"
            ```
        """

        def register(func: Callable[..., Any], name: str | None) -> Callable[..., Any]:
            self._router.add(build_handler(name or func.__name__, func, App))
            return func

        if callable(name_or_func):
            return register(name_or_func, None)
        return lambda func: register(func, name_or_func)

    def on(self, hook: str) -> Callable[[Callable[..., Any]], Callable[..., Any]]:
        """注册生命周期钩子的装饰器。

        钩子不接收参数，可为同步或异步函数；同一钩子的多个函数按注册顺序依次运行。
        各钩子的触发时机与异常处理见类文档。

        Args:
            hook: `"startup"`、`"ready"` 或 `"shutdown"`。

        Returns:
            装饰器，原样返回被装饰的函数。

        Raises:
            ValueError: 未知的钩子名。
        """
        if hook not in self._hooks:
            raise ValueError(f"unknown lifecycle hook '{hook}', expected one of {LIFECYCLE}")

        def decorator(func: Callable[..., Any]) -> Callable[..., Any]:
            self._hooks[hook].append(func)
            return func

        return decorator

    def listen(self, event: str) -> Callable[[Callable[..., Any]], Callable[..., Any]]:
        """注册事件监听器的装饰器，接收前端通过 `pyEmit` 发送的事件。

        监听器接收唯一参数 `Event`，可为同步或异步函数。每次事件为每个监听器启动独立任务，
        监听器的异常仅记录日志。

        Args:
            event: 事件名。

        Returns:
            装饰器，原样返回被装饰的函数。
        """

        def decorator(func: Callable[..., Any]) -> Callable[..., Any]:
            self._events.add(event, func)
            return func

        return decorator

    async def emit(self, event: str, payload: Any = None, *, target: str | None = None) -> None:
        """向前端发送事件。

        Args:
            event: 事件名。`exited` 保留给运行时自身的 `py://exited` 事件。
            payload: 事件载荷：可被 pydantic 序列化为 JSON 的值，或 `Raw` / `bytes`。
            target: 目标 webview 标签；为 `None` 时广播给所有 webview。

        Raises:
            ValueError: 事件名为保留名，或载荷无法编码为合法 JSON（如包含 `NaN` / `±Infinity`）。
            RuntimeError: 应用尚未运行。
            FrameTooLarge: 编码后超过帧大小上限。
        """
        if event in RESERVED_EVENTS:
            raise ValueError(f"event name '{event}' is reserved by the runtime")
        if self._writer is None:
            raise RuntimeError("app is not running")
        fields, body = split_payload(payload)
        self._writer.write(wire.Event(event=event, target=target, **fields), body)

    # 运行 ---------------------------------------------------------------

    def run(self) -> NoReturn:
        """在已接管的 stdio 上运行协议主循环，结束后终止进程，永不返回。

        退出码：正常结束为 0；发生协议错误（流损坏、读错误）为 2；`startup` 钩子失败或有
        其他异常逃逸为 1；处理器中抛出的 `SystemExit`（如 `sys.exit(n)`）使用其退出码。
        退出前会关闭 `logging` 并 flush stderr 与协议输出流。

        Raises:
            RuntimeError: 以 `App(stdio=False)` 构造，此时应改用 `serve()`。
        """
        if self._stdio is None:
            raise RuntimeError("App(stdio=False) must be driven with serve()")
        reader, writer = self._stdio
        code = 1
        try:
            code = self._drive(reader, writer)
        except SystemExit as e:
            code = _exit_code(e)
        except BaseException:
            logger.exception("[ERROR] runtime crashed")
        finally:
            logging.shutdown()
            for stream in (sys.stderr, writer):
                # 管道已断开（OSError）或流已关闭（ValueError）时无处可报，直接忽略。
                with contextlib.suppress(OSError, ValueError):
                    stream.flush()
            # 用 os._exit 跳过解释器退出时对工作线程的 join，卡住的同步处理器不会阻塞退出。
            os._exit(code)

    def _drive(self, reader: BinaryIO, writer: BinaryIO) -> int:
        """在新事件循环上运行 `serve()`。

        不使用 `asyncio.run`：它结束时会取消并等待所有残留任务，吞掉 `CancelledError` 的
        处理器会让进程永远挂起。
        """
        loop = asyncio.new_event_loop()
        try:
            return loop.run_until_complete(self.serve(reader, writer))
        finally:
            loop.close()

    async def serve(self, reader: BinaryIO, writer: BinaryIO) -> int:
        """在给定的流上运行协议主循环，直到收到 `shutdown` 或输入流结束。

        流程：运行 `startup` 钩子 → 发送 `hello` → 循环读取并分发帧 → 取消在途调用与后台任务
        → 运行 `shutdown` 钩子。取消与 `shutdown` 钩子共享同一关闭预算（来自 `timeoutMs`，
        输入流结束时为 3 秒），超出预算的任务被放弃而非等待。

        与 `run()` 不同，本方法不接管 stdio、也不退出进程，适合测试或自定义宿主。

        Args:
            reader: 协议输入流（阻塞读取）。
            writer: 协议输出流。

        Returns:
            退出码：0 为正常结束，1 为 `startup` 钩子失败，2 为发生协议错误。
        """
        self._loop = asyncio.get_running_loop()
        self._writer = Writer(writer)
        self._executor = ThreadPoolExecutor(self._max_workers, thread_name_prefix="tauri-runtime")
        self._protocol_error = False
        # 读取使用独立的单线程池：与处理器线程池分开，繁忙的同步处理器不会饿死读取。
        reader_pool = ThreadPoolExecutor(1, thread_name_prefix="tauri-runtime-reader")
        budget = DEFAULT_SHUTDOWN_TIMEOUT_S
        try:
            try:
                await self._run_hooks("startup")
            except Exception:
                logger.exception("[ERROR] startup hook failed")
                return 1
            self._writer.write(wire.Hello(protocol=PROTOCOL_VERSION, commands=self._router.names()))
            while (frame := await self._next_frame(reader_pool, reader)) is not None:
                if isinstance(frame.message, wire.Shutdown):
                    budget = shutdown_budget(frame.message.timeout_ms)
                    break
                try:
                    self._dispatch(frame)
                except Exception:
                    logger.warning("[WARN] dropped %s frame", frame.message.type, exc_info=True)
            deadline = self._loop.time() + budget
            await self._stop_tasks(budget)
            await self._run_hooks_logged("shutdown", timeout=max(0.0, deadline - self._loop.time()))
            return 2 if self._protocol_error else 0
        finally:
            # 阻塞中的读取和卡住的处理器都不能拖延退出，因此两个线程池都不 join。
            reader_pool.shutdown(wait=False, cancel_futures=True)
            self._executor.shutdown(wait=False, cancel_futures=True)
            self._writer = None

    async def _next_frame(self, pool: ThreadPoolExecutor, reader: BinaryIO) -> Frame | None:
        """读取下一个可解码的帧（跳过无法解码的帧）；流结束或损坏时返回 `None`。"""
        assert self._loop is not None
        while True:
            try:
                return await self._loop.run_in_executor(pool, read_frame, reader)
            except MalformedFrame as e:
                logger.warning("[WARN] dropped frame: %s", e)
            except ProtocolError as e:
                self._protocol_error = True
                logger.warning("[WARN] protocol error: %s", e)
                return None
            except OSError as e:
                self._protocol_error = True
                logger.warning("[WARN] read error: %s", e)
                return None
            except Exception:
                logger.exception("[ERROR] frame reader crashed")
                return None

    def _dispatch(self, frame: Frame) -> None:
        match frame.message:
            case wire.Ready():
                self._spawn(self._run_hooks_logged("ready"))
            case wire.Request() as request:
                self._start_call(request, frame.body)
            case wire.Cancel(id=call_id):
                self._cancel_call(call_id)
            case wire.Event() as event:
                self._deliver_event(event, frame.body)
            case other:
                logger.warning("[WARN] unexpected frame %s", other.type)

    # 辅助方法 -----------------------------------------------------------

    def _spawn(self, coro: Any) -> asyncio.Future[Any]:
        """启动后台任务并持有其强引用，关闭时统一取消。"""
        task = asyncio.ensure_future(coro)
        self._background.add(task)
        task.add_done_callback(self._background.discard)
        return task

    async def _invoke(self, func: Callable[..., Any], *args: Any, **kwargs: Any) -> Any:
        """协程函数在事件循环上 await，普通函数交给处理器线程池执行。"""
        if inspect.iscoroutinefunction(func):
            return await func(*args, **kwargs)
        assert self._loop is not None
        return await self._loop.run_in_executor(self._executor, functools.partial(func, *args, **kwargs))

    async def _run_hooks(self, hook: str) -> None:
        for func in self._hooks[hook]:
            await self._invoke(func)

    async def _run_hooks_logged(self, hook: str, timeout: float | None = None) -> None:
        """运行钩子，异常只记录不抛出；指定 `timeout` 时超时即取消并放弃。"""
        if timeout is None:
            try:
                await self._run_hooks(hook)
            except Exception:
                logger.exception("[ERROR] %s hook failed", hook)
            return
        # 用 asyncio.wait 而非 wait_for：后者超时后会等待被取消的任务结束，
        # 吞掉 CancelledError 的钩子会借此突破关闭预算。
        task = asyncio.ensure_future(self._run_hooks(hook))
        done, _ = await asyncio.wait({task}, timeout=timeout)
        if not done:
            task.cancel()
            logger.warning("[WARN] %s hooks timed out", hook)
        elif not task.cancelled() and task.exception() is not None:
            logger.error("[ERROR] %s hook failed", hook, exc_info=task.exception())

    async def _stop_tasks(self, timeout: float) -> None:
        """取消所有在途调用与后台任务，至多等待 `timeout` 秒，忽略取消的任务被放弃。"""
        for call_id in list(self._calls):
            self._cancel_call(call_id)
        pending = list(self._background)
        for task in pending:
            task.cancel()
        if not pending:
            return
        _, still_running = await asyncio.wait(pending, timeout=timeout)
        if still_running:
            logger.warning("[WARN] %d task(s) ignored cancellation; abandoning them", len(still_running))

    # 调用 ---------------------------------------------------------------

    def _start_call(self, request: wire.Request, body: bytes) -> None:
        """为请求启动调用任务；调用 id 与在途调用重复时直接以 `invalid_args` 拒绝。"""
        call_id = request.id
        ctx = Command(id=call_id, cmd=request.cmd, window=request.window)
        if call_id in self._calls:
            self._send_error(ctx, "invalid_args", f"duplicate call id {call_id}")
            return
        task = self._spawn(self._handle_call(ctx, request, body))
        self._calls[call_id] = (task, ctx)

        def on_done(t: asyncio.Future[Any]) -> None:
            # 同一 id 可能已被取消后复用，只移除属于本任务的条目。
            entry = self._calls.get(call_id)
            if entry is not None and entry[0] is t:
                self._calls.pop(call_id, None)

        task.add_done_callback(on_done)

    def _cancel_call(self, call_id: int) -> None:
        entry = self._calls.pop(call_id, None)
        if entry is not None:
            task, ctx = entry
            ctx.cancelled = True
            task.cancel()

    async def _handle_call(self, ctx: Command, request: wire.Request, body: bytes) -> None:
        """执行调用并将结果或异常映射为 `Response` / `Error`；结束后关闭本次调用的通道。"""
        channels: dict[str, Channel[Any]] = {}
        try:
            result = await self._execute(ctx, request, body, channels)
            if not ctx.cancelled:
                fields, result_body = split_payload(result)
                self._send(wire.Response(id=ctx.id, **fields), result_body)
        except asyncio.CancelledError:
            if not ctx.cancelled:
                self._send_error(ctx, "cancelled", "command was cancelled")
        except TauriError as e:
            self._send_error(ctx, e.code, e.message, e.data)
        except FrameTooLarge as e:
            self._send_error(ctx, "frame_too_large", str(e))
        except Exception as e:
            logger.exception("[ERROR] command %r failed", ctx.cmd)
            data: dict[str, Any] = {"type": type(e).__name__}
            if os.environ.get("TAURI_RUNTIME_DEBUG") == "1":
                data["traceback"] = traceback.format_exc()
            self._send_error(ctx, "internal", f"{type(e).__name__}: {e}", data)
        finally:
            for channel in channels.values():
                channel.closed = True

    async def _execute(
        self, ctx: Command, request: wire.Request, body: bytes, channels: dict[str, Channel[Any]]
    ) -> Any:
        handler = self._router.get(ctx.cmd)
        if handler is None:
            raise TauriError("command_not_found", f"command '{ctx.cmd}' not found")
        try:
            payload = join_payload(request, body)
        except ProtocolError as e:
            raise TauriError("invalid_args", f"invalid request body: {e}") from e
        raw = payload if isinstance(payload, Raw) else None
        args = None if raw is not None else payload
        assert self._writer is not None
        for name, channel_id in request.channels.items():
            channels[to_snake(name)] = Channel(channel_id, self._writer)
        kwargs = bind_arguments(handler, args, raw, channels, ctx, self)
        return await self._invoke(handler.func, **kwargs)

    def _send(self, message: wire.Message, body: bytes = b"") -> None:
        if self._writer is not None:
            self._writer.write(message, body)

    def _send_error(self, ctx: Command, code: str, message: str, data: Any = None) -> None:
        """发送错误响应；`data` 无法编码或过大时去掉 `data` 重发，已取消的调用不发送。"""
        if ctx.cancelled:
            return
        error = wire.Error(id=ctx.id, code=code, message=message, data=data)
        try:
            self._send(error)
        except (ValueError, FrameTooLarge) as e:
            if data is None:
                raise
            logger.warning("[WARN] dropped error data for call %d: %s", ctx.id, e)
            self._send(error.model_copy(update={"data": None}))

    # 事件 ---------------------------------------------------------------

    def _deliver_event(self, message: wire.Event, body: bytes) -> None:
        """为该事件的每个监听器各启动一个后台任务。"""
        try:
            payload = join_payload(message, body)
        except ProtocolError as e:
            logger.warning("[WARN] dropped event frame: %s", e)
            return
        event = Event(message.event, payload, message.window)
        for func in self._events.get(event.name):
            self._spawn(self._run_listener(func, event))

    async def _run_listener(self, func: Callable[..., Any], event: Event) -> None:
        try:
            await self._invoke(func, event)
        except Exception:
            logger.exception("[ERROR] listener for event %r failed", event.name)
