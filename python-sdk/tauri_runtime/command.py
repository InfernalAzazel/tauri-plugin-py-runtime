"""命令处理器：签名解析、参数绑定与校验，以及调用上下文 `Command` 和结构化错误 `TauriError`。

注册命令时，`build_handler` 根据参数注解将每个参数归类为：调用上下文（`Command`）、
应用实例（`App`）、二进制载荷（`Raw`）、通道（`Channel[T]`）或普通 JSON 参数；
调用时由 `bind_arguments` 按类别填充实参，普通参数经 pydantic 校验。
"""

from __future__ import annotations

import inspect
import json
import re
import typing
from dataclasses import dataclass, field
from typing import Any, Callable

from pydantic import TypeAdapter, ValidationError

from .channel import Channel
from .protocol import Raw


class TauriError(Exception):
    """带错误码的结构化错误，在命令处理器中抛出即可让调用以指定错误失败。

    `code` 会原样透传给前端（`pyInvoke` 拒绝时的错误对象中的 `code`），可用于前端按错误码
    分支处理。处理器抛出的其他异常一律转为 `internal` 错误。

    Args:
        code: 错误码，原样透传给前端。
        message: 可读的错误描述。
        data: 附加数据，须可序列化为 JSON；否则发送时会丢弃 `data` 并记录 `[WARN]` 日志。

    Attributes:
        code: 错误码。
        message: 错误描述。
        data: 附加数据。

    Example:
        ```python
        @app.command
        async def load(path: str) -> str:
            if not path:
                raise TauriError("empty_path", "path must not be empty", {"path": path})
            ...
        ```
    """

    def __init__(self, code: str, message: str, data: Any = None) -> None:
        super().__init__(message)
        self.code = code
        self.message = message
        self.data = data


@dataclass
class Command:
    """单次命令调用的上下文；以 `Command` 注解参数即可注入。

    Attributes:
        id: 调用 id。
        cmd: 命令名。
        window: 发起调用的 webview 标签。
        cancelled: 调用是否已被取消。同步处理器在取消后仍会继续运行，可轮询此标志提前退出。
    """

    id: int
    cmd: str
    window: str | None = None
    cancelled: bool = False


_ACRONYM = re.compile(r"([A-Z]+)([A-Z][a-z])")
_WORD = re.compile(r"([a-z0-9])([A-Z])")


def to_snake(name: str) -> str:
    """将 camelCase / PascalCase 转为 snake_case，连续大写视为缩写（`fileURLPath` → `file_url_path`）。

    Args:
        name: 原始名称；已是 snake_case 的名称保持不变。

    Returns:
        snake_case 名称。
    """
    return _WORD.sub(r"\1_\2", _ACRONYM.sub(r"\1_\2", name)).lower()


_CONTEXT, _APP, _RAW, _CHANNEL, _ARG = range(5)


@dataclass
class Param:
    """处理器的一个形参；`kind` 为 `_CONTEXT` 等类别之一，`adapter` 仅普通 JSON 参数持有。"""

    name: str
    kind: int
    default: Any
    adapter: Any = None


@dataclass
class Handler:
    """已注册的命令：命令名、原始函数及预先解析好的形参列表。"""

    name: str
    func: Callable[..., Any]
    is_async: bool
    params: list[Param] = field(default_factory=list)


def _classify(annotation: Any, app_type: type) -> int:
    """按注解判定参数类别；`App` 子类同样视为应用实例。"""
    origin = typing.get_origin(annotation) or annotation
    if annotation is Command:
        return _CONTEXT
    if isinstance(annotation, type) and issubclass(annotation, app_type):
        return _APP
    if annotation is Raw:
        return _RAW
    if origin is Channel:
        return _CHANNEL
    return _ARG


def _adapter(annotation: Any) -> Any:
    """为注解构建 pydantic `TypeAdapter`；无注解或 pydantic 不支持的类型返回 `None`（不校验）。"""
    if annotation is inspect.Parameter.empty:
        return None
    try:
        return TypeAdapter(annotation)
    except Exception:
        return None


def build_handler(name: str, func: Callable[..., Any], app_type: type) -> Handler:
    """解析函数签名，生成可供路由与绑定使用的 `Handler`。

    Args:
        name: 对外暴露的命令名。
        func: 命令处理函数，同步或异步均可。
        app_type: `App` 类型；注解为它（或其子类）的参数将注入应用实例。以参数传入是为了避免循环导入。

    Returns:
        解析后的 `Handler`。
    """
    hints = typing.get_type_hints(func)
    params = []
    for p in inspect.signature(func).parameters.values():
        annotation = hints.get(p.name, inspect.Parameter.empty)
        kind = _classify(annotation, app_type)
        params.append(Param(p.name, kind, p.default, _adapter(annotation) if kind == _ARG else None))
    return Handler(name, func, inspect.iscoroutinefunction(func), params)


def _validate(param: Param, value: Any) -> Any:
    """用 pydantic 校验并转换参数值；失败时抛出 `invalid_args`，`data` 为校验错误列表。"""
    if param.adapter is None:
        return value
    try:
        return param.adapter.validate_python(value)
    except ValidationError as e:
        raise TauriError(
            "invalid_args", f"invalid argument '{param.name}'", json.loads(e.json(include_url=False))
        ) from None


def bind_arguments(
    handler: Handler,
    args: Any,
    raw: Raw | None,
    channels: dict[str, Channel[Any]],
    ctx: Command,
    app: Any,
) -> dict[str, Any]:
    """为一次调用构造处理器的关键字实参。

    JSON 参数对象的键先转为 snake_case 再与形参名匹配；未提供且有默认值的形参不出现在结果中，
    交由 Python 使用默认值。

    Args:
        handler: 目标处理器。
        args: 请求中的 JSON 参数，须为对象或 `None`。
        raw: 二进制请求体；请求未携带二进制载荷时为 `None`。
        channels: 以 snake_case 参数名为键的通道。
        ctx: 调用上下文。
        app: 应用实例。

    Returns:
        关键字实参字典。

    Raises:
        TauriError: 错误码均为 `invalid_args`：参数不是 JSON 对象、缺少必填参数、缺少通道、
            期望二进制载荷但未提供，或 pydantic 校验失败。
    """
    if args is None:
        args = {}
    if not isinstance(args, dict):
        raise TauriError("invalid_args", "arguments must be a JSON object")
    values = {to_snake(k): v for k, v in args.items()}
    kwargs: dict[str, Any] = {}
    for p in handler.params:
        if p.kind == _CONTEXT:
            kwargs[p.name] = ctx
        elif p.kind == _APP:
            kwargs[p.name] = app
        elif p.kind == _RAW:
            if raw is None:
                raise TauriError("invalid_args", f"parameter '{p.name}' expects raw bytes")
            kwargs[p.name] = raw
        elif p.kind == _CHANNEL:
            if p.name not in channels:
                raise TauriError("invalid_args", f"missing channel '{p.name}'")
            kwargs[p.name] = channels[p.name]
        elif p.name in values:
            kwargs[p.name] = _validate(p, values[p.name])
        elif p.default is inspect.Parameter.empty:
            raise TauriError("invalid_args", f"missing argument '{p.name}'")
    return kwargs
