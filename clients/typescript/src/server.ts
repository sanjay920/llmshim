/** Secure auto-managed llmshim proxy lifecycle. */

import { spawn, type ChildProcess } from "node:child_process";
import { randomBytes } from "node:crypto";
import { accessSync, constants as fsConstants } from "node:fs";
import {
  createServer,
  type IncomingHttpHeaders,
  type IncomingMessage,
  type Server as HttpServer,
} from "node:http";
import { request as httpsRequest } from "node:https";
import { createRequire } from "node:module";
import { delimiter, join } from "node:path";

const require = createRequire(import.meta.url);
const MANAGED_PROTOCOL = "llmshim-managed-v1";
const MANAGED_AUTH_HEADER = "x-llmshim-managed-token";
const MAX_READINESS_BYTES = 16 * 1024;
const MAX_STDERR_BYTES = 32 * 1024;
const READINESS_FIELDS = ["auth_token", "base_url", "certificate_pem", "protocol"];
const HOP_BY_HOP_HEADERS = new Set([
  "connection",
  "keep-alive",
  "proxy-authenticate",
  "proxy-authorization",
  "te",
  "trailer",
  "transfer-encoding",
  "upgrade",
]);

interface RelayLimits {
  maxConnections: number;
  headersTimeoutMs: number;
  requestTimeoutMs: number;
  keepAliveTimeoutMs: number;
}

const DEFAULT_RELAY_LIMITS: RelayLimits = {
  maxConnections: 32,
  headersTimeoutMs: 2_000,
  requestTimeoutMs: 120_000,
  keepAliveTimeoutMs: 1_000,
};

interface ManagedChild {
  process: ChildProcess;
  baseUrl: string;
  port: number;
  authToken: string;
  certificatePem: string;
}

interface ReadinessRecord {
  protocol: string;
  base_url: string;
  auth_token: string;
  certificate_pem: string;
}

let managedChild: ManagedChild | null = null;
let startingChild: Promise<ManagedChild> | null = null;
let startingChildProcess: ChildProcess | null = null;
let relayServer: HttpServer | null = null;
let relayBaseUrl: string | null = null;
let startingRelay: Promise<string> | null = null;
let cleanupRegistered = false;
let managedStartupDelayForTestsMs = 0;
let pinnedRequestObserverForTests: (() => void) | null = null;

/** Maps Node's platform and architecture to the optional binary package. */
export function platformPackageName(): string {
  const key = `${process.platform}-${process.arch}`;
  const known: Record<string, string> = {
    "darwin-arm64": "llmshim-darwin-arm64",
    "darwin-x64": "llmshim-darwin-x64",
    "linux-x64": "llmshim-linux-x64",
    "linux-arm64": "llmshim-linux-arm64",
    "win32-x64": "@sanjay920/llmshim-win32-x64",
  };
  const packageName = known[key];
  if (!packageName) {
    throw new Error(`No prebuilt llmshim binary is published for ${key}.`);
  }
  return packageName;
}

function findBundledBinary(): string | null {
  let packageName: string;
  try {
    packageName = platformPackageName();
  } catch {
    return null;
  }
  try {
    const packageJsonPath = require.resolve(`${packageName}/package.json`);
    const binaryName = process.platform === "win32" ? "llmshim.exe" : "llmshim";
    const binaryPath = join(packageJsonPath, "..", "bin", binaryName);
    accessSync(binaryPath, fsConstants.X_OK);
    return binaryPath;
  } catch {
    return null;
  }
}

function findOnPath(): string | null {
  const pathEnvironment = process.env.PATH ?? process.env.Path ?? "";
  const binaryNames = process.platform === "win32" ? ["llmshim.exe", "llmshim.cmd"] : ["llmshim"];
  for (const directory of pathEnvironment.split(delimiter)) {
    if (!directory) continue;
    for (const binaryName of binaryNames) {
      const candidate = join(directory, binaryName);
      try {
        accessSync(candidate, fsConstants.X_OK);
        return candidate;
      } catch {
        // Continue searching.
      }
    }
  }
  return null;
}

function findBinary(): string {
  const binary = findBundledBinary() ?? findOnPath();
  if (binary) return binary;
  throw new Error(
    "llmshim binary not found. Install one of:\n" +
      "  npm install llmshim                     (includes a prebuilt binary for your platform)\n" +
      "  cargo install llmshim                   (from crates.io, puts it on PATH)\n" +
      "  cargo build --release --features proxy  (from source)\n" +
      "Or pass an explicit `baseUrl` to connect to a proxy you're already running.",
  );
}

function readReadiness(child: ChildProcess, timeoutMs = 10_000): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    const stdout = child.stdout;
    if (!stdout) {
      reject(new Error("managed proxy readiness pipe was not created"));
      return;
    }
    let buffer = Buffer.alloc(0);
    const timer = setTimeout(() => finish(new Error("timed out waiting for managed proxy readiness")), timeoutMs);
    timer.unref();

    const finish = (error?: Error, line?: Buffer) => {
      clearTimeout(timer);
      stdout.off("data", onData);
      stdout.off("end", onEnd);
      stdout.off("error", onError);
      child.off("error", onChildError);
      if (error) reject(error);
      else resolve(line!);
    };
    const onData = (chunk: Buffer) => {
      buffer = Buffer.concat([buffer, chunk]);
      if (buffer.length > MAX_READINESS_BYTES) {
        finish(new Error("managed proxy readiness record is oversized"));
        return;
      }
      const newline = buffer.indexOf(0x0a);
      if (newline !== -1) {
        if (newline !== buffer.length - 1) {
          finish(new Error("managed proxy wrote unexpected stdout after readiness"));
          return;
        }
        finish(undefined, buffer);
      }
    };
    const onEnd = () =>
      finish(new Error("llmshim binary does not support secure managed startup; upgrade llmshim"));
    const onError = (error: Error) => finish(new Error(`could not read managed proxy readiness: ${error.message}`));
    const onChildError = (error: Error) => finish(new Error(`could not start managed proxy: ${error.message}`));
    stdout.on("data", onData);
    stdout.once("end", onEnd);
    stdout.once("error", onError);
    child.once("error", onChildError);
  });
}

function parseReadiness(line: Buffer): Omit<ManagedChild, "process"> {
  let unknownRecord: unknown;
  try {
    unknownRecord = JSON.parse(line.toString("utf8"));
  } catch {
    throw new Error("managed proxy returned malformed readiness JSON");
  }
  if (!unknownRecord || typeof unknownRecord !== "object" || Array.isArray(unknownRecord)) {
    throw new Error("managed proxy returned an unsupported readiness record");
  }
  const record = unknownRecord as Partial<ReadinessRecord> & Record<string, unknown>;
  if (Object.keys(record).sort().join("\0") !== READINESS_FIELDS.join("\0")) {
    throw new Error("managed proxy returned an unsupported readiness record");
  }
  if (record.protocol !== MANAGED_PROTOCOL) {
    throw new Error("llmshim binary uses an incompatible managed startup protocol; upgrade llmshim");
  }
  if (
    typeof record.base_url !== "string" ||
    typeof record.auth_token !== "string" ||
    typeof record.certificate_pem !== "string"
  ) {
    throw new Error("managed proxy readiness fields have invalid types");
  }
  let parsed: URL;
  try {
    parsed = new URL(record.base_url);
  } catch {
    throw new Error("managed proxy returned an invalid URL");
  }
  const port = Number(parsed.port);
  if (
    parsed.protocol !== "https:" ||
    parsed.hostname !== "127.0.0.1" ||
    parsed.username ||
    parsed.password ||
    (parsed.pathname !== "/" && parsed.pathname !== "") ||
    parsed.search ||
    parsed.hash ||
    !Number.isInteger(port) ||
    port < 1 ||
    port > 65535
  ) {
    throw new Error("managed proxy readiness URL must be loopback HTTPS");
  }
  if (!/^[0-9a-f]{64}$/.test(record.auth_token)) {
    throw new Error("managed proxy returned an invalid authentication token");
  }
  if (
    !record.certificate_pem.startsWith("-----BEGIN CERTIFICATE-----\n") ||
    !record.certificate_pem.trimEnd().endsWith("-----END CERTIFICATE-----") ||
    Buffer.byteLength(record.certificate_pem) > 8192
  ) {
    throw new Error("managed proxy returned an invalid TLS certificate");
  }
  return {
    baseUrl: record.base_url.replace(/\/+$/, ""),
    port,
    authToken: record.auth_token,
    certificatePem: record.certificate_pem,
  };
}

function pinnedRequest(
  child: ManagedChild,
  method: string,
  path: string,
  headers: IncomingHttpHeaders = {},
) {
  pinnedRequestObserverForTests?.();
  const forwardedHeaders: IncomingHttpHeaders = {};
  for (const [name, value] of Object.entries(headers)) {
    const lowerName = name.toLowerCase();
    if (lowerName !== MANAGED_AUTH_HEADER && lowerName !== "host" && !HOP_BY_HOP_HEADERS.has(lowerName)) {
      forwardedHeaders[lowerName] = value;
    }
  }
  forwardedHeaders[MANAGED_AUTH_HEADER] = child.authToken;
  return httpsRequest({
    protocol: "https:",
    hostname: "127.0.0.1",
    port: child.port,
    method,
    path,
    headers: forwardedHeaders,
    ca: child.certificatePem,
    rejectUnauthorized: true,
    agent: false,
  });
}

function authenticatedHealth(child: ManagedChild): Promise<void> {
  return new Promise((resolve, reject) => {
    const request = pinnedRequest(child, "GET", "/health");
    request.setTimeout(500, () => request.destroy(new Error("health check timed out")));
    request.once("error", reject);
    request.once("response", (response) => {
      response.resume();
      if (response.statusCode === 200) resolve();
      else reject(new Error(`authenticated health check returned HTTP ${response.statusCode}`));
    });
    request.end();
  });
}

async function waitForAuthenticatedHealth(child: ManagedChild, timeoutMs = 10_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  let lastError: unknown;
  while (Date.now() < deadline && child.process.exitCode === null && child.process.signalCode === null) {
    try {
      await authenticatedHealth(child);
      return;
    } catch (error) {
      lastError = error;
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
  }
  const detail = lastError instanceof Error ? `: ${lastError.message}` : "";
  throw new Error(`managed proxy did not become ready${detail}`);
}

function stopChild(child: ManagedChild): void {
  try {
    child.process.kill();
  } catch {
    // The exact child already exited.
  }
}

function startManagedChild(): Promise<ManagedChild> {
  const binary = findBinary();
  const childProcess = spawn(binary, ["proxy", "--managed"], {
    env: { ...process.env },
    stdio: ["pipe", "pipe", "pipe"],
  });
  startingChildProcess = childProcess;
  let stderr = "";
  childProcess.stderr?.on("data", (chunk: Buffer) => {
    stderr = (stderr + chunk.toString()).slice(-MAX_STDERR_BYTES);
  });

  return (async () => {
    try {
      const material = parseReadiness(await readReadiness(childProcess));
      childProcess.stdout?.destroy();
      const child: ManagedChild = { process: childProcess, ...material };
      await waitForAuthenticatedHealth(child);
      if (managedStartupDelayForTestsMs > 0) {
        await new Promise((resolve) => setTimeout(resolve, managedStartupDelayForTestsMs));
      }
      childProcess.once("exit", () => {
        if (managedChild?.process === childProcess) managedChild = null;
      });
      childProcess.unref();
      (childProcess.stdin as NodeJS.WritableStream & { unref?: () => void } | null)?.unref?.();
      (childProcess.stderr as NodeJS.ReadableStream & { unref?: () => void } | null)?.unref?.();
      return child;
    } catch (error) {
      try {
        childProcess.kill();
      } catch {
        // The child already exited.
      }
      if (startingChildProcess === childProcess) startingChildProcess = null;
      if (stderr.includes("No providers configured")) {
        throw new Error(
          "No API keys configured. Set them via environment variables " +
            "(OPENAI_API_KEY, ANTHROPIC_API_KEY, GEMINI_API_KEY, XAI_API_KEY, " +
            "OPENROUTER_API_KEY, or VLLM_BASE_URL / SGLANG_BASE_URL for self-hosted) " +
            "or `llmshim configure`.",
        );
      }
      const message = error instanceof Error ? error.message : String(error);
      throw new Error(stderr ? `${message}\nBinary: ${binary}\nstderr: ${stderr}` : message);
    }
  })();
}

async function ensureManagedChild(): Promise<ManagedChild> {
  if (
    managedChild &&
    managedChild.process.exitCode === null &&
    managedChild.process.signalCode === null &&
    !managedChild.process.killed
  ) {
    return managedChild;
  }
  if (startingChild) return startingChild;
  const thisStart = startManagedChild();
  startingChild = thisStart;
  try {
    const child = await thisStart;
    managedChild = child;
    if (startingChildProcess === child.process) startingChildProcess = null;
    return child;
  } finally {
    if (startingChild === thisStart) startingChild = null;
  }
}

function relayHeaders(headers: IncomingHttpHeaders): IncomingHttpHeaders {
  const result: IncomingHttpHeaders = {};
  for (const [name, value] of Object.entries(headers)) {
    const lowerName = name.toLowerCase();
    if (lowerName !== "host" && !HOP_BY_HOP_HEADERS.has(lowerName)) result[lowerName] = value;
  }
  return result;
}

function configureRelayLimits(server: HttpServer, limits: RelayLimits): void {
  server.maxConnections = limits.maxConnections;
  server.maxHeadersCount = 100;
  server.headersTimeout = limits.headersTimeoutMs;
  server.requestTimeout = limits.requestTimeoutMs;
  server.keepAliveTimeout = limits.keepAliveTimeoutMs;
}

function startRelay(): Promise<string> {
  const capability = randomBytes(32).toString("hex");
  const prefix = `/__llmshim_managed/${capability}`;
  const server = createServer({ connectionsCheckingInterval: 500 }, async (incoming, outgoing) => {
    let parsed: URL;
    try {
      parsed = new URL(incoming.url ?? "/", "http://127.0.0.1");
    } catch {
      outgoing.writeHead(400).end();
      return;
    }
    if (!parsed.pathname.startsWith(prefix + "/")) {
      outgoing.writeHead(404).end();
      return;
    }
    const targetPath = parsed.pathname.slice(prefix.length) + parsed.search;
    let downstreamCancelled = incoming.aborted || incoming.destroyed || outgoing.destroyed;
    let relayFinished = false;
    let upstream: ReturnType<typeof pinnedRequest> | null = null;
    let upstreamResponse: IncomingMessage | null = null;
    const cancelRelayRequest = () => {
      if (relayFinished || downstreamCancelled) return;
      downstreamCancelled = true;
      upstreamResponse?.destroy();
      upstream?.destroy();
    };
    const requestClosedBeforeForward = () =>
      downstreamCancelled || incoming.aborted || incoming.destroyed || outgoing.destroyed;
    const requestCancelledAfterForward = () => downstreamCancelled || outgoing.destroyed;
    outgoing.once("close", cancelRelayRequest);
    outgoing.once("finish", () => {
      relayFinished = true;
    });
    incoming.once("aborted", cancelRelayRequest);

    let child: ManagedChild;
    try {
      child = await ensureManagedChild();
    } catch (error) {
      if (requestClosedBeforeForward()) return;
      const message = error instanceof Error ? error.message : String(error);
      const payload = JSON.stringify({ error: { code: "managed_proxy_unavailable", message } });
      outgoing.writeHead(502, { "content-type": "application/json", "content-length": Buffer.byteLength(payload) });
      outgoing.end(payload);
      return;
    }
    if (requestClosedBeforeForward()) return;

    upstream = pinnedRequest(child, incoming.method ?? "GET", targetPath, relayHeaders(incoming.headers));
    let responseStarted = false;
    upstream.once("response", (response) => {
      if (requestCancelledAfterForward()) {
        response.destroy();
        upstream?.destroy();
        return;
      }
      responseStarted = true;
      upstreamResponse = response;
      response.once("error", (error) => {
        if (!requestCancelledAfterForward()) outgoing.destroy(error);
      });
      const headers: IncomingHttpHeaders = {};
      for (const [name, value] of Object.entries(response.headers)) {
        if (!HOP_BY_HOP_HEADERS.has(name.toLowerCase())) headers[name] = value;
      }
      outgoing.writeHead(response.statusCode ?? 502, headers);
      response.pipe(outgoing);
    });
    upstream.once("error", (error) => {
      if (requestCancelledAfterForward()) return;
      if (responseStarted || outgoing.headersSent) {
        outgoing.destroy(error);
        return;
      }
      const payload = JSON.stringify({
        error: { code: "managed_proxy_unavailable", message: "managed proxy request failed" },
      });
      outgoing.writeHead(502, { "content-type": "application/json", "content-length": Buffer.byteLength(payload) });
      outgoing.end(payload);
    });
    if (requestCancelledAfterForward()) {
      cancelRelayRequest();
      return;
    }
    incoming.pipe(upstream);
  });
  configureRelayLimits(server, DEFAULT_RELAY_LIMITS);

  return new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      server.off("error", reject);
      const address = server.address();
      if (!address || typeof address === "string") {
        server.close();
        reject(new Error("could not determine managed relay port"));
        return;
      }
      relayServer = server;
      relayBaseUrl = `http://127.0.0.1:${address.port}${prefix}`;
      server.unref();
      resolve(relayBaseUrl);
    });
  });
}

async function ensureRelay(): Promise<string> {
  if (relayServer?.listening && relayBaseUrl) return relayBaseUrl;
  if (startingRelay) return startingRelay;
  const thisStart = startRelay();
  startingRelay = thisStart;
  try {
    return await thisStart;
  } finally {
    if (startingRelay === thisStart) startingRelay = null;
  }
}

function stopAll(): void {
  const child = managedChild;
  const startingProcess = startingChildProcess;
  managedChild = null;
  startingChildProcess = null;
  if (child) stopChild(child);
  if (startingProcess && startingProcess !== child?.process) {
    try {
      startingProcess.kill();
    } catch {
      // The exact startup child already exited.
    }
  }
  relayServer?.close();
  relayServer = null;
  relayBaseUrl = null;
}

function registerCleanup(): void {
  if (cleanupRegistered) return;
  process.once("exit", stopAll);
  cleanupRegistered = true;
}

/**
 * Ensure the managed proxy is running and return a process-owned relay URL.
 * The returned URL remains usable until this Node process exits.
 */
export async function ensureServer(): Promise<string> {
  registerCleanup();
  const relay = await ensureRelay();
  await ensureManagedChild();
  return relay;
}

export const __testing = {
  parseReadiness,
  pinnedRequest,
  readReadiness,
  configureRelayLimits,
  setManagedStartupDelay: (delayMs: number) => {
    managedStartupDelayForTestsMs = delayMs;
  },
  setPinnedRequestObserver: (observer: (() => void) | null) => {
    pinnedRequestObserverForTests = observer;
  },
  managedChildSnapshot: () => managedChild,
  stopManagedChild: () => {
    const child = managedChild;
    if (child) stopChild(child);
    return child;
  },
};
