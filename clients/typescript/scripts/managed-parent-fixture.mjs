import { Client, ensureServer } from "../dist/index.js";
import { __testing } from "../dist/server.js";

await ensureServer();
const child = __testing.managedChildSnapshot();
if (!child) throw new Error("managed child was not started");

let activeStream;
if (process.env.LLMSHIM_FIXTURE_ACTIVE_STREAM === "1") {
  activeStream = new Client().stream({
    model: "vllm/test",
    messages: [{ role: "user", content: "hold the stream" }],
  });
  const first = await activeStream.next();
  if (first.done) throw new Error("managed stream ended before the first event");
}

if (process.env.LLMSHIM_FIXTURE_HOST_SIGNAL === "1") {
  let hostSignalCount = 0;
  process.on("SIGTERM", () => {
    hostSignalCount += 1;
    process.stdout.write(JSON.stringify({ hostSignalCount }) + "\n");
  });
}

process.stdout.write(JSON.stringify({ ready: true, childPid: child.process.pid }) + "\n");
setInterval(() => {}, 1_000);
