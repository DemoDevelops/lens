#!/usr/bin/env bash
# lens setup: one command to install for personal use.
#
# Usage (from the lens repo root):
#   ./setup.sh
#
# It builds lens (needs Rust), registers it as MCP server for Claude Code or opencode (grok-build),
# installs the session hooks + RTK (Claude) or commands/ (opencode), and sets the routing level.
# Safe to re-run. Use LENS_HOST=opencode ./setup.sh or pass via env.
#
# Routing level (override with: LENS_ROUTING=wrap ./setup.sh):
#   full  = max savings: darkroom + graph + compaction + nudges, denies WebFetch and
#           redirects curl/build commands into the darkroom (default; matches the author's setup)
#   steer = nudges + WebFetch deny, no SessionStart guide injection
#   wrap  = output wrapping/compaction only: no WebFetch deny, no nudges
#   off   = MCP tools available, no steering at all
set -euo pipefail

ROUTING="${LENS_ROUTING:-full}"

cd "$(dirname "$0")"
say() { printf '\n\033[1;36m==>\033[0m %s\n' "$*"; }
die() { printf '\n\033[1;31mERROR:\033[0m %s\n' "$*" >&2; exit 1; }

[ -f Cargo.toml ] || die "run this from the lens repo root (no Cargo.toml here)."

HOST="${LENS_HOST:-claude}"
case "$HOST" in
  opencode|open-code|grok) HOST=opencode ;;
  claude|*) HOST=claude ;;
esac
if [ "$HOST" = "claude" ]; then
  command -v claude >/dev/null 2>&1 || \
    die "Claude Code (the 'claude' command) is not installed. Install it first, then re-run."
fi
# For opencode we do not require the 'opencode' CLI (direct jsonc edit).

if ! command -v cargo >/dev/null 2>&1; then
  die "Rust is not installed. Install it once with:
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  then open a new terminal and re-run ./setup.sh"
fi

say "Building and installing lens (takes a minute)..."
cargo install --path . --bin lens --force

BIN="$(command -v lens || true)"
[ -n "$BIN" ] || BIN="$HOME/.cargo/bin/lens"
[ -x "$BIN" ] || die "lens not found after install (looked at $BIN). Is ~/.cargo/bin on your PATH?"
say "Installed: $BIN"

say "Registering the MCP server (so the lens_* tools appear)..."
if [ "$HOST" = "claude" ]; then
  claude mcp add lens --scope user -- "$BIN" 2>/dev/null \
    || echo "  already registered (or add skipped); check with: claude mcp list"
else
  LENS_HOST=opencode LENS_BIN_DIR="$(dirname "$BIN")" "$BIN" setup --client opencode --routing "$ROUTING" --bin-dir "$(dirname "$BIN")" \
    || echo "  opencode MCP configured via jsonc (see output above); re-run safe."
fi

say "Installing session hooks..."
if [ "$HOST" = "claude" ]; then
  "$BIN" session install
else
  "$BIN" session install --client opencode
fi

if [ "$HOST" = "claude" ]; then
  say "Installing the RTK rewrite hook..."
  "$BIN" rtk install || echo "  RTK hook install skipped (non-fatal); check with: $BIN rtk status"
else
  echo "  (RTK shell compression is Claude-only for now; opencode uses MCP env for routing.)"
fi

say "Setting routing level to '$ROUTING'..."
if [ "$HOST" = "claude" ]; then
  CONFIG_DIR="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
  SETTINGS="${LENS_SETTINGS:-$CONFIG_DIR/settings.json}"
  if command -v python3 >/dev/null 2>&1; then
    python3 - "$SETTINGS" "$ROUTING" <<'PY'
import json, sys, pathlib
p = pathlib.Path(sys.argv[1]); level = sys.argv[2]
d = json.loads(p.read_text()) if p.exists() and p.read_text().strip() else {}
d.setdefault("env", {})["LENS_ROUTING"] = level
p.write_text(json.dumps(d, indent=2) + "\n")
print(f"  set LENS_ROUTING={level} in {p}")
PY
  else
    echo "  python3 not found. Add this to $SETTINGS by hand, under the top level:"
    echo "      \"env\": { \"LENS_ROUTING\": \"$ROUTING\" }"
  fi
else
  echo "  (for opencode, routing lives in the mcp entry's environment; set during the setup call above)"
fi

say "Verifying..."
if [ "$HOST" = "claude" ]; then
  "$BIN" session status
else
  "$BIN" session status --client opencode || true
fi

cat <<EOF

Done. Next steps:
  1. Restart your client (Claude Code or opencode) so it loads the new MCP server (+ hooks for Claude).
  2. In a session, the lens_* tools should be available.
  3. Watch savings live:  lens dashboard   (then open http://localhost:7878)

Notes:
  - Routing is '$ROUTING'. With 'full' or 'steer', WebFetch is redirected into the
    darkroom; if that gets in your way, re-run with:  LENS_ROUTING=wrap ./setup.sh
  - Lifecycle hooks and RTK are Claude-only for now; opencode gets MCP + commands/ + darkroom tools.
  - Do not also enable the context-mode plugin: the hook install refuses to coexist.
  - To undo:  lens session uninstall && lens rtk uninstall && claude mcp remove lens   (Claude)
             or: lens session uninstall --client opencode && rm the binary   (opencode; edit opencode.jsonc to drop mcp.lens)
EOF
