from __future__ import annotations

from .command import Handler


class Router:
    def __init__(self) -> None:
        self._handlers: dict[str, Handler] = {}

    def add(self, handler: Handler) -> None:
        if handler.name in self._handlers:
            raise ValueError(f"command '{handler.name}' already registered")
        self._handlers[handler.name] = handler

    def get(self, name: str) -> Handler | None:
        return self._handlers.get(name)

    def names(self) -> list[str]:
        return sorted(self._handlers)
