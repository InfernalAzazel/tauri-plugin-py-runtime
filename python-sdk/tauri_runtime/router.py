"""命令路由表：按命令名注册与查找 `Handler`。"""

from __future__ import annotations

from .command import Handler


class Router:
    """命令名到 `Handler` 的映射；命令名不可重复注册。"""

    def __init__(self) -> None:
        self._handlers: dict[str, Handler] = {}

    def add(self, handler: Handler) -> None:
        """注册处理器；命令名已存在时抛出 `ValueError`。"""
        if handler.name in self._handlers:
            raise ValueError(f"command '{handler.name}' already registered")
        self._handlers[handler.name] = handler

    def get(self, name: str) -> Handler | None:
        return self._handlers.get(name)

    def names(self) -> list[str]:
        """已注册的命令名（排序后），用于 `Hello` 握手。"""
        return sorted(self._handlers)
