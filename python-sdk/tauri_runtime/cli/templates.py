"""`tauri-runtime init` 生成的文件模板与打印给用户的代码片段。"""

from __future__ import annotations

import json
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from .scaffold import Source

REPO_URL = "https://github.com/InfernalAzazel/tauri-plugin-py-runtime"
BUNDLE_NAME = "py-backend"

MAIN_PY = '''\
from tauri_runtime import App

app = App()


@app.command
async def greet(name: str) -> str:
    return f"Hello {name}, from Python!"


app.run()
'''

GITIGNORE = """\
.venv/
dist/
__pycache__/
"""

FRONTEND_SNIPPET = """\
import { pyInvoke } from "tauri-plugin-py-runtime-api";

const message = await pyInvoke<string>("greet", { name: "Tauri" });
"""


def rust_snippet(indent: str = "") -> str:
    """注册插件的链式调用片段，每行加 `indent` 前缀。"""
    lines = [
        ".plugin(",
        "    tauri_plugin_py_runtime::Builder::new()",
        '        .script("../src-python/main.py")',
        f'        .bundle("{BUNDLE_NAME}")',
        "        .build(),",
        ")",
    ]
    return "".join(f"{indent}{line}\n" for line in lines)


def uv_source(source: Source) -> str | None:
    """`[tool.uv.sources]` 中 `tauri-runtime` 的取值；registry 来源无需此项。"""
    if source.kind == "git":
        return f'{{ git = "{REPO_URL}", subdirectory = "python-sdk" }}'
    if source.kind == "path":
        assert source.path is not None
        return f"{{ path = {json.dumps((source.path / 'python-sdk').as_posix())}, editable = true }}"
    return None


def pyproject_toml(source: Source) -> str:
    """`src-python/pyproject.toml`：依赖 SDK、dev 组含 Nuitka，打包名与 Rust `bundle` 对应。"""
    text = """\
[project]
name = "backend"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = ["tauri-runtime"]

[dependency-groups]
dev = ["nuitka"]

[tool.uv]
package = false
"""
    value = uv_source(source)
    if value is not None:
        text += f"\n[tool.uv.sources]\ntauri-runtime = {value}\n"
    return text + f'\n[tool.tauri-runtime.build]\nname = "{BUNDLE_NAME}"\n'
