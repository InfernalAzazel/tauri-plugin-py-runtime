import { Channel, invoke } from "@tauri-apps/api/core";
import { listen, type Event, type UnlistenFn } from "@tauri-apps/api/event";

export { Channel };
export type { Event, UnlistenFn };

export type PyStatus = "starting" | "running" | "crashed" | "stopped" | "failed";

export type PyArgs = Record<string, unknown> | Uint8Array | ArrayBuffer;

export interface PyInvokeOptions {
  timeoutMs?: number;
  signal?: AbortSignal;
}

export class PyRuntimeError extends Error {
  readonly code: string;
  readonly data?: unknown;

  constructor(code: string, message: string, data?: unknown) {
    super(message);
    this.name = "PyRuntimeError";
    this.code = code;
    this.data = data;
  }
}

const CHANNEL_PREFIX = "__CHANNEL__:";
let callCounter = 0;

// Duck-typed so channels created by another copy of @tauri-apps/api are still detected.
function isChannel(value: unknown): value is Channel<unknown> {
  if (value instanceof Channel) return true;
  if (typeof value !== "object" || value === null) return false;
  const toJSON = (value as { toJSON?: () => unknown }).toJSON;
  return typeof toJSON === "function" && String(toJSON.call(value)).startsWith(CHANNEL_PREFIX);
}

function toPyRuntimeError(error: unknown): PyRuntimeError {
  if (error instanceof PyRuntimeError) return error;
  if (typeof error === "object" && error !== null && "code" in error && "message" in error) {
    const { code, message, data } = error as { code: unknown; message: unknown; data?: unknown };
    return new PyRuntimeError(String(code), String(message), data);
  }
  return new PyRuntimeError("internal", String(error));
}

export async function pyInvoke<T = unknown>(
  cmd: string,
  args: PyArgs = {},
  options: PyInvokeOptions = {},
): Promise<T> {
  const callId = `${Date.now().toString(36)}-${(callCounter++).toString(36)}`;
  const headers: Record<string, string> = { "x-py-cmd": cmd, "x-py-call-id": callId };
  if (options.timeoutMs !== undefined) headers["x-py-timeout-ms"] = String(options.timeoutMs);

  let body: PyArgs = args;
  if (!(args instanceof Uint8Array) && !(args instanceof ArrayBuffer)) {
    const plain: Record<string, unknown> = {};
    const channels: Record<string, string> = {};
    for (const [key, value] of Object.entries(args)) {
      if (isChannel(value)) channels[key] = String(value.toJSON());
      else plain[key] = value;
    }
    if (Object.keys(channels).length > 0) headers["x-py-channels"] = JSON.stringify(channels);
    body = plain;
  }

  const { signal } = options;
  if (signal?.aborted) throw new PyRuntimeError("cancelled", "call was cancelled");
  const onAbort = () => {
    void invoke("plugin:py-runtime|cancel", { id: callId });
  };
  signal?.addEventListener("abort", onAbort, { once: true });
  try {
    const result = await invoke<unknown>("plugin:py-runtime|invoke", body, { headers });
    return (result instanceof ArrayBuffer ? new Uint8Array(result) : result) as T;
  } catch (error) {
    throw toPyRuntimeError(error);
  } finally {
    signal?.removeEventListener("abort", onAbort);
  }
}

export async function pyEmit(event: string, payload?: unknown): Promise<void> {
  try {
    await invoke("plugin:py-runtime|emit", { event, payload });
  } catch (error) {
    throw toPyRuntimeError(error);
  }
}

export function pyListen<T = unknown>(event: string, handler: (event: Event<T>) => void): Promise<UnlistenFn> {
  return listen<T>(`py://${event}`, handler);
}

export function pyStatus(): Promise<PyStatus> {
  return invoke<PyStatus>("plugin:py-runtime|status");
}
