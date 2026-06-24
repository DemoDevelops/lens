#!/usr/bin/env node
"use strict";
// Resolve the platform-specific lens binary (shipped as an optional dependency)
// and exec it, passing argv + stdio through untouched.
//
// CRITICAL: in server mode lens speaks JSON-RPC over stdout. This launcher must
// never write to stdout. All diagnostics go to stderr.

const { spawnSync } = require("node:child_process");

// host "<platform>-<arch>" -> the package that carries that prebuilt binary
const PACKAGES = {
  "darwin-arm64": "lens-mcp-darwin-arm64",
  "darwin-x64": "lens-mcp-darwin-x64",
  "linux-x64": "lens-mcp-linux-x64",
};

function fail(msg) {
  process.stderr.write(`lens-mcp: ${msg}\n`);
  process.exit(1);
}

const key = `${process.platform}-${process.arch}`;
const pkg = PACKAGES[key];
if (!pkg) {
  fail(
    `no prebuilt binary for ${key} (supported: ${Object.keys(PACKAGES).join(", ")}). ` +
      `Build from source: https://github.com/DemoDevelops/lens`
  );
}

let bin;
try {
  bin = require.resolve(`${pkg}/bin/lens`);
} catch (_) {
  fail(
    `platform package "${pkg}" is not installed. ` +
      `Reinstall with optional dependencies enabled (do not pass --omit=optional).`
  );
}

const r = spawnSync(bin, process.argv.slice(2), { stdio: "inherit" });
if (r.error) {
  fail(`failed to launch ${bin}: ${r.error.message}`);
}
process.exit(typeof r.status === "number" ? r.status : 1);
