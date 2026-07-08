/**
 * Auto-managed llmshim proxy server.
 *
 * Mirrors the Python client's `_server.py`: finds the platform-specific binary
 * bundled via an `optionalDependencies` package (e.g. `llmshim-darwin-arm64`),
 * starts it on a free port, waits for it to be ready, and stops it on process
 * exit. The server is a singleton shared by every {@link Client} in the same
 * process that didn't specify an explicit `baseUrl`.
 */

import { spawn, type ChildProcess } from "node:child_process";
import { accessSync, constants as fsConstants } from "node:fs";
import { createRequire } from "node:module";
import { createServer, createConnection } from "node:net";
import { delimiter, join } from "node:path";

const require = createRequire(import.meta.url);

let serverProcess: ChildProcess | null = null;
let serverPort: number | null = null;
let starting: Promise<string> | null = null;

/** Maps Node's `process.platform`/`process.arch` to the optional-dependency package name. */
export function platformPackageName(): string {
  const plat = process.platform;
  const arch = process.arch;
  const key = `${plat}-${arch}`;
  const known: Record<string, string> = {
    "darwin-arm64": "llmshim-darwin-arm64",
    "darwin-x64": "llmshim-darwin-x64",
    "linux-x64": "llmshim-linux-x64",
    "linux-arm64": "llmshim-linux-arm64",
    // Scoped to dodge npm's spam filter, which flags unscoped `*-win32-*`
    // names (the darwin/linux siblings publish unscoped fine).
    "win32-x64": "@sanjay920/llmshim-win32-x64",
  };
  const pkg = known[key];
  if (!pkg) {
    throw new Error(`No prebuilt llmshim binary is published for ${key}.`);
  }
  return pkg;
}

/** Locate the binary bundled via the platform-specific optional dependency, if installed. */
function findBundledBinary(): string | null {
  let pkgName: string;
  try {
    pkgName = platformPackageName();
  } catch {
    return null;
  }
  try {
    // Resolves to the platform package's package.json; its directory contains bin/.
    const pkgJsonPath = require.resolve(`${pkgName}/package.json`);
    const binName = process.platform === "win32" ? "llmshim.exe" : "llmshim";
    const binPath = join(pkgJsonPath, "..", "bin", binName);
    accessSync(binPath, fsConstants.X_OK);
    return binPath;
  } catch {
    return null;
  }
}

/** Locate an `llmshim` executable on PATH (e.g. `cargo install llmshim`). */
function findOnPath(): string | null {
  const pathEnv = process.env.PATH ?? process.env.Path ?? "";
  const binNames = process.platform === "win32" ? ["llmshim.exe", "llmshim.cmd"] : ["llmshim"];
  for (const dir of pathEnv.split(delimiter)) {
    if (!dir) continue;
    for (const bin of binNames) {
      const candidate = join(dir, bin);
      try {
        accessSync(candidate, fsConstants.X_OK);
        return candidate;
      } catch {
        // not here, keep looking
      }
    }
  }
  return null;
}

/** Find the llmshim binary: bundled platform package first, then PATH. */
function findBinary(): string {
  const bundled = findBundledBinary();
  if (bundled) return bundled;
  const onPath = findOnPath();
  if (onPath) return onPath;
  throw new Error(
    "llmshim binary not found. Install one of:\n" +
      "  npm install llmshim                     (includes a prebuilt binary for your platform)\n" +
      "  cargo install llmshim                   (from crates.io, puts it on PATH)\n" +
      "  cargo build --release --features proxy  (from source)\n" +
      "Or pass an explicit `baseUrl` to connect to a proxy you're already running.",
  );
}

/** Find a free TCP port on localhost. */
function findFreePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const srv = createServer();
    srv.unref();
    srv.on("error", reject);
    srv.listen(0, "127.0.0.1", () => {
      const address = srv.address();
      if (address && typeof address === "object") {
        const port = address.port;
        srv.close(() => resolve(port));
      } else {
        srv.close();
        reject(new Error("Could not determine a free port"));
      }
    });
  });
}

/** Poll until something is accepting TCP connections on `port`, or time out. */
function waitForServer(port: number, timeoutMs = 10_000): Promise<boolean> {
  const deadline = Date.now() + timeoutMs;
  return new Promise((resolve) => {
    const attempt = () => {
      const socket = createConnection({ port, host: "127.0.0.1" });
      socket.once("connect", () => {
        socket.end();
        resolve(true);
      });
      socket.once("error", () => {
        socket.destroy();
        if (Date.now() >= deadline) {
          resolve(false);
        } else {
          setTimeout(attempt, 100);
        }
      });
    };
    attempt();
  });
}

function stopServer(): void {
  if (serverProcess) {
    try {
      serverProcess.kill();
    } catch {
      // already gone
    }
  }
  serverProcess = null;
  serverPort = null;
  starting = null;
}

/**
 * Ensure the bundled proxy is running, starting it if necessary. Returns its
 * base URL. Safe to call repeatedly and concurrently — the server is started
 * at most once per process.
 */
export function ensureServer(): Promise<string> {
  if (serverProcess && serverPort !== null) {
    return Promise.resolve(`http://127.0.0.1:${serverPort}`);
  }
  if (starting) return starting;

  starting = (async () => {
    const binary = findBinary();
    const port = await findFreePort();

    const child = spawn(binary, ["proxy"], {
      env: { ...process.env, LLMSHIM_HOST: "127.0.0.1", LLMSHIM_PORT: String(port) },
      stdio: ["ignore", "ignore", "pipe"],
    });

    let stderr = "";
    child.stderr?.on("data", (chunk: Buffer) => {
      stderr += chunk.toString();
    });

    process.once("exit", stopServer);

    const ready = await waitForServer(port);
    if (!ready) {
      stopServer();
      if (stderr.includes("No API keys found")) {
        throw new Error(
          "No API keys configured. Set them via environment variables " +
            "(OPENAI_API_KEY, ANTHROPIC_API_KEY, GEMINI_API_KEY, XAI_API_KEY) " +
            "or `llmshim configure`.",
        );
      }
      throw new Error(`llmshim proxy failed to start on port ${port}.\nBinary: ${binary}\nstderr: ${stderr}`);
    }

    serverProcess = child;
    serverPort = port;
    return `http://127.0.0.1:${port}`;
  })();

  return starting;
}
