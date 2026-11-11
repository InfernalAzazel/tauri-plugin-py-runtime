/**
 * tauri-plugin-py-runtime 前端 API。
 *
 * 通过 Tauri IPC 调用 Python sidecar 中注册的命令、与其互发事件并查询运行时状态。
 * 所有失败都以 {@link PyRuntimeError} 拒绝。
 *
 * @module
 */

import { Channel, invoke } from "@tauri-apps/api/core";
import { listen, type Event, type UnlistenFn } from "@tauri-apps/api/event";

export { Channel };
export type { Event, UnlistenFn };

/**
 * Python 运行时状态。
 *
 * - `starting`：正在启动或等待握手；开启自动重启时，崩溃后的退避等待期间也处于此状态。
 * - `running`：握手完成，可以处理调用。
 * - `crashed`：进程意外退出，且未开启自动重启。
 * - `stopped`：运行时已被正常关闭。
 * - `failed`：启动或握手失败（如协议版本不匹配），不会再自动重启。
 */
export type PyStatus = "starting" | "running" | "crashed" | "stopped" | "failed";

/**
 * 命令参数：普通对象按 JSON 发送；`Uint8Array` / `ArrayBuffer` 按原始字节发送，
 * 由 Python 侧 `Raw` 注解的参数接收。
 */
export type PyArgs = Record<string, unknown> | Uint8Array | ArrayBuffer;

/** {@link pyInvoke} 的调用选项。 */
export interface PyInvokeOptions {
  /**
   * 本次调用的超时（毫秒），覆盖默认的 `invoke_timeout`（30 秒）。
   * 必须是非负整数，上限为 `invoke_timeout` 的 10 倍；超时后以 `timeout` 拒绝并通知 Python 取消。
   */
  timeoutMs?: number;
  /** 中止信号：触发后调用立即以 `cancelled` 拒绝，并向 Python 转发取消。 */
  signal?: AbortSignal;
}

/**
 * 插件统一的错误类型，与 Rust / Python 两侧的 `{ code, message, data? }` 结构一致。
 *
 * 常见 `code`：`command_not_found`、`invalid_args`、`internal`、`cancelled`、`timeout`、
 * `runtime_unavailable`、`runtime_exited`、`protocol_mismatch`、`frame_too_large`；
 * Python 中 `TauriError` 抛出的自定义错误码原样透传。
 */
export class PyRuntimeError extends Error {
  /** 机器可读的错误码。 */
  readonly code: string;
  /** 附加数据（如 pydantic 校验错误列表、调试模式下的 traceback）。 */
  readonly data?: unknown;

  constructor(code: string, message: string, data?: unknown) {
    super(message);
    this.name = "PyRuntimeError";
    this.code = code;
    this.data = data;
  }
}

/** Tauri `Channel` 序列化（`toJSON()`）结果的前缀，后接通道 id。 */
const CHANNEL_PREFIX = "__CHANNEL__:";
/** 不支持 `crypto.randomUUID` 时生成调用 id 用的自增计数器。 */
let callCounter = 0;

/**
 * 判断值是否为 Tauri `Channel`。
 *
 * 除 `instanceof` 外还按 `toJSON()` 的结果做鸭子类型判断，
 * 这样由另一份 `@tauri-apps/api` 副本创建的通道也能被识别。
 */
function isChannel(value: unknown): value is Channel<unknown> {
  if (value instanceof Channel) return true;
  if (typeof value !== "object" || value === null) return false;
  const toJSON = (value as { toJSON?: () => unknown }).toJSON;
  return typeof toJSON === "function" && String(toJSON.call(value)).startsWith(CHANNEL_PREFIX);
}

/** 将 IPC 抛出的任意错误规整为 {@link PyRuntimeError}；无法识别的错误归为 `internal`。 */
function toPyRuntimeError(error: unknown): PyRuntimeError {
  if (error instanceof PyRuntimeError) return error;
  if (typeof error === "object" && error !== null && "code" in error && "message" in error) {
    const { code, message, data } = error as { code: unknown; message: unknown; data?: unknown };
    return new PyRuntimeError(String(code), String(message), data);
  }
  return new PyRuntimeError("internal", String(error));
}

/**
 * 生成调用 id，优先使用 `crypto.randomUUID`，不可用时退回时间戳 + 计数器 + 随机串。
 *
 * Rust 侧会拒绝同一窗口内仍在进行中的重复 id，因此 id 必须唯一。
 */
function newCallId(): string {
  const cryptoApi = globalThis.crypto as Crypto | undefined;
  if (typeof cryptoApi?.randomUUID === "function") return cryptoApi.randomUUID();
  const random = Math.random().toString(36).slice(2, 10);
  return `${Date.now().toString(36)}-${(callCounter++).toString(36)}-${random}`;
}

/**
 * 调用 Python 运行时中注册的命令。
 *
 * 只识别 `args` 顶层的 `Channel` 值；嵌套在对象或数组内的通道会被当作普通值序列化，
 * Python 侧不会与之连接。二进制结果以 `Uint8Array` 返回。
 *
 * @param cmd 命令名，必须是非空的可打印 ASCII 字符串。
 * @param args 命令参数，见 {@link PyArgs}。
 * @param options 超时与取消选项，见 {@link PyInvokeOptions}。
 * @returns 命令的返回值。
 * @throws {PyRuntimeError} 参数非法、调用失败、超时或被取消时抛出。
 *
 * @example
 * ```ts
 * const greeting = await pyInvoke<string>("hello", { name: "Kylin" }, { timeoutMs: 5000 });
 * ```
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
  // 命令名、调用 id、超时与通道映射都放在请求头中，请求体只承载参数本身。
  const callId = newCallId();
  const headers: Record<string, string> = { "x-py-cmd": cmd, "x-py-call-id": callId };
  if (timeoutMs !== undefined) headers["x-py-timeout-ms"] = String(timeoutMs);

  let body: PyArgs = args;
  if (!(args instanceof Uint8Array) && !(args instanceof ArrayBuffer)) {
    // 把顶层的 Channel 拆到 `x-py-channels` 头（参数名 -> 通道 id），其余参数作为 JSON 发送。
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
      // 取消 IPC 可能先于调用 IPC 到达（或在 Rust 仍等待运行时就绪时到达），
      // 因此中止时必须在 JS 侧直接结束 Promise，不能依赖后端返回。
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

/**
 * 向 Python 发送事件，由 `@app.listen(event)` 注册的处理器接收。
 *
 * Python 侧 `Event.window` 为发送方 webview 的 label。
 *
 * @param event 事件名。
 * @param payload 事件负载，必须可 JSON 序列化。
 * @throws {PyRuntimeError} Python 未运行时以 `runtime_unavailable` 拒绝。
 */
export async function pyEmit(event: string, payload?: unknown): Promise<void> {
  try {
    await invoke("plugin:py-runtime|emit", { event, payload });
  } catch (error) {
    throw toPyRuntimeError(error);
  }
}

/**
 * 监听 Python 通过 `app.emit(event, ...)` 发出的事件（实际监听 `py://<event>`）。
 *
 * 传入 `"exited"` 可监听 Python 进程意外退出，负载为 `{ code: number | null }`。
 *
 * @param event 事件名，不含 `py://` 前缀。
 * @param handler 事件处理函数。
 * @returns 取消监听的函数。
 */
export function pyListen<T = unknown>(event: string, handler: (event: Event<T>) => void): Promise<UnlistenFn> {
  return listen<T>(`py://${event}`, handler);
}

/**
 * 查询 Python 运行时的当前状态。
 *
 * @returns 当前状态，见 {@link PyStatus}。
 */
export async function pyStatus(): Promise<PyStatus> {
  try {
    return await invoke<PyStatus>("plugin:py-runtime|status");
  } catch (error) {
    throw toPyRuntimeError(error);
  }
}
