---
description: Build the lens code graph + FTS index for this entire project up front, so lens_symbol/lens_search/lens_find are fast from the first call instead of paying a lazy first-build cost mid-conversation.
allowed-tools: Bash
---

Build the full lens database (structural graph + FTS5 search index) for THIS project
(the current working directory), covering every file rather than relying on whatever's
been touched so far this session.

Steps:
1. Run `lens warmup` (no path argument defaults to `.`, the repo root) as a normal
   foreground command — it's a one-shot build, not long-running like `lens dashboard`.
2. Report the summary it prints (files parsed, graph nodes/edges, chunks indexed).
3. If `lens` isn't found on PATH, tell the user to run `lens setup` first.
