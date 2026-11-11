"""`tauri-runtime init` / `create`：为 Tauri 项目接入 Python 后端。

文件修改均为纯函数（输入文本 → 输出文本），可重复执行；外部命令经可替换的 `run` 执行。
"""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Literal

from ..protocol import SDK_VERSION
from .templates import (
    BUNDLE_DIR,
    BUNDLE_NAME,
    GITIGNORE,
    MAIN_PY,
    REPO_URL,
    build_rs_snippet,
    pyproject_toml,
    rust_snippet,
)

Runner = Callable[..., Any]

CRATE = "tauri-plugin-py-runtime"
NPM_PACKAGE = "tauri-plugin-py-runtime-api"
GITHUB_SPEC = "github:InfernalAzazel/tauri-plugin-py-runtime"
BUILD_STEP = "cd src-python && uv run tauri-runtime build"
RESOURCE_SRC = f"{BUNDLE_DIR}/"
RESOURCE_DST = f"{BUNDLE_NAME}/"
PERMISSION = "py-runtime:default"
BUILDER_CALL = "tauri::Builder::default()"
BUILD_CALL = "tauri_build::build()"
MANAGERS = ("pnpm", "yarn", "bun", "npm")
LOCKFILES = (
    ("pnpm-lock.yaml", "pnpm"),
    ("yarn.lock", "yarn"),
    ("bun.lock", "bun"),
    ("bun.lockb", "bun"),
    ("package-lock.json", "npm"),
)
INTERPRETER_SETTING = "python.defaultInterpreterPath"
PYTHON_EXTENSION = "ms-python.python"

# create-tauri-app 模板自带的 Rust `greet` 示例及其前端调用
TAURI_INVOKE_IMPORT = 'import { invoke } from "@tauri-apps/api/core";'
PY_INVOKE_IMPORT = f'import {{ pyInvoke }} from "{NPM_PACKAGE}";'
INVOKE_CALL = re.compile(r"\binvoke(?=(?:<[^>()]*>)?\()")
GREET_CALL = re.compile(r'\binvoke(?:<[^>()]*>)?\("greet"')
TEMPLATE_COMMENT = re.compile(r"[ \t]*// Learn more about Tauri commands at https://tauri\.app/develop/calling-rust/\n")
RUST_GREET = re.compile(r"#\[tauri::command\]\nfn greet\(name: &str\) -> String \{\n[^\n]*\n\}\n\n?")
RUST_INVOKE_HANDLER = re.compile(r"[ \t]*\.invoke_handler\(tauri::generate_handler!\[greet\]\)\n")
FRONTEND_SUFFIXES = frozenset({".ts", ".tsx", ".js", ".jsx", ".vue", ".svelte"})


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


def patch_build_rs(text: str) -> str | None:
    """在唯一的 `tauri_build::build()` 前创建 Nuitka 产物目录。

    tauri-build 在 dev / check 时也要求 `bundle.resources` 路径存在，而产物只在打包时生成。
    已处理时原样返回；无法识别时返回 None。
    """
    if BUNDLE_DIR in text:
        return text
    if text.count(BUILD_CALL) != 1:
        return None
    lines = text.splitlines(keepends=True)
    for index, line in enumerate(lines):
        if BUILD_CALL in line:
            if line.strip() != BUILD_CALL:
                return None
            lines.insert(index, build_rs_snippet(line[: len(line) - len(line.lstrip())]))
            return "".join(lines)
    return None


def patch_frontend_greet(text: str) -> str | None:
    """把模板前端对 Rust `greet` 的调用改为调用 Python 的 `greet`。

    不调用 `greet` 时原样返回；无法安全改写（未从 `@tauri-apps/api/core` 导入 `invoke`，
    或还调用了其他命令）时返回 None。
    """
    greet_calls = len(GREET_CALL.findall(text))
    if greet_calls == 0:
        return text
    if TAURI_INVOKE_IMPORT not in text or len(INVOKE_CALL.findall(text)) != greet_calls:
        return None
    text = INVOKE_CALL.sub("pyInvoke", text.replace(TAURI_INVOKE_IMPORT, PY_INVOKE_IMPORT))
    return TEMPLATE_COMMENT.sub("", text)


def patch_rust_greet(text: str) -> str | None:
    """删除模板 `lib.rs` 中的 Rust `greet` 命令及其注册；找不到模板原样代码时返回 None。"""
    if not (RUST_GREET.search(text) and RUST_INVOKE_HANDLER.search(text)):
        return None
    text = RUST_INVOKE_HANDLER.sub("", RUST_GREET.sub("", text, count=1), count=1)
    return TEMPLATE_COMMENT.sub("", text)


def venv_interpreter(windows: bool = os.name == "nt") -> str:
    """VS Code 中 `src-python` 虚拟环境解释器的路径。"""
    relative = ".venv/Scripts/python.exe" if windows else ".venv/bin/python"
    return f"${{workspaceFolder}}/src-python/{relative}"


def load_json_object(text: str) -> dict[str, Any] | None:
    """解析 JSON 对象，空文本视为 `{}`；带注释等无法解析或不是对象时返回 None。"""
    if not text.strip():
        return {}
    try:
        data = json.loads(text)
    except json.JSONDecodeError:
        return None
    return data if isinstance(data, dict) else None


def patch_vscode_settings(text: str, interpreter: str) -> str | None:
    """为 `.vscode/settings.json` 设置 Python 解释器；已设置时原样返回，无法解析时返回 None。"""
    settings = load_json_object(text)
    if settings is None:
        return None
    if INTERPRETER_SETTING in settings:
        return text
    settings[INTERPRETER_SETTING] = interpreter
    return dump_json(settings)


def patch_vscode_extensions(text: str) -> str | None:
    """在 `.vscode/extensions.json` 的推荐中加入 Python 扩展；无法解析时返回 None。"""
    conf = load_json_object(text)
    if conf is None:
        return None
    recommendations = conf.setdefault("recommendations", [])
    if not isinstance(recommendations, list):
        return None
    if PYTHON_EXTENSION in recommendations:
        return text
    recommendations.append(PYTHON_EXTENSION)
    return dump_json(conf)


def setup_vscode(project: Path) -> list[str]:
    """让 VS Code / Cursor 使用 `src-python/.venv`，返回无法自动处理时的提示。"""
    vscode = project / ".vscode"
    interpreter = venv_interpreter()
    patches: list[tuple[str, Callable[[str], str | None], str]] = [
        ("settings.json", lambda text: patch_vscode_settings(text, interpreter), f'set "{INTERPRETER_SETTING}" to "{interpreter}"'),
        ("extensions.json", patch_vscode_extensions, f'add "{PYTHON_EXTENSION}" to recommendations'),
    ]
    hints: list[str] = []
    for name, patch, hint in patches:
        path = vscode / name
        text = path.read_text(encoding="utf-8") if path.is_file() else None
        patched = patch(text or "")
        if patched is None:
            hints.append(f".vscode/{name}: {hint}")
        elif patched != text:
            vscode.mkdir(exist_ok=True)
            path.write_text(patched, encoding="utf-8")
    return hints


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
    if manager == "pnpm" and source.kind == "git":
        # pnpm 10+ 默认禁止 git 依赖执行构建脚本（`prepare` 编译 dist-js），需显式放行。
        return ["pnpm", "add", f"--allow-build={NPM_PACKAGE}", spec]
    return [manager, "install" if manager == "npm" else "add", spec]


def create_command(name: str, template: str, manager: str) -> list[str]:
    """调用 create-tauri-app 创建项目的命令。"""
    options = [name, "--template", template, "--manager", manager, "--yes"]
    if manager == "npm":
        return ["npm", "create", "tauri-app@latest", "--", *options]
    return [manager, "create", "tauri-app", *options]


def check_source(source: Source) -> None:
    """本地来源须指向本仓库根目录（含 `python-sdk` 与 `Cargo.toml`）。"""
    if source.kind != "path":
        return
    assert source.path is not None
    if not ((source.path / "python-sdk").is_dir() and (source.path / "Cargo.toml").is_file()):
        raise ScaffoldError(f"{source.path} is not a tauri-plugin-py-runtime checkout")


def run_step(run: Runner, cmd: Sequence[str], cwd: Path) -> None:
    """执行外部命令，失败时抛出 ScaffoldError；经 `shutil.which` 解析以支持 Windows 的 `.cmd`。"""
    print(f"tauri-runtime: $ {' '.join(cmd)}", flush=True)
    program = shutil.which(cmd[0])
    if program is None:
        raise ScaffoldError(f"`{cmd[0]}` not found on PATH")
    result = run([program, *cmd[1:]], cwd=cwd, check=False)
    if result.returncode != 0:
        raise ScaffoldError(f"`{' '.join(cmd)}` failed with exit code {result.returncode}")


def write_backend(directory: Path, source: Source) -> None:
    """生成 `src-python`；已有文件保持不变。"""
    directory.mkdir(parents=True, exist_ok=True)
    files = {"pyproject.toml": pyproject_toml(source), "main.py": MAIN_PY, ".gitignore": GITIGNORE}
    for name, content in files.items():
        path = directory / name
        if not path.exists():
            path.write_text(content, encoding="utf-8")


def patch_file(path: Path, patch: Callable[[str], str]) -> None:
    text = path.read_text(encoding="utf-8")
    patched = patch(text)
    if patched != text:
        path.write_text(patched, encoding="utf-8")


def init(
    project: Path,
    source: Source | None = None,
    force: bool = False,
    manager: str | None = None,
    run: Runner = subprocess.run,
) -> list[str]:
    """为已有 Tauri 项目接入 Python 后端，返回需用户手动完成的提示。"""
    source = source or Source()
    check_source(source)
    tauri_dir = project / "src-tauri"
    if not tauri_dir.is_dir():
        raise ScaffoldError(f"{tauri_dir} not found; run inside a Tauri project or pass --project")
    backend = project / "src-python"
    if backend.exists() and not force:
        raise ScaffoldError(f"{backend} already exists; pass --force to keep it and finish the setup")
    hints: list[str] = []

    write_backend(backend, source)
    run_step(run, ["uv", "sync"], backend)

    conf = tauri_dir / "tauri.conf.json"
    if conf.is_file():
        text, conf_hints = patch_tauri_conf(conf.read_text(encoding="utf-8"))
        conf.write_text(text, encoding="utf-8")
        hints += [f"tauri.conf.json: {hint}" for hint in conf_hints]
    else:
        hints.append(
            f'Tauri config: append "{BUILD_STEP}" to build.beforeBuildCommand and map '
            f'"{RESOURCE_SRC}" to "{RESOURCE_DST}" in bundle.resources'
        )

    capability = tauri_dir / "capabilities" / "default.json"
    if capability.is_file():
        patch_file(capability, patch_capability)
    else:
        hints.append(f'add "{PERMISSION}" to the permissions of your capability file')

    run_step(run, cargo_add_command(source), project)

    if (project / "package.json").is_file():
        run_step(run, frontend_add_command(manager or detect_package_manager(project), source), project)
        if source.kind == "path" and source.path is not None and not (source.path / "dist-js").is_dir():
            hints.append(f"build the frontend package first: run `pnpm install && pnpm build` in {source.path}")
    else:
        hints.append(f"install the frontend package {NPM_PACKAGE}")

    lib_rs = tauri_dir / "src" / "lib.rs"
    patched = patch_lib_rs(lib_rs.read_text(encoding="utf-8")) if lib_rs.is_file() else None
    if patched is None:
        hints.append(f"register the plugin on your tauri::Builder:\n{rust_snippet('    ')}")
    else:
        patch_file(lib_rs, lambda _: patched)

    build_rs = tauri_dir / "build.rs"
    patched = patch_build_rs(build_rs.read_text(encoding="utf-8")) if build_rs.is_file() else None
    if patched is None:
        hints.append(f"create the Python bundle directory before tauri_build::build() in build.rs:\n{build_rs_snippet('    ')}")
    else:
        patch_file(build_rs, lambda _: patched)
    return hints + setup_vscode(project)


def create(
    name: str,
    template: str = "vue-ts",
    manager: str = "pnpm",
    source: Source | None = None,
    cwd: Path | None = None,
    run: Runner = subprocess.run,
) -> tuple[Path, list[str]]:
    """用 create-tauri-app 新建项目后执行 `init`，返回项目目录与提示。"""
    source = source or Source()
    check_source(source)
    cwd = cwd or Path.cwd()
    project = cwd / name
    if project.exists():
        raise ScaffoldError(f"{project} already exists")
    run_step(run, create_command(name, template, manager), cwd)
    if not project.is_dir():
        raise ScaffoldError(f"create-tauri-app did not create {project}")
    hints = init(project, source, manager=manager, run=run)
    return project, hints + replace_template_greet(project)


def replace_template_greet(project: Path) -> list[str]:
    """把新建项目的 `greet` 示例从 Rust 换成 Python；前端无法全部改写时保留 Rust 版本并返回提示。"""
    src = project / "src"
    files = [p for p in src.rglob("*") if p.suffix in FRONTEND_SUFFIXES and p.is_file()] if src.is_dir() else []
    changes: dict[Path, str] = {}
    for path in files:
        text = path.read_text(encoding="utf-8")
        patched = patch_frontend_greet(text)
        if patched is None:
            relative = path.relative_to(project).as_posix()
            return [f'{relative} calls the template\'s Rust "greet" command; call the Python one with pyInvoke("greet", {{ name }})']
        if patched != text:
            changes[path] = patched
    if not changes:
        return []
    lib_rs = project / "src-tauri" / "src" / "lib.rs"
    rust = patch_rust_greet(lib_rs.read_text(encoding="utf-8")) if lib_rs.is_file() else None
    if rust is not None:
        changes[lib_rs] = rust
    for path, text in changes.items():
        path.write_text(text, encoding="utf-8")
    return []
