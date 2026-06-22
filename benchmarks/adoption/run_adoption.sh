#!/usr/bin/env bash
# Plane B: does Claude actually FIRE each ctxforge feature when it should?
# EVIDENCE tier (real agent, stochastic, small N). For each feature we pose a task
# that SHOULD trigger that feature (without naming the tool), run it N times under
# normal ctxforge steering, and check ops.log for whether the expected mechanism
# fired. The agent also has Read/Grep/Bash available -- the question is whether it
# reaches for the ctxforge tool or falls back to the built-ins.
#
# This is the adoption question in the user's words. No on/off arms: "does it fire
# when supposed to" is a firing RATE under steering, not an A/B. (The earlier
# nudge-on/off A/B is subsumed; isolating the routing-OFF baseline needs a dedicated
# logged-in config dir, which the shared live settings.json cannot provide.)
#
# Only agent-CHOSEN features are measurable here: sandbox (ctx_execute), search
# (ctx_search/index), graph (graph_*/discovery). Compression and wrap fire
# automatically (not an agent choice); recovery is bench_recovery.
#
# CONFIG: runs under your live CLAUDE_CONFIG_DIR (~/.claude-personal) so auth + trust
# work. Completion via --completion-file. Firing measured from ops.log filtered to the
# run's --session-id (the PostToolUse hook stamps it).
#
# Usage:
#   benchmarks/adoption/run_adoption.sh --validate   # 1 run per feature, verbose
#   benchmarks/adoption/run_adoption.sh --runs 3     # N runs per feature
set -uo pipefail

REPO="/Users/gene/Documents/AI Stuff/ctxforge"
BIN="$REPO/target/release/ctxforge"
LIVE_CFG="$HOME/.claude-personal"
OPSLOG="$REPO/.ctxforge/ops.log"

ALLOWED="Read,Grep,Glob,Bash,Write,ToolSearch,mcp__ctxforge__graph_query,mcp__ctxforge__graph_neighbors,mcp__ctxforge__graph_path,mcp__ctxforge__ctx_search,mcp__ctxforge__ctx_index,mcp__ctxforge__ctx_discover,mcp__ctxforge__ctx_execute,mcp__ctxforge__ctx_execute_file,mcp__ctxforge__ctx_retrieve"

# Each task: "feature|expected_mechanism|prompt". Prompts trigger the mechanism
# without naming the tool, so a fallback to Read/Grep counts as NOT fired.
TASKS=(
  "sandbox|sandbox|There is a large log file at benchmarks/accuracy/fixtures/big.log. Identify the single root-cause error type and exactly how many times it occurs. Report only the error type and the count."
  "search|index|Across this entire repository, find every place the routing \`Level\` enum is used (constructed, matched on, or taken as a parameter type). List the file:line sites."
  "graph|discovery|Which functions call \`bump_counter\`, and following call edges does \`route\` reach \`read_graph_nudge\`? Answer both, citing file:line."
)

RUNS=3
VALIDATE=0
while [ $# -gt 0 ]; do
  case "$1" in
    --validate) VALIDATE=1; RUNS=1; shift ;;
    --runs) RUNS="$2"; shift 2 ;;
    *) echo "unknown flag: $1"; exit 2 ;;
  esac
done

command -v claude-pty >/dev/null || { echo "claude-pty not on PATH"; exit 1; }
[ -f "$BIN" ] || { echo "binary missing: $BIN (cargo build --release)"; exit 1; }

OUT=$(mktemp -d)
TSV="$OUT/results.tsv"
: > "$TSV"
echo "results dir: $OUT"

# Did the expected mechanism fire for this session? Echoes "fired tools" where
# fired is 1/0 and tools is a comma list of ctxforge tools the agent actually used.
count_fired() {
  local uuid="$1" expected="$2"
  python3 - "$OPSLOG" "$uuid" "$expected" <<'PY'
import json, sys
log, sid, expected = sys.argv[1:4]
mech = {
    "ctx_execute": "sandbox", "ctx_execute_file": "sandbox",
    "ctx_index": "index", "ctx_search": "index",
    "ctx_discover": "discovery", "graph_query": "discovery",
    "graph_neighbors": "discovery", "graph_path": "discovery",
    "ctx_retrieve": "retrieve",
}
tools = []
try:
    for line in open(log, errors="ignore"):
        line = line.strip()
        if not line: continue
        try: r = json.loads(line)
        except Exception: continue
        if r.get("session_id") != sid: continue
        t = r.get("tool", "")
        if t in mech: tools.append(t)
except FileNotFoundError:
    pass
fired = 1 if any(mech[t] == expected for t in tools) else 0
print(fired, ",".join(tools) if tools else "-")
PY
}

run_one() {
  local feature="$1" mech="$2" task="$3" tag="$4"
  local uuid comp full_task
  uuid=$(python3 -c 'import uuid; print(uuid.uuid4())')
  comp="$OUT/$tag.done"
  full_task="$task

When finished, write your full final answer to the file $comp, and make its very last line exactly: DONE"
  if [ "$VALIDATE" = 1 ]; then
    { echo "  feature=$feature expected=$mech session=$uuid"; } >&2
  fi
  echo "$full_task" | \
    CLAUDE_CONFIG_DIR="$LIVE_CFG" \
    CTXFORGE_ROUTING="full" \
    claude-pty \
      --working-dir "$REPO" \
      --session-id "$uuid" \
      --allowed-tools "$ALLOWED" \
      --dangerously-skip-permissions \
      --completion-file "$comp" \
      --completion-marker "DONE" \
      --max-tool-calls 40 \
      --timeout 360 \
      > "$OUT/$tag.stdout.txt" 2> "$OUT/$tag.err.txt"
  sleep 1
  [ -f "$comp" ] && sed '/^DONE$/d' "$comp" > "$OUT/$tag.answer.txt"
  read -r fired tools <<< "$(count_fired "$uuid" "$mech")"
  printf '%s\t%s\t%s\n' "$feature" "$fired" "$tools" >> "$TSV"
  echo "$fired $tools"
}

echo "# ctxforge Plane B: feature firing (runs=$RUNS per feature)"
echo
printf '%-9s %-6s %-7s %s\n' feature run fired tools_used
for line in "${TASKS[@]}"; do
  feature="${line%%|*}"; rest="${line#*|}"; mech="${rest%%|*}"; prompt="${rest#*|}"
  for i in $(seq 1 "$RUNS"); do
    tag="${feature}_r${i}"
    read -r fired tools <<< "$(run_one "$feature" "$mech" "$prompt" "$tag")"
    printf '%-9s %-6s %-7s %s\n' "$feature" "r${i}" "$fired" "$tools"
    if [ "$VALIDATE" = 1 ]; then
      echo "  --- answer (first 200 chars) ---"; cut -c1-200 "$OUT/$tag.answer.txt" 2>/dev/null; echo
    fi
  done
done

echo
echo "## Firing rate per feature (fired = used the expected ctxforge mechanism)"
python3 - "$TSV" <<'PY'
import sys, collections
rows = [l.rstrip("\n").split("\t") for l in open(sys.argv[1]) if l.strip()]
agg = collections.defaultdict(lambda: [0, 0])  # n, fired
for feature, fired, _tools in rows:
    a = agg[feature]; a[0] += 1; a[1] += int(fired)
print("| feature | runs | fired | rate |")
print("| --- | ---: | ---: | ---: |")
for feature, (n, f) in agg.items():
    print(f"| {feature} | {n} | {f} | {f/n:.0%} |")
PY

echo
echo "answers + logs saved under: $OUT"
[ "$VALIDATE" = 1 ] && echo "VALIDATE run only. Re-run with --runs N for the full pilot." || true
