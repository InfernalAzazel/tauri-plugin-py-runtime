import io
import os
import subprocess
import sys
import textwrap
from pathlib import Path

from tauri_runtime.protocol import Kind, read_frame

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
