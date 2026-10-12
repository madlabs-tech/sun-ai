// my-plugin: a Sun plugin template. Run it with `sun mod dev .` (hot reload).
//
// - Log with console.error; stdout carries the protocol.
// - Network goes through the sandbox's proxy (HTTPS_PROXY), and only to the
//   hosts declared in mod.yaml. Secrets arrive as env vars, only if declared.
// - Throw RpcError.unavailable(...) for failures worth retrying,
//   RpcError.failed(...) for ones that aren't.
import { RpcError, Triggers, code, run } from "sun-mod-sdk";

const triggers = new Triggers();

await run({
  async call({ action, params, idem_key }) {
    switch (action) {
      case "ping": {
        // An outbox side effect carries idem_key: apply it at most once.
        triggers.emit("pinged", { params, idem_key: idem_key ?? null });
        return { pong: true };
      }
      default:
        throw new RpcError(code.METHOD_NOT_FOUND, action);
    }
  },
  subscribe(params, emitter) {
    triggers.set(params, emitter);
  },
});
