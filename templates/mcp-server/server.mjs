#!/usr/bin/env node
// my-mcp-server: an MCP server template for Sun agents.
//
// MCP over stdio: one JSON-RPC 2.0 object per line on stdin/stdout, the
// same framing as Sun mods. No dependencies.
//
// - Log with console.error; stdout carries the protocol.
// - Add tools to TOOLS. Mark a tool `readOnlyHint: true` only if it changes
//   nothing: Sun turns on read-only tools by default and the rest per task.
// - Register it: `sun plugin mcp add my-mcp-server --command <this file>`.
//   It runs sandboxed: add `--network host` for each host it calls and
//   `--secret NAME` for each secret it reads from process.env.
import { createInterface } from "node:readline";

const TOOLS = [
  {
    name: "hello",
    description: "Greets someone by name.",
    inputSchema: {
      type: "object",
      properties: { name: { type: "string", description: "Who to greet." } },
      required: ["name"],
    },
    annotations: { readOnlyHint: true },
    run: ({ name }) => `Hello, ${name}!`,
  },
];

const METHODS = {
  initialize: (params) => ({
    protocolVersion: params?.protocolVersion ?? "2025-06-18",
    capabilities: { tools: {} },
    serverInfo: { name: "my-mcp-server", version: "0.1.0" },
  }),
  ping: () => ({}),
  "tools/list": () => ({ tools: TOOLS.map(({ run, ...tool }) => tool) }),
  "tools/call": async ({ name, arguments: args }) => {
    const tool = TOOLS.find((t) => t.name === name);
    if (!tool) throw { code: -32602, message: `unknown tool ${name}` };
    try {
      return { content: [{ type: "text", text: String(await tool.run(args ?? {})) }] };
    } catch (e) {
      // A failed call is a result the agent can read, not a protocol error.
      return { content: [{ type: "text", text: String(e?.message ?? e) }], isError: true };
    }
  },
};

const send = (message) => process.stdout.write(JSON.stringify({ jsonrpc: "2.0", ...message }) + "\n");

for await (const line of createInterface({ input: process.stdin })) {
  if (!line.trim()) continue;
  let request;
  try {
    request = JSON.parse(line);
  } catch {
    send({ id: null, error: { code: -32700, message: "parse error" } });
    continue;
  }
  const { id, method, params } = request;
  if (id === undefined) continue; // a notification, e.g. notifications/initialized
  const handler = METHODS[method];
  if (!handler) {
    send({ id, error: { code: -32601, message: `method not found: ${method}` } });
    continue;
  }
  try {
    send({ id, result: await handler(params) });
  } catch (e) {
    send({ id, error: { code: e?.code ?? -32603, message: String(e?.message ?? e) } });
  }
}
