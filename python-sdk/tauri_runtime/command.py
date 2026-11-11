"""命令处理器：签名解析、参数绑定与校验，以及调用上下文 `Command` 和结构化错误 `TauriError`。

注册命令时，`build_handler` 根据参数注解将每个参数归类为：调用上下文（`Command`）、
应用实例（`App`）、二进制载荷（`Raw`）、通道（`Channel[T]`）或普通 JSON 参数；
普通参数汇总为每个处理器一个的 pydantic 模型；调用时由 `bind_arguments` 按类别填充实参，
普通参数经该模型一次性校验。
"""

from __future__ import annotations

import inspect
import json
import typing
from collections.abc import Callable
from dataclasses import dataclass
from typing import Any

from pydantic import BaseModel, ConfigDict, ValidationError, create_model
from pydantic.alias_generators import to_camel

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


_CONTEXT, _APP, _RAW, _CHANNEL, _ARG = range(5)

# 键名规则：形参名经 `to_camel` 得到别名（`file_url_path` → `fileUrlPath`），别名与形参名均可作键；
# 连续大写的键（如 `fileURLPath`）不会被识别。通道键则经 `to_snake` 映射（`fileURLPath` → `file_url_path`）。
_MODEL_CONFIG = ConfigDict(
    alias_generator=to_camel, populate_by_name=True, arbitrary_types_allowed=False, protected_namespaces=()
)


@dataclass
class Param:
    """处理器的一个注入型形参；`kind` 为 `_CONTEXT`、`_APP`、`_RAW` 或 `_CHANNEL`。"""

    name: str
    kind: int


@dataclass
class Handler:
    """已注册的命令：命令名、原始函数、注入型形参列表及 JSON 参数的 pydantic 模型。"""

    name: str
    func: Callable[..., Any]
    is_async: bool
    params: list[Param]
    model: type[BaseModel]


def _classify(annotation: Any, app_type: type) -> int:
    """按注解判定参数类别；`App` 子类同样视为应用实例，`Annotated[T, ...]` 按 `T` 判定。"""
    if typing.get_origin(annotation) is typing.Annotated:
        annotation = annotation.__origin__
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


def build_handler(name: str, func: Callable[..., Any], app_type: type) -> Handler:
    """解析函数签名，生成可供路由与绑定使用的 `Handler`。

    注入型参数之外的 JSON 参数汇总为一个 pydantic 模型；无注解的参数按 `Any` 处理。

    Args:
        name: 对外暴露的命令名。
        func: 命令处理函数，同步或异步均可。
        app_type: `App` 类型；注解为它（或其子类）的参数将注入应用实例。以参数传入是为了避免循环导入。

    Returns:
        解析后的 `Handler`。

    Raises:
        TypeError: 某个 JSON 参数的注解无法由 pydantic 校验，或参数名无法作为模型字段（如以 `_` 开头）。
    """
    # 保留 `Annotated` 元数据，否则 `Field(ge=1)` 等约束会被丢弃
    hints = typing.get_type_hints(func, include_extras=True)
    model_name = f"{name}_args"
    params, fields = [], {}
    for p in inspect.signature(func).parameters.values():
        annotation = hints.get(p.name, Any)
        kind = _classify(annotation, app_type)
        if kind != _ARG:
            params.append(Param(p.name, kind))
            continue
        spec = (annotation, ... if p.default is inspect.Parameter.empty else p.default)
        try:
            ok = p.name in create_model(model_name, __config__=_MODEL_CONFIG, **{p.name: spec}).model_fields
        except Exception as e:
            raise TypeError(f"command '{name}': unsupported annotation for parameter '{p.name}': {e}") from e
        if not ok:
            raise TypeError(f"command '{name}': parameter '{p.name}' cannot be a model field")
        fields[p.name] = spec
    model = create_model(model_name, __config__=_MODEL_CONFIG, **fields)
    return Handler(name, func, inspect.iscoroutinefunction(func), params, model)


def bind_arguments(
    handler: Handler,
    args: Any,
    raw: Raw | None,
    channels: dict[str, Channel[Any]],
    ctx: Command,
    app: Any,
) -> dict[str, Any]:
    """为一次调用构造处理器的关键字实参。

    JSON 参数对象经 `handler.model` 一次性校验，camelCase 别名与 snake_case 形参名均可作键，
    未知键忽略。未提供且有默认值的形参不出现在结果中，交由 Python 使用默认值。

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
        TauriError: 错误码均为 `invalid_args`：参数不是 JSON 对象、缺少通道、期望二进制载荷但未提供，
            或 pydantic 校验失败（缺少必填参数、类型不符等；全部字段错误汇总于 `data`）。
    """
    if args is None:
        args = {}
    if not isinstance(args, dict):
        raise TauriError("invalid_args", "arguments must be a JSON object")
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
        elif p.name not in channels:
            raise TauriError("invalid_args", f"missing channel '{p.name}'")
        else:
            kwargs[p.name] = channels[p.name]
    try:
        values = handler.model.model_validate(args)
    except ValidationError as e:
        errors = json.loads(e.json(include_url=False))
        fields = ", ".join(".".join(map(str, err["loc"])) for err in errors)
        raise TauriError("invalid_args", f"invalid arguments: {fields}", errors) from None
    return kwargs | {f: getattr(values, f) for f in values.model_fields_set}
