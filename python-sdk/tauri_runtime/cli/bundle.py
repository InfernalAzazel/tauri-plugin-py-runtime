"""`tauri-runtime build`：用 Nuitka 将 Python 后端打包为 standalone 目录。

配置读取项目 `pyproject.toml` 的 `[tool.tauri-runtime.build]`，产物布局固定为
`<output>/<name>/<name>`（Windows 为 `<name>.exe`），与 Rust 插件的 `Builder::bundle(name)` 对应。
"""

from __future__ import annotations

import importlib.util
import re
import shutil
import subprocess
import sys
import tempfile
import tomllib
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any

NAME_PATTERN = re.compile(r"^[A-Za-z0-9._-]+$")
KEYS = frozenset({"entry", "name", "output", "nuitka-args"})


class BuildError(Exception):
    """打包失败（配置错误、缺少 Nuitka 或编译失败），消息直接展示给用户。"""


@dataclass(frozen=True)
class BuildConfig:
    """`[tool.tauri-runtime.build]` 配置。

    Attributes:
        entry: 入口脚本，相对项目目录。
        name: 可执行文件名与产物目录名，只能包含字母、数字、`.`、`_`、`-`。
        output: 输出目录，相对项目目录。
        nuitka_args: 透传给 Nuitka 的额外参数。
    """

    entry: str = "main.py"
    name: str = "py-backend"
    output: str = "dist"
    nuitka_args: tuple[str, ...] = ()


def load_config(project: Path) -> BuildConfig:
    """读取项目的打包配置；缺少配置段时使用默认值，未知键或非法值抛出 `BuildError`。"""
    pyproject = project / "pyproject.toml"
    if not pyproject.is_file():
        raise BuildError(f"{pyproject} not found; run inside your Python project or pass --project")
    with pyproject.open("rb") as f:
        data: dict[str, Any] = tomllib.load(f)
    table: dict[str, Any] = data.get("tool", {}).get("tauri-runtime", {}).get("build", {})
    unknown = set(table) - KEYS
    if unknown:
        raise BuildError(f"unknown keys in [tool.tauri-runtime.build]: {', '.join(sorted(unknown))}")
    config = BuildConfig(
        entry=str(table.get("entry", BuildConfig.entry)),
        name=str(table.get("name", BuildConfig.name)),
        output=str(table.get("output", BuildConfig.output)),
        nuitka_args=tuple(str(a) for a in table.get("nuitka-args", ())),
    )
    if not NAME_PATTERN.match(config.name):
        raise BuildError(f"invalid name '{config.name}': use letters, digits, '.', '_' or '-'")
    return config


def executable_name(name: str, platform: str = sys.platform) -> str:
    """产物中可执行文件的文件名：Windows 追加 `.exe`。"""
    return f"{name}.exe" if platform == "win32" else name


def nuitka_available() -> bool:
    """当前环境能否导入 Nuitka。"""
    return importlib.util.find_spec("nuitka") is not None


def nuitka_command(config: BuildConfig, build_dir: Path, extra: Sequence[str]) -> list[str]:
    """构造 Nuitka standalone 编译命令；使用当前解释器，因此项目依赖均可被收集。"""
    return [
        sys.executable,
        "-m",
        "nuitka",
        "--standalone",
        "--assume-yes-for-downloads",
        "--remove-output",
        f"--output-dir={build_dir}",
        f"--output-filename={config.name}",
        *config.nuitka_args,
        *extra,
        config.entry,
    ]


def build(
    project: Path,
    extra: Sequence[str] = (),
    run: Callable[..., subprocess.CompletedProcess[Any]] = subprocess.run,
) -> Path:
    """编译项目并将产物整理到 `<output>/<name>`，返回可执行文件路径。

    在临时目录中编译，成功后才替换旧产物，失败时不会留下半成品。

    Args:
        project: Python 项目目录（包含 `pyproject.toml`）。
        extra: 追加给 Nuitka 的命令行参数。
        run: 执行子进程的函数，测试时可替换。

    Raises:
        BuildError: 配置错误、缺少 Nuitka 或入口脚本，或 Nuitka 以非零退出码结束。
    """
    config = load_config(project)
    if not nuitka_available():
        raise BuildError("Nuitka is not installed in this environment; run `uv add --dev nuitka`")
    if not (project / config.entry).is_file():
        raise BuildError(f"entry script {project / config.entry} not found")
    target = project / config.output / config.name
    with tempfile.TemporaryDirectory(prefix="tauri-runtime-build-") as tmp:
        build_dir = Path(tmp)
        result = run(nuitka_command(config, build_dir, extra), cwd=project, check=False)
        if result.returncode != 0:
            raise BuildError(f"nuitka failed with exit code {result.returncode}")
        if target.exists():
            shutil.rmtree(target)
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.move(str(build_dir / f"{Path(config.entry).stem}.dist"), str(target))
    return target / executable_name(config.name)
