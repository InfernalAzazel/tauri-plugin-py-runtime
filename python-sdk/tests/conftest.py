import asyncio
import json
import os
import threading

import pytest

from tauri_runtime.protocol import Frame, Kind, encode, read_frame


class Harness:
    """Runs App.serve on a background thread, connected through two OS pipes."""

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
            self.exit_code = asyncio.run(app.serve(self._app_in, self._app_out))
        finally:
            self._app_out.close()

    def send(self, kind, header=None, body=b""):
        self._to_app.write(encode(Frame(kind, header or {}, body)))
        self._to_app.flush()

    def request(self, call_id, cmd, args=None, *, raw=None, channels=None):
        header = {"id": call_id, "cmd": cmd, "window": "main"}
        if channels:
            header["channels"] = channels
        if raw is not None:
            header["encoding"], body = "raw", raw
        elif args is not None:
            header["encoding"], body = "json", json.dumps(args).encode()
        else:
            header["encoding"], body = "none", b""
        self.send(Kind.REQUEST, header, body)

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
