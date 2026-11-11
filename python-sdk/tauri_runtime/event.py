from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Callable


@dataclass
class Event:
    name: str
    payload: Any = None
    window: str | None = None


class EventBus:
    def __init__(self) -> None:
        self._listeners: dict[str, list[Callable[..., Any]]] = {}

    def add(self, name: str, func: Callable[..., Any]) -> None:
        self._listeners.setdefault(name, []).append(func)

    def get(self, name: str) -> list[Callable[..., Any]]:
        return list(self._listeners.get(name, ()))
