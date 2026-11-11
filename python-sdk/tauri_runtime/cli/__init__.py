"""`tauri-runtime` 命令行入口。"""

from __future__ import annotations

import argparse
import sys
from collections.abc import Sequence
from pathlib import Path

from .bundle import BuildError, build
from .scaffold import MANAGERS, ScaffoldError, Source, create, init
from .templates import FRONTEND_SNIPPET


def add_source_options(parser: argparse.ArgumentParser) -> None:
    group = parser.add_mutually_exclusive_group()
    group.add_argument("--git", action="store_true", help="install the plugin from GitHub instead of the registries")
    group.add_argument("--path", type=Path, metavar="REPO", help="install the plugin from a local checkout")


def source_of(options: argparse.Namespace) -> Source:
    if options.path is not None:
        return Source("path", options.path.resolve())
    return Source("git") if options.git else Source()


def report(project: Path, hints: list[str]) -> None:
    """打印手动步骤与前端调用示例。"""
    for hint in hints:
        print(f"tauri-runtime: manual step: {hint}", file=sys.stderr)
    print(f"tauri-runtime: Python backend ready in {project / 'src-python'}")
    print(f"call it from the frontend:\n\n{FRONTEND_SNIPPET}")


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(prog="tauri-runtime")
    commands = root.add_subparsers(dest="command", required=True)

    build_parser = commands.add_parser("build", help="package the Python backend with Nuitka (standalone)")
    build_parser.add_argument("--project", type=Path, default=Path.cwd(), help="Python project directory")

    init_parser = commands.add_parser("init", help="add a Python backend (src-python) to an existing Tauri project")
    init_parser.add_argument("--project", type=Path, default=Path.cwd(), help="Tauri project root")
    init_parser.add_argument("--force", action="store_true", help="continue when src-python already exists")
    add_source_options(init_parser)

    create_parser = commands.add_parser("create", help="create a Tauri project with a Python backend")
    create_parser.add_argument("name", help="project directory name")
    create_parser.add_argument("--template", default="vue-ts", help="create-tauri-app template")
    create_parser.add_argument("--manager", default="pnpm", choices=MANAGERS, help="frontend package manager")
    add_source_options(create_parser)
    return root


def main(argv: Sequence[str] | None = None) -> int:
    """命令行入口：`tauri-runtime {build,init,create} ...`；`build` 的 `--` 之后的参数传给 Nuitka。"""
    args = list(sys.argv[1:] if argv is None else argv)
    extra: list[str] = []
    if "--" in args:
        index = args.index("--")
        args, extra = args[:index], args[index + 1 :]
    options = parser().parse_args(args)
    try:
        if options.command == "build":
            print(f"tauri-runtime: built {build(options.project.resolve(), extra)}")
        elif options.command == "init":
            project = options.project.resolve()
            report(project, init(project, source_of(options), force=options.force))
        else:
            project, hints = create(options.name, options.template, options.manager, source_of(options))
            report(project, hints)
    except (BuildError, ScaffoldError) as e:
        print(f"tauri-runtime: {e}", file=sys.stderr)
        return 1
    return 0
