// Managed startup unit tests plus opt-in real-binary lifecycle coverage.
// Every network fixture stays on ephemeral loopback and uses no provider key.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { EventEmitter } from "node:events";
import { chmod, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { createServer as createHttpServer } from "node:http";
import { request as rawHttpsRequest } from "node:https";
import { createConnection } from "node:net";
import { createServer } from "node:net";
import { delimiter, dirname } from "node:path";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { PassThrough } from "node:stream";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { Client, ensureServer, platformPackageName } from "../dist/index.js";
import { __testing } from "../dist/server.js";

const testCertificate = `-----BEGIN CERTIFICATE-----
MIICwDCCAaigAwIBAgIJAJGXS+9fJK+EMA0GCSqGSIb3DQEBCwUAMBQxEjAQBgNV
BAMMCTEyNy4wLjAuMTAgFw0yNjA5MjIwOTM3MjZaGA8yMTI2MDgyOTA5MzcyNlow
FDESMBAGA1UEAwwJMTI3LjAuMC4xMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIB
CgKCAQEAzwyCueVAoqCYvYo7dLRgL8PY9vq1RXmWmIbxvqdHTRaacqax+Q6BfJBJ
g2XjSqNzVQ8oYWBP4p6O2v9aSUUDkFk5e6vwzS2+xMk+uMqXBV5LT2PszQHTkJgB
PULZ7lUeAeBqXEMQrM4n5lKEkJPCu2fQOMqOyPLL8JMYXPxtvur4CYWXK+Kce9Fl
9MbQHleHkCugZfJ5oIekmifp3Rgi+dwm7baReMxdCb5ID8mR1x+ZRaCB0s+KrtrD
8H1vHpE1VqJe8pDLAp62XAqVhCehUwNejqRD9gD8AHHlvRzN+Sdwvis9VdSaufPg
sA0QBPN48Wq1NvDZ2ZYRs8dsZxwO+QIDAQABoxMwETAPBgNVHREECDAGhwR/AAAB
MA0GCSqGSIb3DQEBCwUAA4IBAQCXUPGG26iiGM4xvzEb5HWoEBqmzq+yozVgzEXI
imMgAUJ6SHQl+xd0jjtg0jZNErTld2uaZSnKb8u9Rq5F0Z0NLWNvUxBxfs+SDRbe
wI/pvrdOv5r6c5a2OuSB9WUYDK7TmJ2a6HRtPWw28FhUsK4NNAxHcmefq8WFz3fW
30gjjWxq18Ha/98FEsmOsK3F5Lruc3bSzMXMdkqk8zROElC3GXuyXlECDmdM/rOt
lrxSuD2wTiGBjpCNB0/PXzCO0vqJ+rENm5dQqSex1HRXrRutCJstr75y4Vmgk2KN
NYGQAmT/nJGaIzwuJGuRAkmaDQN6uZfj4VwevdyRDY0dfgXk
-----END CERTIFICATE-----
`;

function readiness(overrides = {}) {
  return Buffer.from(JSON.stringify({
    protocol: "llmshim-managed-v1",
    base_url: "https://127.0.0.1:41321",
    auth_token: "a".repeat(64),
    certificate_pem: testCertificate,
    ...overrides,
  }) + "\n");
}

function readJsonLine(stream) {
  return new Promise((resolve, reject) => {
    let buffer = "";
    stream.setEncoding("utf8");
    const onData = (chunk) => {
      buffer += chunk;
      const newline = buffer.indexOf("\n");
      if (newline === -1) return;
      stream.off("data", onData);
      try {
        resolve(JSON.parse(buffer.slice(0, newline)));
      } catch (error) {
        reject(error);
      }
    };
    stream.on("data", onData);
    stream.once("error", reject);
    stream.once("end", () => reject(new Error("fixture stdout ended before a JSON line")));
  });
}

function waitForExit(child, timeoutMs = 3_000) {
  return new Promise((resolve, reject) => {
    if (child.exitCode !== null || child.signalCode !== null) {
      resolve({ code: child.exitCode, signal: child.signalCode });
      return;
    }
    const timeout = setTimeout(() => reject(new Error("fixture process did not exit")), timeoutMs);
    child.once("exit", (code, signal) => {
      clearTimeout(timeout);
      resolve({ code, signal });
    });
  });
}

function processExists(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    if (error?.code === "ESRCH") return false;
    throw error;
  }
}

async function waitForProcessGone(pid, timeoutMs = 3_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (!processExists(pid)) return;
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  throw new Error(`process ${pid} survived cleanup`);
}

async function readPidFile(path, timeoutMs = 3_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      return Number(await readFile(path, "utf8"));
    } catch (error) {
      if (error?.code !== "ENOENT") throw error;
      await new Promise((resolve) => setTimeout(resolve, 20));
    }
  }
  throw new Error("startup child did not publish its pid");
}

/** Run `fn` with process.platform/arch temporarily overridden. */
function withPlatform(platform, arch, fn) {
  const platDesc = Object.getOwnPropertyDescriptor(process, "platform");
  const archDesc = Object.getOwnPropertyDescriptor(process, "arch");
  Object.defineProperty(process, "platform", { value: platform, configurable: true });
  Object.defineProperty(process, "arch", { value: arch, configurable: true });
  try {
    return fn();
  } finally {
    Object.defineProperty(process, "platform", platDesc);
    Object.defineProperty(process, "arch", archDesc);
  }
}

const knownPlatforms = [
  ["darwin", "arm64", "llmshim-darwin-arm64"],
  ["darwin", "x64", "llmshim-darwin-x64"],
  ["linux", "x64", "llmshim-linux-x64"],
  ["linux", "arm64", "llmshim-linux-arm64"],
  ["win32", "x64", "@sanjay920/llmshim-win32-x64"],
];

for (const [platform, arch, expected] of knownPlatforms) {
  test(`platformPackageName() maps ${platform}/${arch} -> ${expected}`, () => {
    withPlatform(platform, arch, () => {
      assert.equal(platformPackageName(), expected);
    });
  });
}

test("platformPackageName() throws a clear error for an unsupported platform", () => {
  withPlatform("sunos", "x64", () => {
    assert.throws(() => platformPackageName(), /No prebuilt llmshim binary is published for sunos-x64/);
  });
});

test("platformPackageName() throws for an unsupported arch on a supported OS", () => {
  withPlatform("linux", "ia32", () => {
    assert.throws(() => platformPackageName(), /linux-ia32/);
  });
});

test("managed readiness requires an exact versioned loopback schema", () => {
  const parsed = __testing.parseReadiness(readiness());
  assert.equal(parsed.baseUrl, "https://127.0.0.1:41321");
  assert.equal(parsed.port, 41321);
  assert.equal(parsed.authToken, "a".repeat(64));
  assert.throws(
    () => __testing.parseReadiness(readiness({ protocol: "llmshim-managed-v2" })),
    /incompatible/,
  );
  assert.throws(
    () => __testing.parseReadiness(readiness({ base_url: "https://localhost:41321" })),
    /loopback HTTPS/,
  );
  const withExtra = JSON.parse(readiness());
  withExtra.extra = true;
  assert.throws(() => __testing.parseReadiness(Buffer.from(JSON.stringify(withExtra) + "\n")), /unsupported/);
  assert.throws(() => __testing.parseReadiness(Buffer.from("not-json\n")), /malformed/);
});

test("managed readiness pipe rejects oversized records and old-binary EOF", async () => {
  const oversizedStdout = new PassThrough();
  const oversizedChild = new EventEmitter();
  oversizedChild.stdout = oversizedStdout;
  const oversized = __testing.readReadiness(oversizedChild, 1_000);
  oversizedStdout.write(Buffer.alloc(16 * 1024 + 1, 0x78));
  await assert.rejects(oversized, /oversized/);

  const oldStdout = new PassThrough();
  const oldChild = new EventEmitter();
  oldChild.stdout = oldStdout;
  const oldBinary = __testing.readReadiness(oldChild, 1_000);
  oldStdout.end();
  await assert.rejects(oldBinary, /upgrade/);
});

test("relay installs bounded connection and header limits", async () => {
  const server = createHttpServer(
    { connectionsCheckingInterval: 10 },
    (_request, response) => response.end("ok"),
  );
  __testing.configureRelayLimits(server, {
    maxConnections: 1,
    headersTimeoutMs: 40,
    requestTimeoutMs: 100,
    keepAliveTimeoutMs: 20,
  });
  assert.equal(server.maxConnections, 1);
  assert.equal(server.maxHeadersCount, 100);
  assert.equal(server.headersTimeout, 40);
  assert.equal(server.requestTimeout, 100);
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const address = server.address();
  assert.ok(address && typeof address === "object");
  const firstSocket = createConnection({ host: "127.0.0.1", port: address.port });
  await new Promise((resolve, reject) => {
    firstSocket.once("connect", resolve);
    firstSocket.once("error", reject);
  });
  const secondSocket = createConnection({ host: "127.0.0.1", port: address.port });
  await new Promise((resolve, reject) => {
    const timeout = setTimeout(() => reject(new Error("excess connection was not dropped")), 500);
    secondSocket.once("close", () => {
      clearTimeout(timeout);
      resolve();
    });
    secondSocket.once("error", () => {});
  });
  firstSocket.destroy();
  await new Promise((resolve) => server.close(resolve));
});

test("replacement listener receives no prompt or managed token before TLS rejection", async () => {
  const received = [];
  const replacement = createServer((socket) => {
    socket.once("data", (chunk) => {
      received.push(chunk);
      socket.destroy();
    });
  });
  await new Promise((resolve, reject) => {
    replacement.once("error", reject);
    replacement.listen(0, "127.0.0.1", resolve);
  });
  const address = replacement.address();
  assert.ok(address && typeof address === "object");
  const secretPrompt = "prompt-that-must-not-reach-replacement";
  const secretToken = "b".repeat(64);
  const request = __testing.pinnedRequest(
    { port: address.port, authToken: secretToken, certificatePem: testCertificate },
    "POST",
    "/v1/chat",
  );
  const failed = new Promise((resolve) => request.once("error", resolve));
  request.end(secretPrompt);
  await failed;
  await new Promise((resolve) => replacement.close(resolve));
  const wire = Buffer.concat(received);
  assert.ok(wire.length > 0);
  assert.equal(wire.includes(Buffer.from(secretPrompt)), false);
  assert.equal(wire.includes(Buffer.from(secretToken)), false);
});

test(
  "SIGTERM reaps the exact live managed child and preserves default termination",
  { skip: !process.env.LLMSHIM_TEST_BINARY || process.platform === "win32" },
  async () => {
    const binary = process.env.LLMSHIM_TEST_BINARY;
    const fixture = spawn(process.execPath, [fileURLToPath(new URL("../scripts/managed-parent-fixture.mjs", import.meta.url))], {
      cwd: dirname(fileURLToPath(import.meta.url)),
      env: {
        ...process.env,
        PATH: `${dirname(binary)}${delimiter}${process.env.PATH ?? ""}`,
        VLLM_BASE_URL: "http://127.0.0.1:9/v1",
      },
      stdio: ["ignore", "pipe", "pipe"],
    });
    const ready = await readJsonLine(fixture.stdout);
    assert.equal(ready.ready, true);
    try {
      fixture.kill("SIGTERM");
      const exited = await waitForExit(fixture);
      assert.equal(exited.code, null);
      assert.equal(exited.signal, "SIGTERM");
      await waitForProcessGone(ready.childPid);
    } finally {
      if (processExists(ready.childPid)) process.kill(ready.childPid, "SIGKILL");
      if (fixture.exitCode === null && fixture.signalCode === null) fixture.kill("SIGKILL");
    }
  },
);

test(
  "a host-owned SIGTERM can keep the parent, relay, and managed child alive",
  { skip: !process.env.LLMSHIM_TEST_BINARY || process.platform === "win32" },
  async () => {
    const binary = process.env.LLMSHIM_TEST_BINARY;
    const fixture = spawn(process.execPath, [fileURLToPath(new URL("../scripts/managed-parent-fixture.mjs", import.meta.url))], {
      cwd: dirname(fileURLToPath(import.meta.url)),
      env: {
        ...process.env,
        PATH: `${dirname(binary)}${delimiter}${process.env.PATH ?? ""}`,
        VLLM_BASE_URL: "http://127.0.0.1:9/v1",
        LLMSHIM_FIXTURE_HOST_SIGNAL: "1",
      },
      stdio: ["ignore", "pipe", "pipe"],
    });
    const ready = await readJsonLine(fixture.stdout);
    const hostSignalLine = readJsonLine(fixture.stdout);
    try {
      fixture.kill("SIGTERM");
      assert.deepEqual(await hostSignalLine, { hostSignalCount: 1 });
      assert.equal(fixture.exitCode, null);
      assert.equal(processExists(ready.childPid), true);
      fixture.kill("SIGKILL");
      const exited = await waitForExit(fixture);
      assert.equal(exited.signal, "SIGKILL");
      await waitForProcessGone(ready.childPid);
    } finally {
      if (processExists(ready.childPid)) process.kill(ready.childPid, "SIGKILL");
      if (fixture.exitCode === null && fixture.signalCode === null) fixture.kill("SIGKILL");
    }
  },
);

test(
  "parent SIGKILL closes an active provider stream within the child shutdown bound",
  { skip: !process.env.LLMSHIM_TEST_BINARY || process.platform === "win32" },
  async () => {
    let providerClosedResolve;
    const providerClosed = new Promise((resolve) => {
      providerClosedResolve = resolve;
    });
    const provider = createHttpServer((request, response) => {
      request.resume();
      request.once("end", () => {
        response.writeHead(200, { "content-type": "text/event-stream" });
        response.write(
          `data: ${JSON.stringify({
            id: "chatcmpl-parent-liveness",
            object: "chat.completion.chunk",
            model: "test",
            choices: [{ index: 0, delta: { content: "first" }, finish_reason: null }],
          })}\n\n`,
        );
        const interval = setInterval(() => response.write(": keepalive\n\n"), 25);
        response.once("close", () => {
          clearInterval(interval);
          providerClosedResolve();
        });
      });
    });
    await new Promise((resolve, reject) => {
      provider.once("error", reject);
      provider.listen(0, "127.0.0.1", resolve);
    });
    const providerAddress = provider.address();
    assert.ok(providerAddress && typeof providerAddress === "object");
    const binary = process.env.LLMSHIM_TEST_BINARY;
    const fixture = spawn(process.execPath, [fileURLToPath(new URL("../scripts/managed-parent-fixture.mjs", import.meta.url))], {
      cwd: dirname(fileURLToPath(import.meta.url)),
      env: {
        ...process.env,
        PATH: `${dirname(binary)}${delimiter}${process.env.PATH ?? ""}`,
        VLLM_BASE_URL: `http://127.0.0.1:${providerAddress.port}/v1`,
        LLMSHIM_FIXTURE_ACTIVE_STREAM: "1",
      },
      stdio: ["ignore", "pipe", "pipe"],
    });
    let ready;
    try {
      ready = await readJsonLine(fixture.stdout);
      fixture.kill("SIGKILL");
      assert.equal((await waitForExit(fixture)).signal, "SIGKILL");
      await Promise.race([
        providerClosed,
        new Promise((_, reject) => setTimeout(() => reject(new Error("active provider stream stayed open")), 3_000)),
      ]);
      await waitForProcessGone(ready.childPid, 3_000);
    } finally {
      if (ready?.childPid && processExists(ready.childPid)) process.kill(ready.childPid, "SIGKILL");
      if (fixture.exitCode === null && fixture.signalCode === null) fixture.kill("SIGKILL");
      await new Promise((resolve) => provider.close(resolve));
    }
  },
);

test(
  "SIGTERM reaps a managed child that is still starting",
  { skip: process.platform === "win32" },
  async () => {
    const temporaryDirectory = await mkdtemp(join(tmpdir(), "llmshim-managed-startup-"));
    const fakeBinary = join(temporaryDirectory, "llmshim");
    const pidFile = join(temporaryDirectory, "child.pid");
    await writeFile(
      fakeBinary,
      "#!/usr/bin/env node\nrequire('node:fs').writeFileSync(process.env.LLMSHIM_FAKE_PID_FILE, String(process.pid));\nprocess.stdin.resume();\nprocess.stdin.on('end', () => process.exit(0));\nsetInterval(() => {}, 1000);\n",
    );
    await chmod(fakeBinary, 0o755);
    const fixture = spawn(process.execPath, [fileURLToPath(new URL("../scripts/managed-parent-fixture.mjs", import.meta.url))], {
      cwd: dirname(fileURLToPath(import.meta.url)),
      env: {
        ...process.env,
        PATH: `${temporaryDirectory}${delimiter}${process.env.PATH ?? ""}`,
        LLMSHIM_FAKE_PID_FILE: pidFile,
      },
      stdio: ["ignore", "pipe", "pipe"],
    });
    let childPid;
    try {
      childPid = await readPidFile(pidFile);
      fixture.kill("SIGTERM");
      const exited = await waitForExit(fixture);
      assert.equal(exited.signal, "SIGTERM");
      await waitForProcessGone(childPid);
    } finally {
      if (childPid && processExists(childPid)) process.kill(childPid, "SIGKILL");
      if (fixture.exitCode === null && fixture.signalCode === null) fixture.kill("SIGKILL");
      await rm(temporaryDirectory, { recursive: true, force: true });
    }
  },
);

test(
  "real managed lifecycle uses child-selected ports, guarded auth, and fresh children",
  { skip: !process.env.LLMSHIM_TEST_BINARY },
  async () => {
    const binary = process.env.LLMSHIM_TEST_BINARY;
    process.env.PATH = `${dirname(binary)}${delimiter}${process.env.PATH ?? ""}`;
    let providerRequests = 0;
    const provider = createHttpServer((request, response) => {
      request.resume();
      request.once("end", () => {
        providerRequests += 1;
        __testing.managedChildSnapshot()?.process.kill();
        response.destroy();
      });
    });
    await new Promise((resolve, reject) => {
      provider.once("error", reject);
      provider.listen(0, "127.0.0.1", resolve);
    });
    const providerAddress = provider.address();
    assert.ok(providerAddress && typeof providerAddress === "object");
    process.env.VLLM_BASE_URL = `http://127.0.0.1:${providerAddress.port}/v1`;

    const unrelated = createServer();
    await new Promise((resolve, reject) => {
      unrelated.once("error", reject);
      unrelated.listen(0, "127.0.0.1", resolve);
    });
    const unrelatedAddress = unrelated.address();
    assert.ok(unrelatedAddress && typeof unrelatedAddress === "object");
    process.env.LLMSHIM_PORT = String(unrelatedAddress.port);

    const relayUrl = await ensureServer();
    assert.match(relayUrl, /^http:\/\/127\.0\.0\.1:\d+\/__llmshim_managed\/[0-9a-f]{64}$/);
    const firstChild = __testing.managedChildSnapshot();
    assert.ok(firstChild);
    assert.notEqual(firstChild.port, unrelatedAddress.port);
    const unauthenticatedStatus = await new Promise((resolve, reject) => {
      const request = rawHttpsRequest({
        hostname: "127.0.0.1",
        port: firstChild.port,
        path: "/health",
        ca: firstChild.certificatePem,
        rejectUnauthorized: true,
        agent: false,
      });
      request.once("error", reject);
      request.once("response", (response) => {
        response.resume();
        resolve(response.statusCode);
      });
      request.end();
    });
    assert.equal(unauthenticatedStatus, 401);
    await new Promise((resolve) => unrelated.close(resolve));

    const stopped = __testing.stopManagedChild();
    assert.equal(stopped, firstChild);
    if (firstChild.process.exitCode === null) {
      await new Promise((resolve) => firstChild.process.once("exit", resolve));
    }

    const replacementBytes = [];
    let replacementConnections = 0;
    const replacement = createServer((socket) => {
      replacementConnections += 1;
      socket.once("data", (chunk) => {
        replacementBytes.push(chunk);
        socket.destroy();
      });
    });
    await new Promise((resolve, reject) => {
      replacement.once("error", reject);
      replacement.listen(firstChild.port, "127.0.0.1", resolve);
    });

    const prompt = "post-exit-prompt-that-must-remain-private";
    const staleRequest = __testing.pinnedRequest(
      firstChild,
      "POST",
      "/v1/chat",
      { "X-LLMSHIM-MANAGED-TOKEN": "caller-controlled" },
    );
    const staleFailure = new Promise((resolve) => staleRequest.once("error", resolve));
    staleRequest.end(prompt);
    await staleFailure;

    const health = await new Client({
      headers: { "X-LLMSHIM-MANAGED-TOKEN": "caller-controlled" },
    }).health();
    assert.equal(health.status, "ok");
    const secondChild = __testing.managedChildSnapshot();
    assert.ok(secondChild);
    assert.notEqual(secondChild.process, firstChild.process);
    assert.notEqual(secondChild.port, firstChild.port);

    const client = new Client();
    await assert.rejects(
      () => client.chat({ model: "vllm/test", messages: [{ role: "user", content: "bill-once" }] }),
      /managed proxy request failed/,
    );
    assert.equal(providerRequests, 1);
    const recoveredHealth = await client.health();
    assert.equal(recoveredHealth.status, "ok");
    const thirdChild = __testing.managedChildSnapshot();
    assert.ok(thirdChild);
    assert.notEqual(thirdChild.process, secondChild.process);

    await new Promise((resolve) => replacement.close(resolve));
    await new Promise((resolve) => provider.close(resolve));
    const wire = Buffer.concat(replacementBytes);
    assert.equal(replacementConnections, 1);
    assert.equal(wire.includes(Buffer.from(prompt)), false);
    assert.equal(wire.includes(Buffer.from(firstChild.authToken)), false);
  },
);

test(
  "early managed stream return closes the provider connection and preserves later requests",
  { skip: !process.env.LLMSHIM_TEST_BINARY },
  async () => {
    let finishNormally = false;
    let cancelledResolve;
    const providerCancelled = new Promise((resolve) => {
      cancelledResolve = resolve;
    });
    let cancelledConnections = 0;
    const provider = createHttpServer((request, response) => {
      request.resume();
      request.once("end", () => {
        response.writeHead(200, { "content-type": "text/event-stream" });
        const firstChunk = JSON.stringify({
          id: "chatcmpl-managed-stream",
          object: "chat.completion.chunk",
          model: "test",
          choices: [{ index: 0, delta: { content: "first" }, finish_reason: null }],
        });
        response.write(`data: ${firstChunk}\n\n`);
        if (finishNormally) {
          const terminalChunk = JSON.stringify({
            id: "chatcmpl-managed-stream",
            object: "chat.completion.chunk",
            model: "test",
            choices: [{ index: 0, delta: {}, finish_reason: "stop" }],
          });
          response.end(`data: ${terminalChunk}\n\ndata: [DONE]\n\n`);
          return;
        }
        const interval = setInterval(() => response.write(": keepalive\n\n"), 25);
        response.once("close", () => {
          clearInterval(interval);
          if (!response.writableEnded) {
            cancelledConnections += 1;
            cancelledResolve();
          }
        });
      });
    });
    await new Promise((resolve, reject) => {
      provider.once("error", reject);
      provider.listen(0, "127.0.0.1", resolve);
    });
    const providerAddress = provider.address();
    assert.ok(providerAddress && typeof providerAddress === "object");
    process.env.VLLM_BASE_URL = `http://127.0.0.1:${providerAddress.port}/v1`;
    const previousChild = __testing.stopManagedChild();
    if (previousChild?.process.exitCode === null) {
      await new Promise((resolve) => previousChild.process.once("exit", resolve));
    }

    try {
      const client = new Client();
      const stream = client.stream({ model: "vllm/test", messages: [{ role: "user", content: "stream" }] });
      const first = await stream.next();
      assert.equal(first.done, false);
      assert.equal(first.value.type, "content");
      assert.equal(first.value.text, "first");
      await stream.return();
      await Promise.race([
        providerCancelled,
        new Promise((_, reject) => setTimeout(() => reject(new Error("provider stream stayed open")), 1_000)),
      ]);
      assert.equal(cancelledConnections, 1);

      const childAfterCancellation = __testing.managedChildSnapshot();
      assert.ok(childAfterCancellation);
      assert.equal((await client.health()).status, "ok");
      assert.equal(__testing.managedChildSnapshot()?.process, childAfterCancellation.process);

      finishNormally = true;
      const completedEvents = [];
      for await (const event of client.stream({
        model: "vllm/test",
        messages: [{ role: "user", content: "complete" }],
      })) {
        completedEvents.push(event.type);
      }
      assert.ok(completedEvents.includes("content"));
      assert.ok(completedEvents.includes("done"));
      assert.equal(cancelledConnections, 1);
    } finally {
      const child = __testing.stopManagedChild();
      if (child?.process.exitCode === null) {
        await new Promise((resolve) => child.process.once("exit", resolve));
      }
      await new Promise((resolve) => provider.close(resolve));
    }
  },
);
