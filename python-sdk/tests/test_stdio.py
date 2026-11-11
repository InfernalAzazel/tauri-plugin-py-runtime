import io
import os
import subprocess

import pytest
import sys
import textwrap
import time
from pathlib import Path

from tauri_runtime.protocol import Frame, Kind, encode, read_frame

SDK_ROOT = Path(__file__).resolve().parents[1]


def test_print_and_fd_writes_go_to_stderr(tmp_path):
    script = tmp_path / "app.py"
    script.write_text(
        textwrap.dedent(
            """
            import os
            from tauri_runtime import App

            app = App()
            print("hello from print")
            os.write(1, b"raw fd write\\n")

            @app.command
            async def ping() -> str:
                return "pong"

            app.run()
            """
        )
    )
    env = {**os.environ, "PYTHONPATH": str(SDK_ROOT)}
    proc = subprocess.run([sys.executable, str(script)], input=b"", capture_output=True, env=env, timeout=10)
    assert proc.returncode == 0, proc.stderr.decode()
    stdout = io.BytesIO(proc.stdout)
    assert read_frame(stdout).kind == Kind.HELLO
    assert read_frame(stdout) is None
    assert b"hello from print" in proc.stderr
    assert b"raw fd write" in proc.stderr


def test_shutdown_exits_despite_slow_sync_handler(tmp_path):
    script = tmp_path / "slow_shutdown.py"
    script.write_text(
        textwrap.dedent(
            """
            import time
            from tauri_runtime import App

            app = App()

            @app.command
            def slow() -> str:
                time.sleep(5)
                return "done"

            app.run()
            """
        )
    )
    env = {**os.environ, "PYTHONPATH": str(SDK_ROOT)}
    proc = subprocess.Popen(
        [sys.executable, str(script)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
    )
    hello = read_frame(proc.stdout)
    assert hello.kind == Kind.HELLO
    proc.stdin.write(
        encode(
            Frame(
                Kind.REQUEST,
                {"id": 1, "cmd": "slow", "encoding": "none", "window": "main"},
                b"",
            )
        )
    )
    proc.stdin.flush()
    time.sleep(0.3)
    proc.stdin.write(encode(Frame(Kind.SHUTDOWN, {"timeoutMs": 1000}, b"")))
    proc.stdin.flush()
    proc.stdin.close()
    try:
        proc.wait(timeout=2)
    except subprocess.TimeoutExpired:
        proc.kill()
        pytest.fail("process did not exit within 2 seconds")
    assert proc.returncode == 0
