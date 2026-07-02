//! `lens dashboard` — a local web view of the op log.
//!
//! A tiny dependency-free HTTP/1.1 server (thread per connection) bound to
//! loopback only. It serves a self-contained page at `/` that polls `/api/stats`
//! (the [`super::stats::snapshot_json`] aggregate) once a second and renders live
//! totals, throughput sparklines, and a per-tool breakdown — all computed in the
//! browser from successive snapshots, so the server stays stateless and read-only.
//!
//! This is a separate process from the MCP server; its stdout is its own. Nothing
//! here touches tool result payloads or the server's JSON-RPC stdout.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::data_dir;
use super::stats::snapshot_json_since;

const DEFAULT_PORT: u16 = 7878;
const DEFAULT_HOST: &str = "127.0.0.1";

/// CLI entry: `args` is everything after `dashboard`. With `--tui` this renders the
/// terminal dashboard (no socket opened); otherwise it serves the web view.
pub fn run_cli(args: &[String]) -> Result<()> {
    let mut port = DEFAULT_PORT;
    let mut host = DEFAULT_HOST.to_string();
    let mut session: Option<String> = None;
    let mut tui = false;
    let mut scope_global = false;
    let mut all = false;
    let mut interval: u64 = 1;
    let mut rate: f64 = 5.0; // Opus 4.8 input @ $5/M — the web's default cost basis
    let mut rt_seconds: f64 = crate::obs::value_model::ROUND_TRIP_SECONDS; // applied-value time basis
    let mut window = crate::obs::tui::Window::All; // TUI time scope (--today / --since)
    let mut theme = crate::obs::tui::ThemeKind::Dark; // TUI palette (--theme dark|70s)
    let mut force_view: Option<bool> = None; // Some(true)=full, Some(false)=mini, None=auto
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => {
                port = args
                    .get(i + 1)
                    .and_then(|v| v.parse().ok())
                    .context("--port expects a number")?;
                i += 1;
            }
            "--host" => {
                host = args.get(i + 1).cloned().context("--host expects a value")?;
                i += 1;
            }
            "--session" => {
                session = args.get(i + 1).cloned();
                i += 1;
            }
            "--tui" => tui = true,
            "--global" => scope_global = true,
            "--all" => all = true,
            "--interval" => {
                interval = args
                    .get(i + 1)
                    .and_then(|v| v.parse().ok())
                    .context("--interval expects seconds")?;
                i += 1;
            }
            "--rate" => {
                rate = args
                    .get(i + 1)
                    .and_then(|v| v.parse().ok())
                    .context("--rate expects $/M tokens")?;
                i += 1;
            }
            "--model" => {
                let m = args.get(i + 1).cloned().unwrap_or_default();
                rate = match m.to_lowercase().as_str() {
                    "opus" => 5.0,
                    "sonnet" => 3.0,
                    "haiku" => 1.0,
                    other => {
                        eprintln!("lens dashboard: unknown --model '{other}' (opus|sonnet|haiku)");
                        std::process::exit(2);
                    }
                };
                i += 1;
            }
            "--rt-seconds" => {
                rt_seconds = args
                    .get(i + 1)
                    .and_then(|v| v.parse().ok())
                    .context("--rt-seconds expects seconds per round-trip")?;
                i += 1;
            }
            "--theme" => {
                let t = args.get(i + 1).cloned().unwrap_or_default();
                theme = crate::obs::tui::ThemeKind::parse(&t)
                    .with_context(|| format!("--theme: unknown '{t}' (dark|70s)"))?;
                i += 1;
            }
            "--today" => window = crate::obs::tui::Window::Today,
            "--since" => {
                let spec = args.get(i + 1).cloned().unwrap_or_default();
                window = crate::obs::tui::Window::parse(&spec)
                    .with_context(|| format!("--since: bad window '{spec}' (today|all|15m|1h|3h|2d)"))?;
                i += 1;
            }
            "--mini" => force_view = Some(false),
            "--full" => force_view = Some(true),
            other => {
                eprintln!("lens dashboard: unknown flag '{other}'");
                eprintln!("usage: lens dashboard [--port <n>] [--host <addr>] [--session <id>]");
                eprintln!("       lens dashboard --tui [--global] [--all] [--today | --since <today|all|15m|1h|3h|2d>]");
                eprintln!(
                    "            [--interval <s>] [--rate <$/M> | --model opus|sonnet|haiku] [--rt-seconds <s>] [--theme dark|70s] [--mini|--full]"
                );
                std::process::exit(2);
            }
        }
        i += 1;
    }

    let dir = data_dir();

    // One dashboard, two renderers: --tui branches to the terminal view before any
    // socket is opened. The shared snapshot keeps web and TUI in feature parity.
    if tui {
        // --all clears any --session filter (show every session); the window is
        // all-time, matching the web dashboard's default window.
        let session = if all { None } else { session };
        return crate::obs::tui::run(
            dir, session, scope_global, interval, window, rate, rt_seconds, theme, force_view,
        );
    }

    let listener = TcpListener::bind((host.as_str(), port))
        .with_context(|| format!("binding {host}:{port}"))?;
    println!("lens dashboard serving on http://{host}:{port}  (Ctrl-C to stop)");
    println!("  data dir: {}", dir.display());
    if let Some(s) = &session {
        println!("  session : {s}");
    }

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let dir = dir.clone();
                let session = session.clone();
                // Thread per connection: a local dashboard sees light traffic, and
                // this keeps a slow client from blocking the accept loop.
                std::thread::spawn(move || handle(s, &dir, session.as_deref()));
            }
            Err(e) => eprintln!("accept error: {e}"),
        }
    }
    Ok(())
}

/// Serve one connection: read the request target, route it, write the response.
fn handle(mut stream: TcpStream, dir: &Path, session: Option<&str>) {
    let target = match read_request_target(&mut stream) {
        Some(t) => t,
        None => return,
    };
    let (status, content_type, body) = route(&target, dir, session);
    let _ = write_response(&mut stream, status, content_type, &body);
}

/// Route a request path to (status, content-type, body). `/api/stats` honors a
/// `?since=<unix-seconds>` cutoff (the page's load time) so the dashboard shows
/// only sessions live since it was opened.
fn route(target: &str, dir: &Path, session: Option<&str>) -> (u16, &'static str, String) {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    match path {
        "/" | "/index.html" => (200, "text/html; charset=utf-8", INDEX_HTML.to_string()),
        "/api/stats" => {
            let since = query_param(query, "since").and_then(|v| v.parse::<i64>().ok());
            let until = query_param(query, "until").and_then(|v| v.parse::<i64>().ok());
            // scope resolution:
            //   "global" -> the machine-global mirror under home_root() (every repo,
            //               cross-repo so the session filter is dropped),
            //   a repo path -> that repo's own <path>/.lens data dir (its own savings),
            //   absent / "repo" -> the server's own launch dir (the default).
            let (d, sess) = match query_param(query, "scope") {
                Some("global") => match crate::rtk::home_root() {
                    Some(home) => (home, None),
                    None => (dir.to_path_buf(), session),
                },
                Some(p) if p != "repo" && !p.is_empty() => {
                    (PathBuf::from(pct_decode(p)).join(".lens"), None)
                }
                _ => (dir.to_path_buf(), session),
            };
            let mut v = snapshot_json_since(&d, sess, since, until);
            // Repo picker: the projects known to the machine-global session store, and
            // which project the server's own dir maps to (so the page can preselect it).
            // Keep only real lens repos — a still-present `<proj>/.lens` dir that isn't the
            // global home itself — which drops the tempdir/parent-dir noise the session
            // store also records (deleted test tmpdirs no longer have a `.lens`). Recency
            // order comes from the query; cap so a huge history stays a usable list.
            if let Some(home) = crate::rtk::home_root() {
                if let Ok(ss) = crate::session::store::SessionStore::open(&home) {
                    if let Ok(projs) = ss.distinct_projects() {
                        let real: Vec<String> = projs
                            .into_iter()
                            .filter(|p| {
                                let d = Path::new(p).join(".lens");
                                d.is_dir() && d != home
                            })
                            .take(30)
                            .collect();
                        v["projects"] = serde_json::json!(real);
                    }
                }
            }
            v["scope_repo"] = serde_json::json!(dir
                .file_name()
                .and_then(|f| f.to_str())
                .filter(|f| *f == ".lens")
                .and(dir.parent())
                .and_then(|p| p.to_str()));
            (200, "application/json", v.to_string())
        }
        _ => (404, "text/plain; charset=utf-8", "not found".to_string()),
    }
}

/// Value of `key` in a `k=v&k2=v2` query string, if present.
fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then_some(v)
    })
}

/// Minimal percent-decoder for a query value (the repo path the picker sends via
/// `encodeURIComponent`: `%20` spaces, `%2F` slashes). `+` is left literal — the
/// frontend never form-encodes, so a `+` in a path stays a `+`.
fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Read an HTTP request and return its target (the path from the request line).
/// Reads until the end of headers or a small cap; GET requests carry no body.
fn read_request_target(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 16 * 1024 {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let first = text.lines().next()?;
    // "GET /path HTTP/1.1"
    let mut parts = first.split_whitespace();
    let _method = parts.next()?;
    Some(parts.next()?.to_string())
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "OK",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

/// The whole UI: one self-contained page (no external assets). Polls /api/stats
/// every second and computes throughput + sparklines from snapshot deltas.
const INDEX_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>lens dashboard</title>
<style>
  :root{ /* default theme — the original dark palette */
    --bg:#0b0d10; --panel:#14181d; --line:#222a31; --ink:#e6edf3;
    --dim:#8b97a3; --accent:#4cc4b0; --warn:#e0a458; --bad:#e06c75;
  }
  body.t70{ /* retro 70s — opt in via the header theme toggle (remembered) */
    --bg:#1a1410; --panel:#241c15; --line:#3a2c1c; --ink:#f0e2c4;
    --dim:#b09a72; --accent:#e0a93c; --warn:#cc6a2a; --bad:#b0431c;
  }
  *{box-sizing:border-box}
  body{margin:0;background:var(--bg);color:var(--ink);color-scheme:dark;
    font:11px/1.3 ui-monospace,SFMono-Regular,Menlo,Consolas,monospace}
  header{display:flex;align-items:baseline;gap:9px;padding:5px 10px;
    border-bottom:1px solid var(--line);flex-wrap:wrap}
  header h1{font-size:13px;margin:0;letter-spacing:.5px;font-weight:600}
  .live{display:flex;align-items:center;gap:5px;color:var(--dim);font-size:10px}
  .dot{width:7px;height:7px;border-radius:50%;background:var(--accent);
    box-shadow:0 0 6px var(--accent);animation:pulse 1.6s infinite}
  .dot.stale{background:var(--bad);box-shadow:none;animation:none}
  @keyframes pulse{0%,100%{opacity:1}50%{opacity:.35}}
  .grow{flex:1}
  /* Custom dropdown — native <select> option menus are OS-drawn on macOS and can't
     be themed, so we render our own button + listbox. */
  .dd{position:relative;display:inline-block}
  .dd-btn{appearance:none;background:var(--panel) url("data:image/svg+xml;utf8,<svg xmlns='http://www.w3.org/2000/svg' width='10' height='6' viewBox='0 0 10 6'><path d='M1 1l4 4 4-4' fill='none' stroke='%238b97a3' stroke-width='1.4'/></svg>") no-repeat right 5px center;
    color:var(--ink);border:1px solid var(--line);border-radius:5px;
    font:10px ui-monospace,Menlo,monospace;padding:1px 17px 1px 6px;cursor:pointer;white-space:nowrap}
  .dd-btn:hover{border-color:var(--dim)}
  .dd.open>.dd-btn{border-color:var(--accent)}
  .dd-list{position:absolute;z-index:70;top:calc(100% + 3px);left:0;min-width:100%;max-height:280px;
    overflow:auto;background:var(--panel);border:1px solid var(--line);border-radius:6px;
    box-shadow:0 8px 24px rgba(0,0,0,.5);padding:3px;display:none}
  .dd.open>.dd-list{display:block}
  .dd-opt{padding:3px 8px;border-radius:4px;color:var(--ink);white-space:nowrap;cursor:pointer;
    font:10px ui-monospace,Menlo,monospace}
  .dd-opt:hover{background:var(--line)}
  .dd-opt.sel{color:var(--accent)}
  input#winFrom,input#winTo{background:var(--panel);color:var(--ink);border:1px solid var(--line);
    border-radius:5px;font:10px ui-monospace,Menlo,monospace;padding:1px 4px;color-scheme:dark}
  #winDash{color:var(--dim);font-size:10px}
  #fullcharts{display:none}
  body.full #fullcharts{display:grid;grid-template-columns:minmax(0,1fr) minmax(0,1fr);gap:10px;margin-top:3px}
  .bigpanel h2{font-size:9px;color:var(--dim);text-transform:uppercase;letter-spacing:.5px;margin:0 0 7px}
  .bigpanel canvas{width:100%;max-width:100%;height:120px;display:block}
  .hbars{display:flex;flex-direction:column;gap:5px}
  .hbar{display:grid;grid-template-columns:88px 1fr 60px;align-items:center;gap:8px;font-size:10px}
  .hbar .lbl{color:var(--dim);white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
  .hbar .track{background:var(--bg);border-radius:3px;height:11px;overflow:hidden;display:flex}
  .hbar .fill{height:100%;background:var(--accent)}
  .hbar .fill.dim{background:var(--line)}
  .hbar .val{text-align:right;color:var(--ink)}
  .hbar.z{opacity:.35}
  .cost{display:flex;align-items:baseline;gap:7px}
  .cost .dollars{color:var(--accent);font-weight:700;font-size:18px;letter-spacing:.3px}
  .cost .dd-list{left:auto;right:0}
  .saved-top{color:var(--dim);font-size:9px}
  main{padding:9px 12px;display:grid;gap:10px}
  .panel{background:var(--panel);border:1px solid var(--line);border-radius:7px;padding:8px 11px;min-width:0;overflow-x:auto}
  .panel h2{font-size:9px;color:var(--dim);text-transform:uppercase;letter-spacing:.5px;margin:0 0 4px}
  .seclabel{color:var(--dim);font-size:9px;letter-spacing:.3px;margin:2px 0 -3px}
  .seclabel b{color:var(--ink);font-weight:600;text-transform:uppercase;letter-spacing:.5px}
  .strip{display:flex;flex-wrap:wrap;gap:6px 22px;align-items:baseline}
  .strip .st i{color:var(--dim);font-style:normal;font-size:9px;text-transform:uppercase;letter-spacing:.4px;margin-right:4px}
  .strip .st b{font-size:13px;font-weight:600}
  .charts{display:grid;grid-template-columns:minmax(0,1fr) minmax(0,1fr);gap:14px}
  .ch{display:flex;flex-direction:column;min-width:0}
  .ch>span{font-size:9px;color:var(--dim);text-transform:uppercase;letter-spacing:.4px;display:flex;justify-content:space-between}
  .ch>span b{color:var(--accent);font-size:11px}
  .ch canvas{width:100%;max-width:100%;height:28px;display:block;margin-top:2px}
  .row2{display:grid;grid-template-columns:minmax(0,1fr) minmax(0,1fr);gap:7px}
  @media(max-width:760px){.charts,.row2,body.full #fullcharts{grid-template-columns:1fr}}
  table{width:100%;border-collapse:collapse;font-size:10px}
  th,td{text-align:right;padding:2px 6px;border-bottom:1px solid var(--line)}
  th:first-child,td:first-child{text-align:left}
  th{color:var(--dim);font-weight:500;font-size:9px;text-transform:uppercase;letter-spacing:.4px}
  tbody tr:last-child td{border-bottom:none}
  tr.z td{opacity:.5}
  .bar{display:inline-block;height:6px;background:var(--accent);border-radius:2px;vertical-align:middle}
  .mech{display:flex;flex-wrap:wrap;gap:2px 13px}
  .mech span{color:var(--dim)}
  .mech b{color:var(--ink)}
  .dim2{color:var(--dim)}
  .av{display:flex;flex-direction:column;gap:3px}
  .avtot{display:flex;flex-wrap:wrap;gap:3px 18px;align-items:baseline;margin-bottom:4px}
  .avtot .k{color:var(--dim);font-size:9px;text-transform:uppercase;letter-spacing:.4px;margin-right:4px}
  .avtot .v{font-size:13px;font-weight:600}
  .avtot .v.big{color:var(--accent);font-size:15px}
  .avtot .basis{color:var(--dim);font-size:9px}
  .avrow{display:grid;grid-template-columns:96px 1fr;align-items:baseline;gap:9px;font-size:10px}
  .avrow .d{color:var(--accent)}
  .avrow .x{color:var(--ink)}
  .avrow.z{opacity:.4}
  .actline{display:flex;flex-wrap:wrap;gap:3px 20px;align-items:baseline}
  .actline i{color:var(--dim);font-style:normal;font-size:9px;text-transform:uppercase;letter-spacing:.4px;margin-right:4px}
  .actline b{font-size:13px;font-weight:600}
  .actrow{display:flex;align-items:center;gap:10px;margin-top:3px}
  .actrow canvas{flex:1;min-width:0;height:22px;display:block}
  .actrow .rate{color:var(--warn);font-size:10px;white-space:nowrap}
  .cats{display:flex;flex-wrap:wrap;gap:2px 13px;margin-top:5px;font-size:10px}
  .cats span{color:var(--dim)}
  .cats b{color:var(--warn)}
  .tname{cursor:help;border-bottom:1px dotted var(--line);padding-bottom:1px}
  .tip{position:fixed;z-index:60;display:none;max-width:320px;background:var(--panel);
    border:1px solid var(--line);border-radius:6px;padding:8px 10px;font-size:11px;
    line-height:1.5;color:var(--ink);box-shadow:0 8px 24px rgba(0,0,0,.5);pointer-events:none}
  footer{color:var(--dim);font-size:9px;padding:3px 10px 8px;text-align:center}
</style>
</head>
<body>
<header>
  <h1>lens</h1>
  <div class="live"><span class="dot" id="dot"></span><span id="status">connecting…</span></div>
  <div id="win" class="dd"></div>
  <input id="winFrom" type="datetime-local" title="custom range start" style="display:none">
  <span id="winDash" style="display:none">→</span>
  <input id="winTo" type="datetime-local" title="custom range end (blank = now)" style="display:none">
  <div id="scope" class="dd"></div>
  <div id="view" class="dd"></div>
  <div id="theme" class="dd"></div>
  <div class="grow"></div>
  <div class="cost" id="cost" title="estimated value of the tokens saved, priced at the selected model's input rate"><span class="dollars" id="dollars">$—</span><div id="rate" class="dd"></div></div>
  <div class="saved-top" id="savedTop">— saved</div>
</header>
<main>
  <div class="panel strip" id="strip"></div>
  <div class="panel charts">
    <div class="ch"><span>tokens saved / min <b id="savedRate">—</b></span><canvas id="savedChart"></canvas></div>
    <div class="ch"><span>bytes returned / min <b id="bytesRate">—</b></span><canvas id="bytesChart"></canvas></div>
  </div>
  <div class="seclabel"><b>by tool</b> + tool adoption &middot; saved &asymp; input tokens avoided &middot; dim = no calls in window</div>
  <div class="panel">
    <table id="tools"><thead><tr>
      <th>tool</th><th>ops</th><th title="raw bytes the tool processed in the darkroom">raw</th><th title="bytes actually returned to context">ret</th><th title="input tokens avoided ≈ (raw − returned) ÷ 4">saved~tok</th><th title="share of bytes kept out of context">save%</th><th title="offloaded ops · total bytes offloaded to the store">off</th><th title="errors">err</th><th title="timeouts">to</th>
    </tr></thead><tbody></tbody></table>
  </div>
  <div class="row2">
    <div class="panel"><h2>by mechanism</h2><div class="mech" id="byMech"></div></div>
    <div class="panel"><h2>RTK shell savings</h2><div class="mech" id="rtkCards"></div></div>
  </div>
  <div class="seclabel"><b>applied value</b> &middot; benchmark rates &times; your live ops &middot; <span id="avNote">estimated, not measured this session</span></div>
  <div class="panel"><div class="av" id="appliedValue"></div></div>
  <div class="seclabel"><b>session activity</b> &middot; built-in tools (Read / Edit / Bash) via hooks &middot; not token savings</div>
  <div class="panel">
    <div class="actline" id="actLine"></div>
    <div class="actrow"><canvas id="actChart"></canvas><span class="rate" id="actRate">—</span></div>
    <div class="cats" id="byCat"></div>
  </div>
  <section id="fullcharts">
    <div class="panel bigpanel"><h2>tokens saved (cumulative)</h2><canvas id="cumChart"></canvas></div>
    <div class="panel bigpanel"><h2>saved tokens by tool</h2><div class="hbars" id="savedByTool"></div></div>
    <div class="panel bigpanel"><h2>tool usage (ops)</h2><div class="hbars" id="opsByTool"></div></div>
    <div class="panel bigpanel"><h2>compression per tool &middot; returned (filled) + saved (dim)</h2><div class="hbars" id="compByTool"></div></div>
  </section>
</main>
<footer id="footer">—</footer>
<script>
// Canonical lens MCP tools, shown in the merged tool table even at 0 calls.
const ADOPTION_TOOLS=['lens_run','lens_run_file','lens_search','lens_index','lens_map','lens_recall','lens_symbol','lens_links','lens_path','lens_find'];
// Hover description per tool, shown as a native title tooltip on the tool name.
const TOOL_DESC={
  lens_run:"Run code (python/js/ts/bash/ruby/go) in a darkroom subprocess; only stdout/stderr returns to context, not the data the script read. Large output is offloaded with a recall ref.",
  lens_run_file:"Analyze one file in the darkroom; your code gets the file path as its first CLI arg (sys.argv[1] / process.argv[2] / $1). Only what it prints returns; the file's bytes stay out of context.",
  lens_index:"Build a full-text index over a file or directory (respects .gitignore). Returns files indexed and chunk count; prerequisite for lens_search.",
  lens_search:"Run one or more BM25-ranked full-text queries in a single call. Returns the top snippets per query with path and relevance score; answers 'where is X mentioned'.",
  lens_map:"Parse the whole repo with tree-sitter into a symbol graph (functions, types, modules) and their relationships (calls, imports, contains). Run once before the other graph tools.",
  lens_symbol:"Find graph symbols by name substring (+ optional kind) and return each with its immediate connections: where a symbol lives and what directly touches it.",
  lens_find:"Find symbols by a natural-language query, ranked lexically by word overlap with symbol names. Use when you know what a symbol does but not its exact name.",
  lens_links:"Return the local subgraph within N hops of a node id: a symbol's neighborhood or blast radius at a chosen depth.",
  lens_path:"Find the shortest path between two symbols via BFS over graph edges: how A reaches B through the call/import chain.",
  lens_recall:"Recover the full blob behind a retrieve_ref returned by another tool, reversing any truncation or offloading.",
  lens_skeleton:"Show a source file's structure cheaply: signatures, types, and nesting with executable bodies elided to '...'. Far fewer tokens than reading the whole file; full text is one lens_recall away.",
  lens_grep_ast:"Structural code search via a tree-sitter query (S-expression): matches syntax, not text, so it finds real calls without the false positives grep hits in comments or strings. Returns path:line matches.",
  lens_overview:"Token-budgeted repo overview: the most structurally important symbols (PageRank-ranked) with their callers and callees, as much as fits a token budget. A high-signal map of a codebase at fixed cost.",
  lens_stats:"Report darkroom usage, estimated tokens saved, and current index/graph sizes for this repo."
};
let rtkBase=null;
// Custom dropdown: a styled button + listbox. Native <select> option menus are drawn by
// the OS on macOS and can't be themed, so we render our own. API mirrors the slice of
// <select> the page uses: set(options,value), .value, selectedLabel(), onChange(cb).
function makeDD(el,title){
  el.classList.add('dd');
  const btn=document.createElement('button'); btn.type='button'; btn.className='dd-btn'; if(title)btn.title=title;
  const lbl=document.createElement('span'); btn.appendChild(lbl);
  const list=document.createElement('div'); list.className='dd-list';
  el.appendChild(btn); el.appendChild(list);
  let opts=[], val=null, cb=null;
  function draw(){
    const cur=opts.find(o=>o.value===val);
    lbl.textContent=cur?cur.label:'';
    list.innerHTML='';
    opts.forEach(o=>{
      const d=document.createElement('div'); d.className='dd-opt'+(o.value===val?' sel':''); d.textContent=o.label; if(o.title)d.title=o.title;
      d.addEventListener('click',function(ev){ev.stopPropagation(); el.classList.remove('open'); if(o.value!==val){val=o.value; draw(); if(cb)cb(val);}});
      list.appendChild(d);
    });
  }
  btn.addEventListener('click',function(ev){ev.stopPropagation(); const open=el.classList.contains('open');
    document.querySelectorAll('.dd.open').forEach(x=>x.classList.remove('open'));
    if(!open){el.classList.add('open'); const s=list.querySelector('.dd-opt.sel'); if(s)s.scrollIntoView({block:'nearest'});}});
  return {
    set(o,v){opts=o.slice(); if(v!==undefined)val=v; if(!opts.some(x=>x.value===val)&&opts.length)val=opts[0].value; draw();},
    get value(){return val;},
    set value(v){val=v; draw();},
    selectedLabel(){const c=opts.find(o=>o.value===val); return c?c.label:'';},
    onChange(fn){cb=fn;}
  };
}
document.addEventListener('click',function(){document.querySelectorAll('.dd.open').forEach(x=>x.classList.remove('open'));});
document.addEventListener('keydown',function(e){if(e.key==='Escape')document.querySelectorAll('.dd.open').forEach(x=>x.classList.remove('open'));});

// Scope: which repo's data to show. 'global' = machine-global aggregate; a repo path =
// that repo's own .lens. The picker is populated from the first snapshot (it carries the
// project list); until then we fetch the stored or server-derived scope.
let scope=null;
try{const s=localStorage.getItem('lens_scope');if(s)scope=s;}catch(e){}
const scopeDD=makeDD(document.getElementById('scope'),'which repo to show: a single project, or all repos combined');
let scopeBuilt=false;
function repoName(p){const parts=(p||'').split('/').filter(Boolean);return parts.length?parts[parts.length-1]:(p||'');}
function scopeLabel(){return scope==='global'?'GLOBAL':(scope?repoName(scope):'');}
function buildScope(projs,repoPath){
  projs=projs||[];
  const opts=[{label:'global',value:'global'}].concat(projs.map(p=>({label:repoName(p),value:p,title:p})));
  const valid=v=> v==='global' || projs.includes(v);
  if(!valid(scope)) scope = valid(repoPath)?repoPath:'global';
  scopeDD.set(opts,scope);
  try{localStorage.setItem('lens_scope',scope);}catch(e){}
  scopeDD.onChange(function(v){scope=v;try{localStorage.setItem('lens_scope',scope);}catch(e){}tick();});
  scopeBuilt=true;
}

// Time window: presets scope [since, now]; the custom option opens a start/end date
// range so any window is selectable. RTK savings stay "since page open" (rtk gain is a
// cumulative counter that can't honor an arbitrary cutoff).
const PAGELOAD=Math.floor(Date.now()/1000);
let activeSince=0, activeUntil=0, winMode='all', atLabel='';
const winDD=makeDD(document.getElementById('win'),'time window for savings + activity');
const winFrom=document.getElementById('winFrom'), winTo=document.getElementById('winTo'), winDash=document.getElementById('winDash');
let winOpts=[];
function addOpt(label,val){winOpts.push({label,value:val});}
(function buildWinOptions(){
  addOpt('live','live'); addOpt('last 15m','15m'); addOpt('last 1h','1h'); addOpt('last 3h','3h'); addOpt('today','today');
  // Concrete top-of-hour marks so "since 2pm" is a one-click pick, no fiddly time spinner.
  const h0=new Date().getHours();
  for(let h=h0; h>=Math.max(0,h0-7); h--){
    const d=new Date(); d.setHours(h,0,0,0);
    if(d.getTime()>Date.now()) continue;
    addOpt('since '+d.toLocaleTimeString([],{hour:'numeric',minute:'2-digit'}), 'at:'+Math.floor(d.getTime()/1000));
  }
  addOpt('all time','all'); addOpt('custom range…','custom');
})();
function presetSince(m){
  const now=Math.floor(Date.now()/1000);
  if(m==='all') return 0;
  if(m==='today'){const d=new Date();d.setHours(0,0,0,0);return Math.floor(d.getTime()/1000);}
  if(m==='3h') return now-10800;
  if(m==='1h') return now-3600;
  if(m==='15m') return now-900;
  return PAGELOAD; // live
}
// datetime-local value ("YYYY-MM-DDTHH:MM", local) -> unix seconds; 0 when blank.
function dtSecs(el){const v=el.value; if(!v) return 0; const t=Date.parse(v); return isNaN(t)?0:Math.floor(t/1000);}
function fmtTime(s){return new Date(s*1000).toLocaleString([],{month:'short',day:'numeric',hour:'numeric',minute:'2-digit'});}
function winLabel(){
  if(winMode==='custom'){return (activeSince?fmtTime(activeSince):'start')+' → '+(activeUntil?fmtTime(activeUntil):'now');}
  const at=new Date(activeSince*1000).toLocaleTimeString();
  if(winMode==='all') return 'all time';
  if(winMode==='live') return 'live · since '+at;
  if(winMode==='today') return 'today · since '+at;
  if(winMode==='3h') return 'last 3h · since '+at;
  if(winMode==='1h') return 'last 1h · since '+at;
  if(winMode==='15m') return 'last 15m · since '+at;
  if(winMode==='at') return atLabel;
  return 'since '+at;
}
function applyWin(){tick();}
// Update window state from a selector value, without ticking or focusing — shared by
// the change handler and the on-load restore.
function applyWinValue(v){
  const custom = v==='custom';
  winFrom.style.display=winTo.style.display=winDash.style.display = custom?'':'none';
  if(custom){ winMode='custom'; activeSince=dtSecs(winFrom); activeUntil=dtSecs(winTo); return; }
  activeUntil=0;
  if(v.indexOf('at:')===0){ winMode='at'; atLabel=winDD.selectedLabel(); activeSince=parseInt(v.slice(3),10); }
  else { winMode=v; activeSince=presetSince(v); }
}
// Restore the last-picked window, but only if its option still exists: the concrete
// "since 2pm" marks regenerate per current hour, so an old absolute pick may be gone —
// fall back to all time then. Presets ('live','15m',…) always exist and recompute now.
(function restoreWin(){
  let w='all';
  try{const s=localStorage.getItem('lens_win');if(s)w=s;
      const f=localStorage.getItem('lens_winfrom');if(f!=null)winFrom.value=f;
      const t=localStorage.getItem('lens_winto');if(t!=null)winTo.value=t;}catch(e){}
  if(!winOpts.some(o=>o.value===w)) w='all';
  winDD.set(winOpts,w); applyWinValue(w);
})();
winDD.onChange(function(v){
  applyWinValue(v);
  try{localStorage.setItem('lens_win',v);}catch(e){}
  if(v==='custom') winFrom.focus();
  applyWin();
});
function commitRange(){
  if(winMode!=='custom') return;
  activeSince=dtSecs(winFrom); activeUntil=dtSecs(winTo);
  try{localStorage.setItem('lens_winfrom',winFrom.value);localStorage.setItem('lens_winto',winTo.value);}catch(e){}
  // Reject an inverted range (end at/before start): flag both inputs, don't fetch.
  if(activeSince&&activeUntil&&activeUntil<=activeSince){
    [winFrom,winTo].forEach(el=>{el.style.borderColor='var(--bad)';setTimeout(()=>{el.style.borderColor='';},900);});
    return;
  }
  applyWin();
}
winFrom.addEventListener('change',commitRange);
winTo.addEventListener('change',commitRange);

// Cost estimate: "tokens saved" are context INPUT tokens you avoided sending, so price
// them at the input rate. The header dropdown chooses the model; remembered in localStorage.
const RATES=[{m:'Opus 4.8',r:5},{m:'Fable 5',r:10},{m:'Sonnet 5',r:3},{m:'Haiku 4.5',r:1}];
let rateIdx=0;
try{const s=localStorage.getItem('lens_rate_model');const i=RATES.findIndex(x=>x.m===s);if(i>=0)rateIdx=i;}catch(e){}
let savedTotal=0, lastAv=null;
function money(v){return '$'+(v>=1?v.toFixed(2):v>=0.01?v.toFixed(3):v.toFixed(4));}
function renderCost(){
  document.getElementById('dollars').textContent=money(savedTotal*RATES[rateIdx].r/1e6)+' saved';
}
// Applied-value plane. Rendered from a cached snapshot so switching model re-prices the
// "est. value" total instantly (the token/round-trip figures are rate-independent).
function renderApplied(av){
  const rts=av.round_trips_avoided||0, x=RATES[rateIdx];
  document.getElementById('appliedValue').innerHTML=
    `<div class="avtot">`+
      `<span><i class="k">measured saved</i><b class="v">${humanCount(av.measured_tokens||0)} tok</b></span>`+
      `<span><i class="k">est. counterfactual</i><b class="v">+${humanCount(av.est_counterfactual_tokens||0)} tok</b></span>`+
      `<span><i class="k">est. total avoided</i><b class="v big">${humanCount(av.est_total_tokens||0)} tok</b></span>`+
      `<span><i class="k">est. value</i><b class="v big">${money((av.est_total_tokens||0)*x.r/1e6)}</b> <span class="basis">@ $${x.r}/M · ${x.m}</span></span>`+
      `<span><i class="k">round-trips avoided</i><b class="v">~${Math.round(rts)}</b></span>`+
      `<span><i class="k">time saved</i><b class="v big">${humanTime(rts*RT_SECONDS)}</b> <span class="basis">@ ${RT_SECONDS}s/round-trip</span></span>`+
    `</div>`+
    (av.rows||[]).map(r=>{
      const parts=[];
      if(r.est_tokens>0) parts.push(`~${humanCount(r.est_tokens)} tok`);
      if(r.round_trips>0) parts.push(`~${r.round_trips.toFixed(1)} rt`);
      if(r.dimension==='darkroom'||r.dimension==='skeleton') parts.push('tok measured live');
      const txt=parts.length?parts.join(', '):'—';
      return `<div class="avrow${r.ops?'':' z'}" title="source ${r.source||'modeling floor'}"><span class="d">${r.dimension} <span class="dim2">×${r.ops}</span></span><span class="x">${txt}</span></div>`;
    }).join('');
  document.getElementById('avNote').textContent=(av.note||'')+(av.model?(' · '+av.model):'');
}
// Model picker: option value is the RATES index; prices the saved tokens at that model's
// input $/M and re-renders the applied-value plane. Remembered by model name.
const rateDD=makeDD(document.getElementById('rate'),'model to price saved tokens against');
rateDD.set(RATES.map((x,i)=>({label:'$'+x.r+'/M · '+x.m,value:i})),rateIdx);
rateDD.onChange(function(v){
  rateIdx=v;
  try{localStorage.setItem('lens_rate_model',RATES[rateIdx].m);}catch(e){}
  renderCost(); if(lastAv) renderApplied(lastAv);
});
renderCost();

function humanBytes(n){
  const u=['B','KB','MB','GB','TB']; let v=n,i=0;
  while(v>=1024&&i<u.length-1){v/=1024;i++;}
  return i===0?n+' B':v.toFixed(1)+' '+u[i];
}
function humanCount(n){
  if(n>=1e6) return (n/1e6).toFixed(1)+'M';
  if(n>=1e3) return (n/1e3).toFixed(1)+'K';
  return ''+n;
}
// Seconds per avoided agent round-trip (tool call + model turn). Matches the TUI's
// ROUND_TRIP_SECONDS default; the figure is shown so the assumption is visible.
const RT_SECONDS=4;
function humanTime(s){
  if(s<90) return '~'+Math.round(s)+'s';
  if(s<5400) return '~'+(s/60).toFixed(1)+' min';
  return '~'+(s/3600).toFixed(1)+' h';
}
// Current value of a CSS custom property off <body>, so canvas colors track the theme.
function cssVar(n){return getComputedStyle(document.body).getPropertyValue(n).trim()||'#4cc4b0';}
function spark(id,series,color,fromZero){
  const c=document.getElementById(id); if(!c.clientWidth) return; const dpr=window.devicePixelRatio||1;
  const w=c.clientWidth, h=c.clientHeight;
  c.width=w*dpr; c.height=h*dpr;
  const x=c.getContext('2d'); x.scale(dpr,dpr); x.clearRect(0,0,w,h);
  if(series.length<2) return;
  const n=series.length, px=i=>i/(n-1)*w;
  let py;
  if(fromZero===false){ // cumulative: auto-scale to min..max so the shape shows; flat -> centered line
    const mx=Math.max(...series), mn=Math.min(...series), pad=4;
    py = mx===mn ? (()=>h/2) : (v=>h-pad-((v-mn)/(mx-mn))*(h-2*pad));
  } else {              // rates: 0-baseline (a spike from zero should read as a spike)
    const max=Math.max(1,...series); py=v=>h-3-(v/max)*(h-6);
  }
  x.beginPath();
  series.forEach((v,i)=>{const X=px(i),Y=py(v); i?x.lineTo(X,Y):x.moveTo(X,Y);});
  x.lineTo(w,h); x.lineTo(0,h); x.closePath();
  x.fillStyle=color+'22'; x.fill();
  x.beginPath();
  series.forEach((v,i)=>{const X=px(i),Y=py(v); i?x.lineTo(X,Y):x.moveTo(X,Y);});
  x.strokeStyle=color; x.lineWidth=1.5; x.stroke();
}
function diffs(a){return a.map((v,i)=>i?v-a[i-1]:v);}
function stat(k,v){return `<span class="st"><i>${k}</i><b>${v}</b></span>`;}
function hbar(label,frac,val,zero){
  const w=Math.max(0,Math.min(100,Math.round(frac*100)));
  return `<div class="hbar${zero?' z':''}"><span class="lbl">${label}</span><span class="track"><span class="fill" style="width:${w}%"></span></span><span class="val">${val}</span></div>`;
}
function compbar(label,raw,ret,maxraw){
  const retW=ret/maxraw*100, savW=Math.max(0,(raw-ret)/maxraw*100), pct=raw>0?Math.round((raw-ret)/raw*100):0;
  return `<div class="hbar"><span class="lbl">${label}</span><span class="track"><span class="fill" style="width:${retW}%"></span><span class="fill dim" style="width:${savW}%"></span></span><span class="val">${pct}%</span></div>`;
}
function setStale(){
  document.getElementById('dot').classList.add('stale');
  document.getElementById('status').textContent='disconnected — retrying';
}
async function tick(){
  let d;
  const useScope=scope;
  const sp = useScope==='global'?'&scope=global':(useScope?'&scope='+encodeURIComponent(useScope):'');
  const uq = activeUntil?('&until='+activeUntil):'';
  try{ d=await (await fetch('/api/stats?since='+activeSince+uq+sp,{cache:'no-store'})).json(); }
  catch(e){ setStale(); return; }
  document.getElementById('dot').classList.remove('stale');
  // First snapshot carries the project list; build the repo picker, then refetch if
  // resolving the stored scope landed on a different repo than we just fetched.
  if(!scopeBuilt){ buildScope(d.projects,d.scope_repo); if(scope!==useScope){ tick(); return; } }
  const sl=scopeLabel();
  document.getElementById('status').textContent=(sl?sl+' · ':'')+winLabel()+(d.session?(' · '+d.session):'')+' · '+(d.activity&&d.activity.sessions||0)+' session(s)';

  const savedMcp=(d.tokens_saved_mcp!==undefined?d.tokens_saved_mcp:d.tokens_saved_est);
  // Windowed sparklines: the backend buckets the selected window [since, now];
  // diff the cumulative buckets for per-bucket throughput, and divide the window
  // total by window-minutes for the rate label.
  const winMin=Math.max(1/60,((d.window_end||0)-(d.window_start||0))/60);
  const sB=d.saved_buckets||[], bB=d.bytes_buckets||[], eB=d.event_buckets||[];

  // Stat strip — the old six cards, now one compact line.
  const overallPct=d.raw_bytes_in>0?Math.round((d.raw_bytes_in-d.bytes_returned)/d.raw_bytes_in*100):0;
  const fired=ADOPTION_TOOLS.filter(n=>d.by_tool.some(t=>t.tool===n&&t.ops>0)).length;
  document.getElementById('strip').innerHTML=
    stat('ops',d.ops.toLocaleString()+' ('+d.errors+' err, '+d.timeouts+' timeout)')+
    stat('raw in',humanBytes(d.raw_bytes_in))+
    stat('returned',humanBytes(d.bytes_returned))+
    stat('saved',humanCount(savedMcp)+' tok')+
    stat('save%',overallPct+'%')+
    stat('tools used',fired+'/'+ADOPTION_TOOLS.length)+
    stat('offloaded',d.offloaded_ops+' ('+humanBytes(d.offloaded_bytes)+')')+
    stat('lock',d.lock_wait_ms+' ms');

  const cAccent=cssVar('--accent'), cWarn=cssVar('--warn');
  spark('savedChart',diffs(sB),cAccent);
  spark('bytesChart',diffs(bB),cAccent);
  document.getElementById('savedRate').textContent=humanCount(Math.round((sB.at(-1)||0)/winMin))+' tok/min';
  document.getElementById('bytesRate').textContent=humanBytes(Math.round((bB.at(-1)||0)/winMin))+'/min';

  // Merged tool table = by-tool (savings) + tool adoption (firing/err/timeout) in one.
  // Canonical tools always listed (dim if 0 calls) so a dormant tool is visible, not absent.
  const tmap={}; d.by_tool.forEach(t=>tmap[t.tool]=t);
  const extra=d.by_tool.map(t=>t.tool).filter(n=>!ADOPTION_TOOLS.includes(n));
  const rows=ADOPTION_TOOLS.concat(extra);
  const maxRaw=Math.max(1,...d.by_tool.map(t=>t.raw));
  document.querySelector('#tools tbody').innerHTML=rows.map(name=>{
    const t=tmap[name];
    const ops=t?t.ops:0, raw=t?t.raw:0, ret=t?t.returned:0, saved=t?t.saved:0;
    const offc=t?(t.offloaded_ops||0):0, offb=t?(t.offloaded_bytes||0):0;
    const err=t?t.errors:0, to=t?t.timeouts:0;
    const w=Math.round(raw/maxRaw*48);
    const pct=raw>0?Math.round((raw-ret)/raw*100):null;
    const offtxt=offc?(offc+'·'+humanBytes(offb)):'—';
    const desc=TOOL_DESC[name]||'Historical or third-party tool recorded in this data dir; not a current lens tool.';
    const nm=`<span class="tname" data-desc="${desc}">${name}</span>`;
    return `<tr${ops?'':' class="z"'}><td>${nm}</td><td>${ops.toLocaleString()}</td>`+
      `<td>${t?('<span class="bar" style="width:'+w+'px"></span> '+humanBytes(raw)):'—'}</td>`+
      `<td>${t?humanBytes(ret):'—'}</td><td>${saved?saved.toLocaleString():'—'}</td>`+
      `<td>${pct==null?'—':pct+'%'}</td>`+
      `<td>${offtxt}</td><td>${err||'—'}</td><td>${to||'—'}</td></tr>`;
  }).join('');

  document.getElementById('byMech').innerHTML=d.by_mechanism.map(m=>
    `<span>${m.mechanism} <b>${m.ops}</b>op · <b>${humanCount(m.saved)}</b>tok</span>`
  ).join('')||'<span class="dim2">—</span>';

  // RTK shell savings — delta from a first-poll baseline (rtk gain is cumulative).
  const r=d.rtk||{installed:false};
  if(r.installed){
    if(!rtkBase) rtkBase={commands:r.total_commands||0,saved:r.total_saved||0,input:r.total_input||0};
    const dCmd=Math.max(0,(r.total_commands||0)-rtkBase.commands);
    const dSaved=Math.max(0,(r.total_saved||0)-rtkBase.saved);
    const dInput=Math.max(0,(r.total_input||0)-rtkBase.input);
    const pct=dInput>0?(dSaved/dInput*100):0;
    document.getElementById('rtkCards').innerHTML=
      `<span>cmds <b>${dCmd.toLocaleString()}</b></span>`+
      `<span>saved <b>${humanCount(dSaved)}</b>tok</span>`+
      `<span>avg <b>${pct.toFixed(1)}%</b></span>`+
      `<span class="dim2">since opened</span>`;
    document.getElementById('savedTop').textContent='≈ '+humanCount((d.tokens_saved_mcp||0)+dSaved)+' tok';
    savedTotal=(d.tokens_saved_mcp||0)+dSaved; renderCost();
  } else {
    document.getElementById('rtkCards').innerHTML='<span class="dim2">not installed — run lens rtk install</span>';
    document.getElementById('savedTop').textContent='≈ '+humanCount(savedMcp)+' tok';
    savedTotal=savedMcp; renderCost();
  }

  // Applied value — benchmark per-op rates × your live op counts. Estimates only;
  // never folded into savedTotal / the $ headline. Time = round-trips × RT_SECONDS.
  lastAv=d.applied_value||{rows:[],measured_tokens:0,est_counterfactual_tokens:0,est_total_tokens:0,round_trips_avoided:0,note:'',model:''};
  renderApplied(lastAv);

  // session activity (built-in tools via hooks)
  const a=d.activity||{total_events:0,sessions:0,by_category:[],last_ts:null};
  document.getElementById('actLine').innerHTML=
    `<span class="st"><i>events</i><b>${(a.total_events||0).toLocaleString()}</b></span>`+
    `<span class="st"><i>sessions</i><b>${a.sessions||0}</b></span>`+
    `<span class="st"><i>last</i><b>${a.last_ts?new Date(a.last_ts*1000).toLocaleTimeString():'—'}</b></span>`;
  spark('actChart',diffs(eB),cWarn);
  document.getElementById('actRate').textContent=Math.round((eB.at(-1)||0)/winMin)+' ev/min';
  document.getElementById('byCat').innerHTML=a.by_category.map(c=>
    `<span>${c.category} <b>${c.count}</b></span>`
  ).join('')||'<span class="dim2">no activity captured yet</span>';

  document.getElementById('footer').textContent=
    `store ${humanBytes(d.store_size)} · index ${d.index_chunks} · `+
    `graph ${d.graph_nodes}n/${d.graph_edges}e · updated ${d.ts}`;

  // Expansive full-view charts (computed always; CSS shows them only in full mode).
  // Cumulative uses the server-bucketed timeline so it reflects the whole selected
  // window, not just samples taken since the page opened.
  spark('cumChart', d.saved_buckets||[], cAccent, false);
  const tt=d.by_tool;
  const bySaved=tt.filter(t=>t.saved>0).sort((a,b)=>b.saved-a.saved).slice(0,12);
  const maxSaved=Math.max(1,...bySaved.map(t=>t.saved));
  document.getElementById('savedByTool').innerHTML=bySaved.map(t=>hbar(t.tool,t.saved/maxSaved,humanCount(t.saved),false)).join('')||'<span class="dim2">no savings yet</span>';
  const byOps=tt.filter(t=>t.ops>0).sort((a,b)=>b.ops-a.ops).slice(0,12);
  const maxOps=Math.max(1,...byOps.map(t=>t.ops));
  document.getElementById('opsByTool').innerHTML=byOps.map(t=>hbar(t.tool,t.ops/maxOps,t.ops.toLocaleString(),false)).join('')||'<span class="dim2">no calls yet</span>';
  const comp=tt.filter(t=>t.raw>0).sort((a,b)=>b.raw-a.raw).slice(0,12);
  const maxCompRaw=Math.max(1,...comp.map(t=>t.raw));
  document.getElementById('compByTool').innerHTML=comp.map(t=>compbar(t.tool,t.raw,t.returned,maxCompRaw)).join('')||'<span class="dim2">no offloading tool calls yet</span>';
}
let view='mini';
const viewDD=makeDD(document.getElementById('view'),'layout: mini fits a narrow pane (cmux); full uses the whole window');
function applyView(){document.body.classList.toggle('full',view==='full');}
try{const v=localStorage.getItem('lens_view');if(v==='full')view='full';}catch(e){}
viewDD.set([{label:'mini',value:'mini'},{label:'full',value:'full'}],view);
applyView();
viewDD.onChange(function(v){view=v;try{localStorage.setItem('lens_view',view);}catch(e){}applyView();tick();});
// Color theme — dark (the :root default) or retro 70s (body.t70), remembered.
// Sparklines read --accent/--warn at draw time, so they follow with no extra wiring.
const THEMES=['dark','seventies'];
let theme='dark';
try{const t=localStorage.getItem('lens_theme');if(THEMES.includes(t))theme=t;}catch(e){}
const themeDD=makeDD(document.getElementById('theme'),'color theme');
function applyTheme(){document.body.classList.toggle('t70',theme==='seventies');}
themeDD.set([{label:'dark',value:'dark'},{label:'70s',value:'seventies'}],theme);
applyTheme();
themeDD.onChange(function(v){theme=v;try{localStorage.setItem('lens_theme',theme);}catch(e){}applyTheme();tick();});
// Instant styled tooltip for tool descriptions (native title is delayed + clipped by the panel).
(function(){
  const tip=document.createElement('div'); tip.className='tip'; document.body.appendChild(tip);
  let cur=null;
  function place(el){
    tip.textContent=el.getAttribute('data-desc')||'';
    tip.style.display='block';
    const r=el.getBoundingClientRect(), tw=tip.offsetWidth, th=tip.offsetHeight;
    let x=r.left, y=r.bottom+6;
    if(x+tw>innerWidth-8) x=Math.max(8,innerWidth-8-tw);
    if(y+th>innerHeight-8) y=Math.max(8,r.top-6-th);
    tip.style.left=x+'px'; tip.style.top=y+'px';
  }
  const tbl=document.getElementById('tools');
  tbl.addEventListener('mouseover',function(e){
    const el=e.target.closest('.tname');
    if(el){ if(el!==cur){cur=el; place(el);} } else { cur=null; tip.style.display='none'; }
  });
  tbl.addEventListener('mouseleave',function(){ cur=null; tip.style.display='none'; });
})();
tick(); setInterval(tick,1000);
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obs::OpLog;
    use serde_json::{json, Value};
    use tempfile::tempdir;

    #[test]
    fn route_serves_html_and_json_and_404() {
        let dir = tempdir().unwrap();
        OpLog::open(dir.path()).start("lens_run", json!({})).finish(
            8000,
            100,
            Some("a".into()),
            "ok",
            "",
            None,
        );
        // Seed session-hook activity (the "first plane") so the JSON carries it.
        let ss = crate::session::store::SessionStore::open(dir.path()).unwrap();
        ss.insert_events(&[crate::session::Event {
            session_id: "s1".into(),
            project: "/p".into(),
            timestamp: 100,
            category: "file".into(),
            priority: 1,
            payload: json!({"path": "x.rs"}),
            source_hook: "PostToolUse".into(),
        }])
        .unwrap();

        let (s, ct, body) = route("/api/stats?x=1", dir.path(), None);
        assert_eq!(s, 200);
        assert!(ct.contains("json"));
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ops"], json!(1));
        assert!(v["tokens_saved_est"].as_i64().unwrap() > 0);
        assert_eq!(v["by_tool"][0]["tool"], json!("lens_run"));
        // Per-tool offload breakdown for the measured-savings vs adoption split: the
        // seeded op stored a blob (store_ref) with raw > returned, so it offloaded.
        assert_eq!(v["by_tool"][0]["offloaded_ops"], json!(1));
        // Session-activity block reflects the seeded hook event.
        assert_eq!(v["activity"]["total_events"], json!(1));
        assert_eq!(v["activity"]["by_category"][0]["category"], json!("file"));
        // The RTK plane (third plane) is always present; its `installed` flag is a
        // bool whose value depends on machine state, so assert structure only.
        assert!(v["rtk"].is_object(), "snapshot must carry an rtk object");
        assert!(
            v["rtk"]["installed"].is_boolean(),
            "rtk.installed is a bool"
        );
        // The applied-value plane: 5 dimension rows + estimated totals, flagged as an
        // estimate (never folded into the $ ledger).
        assert_eq!(v["applied_value"]["rows"].as_array().unwrap().len(), 5);
        assert_eq!(v["applied_value"]["estimate"], json!(true));
        assert!(v["applied_value"]["round_trips_avoided"].is_number());
        assert_eq!(v["applied_value"]["measured_tokens"], v["tokens_saved_mcp"]);
        assert!(v["applied_value"]["model"]
            .as_str()
            .unwrap()
            .contains("claude-opus-4-8"));

        let (s2, ct2, body2) = route("/", dir.path(), None);
        assert_eq!(s2, 200);
        assert!(ct2.contains("html"));
        assert!(body2.contains("lens"));
        assert!(body2.contains("/api/stats"));
        // The RTK shell-savings panel markup is baked into the self-contained page.
        assert!(body2.contains("RTK shell savings"));
        assert!(body2.contains("rtkCards"));
        // The tool-adoption panel and its canonical tool list are baked into the page.
        assert!(body2.contains("tool adoption"));
        assert!(body2.contains("ADOPTION_TOOLS"));
        assert!(body2.contains("lens_links"));
        // The applied-value panel + its data binding are baked into the page.
        assert!(body2.contains("applied value"));
        assert!(body2.contains("d.applied_value"));
        assert!(body2.contains("id=\"appliedValue\""));
        // Both color themes ship: dark is the restored :root default, 70s is the opt-in
        // body.t70 override, and the header carries the persisted toggle.
        assert!(body2.contains("--accent:#4cc4b0"), "default dark palette must be present");
        assert!(body2.contains("body.t70"), "70s theme override must be present");
        assert!(body2.contains("id=\"theme\""), "theme toggle button must be in the page");

        let (s3, _, _) = route("/nope", dir.path(), None);
        assert_eq!(s3, 404);
    }

    #[test]
    fn tcp_roundtrip_serves_json() {
        let dir = tempdir().unwrap();
        OpLog::open(dir.path())
            .start("lens_search", json!({}))
            .finish(10, 10, None, "ok", "", None);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let dirp = dir.path().to_path_buf();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle(stream, &dirp, None);
        });

        let mut c = TcpStream::connect(addr).unwrap();
        c.write_all(b"GET /api/stats HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap();
        let mut resp = String::new();
        c.read_to_string(&mut resp).unwrap();
        server.join().unwrap();

        assert!(resp.contains("200 OK"), "got: {resp}");
        assert!(resp.contains("application/json"));
        assert!(resp.contains("\"ops\":1"));
    }

    /// Parity tripwire: both renderers must reference every snapshot display
    /// dimension, and every dimension must exist in a real snapshot. Coarse by design
    /// (proves mention, not correctness) — the shared snapshot is the real guarantee.
    /// Catches "added a panel to the web, forgot the TUI" (or vice versa).
    #[test]
    fn both_renderers_reference_every_snapshot_dimension() {
        use crate::obs::stats::{snapshot_json, SNAPSHOT_DIMENSIONS};
        let tui_src = include_str!("tui.rs");
        for key in SNAPSHOT_DIMENSIONS {
            assert!(INDEX_HTML.contains(key), "web INDEX_HTML omits snapshot key '{key}'");
            assert!(tui_src.contains(key), "tui.rs omits snapshot key '{key}'");
        }
        // Each key is a live snapshot key, not a stale name.
        let dir = tempdir().unwrap();
        let snap = snapshot_json(dir.path(), None);
        for key in SNAPSHOT_DIMENSIONS {
            assert!(snap.get(*key).is_some(), "snapshot missing dimension key '{key}'");
        }
    }
}
