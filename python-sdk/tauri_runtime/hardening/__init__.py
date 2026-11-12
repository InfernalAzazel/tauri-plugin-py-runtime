"""Nuitka 打包加固插件。

对外只暴露 `plugin_path()`：返回 Nuitka `--user-plugin` 要用的插件文件路径。插件本体在 plugin.py，
它 import 了 `nuitka.*`，所以这里不 import 它——`tauri-runtime build` 在没装 Nuitka 的环境里也要能 import
本包（只有真正构建时才需要 Nuitka）。
"""

from __future__ import annotations

from pathlib import Path


def plugin_path() -> str:
    """加固插件文件的绝对路径，传给 Nuitka 的 `--user-plugin`。"""
    return str(Path(__file__).resolve().parent / "plugin.py")
