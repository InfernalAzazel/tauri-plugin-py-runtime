"""命令注册：对外的分组路由 `Router`，以及运行时内部的命令表 `CommandTable`。"""

from __future__ import annotations

from collections.abc import Callable
from typing import Any

from .command import Handler

LIFECYCLE = ("startup", "ready", "shutdown")


def join_name(prefix: str, name: str) -> str:
    """用 `.` 拼接命令名前缀与命令名；前缀为空时原样返回命令名。"""
    return f"{prefix}.{name}" if prefix else name


def check_hook(hook: str) -> None:
    """校验生命周期钩子名；未知钩子名抛出 `ValueError`。"""
    if hook not in LIFECYCLE:
        raise ValueError(f"unknown lifecycle hook '{hook}', expected one of {LIFECYCLE}")


class Router:
    """命令、事件监听器与生命周期钩子的分组，用于按模块组织大型项目。

    业务模块只需创建并填充自己的 `Router`，无需导入 `App` 实例，由入口文件统一通过
    `App.include()` 挂载，从而避免循环导入。`prefix` 以 `.` 拼接到命令名之前，嵌套
    `include()` 时逐级拼接；事件名与钩子不受前缀影响。

    注册只做收集：处理器签名的校验与命令名重复检查在挂载到 `App` 时进行，挂载时取当时
    已注册内容的快照，之后再向该 `Router` 添加的内容不会生效。

    Example:
        ```python
        # user/commands.py
        from tauri_runtime import Router

        router = Router(prefix="user")

        @router.command
        async def login(name: str) -> str:  # 前端调用名为 "user.login"
            return f"welcome {name}"

        def logout() -> None: ...

        router.add_command(logout)

        # main.py
        from tauri_runtime import App
        from user.commands import router as user_router

        app = App()
        app.include(user_router)
        app.run()
        ```

    Args:
        prefix: 命令名前缀，如 `"user"`；空字符串表示不加前缀。不能以 `.` 开头或结尾。
    """

    def __init__(self, prefix: str = "") -> None:
        if prefix.startswith(".") or prefix.endswith("."):
            raise ValueError(f"invalid router prefix '{prefix}': must not start or end with '.'")
        self.prefix = prefix
        self.commands: list[tuple[str, Callable[..., Any]]] = []
        self.listeners: list[tuple[str, Callable[..., Any]]] = []
        self.hooks: list[tuple[str, Callable[..., Any]]] = []

    # 命令式注册 ---------------------------------------------------------

    def add_command(self, func: Callable[..., Any], name: str | None = None) -> None:
        """注册命令；`name` 省略时使用函数名，最终命令名会加上本 `Router` 的前缀。"""
        self.commands.append((join_name(self.prefix, name or func.__name__), func))

    def add_commands(self, *funcs: Callable[..., Any]) -> None:
        """批量注册命令，命令名均取函数名。"""
        for func in funcs:
            self.add_command(func)

    def add_listener(self, event: str, func: Callable[..., Any]) -> None:
        """注册事件监听器；事件名不加前缀。"""
        self.listeners.append((event, func))

    def add_hook(self, hook: str, func: Callable[..., Any]) -> None:
        """注册生命周期钩子（`"startup"` / `"ready"` / `"shutdown"`）。"""
        check_hook(hook)
        self.hooks.append((hook, func))

    def include(self, router: Router) -> None:
        """把另一个 `Router` 的内容并入本 `Router`，其命令名再加上本 `Router` 的前缀。"""
        self.commands.extend((join_name(self.prefix, name), func) for name, func in router.commands)
        self.listeners.extend(router.listeners)
        self.hooks.extend(router.hooks)

    # 装饰器 -------------------------------------------------------------

    def command(self, name_or_func: Any = None) -> Any:
        """注册命令的装饰器，用法同 `App.command`：`@router.command` 或 `@router.command("name")`。"""
        if callable(name_or_func):
            self.add_command(name_or_func)
            return name_or_func

        def decorator(func: Callable[..., Any]) -> Callable[..., Any]:
            self.add_command(func, name_or_func)
            return func

        return decorator

    def listen(self, event: str) -> Callable[[Callable[..., Any]], Callable[..., Any]]:
        """注册事件监听器的装饰器，用法同 `App.listen`。"""

        def decorator(func: Callable[..., Any]) -> Callable[..., Any]:
            self.add_listener(event, func)
            return func

        return decorator

    def on(self, hook: str) -> Callable[[Callable[..., Any]], Callable[..., Any]]:
        """注册生命周期钩子的装饰器，用法同 `App.on`。"""
        check_hook(hook)

        def decorator(func: Callable[..., Any]) -> Callable[..., Any]:
            self.add_hook(hook, func)
            return func

        return decorator


class CommandTable:
    """运行时内部的命令名到 `Handler` 的映射；命令名不可重复注册。"""

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
