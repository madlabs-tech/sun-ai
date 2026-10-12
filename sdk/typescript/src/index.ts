// sun-mod-sdk (TypeScript): build Sun mods in TypeScript or JavaScript.
// Apache-2.0. Mirrors the Rust SDK (sdk/rust); the wire contract is
// sdk/rust/src/protocol.rs. Zero runtime dependencies; Node >= 22.18 runs this
// file directly (type stripping).
//
// A mod is an executable. Implement `Mod` and hand it to `run`:
//
//   import { run, RpcError, code } from "sun-mod-sdk";
//   run({
//     call({ action, params }) {
//       if (action === "echo") return params;
//       throw new RpcError(code.METHOD_NOT_FOUND, action);
//     },
//   });
//
// stdout carries the protocol: log to stderr (console.error), never
// console.log. Requests are handled one at a time, in order. An extension
// reaches third parties only through its declared plugins: use the `kernel`
// handed to `call` (`kernel.call_plugin(plugin, action, params)`).

import { createInterface } from "node:readline";

/** The mod API version this SDK speaks. */
export const API_VERSION = "1.2.0";

/** JSON-RPC method names. */
export const method = {
  INIT: "init",
  CALL: "call",
  SUBSCRIBE: "subscribe",
  EVENT: "event",
  HEALTH: "health",
  SHUTDOWN: "shutdown",
  /** Mod → kernel request (API 1.1, extensions only). */
  PLUGIN_CALL: "plugin_call",
} as const;

/** Reserved `call` actions of a plugin with `provides.mcp_tools` (API 1.1). */
export const mcp_action = {
  /** Params null; result `{ tools: McpToolSpec[] }`. */
  TOOLS: "mcp_tools",
  /** Params `{ name, arguments }`; result: the tool's output. */
  CALL: "mcp_call",
} as const;

/**
 * Reserved `call` actions of run hooks and metrics (API 1.2), for an
 * extension declaring `provides.hooks` / `provides.metrics`.
 */
export const hook = {
  /** Params `{ events: [{ hook, seq, run_id, step_key, kind, payload, blob, at }] }`. */
  OBSERVE: "observe",
  /** Params `{ run }`; result `{ [metric name]: number }`. */
  METRICS: "metrics",
  /** Every event `provides.hooks` may name. */
  EVENTS: ["task_created", "step_started", "step_finished", "gate_result", "run_finished"],
} as const;

/** One MCP tool a plugin serves. */
export interface McpToolSpec {
  name: string;
  description?: string;
  /** JSON Schema of the arguments; `{"type": "object"}` when absent. */
  input_schema?: Record<string, unknown>;
  /** Changes nothing; a grant without a tool list enables only these. */
  read_only?: boolean;
}

/** JSON-RPC error codes, and how the kernel maps them. */
export const code = {
  PARSE_ERROR: -32700,
  INVALID_REQUEST: -32600,
  METHOD_NOT_FOUND: -32601,
  /** Bad input; not retried. */
  INVALID_PARAMS: -32602,
  INTERNAL_ERROR: -32603,
  /** Temporary failure; the kernel retries. */
  UNAVAILABLE: -32000,
  /** The action ran and failed; not retried. */
  FAILED: -32001,
} as const;

// ---- mod.yaml ----------------------------------------------------

export type ModKind = "plugin" | "extension";

export interface Manifest {
  name: string;
  kind: ModKind;
  version: string;
  api: string;
  entry: { command: string; args?: string[] };
  provides?: Provides;
  permissions?: Permissions;
}

/**
 * Plugin-only: actions, triggers, mcp_tools.
 * Extension-only: step_types, gates, hooks, metrics, runtime_adapters and
 * exec_envs (API 1.2; reserved `runtime.*` and `exec_env.*` calls).
 */
export interface Provides {
  actions?: string[];
  triggers?: string[];
  mcp_tools?: boolean;
  step_types?: string[];
  gates?: string[];
  /** Run hooks (API 1.2): `hook.EVENTS` entries, sent as `call observe`. */
  hooks?: string[];
  /** Metric names (API 1.2): `call metrics {run}` at each run end. */
  metrics?: string[];
  /** Runtime adapter ids (API 1.2): runtime `<mod>.<id>`, `runtime.*` calls. */
  runtime_adapters?: string[];
  /** Exec env kinds (API 1.2): `exec_env.*` calls. */
  exec_envs?: string[];
}

/** Plugins may not request devices; extensions may not request network or secrets. */
export interface Permissions {
  network?: string[];
  secrets?: string[];
  devices?: string[];
  paths?: string[];
  processes?: string[];
  plugins?: string[];
}

// ---- protocol --------------------------------------------------------------

export interface Message {
  jsonrpc: "2.0";
  id?: number | string | null;
  method?: string;
  params?: unknown;
  result?: unknown;
  error?: { code: number; message: string; data?: unknown };
}

export interface InitParams {
  api_version: string;
  mod_name: string;
  config?: unknown;
}

export interface CallParams {
  action: string;
  params?: unknown;
  /** Set for outbox side effects: apply at most once per key. */
  idem_key?: string;
}

export interface SubscribeParams {
  triggers: string[];
}

export interface EventParams {
  topic: string;
  payload?: unknown;
}

/** `plugin_call` params (mod → kernel). */
export interface PluginCallParams {
  plugin: string;
  action: string;
  params?: unknown;
  idem_key?: string;
}

/** A JSON-RPC error. Throw it from a handler; anything else thrown becomes INTERNAL_ERROR. */
export class RpcError extends Error {
  code: number;
  data?: unknown;

  constructor(code: number, message: string, data?: unknown) {
    super(message);
    this.code = code;
    this.data = data;
  }

  static invalid(message: string): RpcError {
    return new RpcError(code.INVALID_PARAMS, message);
  }

  static failed(message: string): RpcError {
    return new RpcError(code.FAILED, message);
  }

  static unavailable(message: string): RpcError {
    return new RpcError(code.UNAVAILABLE, message);
  }
}

type Awaitable<T> = T | Promise<T>;

/** A mod's handlers. Only `call` is required. */
export interface Mod {
  /** First request after start. Throw to refuse to run. */
  init?(params: InitParams): Awaitable<void>;
  /**
   * Runs an action, step type or gate. Throw RpcError.unavailable for
   * retryable failures. `kernel` calls back into the kernel, e.g. an
   * extension calling a plugin it declares.
   */
  call(call: CallParams, kernel: Kernel): Awaitable<unknown>;
  /** Replaces the set of triggers to emit. Keep `emitter` and emit when one fires. */
  subscribe?(params: SubscribeParams, emitter: Emitter): Awaitable<void>;
  /** Liveness probe. */
  health?(): Awaitable<unknown>;
}

/** Anything with a `write(string)` method, e.g. process.stdout. */
export interface Output {
  write(chunk: string): unknown;
}

/** Writes messages to the kernel. */
export class Emitter {
  #out: Output;

  constructor(out: Output) {
    this.#out = out;
  }

  /** Sends an `event` notification for trigger `topic`. */
  emit(topic: string, payload: unknown = null): void {
    this.send({ jsonrpc: "2.0", method: method.EVENT, params: { topic, payload } });
  }

  send(msg: Message): void {
    this.#out.write(JSON.stringify(msg) + "\n");
  }
}

/** Requests to the kernel (API 1.1). Answers arrive on the same stdin as requests. */
export class Kernel {
  #emitter: Emitter;
  #next = 1;
  #pending = new Map<number, { resolve: (v: unknown) => void; reject: (e: RpcError) => void }>();

  constructor(emitter: Emitter) {
    this.#emitter = emitter;
  }

  /**
   * Calls `action` of `plugin` (declared in `permissions.plugins`) and
   * resolves with its output; rejects with the plugin's or kernel's RpcError.
   */
  call_plugin(plugin: string, action: string, params: unknown = null, idem_key?: string): Promise<unknown> {
    const id = this.#next++;
    const call: PluginCallParams = { plugin, action, params };
    if (idem_key !== undefined) call.idem_key = idem_key;
    return new Promise((resolve, reject) => {
      this.#pending.set(id, { resolve, reject });
      this.#emitter.send({ jsonrpc: "2.0", id, method: method.PLUGIN_CALL, params: call });
    });
  }

  /** Settles the request a response answers; others are ignored. */
  settle(msg: Message): void {
    const waiting = typeof msg.id === "number" ? this.#pending.get(msg.id) : undefined;
    if (!waiting) return;
    this.#pending.delete(msg.id as number);
    if (msg.error) waiting.reject(new RpcError(msg.error.code, msg.error.message, msg.error.data));
    else waiting.resolve(msg.result ?? null);
  }

  /** Fails every open request (the kernel went away). */
  close(): void {
    for (const waiting of this.#pending.values()) {
      waiting.reject(RpcError.unavailable("the kernel closed the connection"));
    }
    this.#pending.clear();
  }
}

/** Subscribe bookkeeping: call `set` from `Mod.subscribe`, then `emit` when a trigger fires. */
export class Triggers {
  #wanted = new Set<string>();
  #emitter: Emitter | undefined;

  /** Replaces the set (each `subscribe` carries the full set). */
  set(params: SubscribeParams, emitter: Emitter): void {
    this.#wanted = new Set(params.triggers);
    this.#emitter = emitter;
  }

  /** Emits `topic` if it is subscribed. Returns whether it was sent. */
  emit(topic: string, payload: unknown = null): boolean {
    if (!this.#emitter || !this.#wanted.has(topic)) return false;
    this.#emitter.emit(topic, payload);
    return true;
  }
}

/** Serves `mod` on stdin/stdout until `shutdown` or EOF. */
export async function run(mod: Mod): Promise<void> {
  const lines = createInterface({ input: process.stdin, crlfDelay: Infinity });
  await serve(mod, lines, process.stdout);
  // Stop reading so the process can exit once stdout is flushed.
  lines.close();
  process.stdin.destroy();
}

/**
 * Serves `mod` on any line source / output pair (for tests). Lines are read
 * as they come so answers to `kernel.call_plugin` arrive while a request is
 * being handled; requests still run one at a time, in order.
 */
export async function serve(mod: Mod, lines: AsyncIterable<string>, out: Output): Promise<void> {
  const emitter = new Emitter(out);
  const kernel = new Kernel(emitter);
  // Replies go out in request order, parse errors included.
  let queue: Promise<void> = Promise.resolve();
  for await (const line of lines) {
    if (line.trim() === "") continue;
    let msg: Message;
    try {
      msg = JSON.parse(line);
    } catch (e) {
      const error = new RpcError(code.PARSE_ERROR, String(e));
      queue = queue.then(() => emitter.send(response(null, error)));
      continue;
    }
    if (typeof msg.method !== "string") {
      kernel.settle(msg);
      continue;
    }
    const request = msg as Message & { method: string };
    if (request.method === method.SHUTDOWN) {
      await queue;
      if (request.id !== undefined) emitter.send(response(request.id, null, null));
      return;
    }
    queue = queue.then(() => handle(mod, request, emitter, kernel));
  }
  // EOF: nothing will answer a pending plugin call.
  kernel.close();
  await queue;
}

async function handle(
  mod: Mod,
  msg: Message & { method: string },
  emitter: Emitter,
  kernel: Kernel,
): Promise<void> {
  let outcome: unknown;
  try {
    outcome = await dispatch(mod, msg.method, msg.params ?? null, emitter, kernel);
  } catch (e) {
    outcome = e instanceof RpcError ? e : new RpcError(code.INTERNAL_ERROR, String(e));
  }
  if (msg.id !== undefined) {
    emitter.send(outcome instanceof RpcError ? response(msg.id, outcome) : response(msg.id, null, outcome ?? null));
  }
}

async function dispatch(
  mod: Mod,
  name: string,
  params: unknown,
  emitter: Emitter,
  kernel: Kernel,
): Promise<unknown> {
  switch (name) {
    case method.INIT:
      await mod.init?.(object(params) as unknown as InitParams);
      return null;
    case method.CALL: {
      const call = object(params);
      if (typeof call.action !== "string") throw RpcError.invalid("call.action must be a string");
      return mod.call(call as unknown as CallParams, kernel);
    }
    case method.SUBSCRIBE: {
      const sub = object(params);
      if (!Array.isArray(sub.triggers)) throw RpcError.invalid("subscribe.triggers must be an array");
      if (!mod.subscribe) throw new RpcError(code.METHOD_NOT_FOUND, "this mod has no triggers");
      await mod.subscribe(sub as unknown as SubscribeParams, emitter);
      return null;
    }
    case method.HEALTH:
      return mod.health ? mod.health() : { ok: true };
    default:
      throw new RpcError(code.METHOD_NOT_FOUND, `unknown method ${name}`);
  }
}

function object(params: unknown): Record<string, unknown> {
  if (typeof params !== "object" || params === null || Array.isArray(params)) {
    throw RpcError.invalid("params must be an object");
  }
  return params as Record<string, unknown>;
}

function response(id: Message["id"], error: RpcError | null, result?: unknown): Message {
  if (error) {
    const err: Message["error"] = { code: error.code, message: error.message };
    if (error.data !== undefined) err.data = error.data;
    return { jsonrpc: "2.0", id, error: err };
  }
  return { jsonrpc: "2.0", id, result };
}

/** For tests: serves newline-separated `input` and returns every message the mod wrote. */
export async function serve_lines(mod: Mod, input: string): Promise<Message[]> {
  let written = "";
  await serve(mod, line_source(input), { write: (chunk: string) => (written += chunk) });
  return written
    .split("\n")
    .filter((line) => line !== "")
    .map((line) => JSON.parse(line));
}

async function* line_source(input: string): AsyncIterable<string> {
  yield* input.split("\n");
}
