"""`tauri-runtime build` 命令的测试：配置读取、Nuitka 命令行与产物整理（不真实编译）。"""

import subprocess
import sys
from pathlib import Path

import pytest

from tauri_runtime import cli
from tauri_runtime.cli import bundle


def write_project(tmp_path: Path, tool: str = "") -> Path:
    (tmp_path / "pyproject.toml").write_text(f'[project]\nname = "demo"\nversion = "0"\n{tool}')
    (tmp_path / "main.py").write_text("print('hi')\n")
    return tmp_path


def test_defaults_without_tool_section(tmp_path):
    assert bundle.load_config(write_project(tmp_path)) == bundle.BuildConfig()


def test_reads_tool_section(tmp_path):
    project = write_project(
        tmp_path,
        '[tool.tauri-runtime.build]\nentry = "app.py"\nname = "svc"\noutput = "out"\nnuitka-args = ["--lto=no"]\n',
    )
    assert bundle.load_config(project) == bundle.BuildConfig(
        entry="app.py", name="svc", output="out", nuitka_args=("--lto=no",)
    )


def test_rejects_unknown_keys_and_bad_name(tmp_path):
    with pytest.raises(bundle.BuildError, match="nam"):
        bundle.load_config(write_project(tmp_path, '[tool.tauri-runtime.build]\nnam = "x"\n'))
    with pytest.raises(bundle.BuildError, match="name"):
        bundle.load_config(write_project(tmp_path, '[tool.tauri-runtime.build]\nname = "a/b"\n'))


def test_executable_name():
    assert bundle.executable_name("svc", "win32") == "svc.exe"
    assert bundle.executable_name("svc", "darwin") == "svc"


def test_nuitka_command(tmp_path):
    config = bundle.BuildConfig(nuitka_args=("--lto=no",))
    cmd = bundle.nuitka_command(config, tmp_path, ["--jobs=2"])
    assert cmd[:3] == [sys.executable, "-m", "nuitka"]
    assert "--standalone" in cmd
    assert "--remove-output" in cmd
    assert f"--output-dir={tmp_path}" in cmd
    assert "--output-filename=py-backend" in cmd
    assert cmd[-3:] == ["--lto=no", "--jobs=2", "main.py"]


def fake_nuitka(returncode: int = 0):
    """模拟 Nuitka：成功时在 `--output-dir` 下生成 `<entry 主名>.dist/<name>`。"""

    def run(cmd, cwd, check):
        out = Path(next(a for a in cmd if a.startswith("--output-dir=")).split("=", 1)[1])
        name = next(a for a in cmd if a.startswith("--output-filename=")).split("=", 1)[1]
        if returncode == 0:
            dist = out / f"{Path(cmd[-1]).stem}.dist"
            dist.mkdir(parents=True)
            (dist / bundle.executable_name(name)).write_text("bin")
        return subprocess.CompletedProcess(cmd, returncode)

    return run


def test_build_moves_dist_into_output(tmp_path, monkeypatch):
    monkeypatch.setattr(bundle, "nuitka_available", lambda: True)
    project = write_project(tmp_path)
    stale = project / "dist" / "py-backend" / "stale.txt"
    stale.parent.mkdir(parents=True)
    stale.write_text("old")
    exe = bundle.build(project, run=fake_nuitka())
    assert exe == project / "dist" / "py-backend" / bundle.executable_name("py-backend")
    assert exe.read_text() == "bin"
    assert not stale.exists()


def test_build_failure_keeps_no_output(tmp_path, monkeypatch):
    monkeypatch.setattr(bundle, "nuitka_available", lambda: True)
    project = write_project(tmp_path)
    with pytest.raises(bundle.BuildError, match="exit code 3"):
        bundle.build(project, run=fake_nuitka(3))
    assert not (project / "dist" / "py-backend").exists()


def test_missing_nuitka_and_entry(tmp_path, monkeypatch):
    project = write_project(tmp_path)
    monkeypatch.setattr(bundle, "nuitka_available", lambda: False)
    with pytest.raises(bundle.BuildError, match="uv add --dev nuitka"):
        bundle.build(project)
    monkeypatch.setattr(bundle, "nuitka_available", lambda: True)
    (project / "main.py").unlink()
    with pytest.raises(bundle.BuildError, match="main.py"):
        bundle.build(project)


def test_main_reports_errors(tmp_path, capsys):
    assert cli.main(["build", "--project", str(tmp_path)]) == 1
    assert "pyproject.toml" in capsys.readouterr().err
