// Echo test mod for test/sdk.test.ts: runs `run()` over real stdio.
import { RpcError, Triggers, code, run } from "../../src/index.ts";

const triggers = new Triggers();
await run({
  call({ action, params, idem_key }) {
    if (action === "echo") return { params, idem_key: idem_key ?? null };
    if (action === "fail") throw RpcError.failed("boom");
    if (action === "tick") return triggers.emit("tick", { n: 1 });
    throw new RpcError(code.METHOD_NOT_FOUND, action);
  },
  subscribe(params, emitter) {
    triggers.set(params, emitter);
  },
});
