"""纯验证脚本：用加固插件在当前系统上真打一个一次性后端，断言防护确实生效。

供 CI 跨平台（尤其 Windows 的 MSVC）使用：把 tests/fixtures/hardening_target 这个靶子项目拷到
临时目录，用 Nuitka + 加固插件打包，然后
1）运行产物，确认运行时解密成功、且靶子自检全过（常量/f-string 解密、跨模块 import、
   第三方库字节码导入与元数据查询都正常），会打出约定 token；
2）扫描产物字节，确认密语字符串、自有模块名、以及 strip 探针函数名都查不到明文。
需要环境里有 Nuitka 和 C 编译器（CI 提供；本脚本不是 pytest 用例，不会被自动收集）。
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

from tauri_runtime.hardening import plugin_path

# Windows 控制台默认 cp1252，打印中文会 UnicodeEncodeError；统一改用 UTF-8（含失败信息）。
for _stream in (sys.stdout, sys.stderr):
    try:
        _stream.reconfigure(encoding="utf-8")
    except (AttributeError, ValueError):
        pass

# 靶子项目：真实文件，供阅读与单独运行。打包前整份拷到临时目录，保持仓库干净、每次构建隔离。
FIXTURE = Path(__file__).resolve().parent / "fixtures" / "hardening_target"

SECRET = "SPARROW-PROMPT-6f1c2a-DO-NOT-LEAK"  # 明文禁止出现在产物里，见 fixtures/hardening_target/secret_pkg/analysis.py
MODULES = ("secret_pkg.analysis", "secret_pkg.engine", "secret_pkg.vendored")  # 自有模块名禁止出现在产物里
SYMBOL = "strip_marker_9c7d41e2"  # 自有函数名：strip 后不该在产物符号表里留下明文，见 analysis.py
TOKEN = "handshake-ok-42"  # 靶子自检全部通过后才打印，是运行时解密成功的证据


def check_fixture() -> None:
    """靶子与本脚本的约定值防漂移：真改了靶子里的密语/token/探针名，这里会先报出来。"""
    analysis = (FIXTURE / "secret_pkg" / "analysis.py").read_text(encoding="utf-8")
    main = (FIXTURE / "main.py").read_text(encoding="utf-8")
    if SECRET not in analysis:
        raise SystemExit(f"靶子漂移：{FIXTURE}/secret_pkg/analysis.py 里找不到密语 {SECRET!r}")
    if SYMBOL not in analysis:
        raise SystemExit(f"靶子漂移：{FIXTURE}/secret_pkg/analysis.py 里找不到探针函数 {SYMBOL!r}")
    if TOKEN not in main:
        raise SystemExit(f"靶子漂移：{FIXTURE}/main.py 里找不到 token {TOKEN!r}")


def build(root: Path) -> Path:
    """用 Nuitka + 加固插件打包 root 下的靶子，返回可执行文件路径。"""
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
    check_fixture()
    # 用靶子名建临时根目录，把靶子整份拷进去再打包（out/ 等产物都落在临时目录，不污染仓库）。
    scratch = Path(tempfile.mkdtemp(prefix="tauri-runtime-hardening-"))
    try:
        root = scratch / "hardening_target"
        shutil.copytree(FIXTURE, root)
        # 告诉加固插件哪个目录是项目根（决定哪些模块名要加密）。
        os.environ["TAURI_RUNTIME_PROJECT_ROOT"] = str(root)
        exe = build(root)

        result = subprocess.run([str(exe)], check=False, capture_output=True, text=True, timeout=120)
        if TOKEN not in result.stdout:
            print(f"FAIL: 产物没能正常运行（解密失败或自检未过？）\nstdout={result.stdout!r}\nstderr={result.stderr!r}")
            return 1

        blob = dist_bytes(exe)
        failures = []
        if SECRET.encode() in blob:
            failures.append("密语字符串以明文出现在产物里")
        leaked = [m for m in MODULES if m.encode() in blob]
        if leaked:
            failures.append(f"自有模块名以明文出现在产物里：{', '.join(leaked)}")
        if SYMBOL.encode() in blob:
            failures.append(f"自有函数名 {SYMBOL} 未被 strip，仍以明文出现在产物里")
        if failures:
            print("FAIL: " + "; ".join(failures))
            return 1
    finally:
        shutil.rmtree(scratch, ignore_errors=True)

    print(f"加固验证通过：{sys.platform}（运行时解密 OK，靶子自检通过，密语/模块名/函数名均无明文）")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
