// Manual smoke check — proves auto-spawn works end-to-end against a REAL
// bundled binary. Not part of `npm test` (deliberately named to avoid Node's
// test-file auto-discovery, and needs a real binary that CI doesn't have).
// $0 cost — only hits /health, never a real provider.
//
// Setup (run from clients/typescript/):
//   cargo build --release --features proxy   # from the repo root, or adjust the path below
//   mkdir -p packages/llmshim-<platform>-<arch>/bin
//   cp ../../target/release/llmshim packages/llmshim-<platform>-<arch>/bin/llmshim
//   npm install ./packages/llmshim-<platform>-<arch> --no-save
//   npm run build
//   node scripts/manual-smoke-check.mjs
import { Client } from "../dist/index.js";

const client = new Client(); // no baseUrl -> must auto-spawn
console.log("calling health() with no baseUrl set (should auto-spawn)...");
const health = await client.health();
console.log("SUCCESS:", health);

// second call on the SAME client should reuse the already-started server (fast)
const t0 = Date.now();
await client.health();
console.log(`second call took ${Date.now() - t0}ms (should be fast, no respawn)`);

// a second, independent Client with no baseUrl should reuse the process-wide
// singleton proxy (ensureServer() memoizes across instances too)
const client2 = new Client();
const t1 = Date.now();
const health2 = await client2.health();
console.log(`second Client instance took ${Date.now() - t1}ms:`, health2);

process.exit(0);
