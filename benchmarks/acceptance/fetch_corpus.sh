#!/usr/bin/env bash
# Populate the gitignored bench_acceptance corpus: shallow-clone each language's
# pinned-SHA repo (from thresholds.json) into corpus/<lang>/. Idempotent: a repo
# already present is left untouched. The corpus is never committed (it is in
# .git/info/exclude); only thresholds.json (repo + SHA) is.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
corpus="$here/corpus"
mkdir -p "$corpus"

python3 -c '
import json, sys
for lang, t in json.load(open(sys.argv[1])).items():
    print(lang + "\t" + t["repo"] + "\t" + t["sha"])
' "$here/thresholds.json" | while IFS=$'\t' read -r lang repo sha; do
  dir="$corpus/$lang"
  if [ -e "$dir/.git" ]; then
    echo "[$lang] cached"
    continue
  fi
  echo "[$lang] fetching $repo @ ${sha:0:12}"
  rm -rf "$dir"
  git init -q "$dir"
  git -C "$dir" remote add origin "$repo"
  # GitHub serves reachable SHAs to a depth-1 want, so this pins exactly.
  git -C "$dir" fetch -q --depth 1 origin "$sha"
  git -C "$dir" checkout -q FETCH_HEAD
done

echo "corpus ready at $corpus"
