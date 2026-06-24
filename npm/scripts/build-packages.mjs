// Stage the npm packages for publishing, from the binaries CI built.
//
//   VERSION=v1.2.3 node npm/scripts/build-packages.mjs
//
// Reads each per-target binary from artifacts/lens-<target>/lens (the layout
// download-artifact produces), stages one platform package per target under
// npm/dist/, and stages the main lens-mcp launcher with optionalDependencies
// pinned to the same version. Prints the staged dirs (platform packages first).

import {
  chmodSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = join(here, "..", "..");
const distRoot = join(repoRoot, "npm", "dist");

const version = (process.env.VERSION || "").replace(/^v/, "");
if (!version) {
  process.stderr.write("build-packages: VERSION env is required (e.g. VERSION=v1.2.3)\n");
  process.exit(1);
}

// rust target triple -> npm platform package (npm `os`/`cpu` use node naming)
const PLATFORMS = [
  { target: "aarch64-apple-darwin", pkg: "lens-mcp-darwin-arm64", os: "darwin", cpu: "arm64" },
  { target: "x86_64-apple-darwin", pkg: "lens-mcp-darwin-x64", os: "darwin", cpu: "x64" },
  { target: "x86_64-unknown-linux-gnu", pkg: "lens-mcp-linux-x64", os: "linux", cpu: "x64" },
];

const common = {
  version,
  license: "Elastic-2.0",
  repository: { type: "git", url: "git+https://github.com/DemoDevelops/lens.git" },
  homepage: "https://github.com/DemoDevelops/lens#readme",
};

rmSync(distRoot, { recursive: true, force: true });
mkdirSync(distRoot, { recursive: true });

const staged = [];

for (const p of PLATFORMS) {
  const src = join(repoRoot, "artifacts", `lens-${p.target}`, "lens");
  if (!existsSync(src)) {
    process.stderr.write(`build-packages: missing artifact for ${p.target} at ${src}\n`);
    process.exit(1);
  }
  const dir = join(distRoot, p.pkg);
  mkdirSync(join(dir, "bin"), { recursive: true });
  const dest = join(dir, "bin", "lens");
  copyFileSync(src, dest);
  chmodSync(dest, 0o755);
  writeFileSync(
    join(dir, "package.json"),
    JSON.stringify(
      {
        name: p.pkg,
        description: `Prebuilt lens MCP server binary for ${p.os}-${p.cpu}.`,
        os: [p.os],
        cpu: [p.cpu],
        files: ["bin/"],
        ...common,
      },
      null,
      2
    ) + "\n"
  );
  staged.push(dir);
}

// Main launcher: copy bin/lens.js + README, pin optionalDependencies to version.
const mainSrc = join(repoRoot, "npm", "lens-mcp");
const mainDir = join(distRoot, "lens-mcp");
mkdirSync(join(mainDir, "bin"), { recursive: true });
copyFileSync(join(mainSrc, "bin", "lens.js"), join(mainDir, "bin", "lens.js"));
copyFileSync(join(mainSrc, "README.md"), join(mainDir, "README.md"));

const mainPkg = JSON.parse(readFileSync(join(mainSrc, "package.json"), "utf8"));
mainPkg.version = version;
mainPkg.optionalDependencies = Object.fromEntries(PLATFORMS.map((p) => [p.pkg, version]));
writeFileSync(join(mainDir, "package.json"), JSON.stringify(mainPkg, null, 2) + "\n");
staged.push(mainDir);

process.stdout.write(staged.join("\n") + "\n");
