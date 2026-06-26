#!/usr/bin/env bash
# Fetch a fixed external corpus for bench_tuning's REPORT-ONLY real-corpus arm.
#
# This is never gated (network + per-machine variance make it unfit for CI). It
# exists so a tuning claim can be sanity-checked against a real repo, not only the
# replicated `code_search` fixture (which over-represents duplicate trigrams).
#
# Usage:
#   scripts/fetch_bench_corpus.sh [dest_dir]
#   LENS_BENCH_CORPUS="$dest_dir" cargo run --release --bin bench_tuning
#
# Pin is by release tag (a stable, verifiable ref); override with a 40-char SHA via
# BENCH_CORPUS_REF if you want exact-commit reproducibility.
set -euo pipefail

REPO="${BENCH_CORPUS_REPO:-https://github.com/BurntSushi/ripgrep.git}"
REF="${BENCH_CORPUS_REF:-14.1.0}"
DEST="${1:-${TMPDIR:-/tmp}/lens-bench-corpus/ripgrep-${REF}}"

if [ -d "$DEST/.git" ]; then
  echo "corpus already present: $DEST" >&2
else
  mkdir -p "$(dirname "$DEST")"
  git clone --depth 1 --branch "$REF" "$REPO" "$DEST"
fi

SHA="$(git -C "$DEST" rev-parse HEAD)"
echo "fetched $REPO @ $REF ($SHA)" >&2
echo "export LENS_BENCH_CORPUS=$DEST"
