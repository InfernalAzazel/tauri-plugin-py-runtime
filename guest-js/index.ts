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

// The Rust side rejects a call id that is still pending in the same window, so ids must be unique.
function newCallId(): string {
  const cryptoApi = globalThis.crypto as Crypto | undefined;
  if (typeof cryptoApi?.randomUUID === "function") return cryptoApi.randomUUID();
  const random = Math.random().toString(36).slice(2, 10);
  return `${Date.now().toString(36)}-${(callCounter++).toString(36)}-${random}`;
}

/**
 * Calls a Python command registered with the runtime.
 *
 * `Channel` values are only detected at the top level of `args`; channels nested inside
 * objects or arrays are serialized as plain values and won't be wired up on the Python side.
 */
export async function pyInvoke<T = unknown>(
  cmd: string,
  args: PyArgs = {},
  options: PyInvokeOptions = {},
): Promise<T> {
  if (typeof cmd !== "string" || !/^[\x20-\x7e]+$/.test(cmd)) {
    throw new PyRuntimeError("invalid_args", "cmd must be a non-empty printable ASCII string");
  }
  const { timeoutMs } = options;
  if (timeoutMs !== undefined && !(Number.isSafeInteger(timeoutMs) && timeoutMs >= 0)) {
    throw new PyRuntimeError("invalid_args", "timeoutMs must be a non-negative integer");
  }
  const callId = newCallId();
  const headers: Record<string, string> = { "x-py-cmd": cmd, "x-py-call-id": callId };
  if (timeoutMs !== undefined) headers["x-py-timeout-ms"] = String(timeoutMs);

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

  const call = invoke<unknown>("plugin:py-runtime|invoke", body, { headers });
  let onAbort: (() => void) | undefined;
  try {
    let result: unknown;
    if (signal) {
      // The cancel IPC can overtake the invoke IPC (or arrive while Rust is still waiting for the
      // runtime), so the abort must settle the promise on the JS side regardless of the backend.
      call.catch(() => {});
      const aborted = new Promise<never>((_, reject) => {
        onAbort = () => {
          invoke("plugin:py-runtime|cancel", { id: callId }).catch(() => {});
          reject(new PyRuntimeError("cancelled", "call was cancelled"));
        };
        signal.addEventListener("abort", onAbort, { once: true });
      });
      result = await Promise.race([call, aborted]);
    } else {
      result = await call;
    }
    return (result instanceof ArrayBuffer ? new Uint8Array(result) : result) as T;
  } catch (error) {
    throw toPyRuntimeError(error);
  } finally {
    if (onAbort) signal?.removeEventListener("abort", onAbort);
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

export async function pyStatus(): Promise<PyStatus> {
  try {
    return await invoke<PyStatus>("plugin:py-runtime|status");
  } catch (error) {
    throw toPyRuntimeError(error);
  }
}
