// lens composition API: call back into this repo's live index/graph from a
// darkroom script without another MCP round-trip.
//
// Every function below execs `lens q <verb> ...` (the read-only CLI family)
// via `execFileSync` with an argv array (never a shell string), parses the
// JSON line `lens q` prints to stdout, and throws on a nonzero exit.
// `LENS_BIN` / `LENS_DIR` are set on this process's environment by the
// darkroom runner that spawned this script.

import { execFileSync } from "node:child_process";

function q(args) {
  const lensBin = process.env.LENS_BIN;
  if (!lensBin) {
    throw new Error(
      "LENS_BIN is not set; lens q calls require the darkroom-injected environment"
    );
  }
  let stdout;
  try {
    stdout = execFileSync(lensBin, ["q", ...args], { encoding: "utf8" });
  } catch (err) {
    const stderr = err.stderr ? String(err.stderr).trim() : "";
    throw new Error(stderr || err.message);
  }
  return JSON.parse(stdout);
}

export function search(query, limit = 20) {
  return q(["search", query, "--limit", String(limit)]);
}

export function symbol(name, kind = null) {
  const args = ["symbol", name];
  if (kind !== null) args.push("--kind", String(kind));
  return q(args);
}

export function callers(name, transitive = false, depth = 2, prodOnly = false) {
  const args = ["callers", name, "--depth", String(depth)];
  if (transitive) args.push("--transitive");
  if (prodOnly) args.push("--prod-only");
  return q(args);
}

export function callees(name, transitive = false, depth = 2, prodOnly = false) {
  const args = ["callees", name, "--depth", String(depth)];
  if (transitive) args.push("--transitive");
  if (prodOnly) args.push("--prod-only");
  return q(args);
}

export function path(frm, to) {
  return q(["path", frm, to]);
}

// `includeBodies`/`withLines` mirror the MCP tool's parameter names, the ones
// models reuse in scripts.
export function skeleton(path, bodies = null, includeBodies = null, withLines = null) {
  const args = ["skeleton", path];
  const b = bodies ?? includeBodies;
  if (b && b.length) args.push("--bodies", b.join(","));
  if (withLines === false) args.push("--no-lines");
  return q(args);
}

export function grep_ast(
  pattern = null,
  query = null,
  lang = null,
  path = null,
  limit = null,
  prodOnly = false,
  language = null
) {
  const args = ["grep-ast"];
  if (pattern !== null) args.push("--pattern", pattern);
  if (query !== null) args.push("--query", query);
  const l = lang ?? language;
  if (l !== null) args.push("--lang", l);
  if (path !== null) args.push("--path", path);
  if (limit !== null) args.push("--limit", String(limit));
  if (prodOnly) args.push("--prod-only");
  return q(args);
}

export function overview(query = null, budget = 8000) {
  const args = ["overview", "--budget", String(budget)];
  if (query !== null) args.push("--query", query);
  return q(args);
}

export function recall(ref, grep = null, offset = null, limit = null) {
  const args = ["recall", ref];
  if (grep !== null) args.push("--grep", grep);
  if (offset !== null) args.push("--offset", String(offset));
  if (limit !== null) args.push("--limit", String(limit));
  return q(args);
}
