import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

import { type Message, type Mod, code, serve, serve_lines } from "../src/index.ts";

const echo: Mod = {
  call: ({ params }) => params,
  subscribe({ triggers }, emitter) {
    for (const t of triggers) emitter.emit(t, 1);
  },
};

test("serve answers requests, emits events and stops on shutdown", async () => {
  const input = [
    '{"jsonrpc":"2.0","id":1,"method":"init","params":{"api_version":"1.0.0","mod_name":"echo"}}',
    '{"jsonrpc":"2.0","id":2,"method":"call","params":{"action":"echo","params":{"a":1}}}',
    '{"jsonrpc":"2.0","id":3,"method":"subscribe","params":{"triggers":["tick"]}}',
    '{"jsonrpc":"2.0","id":4,"method":"nope"}',
    "not json",
    '{"jsonrpc":"2.0","id":5,"method":"shutdown"}',
    '{"jsonrpc":"2.0","id":6,"method":"health"}',
  ].join("\n");
  const msgs = await serve_lines(echo, input);
  assert.equal(msgs.length, 7, JSON.stringify(msgs));
  assert.deepEqual(msgs[0], { jsonrpc: "2.0", id: 1, result: null });
  assert.deepEqual(msgs[1].result, { a: 1 });
  assert.deepEqual(msgs[2], {
    jsonrpc: "2.0",
    method: "event",
    params: { topic: "tick", payload: 1 },
  });
  assert.equal(msgs[3].id, 3);
  assert.equal(msgs[4].error?.code, code.METHOD_NOT_FOUND);
  assert.equal(msgs[5].error?.code, code.PARSE_ERROR);
  assert.deepEqual(msgs[6], { jsonrpc: "2.0", id: 5, result: null });
});

test("bad params are INVALID_PARAMS; other throws are INTERNAL_ERROR", async () => {
  const buggy: Mod = {
    call() {
      throw new TypeError("bug");
    },
  };
  const msgs = await serve_lines(
    buggy,
    [
      '{"jsonrpc":"2.0","id":1,"method":"call","params":{"params":{}}}',
      '{"jsonrpc":"2.0","id":2,"method":"call","params":{"action":"x"}}',
      '{"jsonrpc":"2.0","id":3,"method":"subscribe","params":{"triggers":[]}}',
      '{"jsonrpc":"2.0","id":4,"method":"health"}',
    ].join("\n"),
  );
  assert.equal(msgs[0].error?.code, code.INVALID_PARAMS);
  assert.equal(msgs[1].error?.code, code.INTERNAL_ERROR);
  assert.equal(msgs[2].error?.code, code.METHOD_NOT_FOUND);
  assert.deepEqual(msgs[3].result, { ok: true });
});

test("run() serves a real mod process over stdio", async () => {
  const mod = fileURLToPath(new URL("./fixtures/echo_mod.ts", import.meta.url));
  const child = spawn(process.execPath, [mod], { stdio: ["pipe", "pipe", "inherit"] });
  const requests = [
    { id: 1, method: "init", params: { api_version: "1.0.0", mod_name: "echo" } },
    { id: 2, method: "call", params: { action: "echo", params: { a: 1 }, idem_key: "k" } },
    { id: 3, method: "call", params: { action: "fail" } },
    { id: 4, method: "subscribe", params: { triggers: ["tick"] } },
    { id: 5, method: "call", params: { action: "tick" } },
    { id: 6, method: "shutdown" },
  ];
  for (const r of requests) child.stdin.write(JSON.stringify({ jsonrpc: "2.0", ...r }) + "\n");
  let out = "";
  for await (const chunk of child.stdout) out += chunk;
  const exit_code = await new Promise((resolve) => child.on("close", resolve));
  assert.equal(exit_code, 0);
  const msgs = out
    .trim()
    .split("\n")
    .map((line) => JSON.parse(line));
  assert.deepEqual(msgs[1].result, { params: { a: 1 }, idem_key: "k" });
  assert.deepEqual(msgs[2].error, { code: code.FAILED, message: "boom" });
  assert.deepEqual(msgs[4].params, { topic: "tick", payload: { n: 1 } });
  assert.equal(msgs[5].result, true);
  assert.deepEqual(msgs[6], { jsonrpc: "2.0", id: 6, result: null });
});

test("call_plugin sends plugin_call and settles on the kernel's answer", async () => {
  const sent: Message[] = [];
  let wake = () => {};
  const out = {
    write(chunk: string) {
      for (const line of chunk.split("\n")) if (line) sent.push(JSON.parse(line));
      wake();
    },
  };
  const next_sent = async (wanted: (m: Message) => boolean): Promise<Message> => {
    while (!sent.some(wanted)) await new Promise<void>((resolve) => (wake = resolve));
    return sent.find(wanted)!;
  };
  const relay: Mod = {
    call: ({ params }, kernel) => kernel.call_plugin((params as { plugin: string }).plugin, "echo", { x: 1 }),
  };
  const call = (id: number, plugin: string) =>
    JSON.stringify({ jsonrpc: "2.0", id, method: "call", params: { action: "relay", params: { plugin } } });
  async function* lines() {
    yield call(7, "echo");
    const first = await next_sent((m) => m.method === "plugin_call");
    assert.deepEqual(first.params, { plugin: "echo", action: "echo", params: { x: 1 } });
    yield JSON.stringify({ jsonrpc: "2.0", id: first.id, result: { params: { x: 1 } } });
    yield call(8, "nope");
    const second = await next_sent((m) => m.method === "plugin_call" && m.id !== first.id);
    yield JSON.stringify({ jsonrpc: "2.0", id: second.id, error: { code: code.INVALID_PARAMS, message: "may not call nope" } });
    await next_sent((m) => m.id === 8 && m.method === undefined);
  }
  await serve(relay, lines(), out);
  const replies = sent.filter((m) => m.method === undefined);
  assert.deepEqual(replies[0], { jsonrpc: "2.0", id: 7, result: { params: { x: 1 } } });
  assert.deepEqual(replies[1].error, { code: code.INVALID_PARAMS, message: "may not call nope" });
});
