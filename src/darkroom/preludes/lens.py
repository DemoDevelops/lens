"""lens composition API: call back into this repo's live index/graph from a
darkroom script without another MCP round-trip.

Every function below execs `lens q <verb> ...` (the read-only CLI family)
via `subprocess.run` with an argv list (never a shell), parses the JSON line
`lens q` prints to stdout, and raises `RuntimeError` on a nonzero exit.
`LENS_BIN` / `LENS_DIR` are set on this process's environment by the
darkroom runner that spawned this script.
"""

import json
import os
import subprocess


def _q(args):
    lens_bin = os.environ.get("LENS_BIN")
    if not lens_bin:
        raise RuntimeError(
            "LENS_BIN is not set; lens q calls require the darkroom-injected environment"
        )
    result = subprocess.run(
        [lens_bin, "q", *args],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(result.stderr.strip() or f"lens q {args[0]} exited {result.returncode}")
    return json.loads(result.stdout)


def search(query, limit=20):
    return _q(["search", query, "--limit", str(limit)])


def symbol(name, kind=None):
    args = ["symbol", name]
    if kind is not None:
        args += ["--kind", str(kind)]
    return _q(args)


def callers(name, transitive=False, depth=2, prod_only=False):
    args = ["callers", name, "--depth", str(depth)]
    if transitive:
        args.append("--transitive")
    if prod_only:
        args.append("--prod-only")
    return _q(args)


def callees(name, transitive=False, depth=2, prod_only=False):
    args = ["callees", name, "--depth", str(depth)]
    if transitive:
        args.append("--transitive")
    if prod_only:
        args.append("--prod-only")
    return _q(args)


def path(frm, to):
    return _q(["path", frm, to])


def skeleton(path, bodies=None, include_bodies=None, with_lines=None, query=None, only=None):
    # `include_bodies`/`with_lines`/`query`/`only` mirror the MCP tool's
    # parameter names, the ones models reuse in scripts (19/28 mined script
    # errors were kwarg mismatches against this shim).
    args = ["skeleton", path]
    bodies = bodies if bodies is not None else include_bodies
    if bodies:
        args += ["--bodies", ",".join(bodies)]
    if with_lines is False:
        args.append("--no-lines")
    if query is not None:
        args += ["--query", query]
    if only is not None:
        args += ["--only", only]
    return _q(args)


def grep_ast(
    pattern=None, query=None, lang=None, path=None, limit=None, prod_only=False, language=None
):
    args = ["grep-ast"]
    if pattern is not None:
        args += ["--pattern", pattern]
    if query is not None:
        args += ["--query", query]
    lang = lang if lang is not None else language
    if lang is not None:
        args += ["--lang", lang]
    if path is not None:
        args += ["--path", path]
    if limit is not None:
        args += ["--limit", str(limit)]
    if prod_only:
        args.append("--prod-only")
    return _q(args)


def overview(query=None, budget=8000):
    args = ["overview", "--budget", str(budget)]
    if query is not None:
        args += ["--query", query]
    return _q(args)


def recall(ref, grep=None, offset=None, limit=None):
    args = ["recall", ref]
    if grep is not None:
        args += ["--grep", grep]
    if offset is not None:
        args += ["--offset", str(offset)]
    if limit is not None:
        args += ["--limit", str(limit)]
    return _q(args)
