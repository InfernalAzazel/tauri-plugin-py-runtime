"""纯验证脚本：用加固插件在当前系统上真打一个一次性后端，断言防护确实生效。

供 CI 跨平台（尤其 Windows 的 MSVC）使用：生成一个临时项目，用 Nuitka + 加固插件打包，然后
1）运行产物，确认运行时解密成功（能打出约定 token）；
2）扫描产物字节，确认密语字符串和自有模块名都查不到明文。
需要环境里有 Nuitka 和 C 编译器（CI 提供；本脚本不是 pytest 用例，不会被自动收集）。
"""

from __future__ import annotations

import os
import subprocess
import sys
import tempfile
import textwrap
from pathlib import Path

from tauri_runtime.hardening import plugin_path

SECRET = "SPARROW-PROMPT-6f1c2a-DO-NOT-LEAK"  # 明文禁止出现在产物里
MODULE = "secret_pkg.analysis"  # 自有模块名禁止出现在产物里
TOKEN = "handshake-ok-42"  # 运行时解密成功的证据


def write_fixture(root: Path) -> None:
    """写一个最小项目：一个带密语和 f-string 的自有包 + 打印 token 的入口。"""
    pkg = root / "secret_pkg"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("", encoding="utf-8")
    (pkg / "analysis.py").write_text(
        textwrap.dedent(f'''
            PROMPT = "{SECRET}"

            def greeting(name: str) -> str:
                return f"{{name}} says {SECRET}"  # f-string 的静态段也必须被加密
        '''),
        encoding="utf-8",
    )
    (root / "main.py").write_text(
        textwrap.dedent(f'''
            from secret_pkg.analysis import PROMPT, greeting

            if __name__ == "__main__":
                print(len(PROMPT), len(greeting("x")))
                print("{TOKEN}")
        '''),
        encoding="utf-8",
    )


def build(root: Path) -> Path:
    """用 Nuitka + 加固插件打包，返回可执行文件路径。"""
    out = root / "out"
    subprocess.run(
        [
            sys.executable,
            "-m",
            "nuitka",
            "--standalone",
            "--assume-yes-for-downloads",
            "--remove-output",
            f"--output-dir={out}",
            "--output-filename=app",
            f"--user-plugin={plugin_path()}",
            "--python-flag=no_docstrings",
            "--python-flag=no_asserts",
            "main.py",
        ],
        cwd=root,
        check=True,
    )
    exe = "app.exe" if sys.platform == "win32" else "app"
    return out / "main.dist" / exe


def dist_bytes(exe: Path) -> bytes:
    """产物目录里所有文件的字节拼起来，用来查明文（常量块嵌在可执行文件里）。"""
    return b"".join(p.read_bytes() for p in exe.parent.rglob("*") if p.is_file())


def main() -> int:
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        write_fixture(root)
        # 告诉加固插件哪个目录是项目根（决定哪些模块名要加密）。
        os.environ["TAURI_RUNTIME_PROJECT_ROOT"] = str(root)
        exe = build(root)

        result = subprocess.run([str(exe)], check=False, capture_output=True, text=True, timeout=120)
        if TOKEN not in result.stdout:
            print(f"FAIL: 产物没能正常运行（解密失败？）\nstdout={result.stdout!r}\nstderr={result.stderr!r}")
            return 1

        blob = dist_bytes(exe)
        failures = []
        if SECRET.encode() in blob:
            failures.append("密语字符串以明文出现在产物里")
        if MODULE.encode() in blob:
            failures.append("自有模块名以明文出现在产物里")
        if failures:
            print("FAIL: " + "; ".join(failures))
            return 1

    print(f"加固验证通过：{sys.platform}（运行时解密 OK，密语与模块名均无明文）")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
