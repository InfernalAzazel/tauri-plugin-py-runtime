# tauri-runtime

**The Python SDK for [tauri-plugin-py-runtime](https://github.com/InfernalAzazel/tauri-plugin-py-runtime):
write the backend of a Tauri 2 desktop app in Python.**

Python functions become commands your frontend can call, with pydantic validation, streaming
channels, events, cancellation and structured errors. The `tauri-runtime` CLI scaffolds projects
and packages your backend with Nuitka into a standalone executable, so end users never need to
install Python.

```python
from tauri_runtime import App

app = App()

@app.command
async def greet(name: str) -> str:
    return f"Hello {name}, from Python!"

app.run()
```

```ts
import { pyInvoke } from "tauri-plugin-py-runtime-api";

const message = await pyInvoke<string>("greet", { name: "Tauri" });
```

## Quick start

Create a new Tauri app with a Python backend:

```bash
uvx --from tauri-runtime tauri-runtime create my-app --template vue-ts --manager pnpm
```

Add a Python backend to an existing Tauri project (run next to `src-tauri`):

```bash
uvx --from tauri-runtime tauri-runtime init
```

Package the backend for release (run in `src-python`, requires Nuitka and a C compiler):

```bash
uv run tauri-runtime build
```

## Features

- `async def` and plain `def` commands, with arguments validated by pydantic
- `Channel[T]` streams, raw binary payloads, and events in both directions
- Timeouts, cancellation and `TauriError` structured errors
- `startup`, `ready` and `shutdown` lifecycle hooks
- `Router` with prefixes for organizing large backends
- Standalone packaging with Nuitka via `tauri-runtime build`

Requires Python 3.11 or newer. Full documentation lives in the
[project README](https://github.com/InfernalAzazel/tauri-plugin-py-runtime#readme).

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your option.
