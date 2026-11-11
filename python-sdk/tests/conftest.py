"""测试公共设施：构造任意帧的 `raw_frame`，以及通过管道驱动 `App` 的 `Harness` / `harness` fixture。"""

import os
import threading

import pytest

from tauri_runtime import protocol as wire
from tauri_runtime.protocol import Frame, encode, read_frame


def raw_frame(header: bytes, body: bytes = b"") -> bytes:
    """以任意（可能非法的）header 字节构造一帧。"""
    return (4 + len(header) + len(body)).to_bytes(4, "big") + len(header).to_bytes(4, "big") + header + body


class Harness:
    """在后台线程运行 `App.serve`，通过两条 OS 管道模拟 Rust 宿主收发帧。

    构造时会读取 App 发出的第一帧并保存在 `hello` 中。
    """

    def __init__(self, app):
        app_in_r, app_in_w = os.pipe()
        app_out_r, app_out_w = os.pipe()
        self._app_in = os.fdopen(app_in_r, "rb")
        self._app_out = os.fdopen(app_out_w, "wb")
        self._to_app = os.fdopen(app_in_w, "wb")
        self._from_app = os.fdopen(app_out_r, "rb")
        self.exit_code = None
        self.thread = threading.Thread(target=self._run, args=(app,), daemon=True)
        self.thread.start()
        self.hello = self.recv()

    def _run(self, app):
        try:
            self.exit_code = app._drive(self._app_in, self._app_out)
        finally:
            self._app_out.close()

    def send(self, message, body=b""):
        self.send_bytes(encode(Frame(message, body)))

    def send_bytes(self, data):
        self._to_app.write(data)
        self._to_app.flush()

    def request(self, call_id, cmd, args=None, *, raw=None, channels=None):
        message = wire.Request(id=call_id, cmd=cmd, window="main", channels=channels or {}, value=args)
        if raw is not None:
            message.raw = True
        self.send(message, raw or b"")

    def recv(self, timeout=5.0):
        result = {}
        reader = threading.Thread(target=lambda: result.update(frame=read_frame(self._from_app)), daemon=True)
        reader.start()
        reader.join(timeout)
        if reader.is_alive():
            raise TimeoutError("no frame from app")
        return result["frame"]

    def close(self, timeout=5.0):
        if not self._to_app.closed:
            self._to_app.close()
        self.thread.join(timeout)


@pytest.fixture
def harness():
    created = []

    def make(app):
        h = Harness(app)
        created.append(h)
        return h

    yield make
    for h in created:
        h.close()
