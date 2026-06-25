---
description: Launch the lens live dashboard for the current repo and print its URL
allowed-tools: Bash
---

Launch the lens live dashboard for THIS repo (the current working directory),
then give me the URL. Notes:

- It reads `<cwd>/.lens`, so launching it from the session's own dir makes the
  MCP-savings and activity panes reflect THIS repo. (The RTK shell-savings pane is
  global via `rtk gain` either way.)
- `lens` is on PATH after `lens setup`, so call it directly.

Steps:
1. Target port 7878. If something is already listening there, `curl -s
   http://127.0.0.1:7878/api/stats` — if it responds with lens JSON, just report
   that URL and stop (don't start a second one). Otherwise pick the next free port
   (7879, 7880, ...).
2. Start it as a **background process** (do not block the turn): run
   `lens dashboard --port <port>` with run_in_background.
3. Wait ~1s, then `curl -s http://127.0.0.1:<port>/api/stats >/dev/null` to confirm
   it bound. If it didn't, show the last lines of its output and stop.
4. Report: the clickable URL `http://127.0.0.1:<port>`, the data dir it's reading
   (`<cwd>/.lens`), and that it stops when this session ends (re-run `/dashboard`
   to restart). The page shows live-since-you-opened-it numbers; refresh to reset.
