import io
import os
import subprocess

import pytest
import sys
import textwrap
import time
from pathlib import Path

from tauri_runtime import protocol as wire
from tauri_runtime.protocol import Frame, encode, read_frame

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
    assert read_frame(stdout).message.type == "hello"
    assert read_frame(stdout) is None
    assert b"hello from print" in proc.stderr
    assert b"raw fd write" in proc.stderr


def spawn_app(tmp_path, source):
    script = tmp_path / "app.py"
    script.write_text(textwrap.dedent(source))
    env = {**os.environ, "PYTHONPATH": str(SDK_ROOT)}
    proc = subprocess.Popen(
        [sys.executable, str(script)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
    )
    assert read_frame(proc.stdout).message.type == "hello"
    return proc


def send(proc, message):
    proc.stdin.write(encode(Frame(message)))
    proc.stdin.flush()


def wait_exit(proc, timeout):
    try:
        return proc.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        proc.kill()
        pytest.fail(f"process did not exit within {timeout} seconds")


def test_handler_calling_sys_exit_still_exits(tmp_path):
    proc = spawn_app(
        tmp_path,
        """
        import sys
        import time
        from tauri_runtime import App

        app = App()

        @app.command
        def stuck() -> None:
            time.sleep(60)

        @app.command
        async def quit() -> None:
            sys.exit(7)

        app.run()
        """,
    )
    send(proc, wire.Request(id=1, cmd="stuck", window="main"))
    time.sleep(0.2)
    send(proc, wire.Request(id=2, cmd="quit", window="main"))
    code = wait_exit(proc, 5)
    proc.stdin.close()
    assert code == 7


def test_malformed_shutdown_with_stubborn_handler_exits(tmp_path):
    proc = spawn_app(
        tmp_path,
        """
        import asyncio
        import time
        from tauri_runtime import App

        app = App()

        @app.command
        async def stubborn() -> None:
            while True:
                try:
                    await asyncio.sleep(10)
                except asyncio.CancelledError:
                    pass

        @app.command
        def stuck() -> None:
            time.sleep(60)

        app.run()
        """,
    )
    send(proc, wire.Request(id=1, cmd="stubborn", window="main"))
    send(proc, wire.Request(id=2, cmd="stuck", window="main"))
    time.sleep(0.3)
    send(proc, wire.Shutdown(timeout_ms="later"))
    proc.stdin.close()
    started = time.monotonic()
    assert wait_exit(proc, 8) == 0
    assert time.monotonic() - started < 6


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
    assert read_frame(proc.stdout).message.type == "hello"
    send(proc, wire.Request(id=1, cmd="slow", window="main"))
    time.sleep(0.3)
    send(proc, wire.Shutdown(timeout_ms=1000))
    proc.stdin.close()
    try:
        proc.wait(timeout=2)
    except subprocess.TimeoutExpired:
        proc.kill()
        pytest.fail("process did not exit within 2 seconds")
    assert proc.returncode == 0
