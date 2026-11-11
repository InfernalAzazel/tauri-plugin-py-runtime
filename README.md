# Tauri Plugin py-runtime

Write your Tauri backend in Python. The Rust plugin runs a Python sidecar and forwards
calls, events and streams between the frontend and Python.

| Part | Package |
| --- | --- |
| Rust plugin | `tauri-plugin-py-runtime` (plugin name `py-runtime`) |
| Python SDK | `tauri-runtime` on PyPI, imported as `tauri_runtime` (Python ≥ 3.10, requires `pydantic>=2.9`) |
| Frontend API | `tauri-plugin-py-runtime-api` |

The Rust side and Python talk over the child process's stdin/stdout using a small
length-prefixed protocol of JSON messages (protocol version 2, frames up to 64 MiB, see
[Protocol](#protocol)).

## Python

```python
from tauri_runtime import App

app = App()

@app.command
async def hello(name: str) -> str:
    return f"Hello {name}"

@app.on("ready")
async def on_ready():
    print("Application started")

app.run()
```

- `async def` commands run on the event loop; plain `def` commands run in a thread pool.
- Parameters are filled from the JSON arguments (camelCase keys are mapped to snake_case).
  Missing required parameters fail with `invalid_args`. Annotated parameters are validated with
  pydantic (`invalid_args` with the validation errors as `data`).
- Annotate a parameter with `Raw`, `Channel[T]`, `Command` or `App` to receive those instead
  of a JSON value:
  - `Raw` – the raw request body (`data.bytes`) when the frontend passes a `Uint8Array` / `ArrayBuffer`.
  - `Channel[T]` – a stream to a frontend `Channel` passed under the same name
    (`onProgress` → `on_progress`). Use `await ch.send(value)` (or `ch.send_sync(value)` from a
    plain `def` command). A channel is only valid until the command returns.
  - `Command` – call context: `id`, `cmd`, `window` (label of the calling webview), `cancelled`.
  - `App` – the app instance.
- Return any value pydantic can serialize to JSON (including dataclasses and pydantic models).
  Return `Raw(...)` or `bytes` to send binary data; the frontend receives a `Uint8Array`.
  `NaN` and `±Infinity` are not valid JSON and are rejected: returning them fails the call
  with `internal` (sending them on a channel or with `emit` raises `ValueError`).
- Raise `TauriError(code, message, data)` to return a structured error with your own code.
  If `data` isn't JSON-serializable, the error is sent without `data` and a `[WARN]` is logged.
  Any other exception becomes `internal` with `data = {"type": "<ExceptionName>"}`; set
  `TAURI_RUNTIME_DEBUG=1` in the Python environment to also include the traceback.
- `@app.command("name")` registers under a different name.
- Lifecycle hooks: `@app.on("startup")` runs before the handshake (an exception aborts the
  runtime), `@app.on("ready")` after the Rust side confirmed the handshake,
  `@app.on("shutdown")` when the app exits (bounded by the shutdown timeout).
- Events: `@app.listen("name")` receives events sent with `pyEmit` (handler gets an `Event`
  with `name`, `payload`, `window`). `await app.emit("name", payload, target=None)` sends an
  event to the frontend; `target` is a webview label, otherwise it is broadcast. The name
  `exited` is reserved for the runtime's own `py://exited` event; `app.emit("exited", ...)`
  raises `ValueError`.
- Cancelling a call cancels the asyncio task of an `async def` command. A plain `def` command
  keeps running in its worker thread until it returns (Python can't interrupt threads); its
  result is discarded.
- `print` is safe: stdout is reserved for the protocol, so `sys.stdout` is redirected to
  stderr, and everything written to stderr ends up in the app log (see [Logging](#logging)).

## Rust

```rust
tauri::Builder::default()
    .plugin(
        tauri_plugin_py_runtime::Builder::new()
            .script("../python/main.py")
            .build(),
    )
```

`Builder` options:

| Method | Default | Notes |
| --- | --- | --- |
| `script(path)` | – (required) | Python entry script, see [Script path resolution](#script-path-resolution) |
| `python(program)` | `python3` (`python` on Windows) | Interpreter to run |
| `args(iter)` | none | Extra arguments passed after the script path |
| `env(key, value)` | none | Extra environment variable for the Python process; call repeatedly |
| `invoke_timeout(Duration)` | 30 s | Default timeout per call; a per-call `timeoutMs` is capped at 10× this value |
| `shutdown_timeout(Duration)` | 3 s | Time Python gets to run shutdown hooks |
| `handshake_timeout(Duration)` | 10 s | Time Python gets to start and send its handshake; also how long a call waits for a runtime that is still starting |
| `restart_on_crash(bool)` | `false` | See [Process lifecycle](#process-lifecycle) |

The Python process always gets `PYTHONUNBUFFERED=1` and `PYTHONIOENCODING=utf-8`.

### `tauri.conf.json`

The same settings (except `env` and `handshake_timeout`, which are `Builder`-only) can be set
in `plugins.py-runtime`. Keys are camelCase and unknown keys are rejected, so a typo fails at
startup instead of being ignored:

```json
{
  "plugins": {
    "py-runtime": {
      "python": "python3",
      "script": "../python/main.py",
      "args": ["--verbose"],
      "invokeTimeoutMs": 30000,
      "shutdownTimeoutMs": 3000,
      "restartOnCrash": false
    }
  }
}
```

Values set on the `Builder` take precedence over the config file. To configure the plugin
only through the file, use `tauri_plugin_py_runtime::init()`.

From Rust code you can reach the runtime via `app.py_runtime().supervisor()`
(`PyRuntimeExt`), e.g. to call commands or read the status.

### Permissions

Add `"py-runtime:default"` to your capability. It allows `invoke`, `cancel`, `emit` and
`status`; the individual permissions are `py-runtime:allow-<name>`.

### Script path resolution

- An absolute path is used as is.
- A relative path is tried against these locations, first existing file wins:
  1. Debug builds only: the process working directory. Under `tauri dev` that is `src-tauri`,
     so `"../python/main.py"` points at `<project>/python/main.py`.
  2. The app's resource directory, with every `..` component replaced by `_up_` (the same
     layout Tauri's bundler uses for `bundle.resources`).
- Release builds only look in the resource directory, so the script has to be shipped via
  `bundle.resources`:
  - list form: `"resources": ["../python/**/*"]` together with `script: "../python/main.py"`;
  - map form: `"resources": { "../python/": "python/" }` together with `script: "python/main.py"`.
- If nothing is found, plugin setup fails with an error listing every path that was tried.
- Python runs with the script's directory as its working directory.

> This phase does not package a Python interpreter or your dependencies: the interpreter you
> configure must have `tauri-runtime` (and therefore `pydantic`) importable. The example app runs
> the in-repo SDK's venv (`python-sdk/.venv`, created by `uv sync`, which installs the SDK in
> editable mode) via an absolute, compile-time path
> (`concat!(env!("CARGO_MANIFEST_DIR"), "/../../../python-sdk/.venv/bin/python")`), which only
> works on the machine that built it. Bundling Python and dependencies (uv / Nuitka) is planned
> for phase 5.

## Frontend

```ts
import { Channel, pyEmit, pyInvoke, pyListen, pyStatus, PyRuntimeError } from "tauri-plugin-py-runtime-api";

const msg = await pyInvoke<string>("hello", { name: "Kylin" });

const onItem = new Channel<number>();
onItem.onmessage = (n) => console.log(n);
await pyInvoke("stream", { count: 10, onItem });

await pyListen("welcome", (e) => console.log(e.payload));
await pyEmit("user:login", { name: "Kylin" });

console.log(await pyStatus()); // "starting" | "running" | "crashed" | "stopped" | "failed"
```

### `pyInvoke(cmd, args?, options?)`

- `args` is a plain object (sent as JSON) or a `Uint8Array` / `ArrayBuffer` (sent as raw
  bytes, received by a `Raw` parameter). Binary results come back as a `Uint8Array`.
- `Channel` values are only detected at the **top level** of `args`. A channel nested inside
  an object or array is serialized as a plain value and is not wired up in Python.
- `options.timeoutMs` overrides the default invoke timeout for this call. It must be a
  non-negative integer (otherwise `pyInvoke` throws `invalid_args`) and is capped at 10× the
  configured `invoke_timeout` (5 min with the 30 s default). On timeout the call rejects with
  `timeout` and Python receives a cancel for it.
- `options.signal` (an `AbortSignal`) cancels the call: the promise rejects with `cancelled`
  immediately on abort, without waiting for the backend, and a cancel is forwarded to Python.
  An already-aborted signal rejects before anything is sent.
- Each call carries a client call id that is scoped to the calling window: a webview can only
  cancel its own calls, and a second call reusing an id that is still pending in the same
  window is rejected with `invalid_args`. `pyInvoke` generates unique ids for you.

### Channels

- Values sent with `ch.send(...)` arrive in `onmessage` in order. Raw bytes (`Raw` / `bytes`)
  arrive as an `ArrayBuffer`, not a `Uint8Array`; declare the channel as `Channel<ArrayBuffer>`.
- Channel messages and the call result travel separately, so the last `onmessage` can run
  **after** `pyInvoke` has resolved. If you need strict ordering, send an explicit end marker
  on the channel (e.g. `{ done: true }`) and wait for it instead of relying on the promise.

### Events

- `pyListen(event, handler)` listens to `py://<event>`; Python's `app.emit("x", ...)` arrives
  as `py://x`. `pyListen` returns an unlisten function.
- `pyListen("exited", ...)` (`py://exited`) fires whenever the Python process exits
  unexpectedly, with payload `{ code: number | null }`.
- `pyEmit(event, payload)` sends an event to Python's `@app.listen(event)` handlers; the
  Python `Event.window` is the label of the sending webview. Fails with `runtime_unavailable`
  if Python isn't running.
- Event payloads are JSON. If Python emits raw bytes as an event, the frontend receives them as
  an array of numbers. Use a channel or a `pyInvoke` result for binary data.

### Errors

Every failure rejects with a `PyRuntimeError` (`name`, `code`, `message`, optional `data`),
mirroring the error shape `{ code, message, data? }` used across Rust and Python:

```ts
try {
  await pyInvoke("fail", { message: "boom" });
} catch (e) {
  if (e instanceof PyRuntimeError) console.log(e.code, e.message, e.data);
}
```

| Code | Meaning |
| --- | --- |
| `command_not_found` | No Python command with that name |
| `invalid_args` | Bad arguments (missing parameter, pydantic validation, malformed request) |
| `internal` | Unhandled Python exception or internal error |
| `cancelled` | The call was cancelled |
| `timeout` | The call exceeded its timeout |
| `runtime_unavailable` | Python isn't running (failed to start, handshake failed or timed out, stopped) |
| `runtime_exited` | Python exited while the call was in flight |
| `protocol_mismatch` | The Python SDK speaks a different protocol version |
| `frame_too_large` | A message exceeded the 64 MiB frame limit |

Codes raised with `TauriError` in Python are passed through unchanged.

## Process lifecycle

- The Python process starts during plugin setup. Python sends a handshake listing its
  commands; Rust checks the protocol version and replies, after which `ready` hooks run and the
  status becomes `running`. Calls made while starting wait for the handshake.
- If Python exits unexpectedly, in-flight calls fail with `runtime_exited`, `py://exited` is
  emitted and the status becomes `crashed`.
- With `restart_on_crash` / `restartOnCrash` enabled (off by default), the process is restarted
  with exponential backoff: 1 s, 2 s, 4 s … capped at 30 s. The backoff resets once a process
  stayed up for at least 30 s. If a restarted process fails its handshake, restarting stops and
  the status becomes `failed`.
- On app exit, Rust sends a `shutdown` message with `timeoutMs` (the shutdown timeout). Python
  cancels running calls and runs `shutdown` hooks, all within that one budget (an invalid
  `timeoutMs` or a closed stdin falls back to 3 s), then exits. Handlers that ignore
  cancellation and stuck `def` handlers are abandoned rather than awaited. If the process is
  still alive after the shutdown timeout plus 1 s of grace, it is killed.
- `app.run()` always ends the process: if an exception escapes (including `sys.exit()` or
  `KeyboardInterrupt` raised from a handler) it flushes output and exits with the
  `sys.exit` code, or 1 for any other exception.

## Protocol

Every frame is `[u32 BE length][u32 BE header length][header JSON][body]`; the outer length
counts everything after itself and is capped at 64 MiB. The header is one JSON object tagged by
`type`:

| `type` | Direction | Fields |
| --- | --- | --- |
| `hello` | Python → Rust | `protocol`, `sdk`, `commands` |
| `ready` | Rust → Python | `protocol`, `app: { name, version }` |
| `shutdown` | Rust → Python | `timeoutMs` |
| `request` | Rust → Python | `id`, `cmd`, `channels?` (argument name → channel id), `window?`, payload |
| `response` | Python → Rust | `id`, payload |
| `error` | Python → Rust | `id`, `code`, `message`, `data?` |
| `cancel` | Rust → Python | `id`, `reason` (`"client"` or `"timeout"`) |
| `event` | both | `event`, `target?` (Python → Rust), `window?` (Rust → Python), payload |
| `channel_data` | Python → Rust | `channel`, payload |
| `channel_close` | Python → Rust | `channel` |

A payload is either a JSON value inline in the header as `value` (absent means none/`null`), or
binary data in the frame body with `"raw": true`. The body must be empty unless `raw` is set.
`NaN` and `±Infinity` are never sent. Unknown header fields are ignored. A frame whose header
can't be decoded (invalid JSON, unknown `type`, missing or mistyped field) is logged and
skipped; an oversized frame ends the connection. If the first frame from Python isn't a
decodable `hello` with the same protocol version, startup fails with `protocol_mismatch`.

The message types are a serde enum on the Rust side (`bridge::protocol::Message`) and pydantic
models on the Python side (`tauri_runtime.protocol`). `python-sdk/tests/fixtures/golden_frames.json`
holds one encoded frame per type that both test suites encode and decode byte for byte.

## Logging

Python's stderr (including `print`, which is redirected there) is forwarded line by line to the
Rust `log` crate under target `py-runtime`. A leading `[DEBUG]`, `[INFO]`, `[WARN]` or
`[ERROR]` selects the level and is stripped; other lines are logged at `INFO`:

```python
print("[WARN] disk almost full")   # -> WARN  py-runtime: disk almost full
```

Install a logger (e.g. `tauri-plugin-log`) to see the output.

## Example

`examples/vue` is a Vue app demonstrating commands, raw bytes, concurrency, errors, timeouts,
cancellation, channels and events. Its Python backend is `examples/vue/python/main.py`.

```bash
cd examples/vue && pnpm install && pnpm tauri dev
```

## Development

```bash
cd python-sdk && uv sync                # SDK venv, used by the Rust integration tests and the example
cargo test                              # Rust unit + integration tests ($PYTHON overrides the interpreter)
cd python-sdk && uv run pytest          # Python SDK tests
pnpm install && pnpm build              # build guest-js
```

## Roadmap

1. **Runtime core** (this release): process supervision, protocol, commands, events, channels,
   Python SDK and frontend API.
2. Desktop native APIs.
3. System extensions.
4. Hardware access.
5. Build toolchain: packaging Python and dependencies with uv / Nuitka.
