#!/bin/sh
# lens installer: no Rust. Prebuilt binary + MCP tools + session hooks
# (continuity, dashboard, adoption nudges).
#
#   curl -fsSL https://raw.githubusercontent.com/DemoDevelops/lens/master/install.sh | sh
#
# Default is the aggressive mode: routing `full` (denies WebFetch, redirects
# curl/build into the darkroom, wraps output) plus RTK shell compression. Opt down to
# nudges-only (encourages the lens tools, never denies WebFetch or rewrites commands):
#
#   curl -fsSL https://raw.githubusercontent.com/DemoDevelops/lens/master/install.sh | LENS_ROUTING=nudge sh
#
# Overrides: LENS_ROUTING=<off|nudge|steer|wrap|full>, LENS_VERSION=vX.Y.Z,
# LENS_BIN_DIR=<dir>, CLAUDE_CONFIG_DIR=<dir>.
set -eu

REPO="DemoDevelops/lens"
ROUTING="${LENS_ROUTING:-full}"
WITH_RTK=1
for arg in "$@"; do
  case "$arg" in
    --full) ROUTING="full"; WITH_RTK=1 ;;
    *) echo "lens install: unknown option: $arg" >&2; exit 1 ;;
  esac
done

say() { printf '\n\033[1;36m==>\033[0m %s\n' "$*"; }
die() { printf '\n\033[1;31mERROR:\033[0m %s\n' "$*" >&2; exit 1; }

command -v claude >/dev/null 2>&1 \
  || die "Claude Code ('claude') not found. Install it first: https://claude.com/claude-code"

# Map the host to the release asset built by .github/workflows/release.yml.
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Darwin)
    case "$arch" in
      arm64|aarch64) target="aarch64-apple-darwin" ;;
      x86_64) target="x86_64-apple-darwin" ;;
      *) die "unsupported macOS arch: $arch" ;;
    esac ;;
  Linux)
    case "$arch" in
      x86_64|amd64) target="x86_64-unknown-linux-gnu" ;;
      arm64|aarch64) target="aarch64-unknown-linux-gnu" ;;
      *) die "no prebuilt Linux binary for $arch yet. Build from source: https://github.com/$REPO" ;;
    esac ;;
  *) die "unsupported OS: $os. Build from source: https://github.com/$REPO" ;;
esac

if [ -n "${LENS_VERSION:-}" ]; then
  url="https://github.com/$REPO/releases/download/$LENS_VERSION/lens-$target"
else
  url="https://github.com/$REPO/releases/latest/download/lens-$target"
fi

bindir="${LENS_BIN_DIR:-$HOME/.local/bin}"
bin="$bindir/lens"
mkdir -p "$bindir"

say "Downloading lens ($target)..."
downloaded=0
if command -v curl >/dev/null 2>&1; then
  if curl -fsSL "$url" -o "$bin" 2>/dev/null; then downloaded=1; fi
elif command -v wget >/dev/null 2>&1; then
  if wget -qO "$bin" "$url" 2>/dev/null; then downloaded=1; fi
fi
if [ "$downloaded" = "0" ]; then
  # Public download failed (private repo, or asset missing). Fall back to an
  # authenticated gh download so the owner can install before going public.
  command -v gh >/dev/null 2>&1 \
    || die "download failed and 'gh' not found. If $REPO is private, install GitHub CLI (https://cli.github.com), run 'gh auth login', then re-run."
  say "Public download failed; pulling via authenticated gh (private repo)..."
  if [ -n "${LENS_VERSION:-}" ]; then
    gh release download "$LENS_VERSION" --repo "$REPO" --pattern "lens-$target" --output "$bin" --clobber \
      || die "gh release download failed. Check 'gh auth status' and your access to $REPO."
  else
    gh release download --repo "$REPO" --pattern "lens-$target" --output "$bin" --clobber \
      || die "gh release download failed. Check 'gh auth status' and your access to $REPO."
  fi
fi
chmod +x "$bin"
# curl/wget downloads are not quarantined like browser downloads, but strip it
# anyway so a re-download can never trip Gatekeeper.
if [ "$os" = "Darwin" ]; then
  xattr -d com.apple.quarantine "$bin" 2>/dev/null || true
fi
say "Installed: $bin"

say "Registering the MCP server (lens_* tools)..."
claude mcp add lens --scope user -- "$bin" 2>/dev/null \
  || echo "  already registered; check: claude mcp list"

say "Installing session hooks (continuity + dashboard + nudges)..."
"$bin" session install

if [ "$WITH_RTK" = "1" ]; then
  say "Installing RTK shell-output compression..."
  "$bin" rtk install || echo "  RTK install skipped (non-fatal)."
fi

say "Setting routing level to '$ROUTING'..."
CONFIG_DIR="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
SETTINGS="$CONFIG_DIR/settings.json"
if command -v python3 >/dev/null 2>&1; then
  python3 - "$SETTINGS" "$ROUTING" <<'PY'
import json, sys, pathlib
p = pathlib.Path(sys.argv[1]); level = sys.argv[2]
d = json.loads(p.read_text()) if p.exists() and p.read_text().strip() else {}
d.setdefault("env", {})["LENS_ROUTING"] = level
p.parent.mkdir(parents=True, exist_ok=True)
p.write_text(json.dumps(d, indent=2) + "\n")
print(f"  set LENS_ROUTING={level} in {p}")
PY
else
  echo "  python3 not found; add to $SETTINGS by hand:  \"env\": { \"LENS_ROUTING\": \"$ROUTING\" }"
fi

# Put `lens` on PATH for new terminals so `lens dashboard` works by name. The
# hooks and MCP server use the absolute path and do not need this.
case ":$PATH:" in
  *":$bindir:"*) ;;
  *)
    case "$(basename "${SHELL:-sh}")" in
      zsh)  profile="$HOME/.zshrc" ;;
      bash) profile="$HOME/.bashrc" ;;
      *)    profile="$HOME/.profile" ;;
    esac
    if [ ! -f "$profile" ] || ! grep -qF "$bindir" "$profile" 2>/dev/null; then
      printf '\n# added by lens installer\nexport PATH="%s:$PATH"\n' "$bindir" >> "$profile"
      say "Added $bindir to PATH in $profile (open a new terminal to use 'lens' by name)."
    fi
    ;;
esac

note=""
if [ "$ROUTING" = "nudge" ]; then
  note=" (nudges only; no WebFetch deny or command rewrites)"
fi
rtk_uninstall=""
if [ "$WITH_RTK" = "1" ]; then
  rtk_uninstall="  &&  $bin rtk uninstall"
fi

cat <<EOF

Done. Restart Claude Code to load lens.
  Tools:     available immediately (run lens_stats to verify)
  Routing:   $ROUTING$note
  Dashboard: $bin dashboard   then open http://localhost:7878
             (or just 'lens dashboard' in a new terminal)

Opt down to nudges-only with LENS_ROUTING=nudge, or edit "env":{"LENS_ROUTING":"..."} in $SETTINGS.
Uninstall:  $bin session uninstall$rtk_uninstall  &&  claude mcp remove lens  &&  rm "$bin"
EOF
