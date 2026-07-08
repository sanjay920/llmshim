// Tests for the pure platform-mapping logic in server.ts (the auto-spawn
// binary resolution). Does NOT spawn any process or touch the filesystem for
// a real binary — those paths need a real bundled binary and are covered
// manually by scripts/manual-smoke-check.mjs instead. Fully mocked, $0 to run.

import assert from "node:assert/strict";
import { test } from "node:test";

import { platformPackageName } from "../dist/index.js";

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
