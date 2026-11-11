"""`tauri-runtime init` / `create`：为 Tauri 项目接入 Python 后端。

文件修改均为纯函数（输入文本 → 输出文本），可重复执行；外部命令经可替换的 `run` 执行。
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path
from typing import Literal

from ..protocol import SDK_VERSION
from .templates import BUNDLE_NAME, REPO_URL, rust_snippet

CRATE = "tauri-plugin-py-runtime"
NPM_PACKAGE = "tauri-plugin-py-runtime-api"
GITHUB_SPEC = "github:InfernalAzazel/tauri-plugin-py-runtime"
BUILD_STEP = "cd src-python && uv run tauri-runtime build"
RESOURCE_SRC = f"../src-python/dist/{BUNDLE_NAME}/"
RESOURCE_DST = f"{BUNDLE_NAME}/"
PERMISSION = "py-runtime:default"
BUILDER_CALL = "tauri::Builder::default()"
MANAGERS = ("pnpm", "yarn", "bun", "npm")
LOCKFILES = (
    ("pnpm-lock.yaml", "pnpm"),
    ("yarn.lock", "yarn"),
    ("bun.lock", "bun"),
    ("bun.lockb", "bun"),
    ("package-lock.json", "npm"),
)


class ScaffoldError(Exception):
    """脚手架步骤失败（项目结构不符或外部命令出错）。"""


@dataclass(frozen=True)
class Source:
    """插件三个包的来源：发布版本、GitHub 仓库或本地仓库目录（`path`）。"""

    kind: Literal["registry", "git", "path"] = "registry"
    path: Path | None = None


def dump_json(data: object) -> str:
    return json.dumps(data, indent=2, ensure_ascii=False) + "\n"


def patch_tauri_conf(text: str) -> tuple[str, list[str]]:
    """在 `beforeBuildCommand` 后追加 Nuitka 打包，并把产物加入 `bundle.resources`。

    返回新文本与无法自动处理、需用户手动完成的提示。
    """
    conf = json.loads(text)
    hints: list[str] = []

    build = conf.setdefault("build", {})
    command = build.get("beforeBuildCommand")
    if not command:
        build["beforeBuildCommand"] = BUILD_STEP
    elif isinstance(command, str):
        if BUILD_STEP not in command:
            build["beforeBuildCommand"] = f"{command} && {BUILD_STEP}"
    else:
        hints.append(f'append "{BUILD_STEP}" to build.beforeBuildCommand')

    bundle = conf.setdefault("bundle", {})
    resources = bundle.get("resources")
    if resources is None:
        bundle["resources"] = {RESOURCE_SRC: RESOURCE_DST}
    elif isinstance(resources, dict):
        resources[RESOURCE_SRC] = RESOURCE_DST
    else:
        hints.append(f'add "{RESOURCE_SRC}" -> "{RESOURCE_DST}" to bundle.resources (map form)')

    return dump_json(conf), hints


def patch_capability(text: str) -> str:
    """为 capability 文件的 `permissions` 加上插件默认权限。"""
    conf = json.loads(text)
    permissions = conf.setdefault("permissions", [])
    if PERMISSION not in permissions:
        permissions.append(PERMISSION)
    return dump_json(conf)


def patch_lib_rs(text: str) -> str | None:
    """在唯一的 `tauri::Builder::default()` 行后插入插件注册。

    已注册时原样返回；无法识别（没有或有多处该调用、调用不在行尾）时返回 None。
    """
    if "tauri_plugin_py_runtime" in text:
        return text
    if text.count(BUILDER_CALL) != 1:
        return None
    lines = text.splitlines(keepends=True)
    for index, line in enumerate(lines):
        if BUILDER_CALL in line:
            if line.strip() != BUILDER_CALL:
                return None
            indent = line[: len(line) - len(line.lstrip())]
            lines.insert(index + 1, rust_snippet(indent + "    "))
            return "".join(lines)
    return None


def detect_package_manager(root: Path) -> str:
    """依据锁文件判断前端包管理器；没有锁文件时看 `beforeDevCommand` 的首个单词，默认 npm。"""
    for lockfile, manager in LOCKFILES:
        if (root / lockfile).exists():
            return manager
    conf = root / "src-tauri" / "tauri.conf.json"
    if conf.is_file():
        command = json.loads(conf.read_text(encoding="utf-8")).get("build", {}).get("beforeDevCommand")
        if isinstance(command, str) and command.split() and command.split()[0] in MANAGERS:
            return command.split()[0]
    return "npm"


def cargo_add_command(source: Source) -> list[str]:
    """在项目根目录执行的 `cargo add` 命令。"""
    if source.kind == "git":
        spec = [CRATE, "--git", REPO_URL]
    elif source.kind == "path":
        assert source.path is not None
        spec = [CRATE, "--path", str(source.path)]
    else:
        spec = [f"{CRATE}@{SDK_VERSION}"]
    return ["cargo", "add", *spec, "--manifest-path", str(Path("src-tauri") / "Cargo.toml")]


def frontend_add_command(manager: str, source: Source) -> list[str]:
    """安装前端 API 包的命令。"""
    if source.kind == "git":
        spec = GITHUB_SPEC
    elif source.kind == "path":
        assert source.path is not None
        spec = f"{'link' if manager == 'pnpm' else 'file'}:{source.path}"
    else:
        spec = f"{NPM_PACKAGE}@{SDK_VERSION}"
    return [manager, "install" if manager == "npm" else "add", spec]


def create_command(name: str, template: str, manager: str) -> list[str]:
    """调用 create-tauri-app 创建项目的命令。"""
    options = [name, "--template", template, "--manager", manager, "--yes"]
    if manager == "npm":
        return ["npm", "create", "tauri-app@latest", "--", *options]
    return [manager, "create", "tauri-app", *options]
