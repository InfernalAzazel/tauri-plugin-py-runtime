from __future__ import annotations

import inspect
import json
import re
import typing
from dataclasses import dataclass, field
from typing import Any, Callable

from .channel import Channel
from .protocol import Raw

try:
    from pydantic import TypeAdapter, ValidationError
except ImportError:  # pydantic is optional
    TypeAdapter = None  # type: ignore[assignment,misc]
    ValidationError = None  # type: ignore[assignment,misc]


class TauriError(Exception):
    def __init__(self, code: str, message: str, data: Any = None) -> None:
        super().__init__(message)
        self.code = code
        self.message = message
        self.data = data


@dataclass
class Command:
    id: int
    cmd: str
    window: str | None = None
    cancelled: bool = False


_ACRONYM = re.compile(r"([A-Z]+)([A-Z][a-z])")
_WORD = re.compile(r"([a-z0-9])([A-Z])")


def to_snake(name: str) -> str:
    return _WORD.sub(r"\1_\2", _ACRONYM.sub(r"\1_\2", name)).lower()


_CONTEXT, _APP, _RAW, _CHANNEL, _ARG = range(5)


@dataclass
class Param:
    name: str
    kind: int
    default: Any
    adapter: Any = None


@dataclass
class Handler:
    name: str
    func: Callable[..., Any]
    is_async: bool
    params: list[Param] = field(default_factory=list)


def _classify(annotation: Any, app_type: type) -> int:
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
    if TypeAdapter is None or annotation is inspect.Parameter.empty:
        return None
    try:
        return TypeAdapter(annotation)
    except Exception:
        return None


def build_handler(name: str, func: Callable[..., Any], app_type: type) -> Handler:
    hints = typing.get_type_hints(func)
    params = []
    for p in inspect.signature(func).parameters.values():
        annotation = hints.get(p.name, inspect.Parameter.empty)
        kind = _classify(annotation, app_type)
        params.append(Param(p.name, kind, p.default, _adapter(annotation) if kind == _ARG else None))
    return Handler(name, func, inspect.iscoroutinefunction(func), params)


def _validate(param: Param, value: Any) -> Any:
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
