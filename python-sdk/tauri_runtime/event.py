"""事件：前端发来的 `Event` 对象，以及按事件名分发监听器的 `EventBus`。"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Callable


@dataclass
class Event:
    """前端通过 `pyEmit` 发送给 Python 的事件，作为唯一参数传给 `@app.listen` 监听器。

    Attributes:
        name: 事件名。
        payload: 事件载荷；JSON 值，或二进制载荷对应的 `Raw`。
        window: 发出事件的 webview 标签。
    """

    name: str
    payload: Any = None
    window: str | None = None


class EventBus:
    """事件名到监听器列表的注册表。"""

    def __init__(self) -> None:
        self._listeners: dict[str, list[Callable[..., Any]]] = {}

    def add(self, name: str, func: Callable[..., Any]) -> None:
        self._listeners.setdefault(name, []).append(func)

    def get(self, name: str) -> list[Callable[..., Any]]:
        """返回监听器列表的副本，便于在分发期间安全地新增监听器。"""
        return list(self._listeners.get(name, ()))
