"""`tauri-runtime` 命令行入口。"""

from __future__ import annotations

import argparse
import sys
from collections.abc import Sequence
from pathlib import Path

from .bundle import BuildError, build


def main(argv: Sequence[str] | None = None) -> int:
    """命令行入口：`tauri-runtime build [--project DIR] [-- NUITKA_ARGS...]`。"""
    args = list(sys.argv[1:] if argv is None else argv)
    extra: list[str] = []
    if "--" in args:
        index = args.index("--")
        args, extra = args[:index], args[index + 1 :]
    parser = argparse.ArgumentParser(prog="tauri-runtime")
    commands = parser.add_subparsers(dest="command", required=True)
    build_parser = commands.add_parser("build", help="package the Python backend with Nuitka (standalone)")
    build_parser.add_argument("--project", type=Path, default=Path.cwd(), help="Python project directory")
    options = parser.parse_args(args)
    try:
        exe = build(options.project.resolve(), extra)
    except BuildError as e:
        print(f"tauri-runtime: {e}", file=sys.stderr)
        return 1
    print(f"tauri-runtime: built {exe}")
    return 0
