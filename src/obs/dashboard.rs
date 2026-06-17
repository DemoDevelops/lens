//! `ctxforge dashboard` — a local web view of the op log.
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
use std::path::Path;

use anyhow::{Context, Result};

use super::data_dir;
use super::stats::snapshot_json_since;

const DEFAULT_PORT: u16 = 7878;
const DEFAULT_HOST: &str = "127.0.0.1";

/// CLI entry: `args` is everything after `dashboard`.
pub fn run_cli(args: &[String]) -> Result<()> {
    let mut port = DEFAULT_PORT;
    let mut host = DEFAULT_HOST.to_string();
    let mut session: Option<String> = None;
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
            other => {
                eprintln!("ctxforge dashboard: unknown flag '{other}'");
                eprintln!("usage: ctxforge dashboard [--port <n>] [--host <addr>] [--session <id>]");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    let dir = data_dir();
    let listener = TcpListener::bind((host.as_str(), port))
        .with_context(|| format!("binding {host}:{port}"))?;
    println!("ctxforge dashboard serving on http://{host}:{port}  (Ctrl-C to stop)");
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
            (
                200,
                "application/json",
                snapshot_json_since(dir, session, since).to_string(),
            )
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
<title>ctxforge dashboard</title>
<style>
  :root{
    --bg:#0b0d10; --panel:#14181d; --line:#222a31; --ink:#e6edf3;
    --dim:#8b97a3; --accent:#4cc4b0; --warn:#e0a458; --bad:#e06c75;
  }
  *{box-sizing:border-box}
  body{margin:0;background:var(--bg);color:var(--ink);
    font:14px/1.45 ui-monospace,SFMono-Regular,Menlo,Consolas,monospace}
  header{display:flex;align-items:center;gap:14px;padding:14px 20px;
    border-bottom:1px solid var(--line)}
  header h1{font-size:15px;margin:0;letter-spacing:.5px;font-weight:600}
  .live{display:flex;align-items:center;gap:6px;color:var(--dim);font-size:12px}
  .dot{width:8px;height:8px;border-radius:50%;background:var(--accent);
    box-shadow:0 0 8px var(--accent);animation:pulse 1.6s infinite}
  .dot.stale{background:var(--bad);box-shadow:none;animation:none}
  @keyframes pulse{0%,100%{opacity:1}50%{opacity:.35}}
  .grow{flex:1}
  .saved-top{color:var(--accent);font-weight:600}
  main{padding:20px;display:grid;gap:16px;max-width:1100px;margin:0 auto}
  .cards{display:grid;grid-template-columns:repeat(auto-fit,minmax(150px,1fr));gap:12px}
  .card{background:var(--panel);border:1px solid var(--line);border-radius:10px;padding:12px 14px}
  .card .k{color:var(--dim);font-size:11px;text-transform:uppercase;letter-spacing:.6px}
  .card .v{font-size:22px;margin-top:4px;font-weight:600}
  .card .sub{color:var(--dim);font-size:11px;margin-top:2px}
  .row2{display:grid;grid-template-columns:1fr 1fr;gap:16px}
  @media(max-width:760px){.row2{grid-template-columns:1fr}}
  .panel{background:var(--panel);border:1px solid var(--line);border-radius:10px;padding:14px}
  .panel h2{font-size:12px;color:var(--dim);text-transform:uppercase;letter-spacing:.6px;margin:0 0 10px}
  canvas{width:100%;height:90px;display:block}
  .rate{color:var(--accent);font-size:13px;margin-top:6px}
  table{width:100%;border-collapse:collapse;font-size:13px}
  th,td{text-align:right;padding:5px 8px;border-bottom:1px solid var(--line)}
  th:first-child,td:first-child{text-align:left}
  th{color:var(--dim);font-weight:500;font-size:11px;text-transform:uppercase;letter-spacing:.5px}
  .bar{display:inline-block;height:8px;background:var(--accent);border-radius:2px;vertical-align:middle}
  footer{color:var(--dim);font-size:12px;padding:0 20px 24px;text-align:center}
  .mech span{display:inline-block;margin-right:14px;color:var(--dim)}
  .mech b{color:var(--ink)}
  .seclabel{color:var(--dim);font-size:11px;letter-spacing:.4px;margin:6px 0 -2px}
  #byCat .catrow{display:flex;align-items:center;gap:8px;padding:3px 0}
  #byCat .catname{width:130px;color:var(--dim)}
  #byCat .catn{color:var(--ink)}
</style>
</head>
<body>
<header>
  <h1>ctxforge</h1>
  <div class="live"><span class="dot" id="dot"></span><span id="status">connecting…</span></div>
  <div class="grow"></div>
  <div class="saved-top" id="savedTop">— saved</div>
</header>
<main>
  <div class="seclabel">ctxforge MCP tool savings &mdash; ctx_execute / ctx_search / graph_* &middot; <b>live sessions only</b> (since you opened this page)</div>
  <div class="cards" id="cards"></div>
  <div class="row2">
    <div class="panel">
      <h2>tokens saved / min</h2>
      <canvas id="savedChart"></canvas>
      <div class="rate" id="savedRate">—</div>
    </div>
    <div class="panel">
      <h2>bytes returned / min</h2>
      <canvas id="bytesChart"></canvas>
      <div class="rate" id="bytesRate">—</div>
    </div>
  </div>
  <div class="panel">
    <h2>by tool</h2>
    <table id="byTool"><thead><tr>
      <th>tool</th><th>ops</th><th>raw</th><th>returned</th><th>saved~tok</th>
    </tr></thead><tbody></tbody></table>
  </div>
  <div class="panel">
    <h2>by mechanism</h2>
    <div class="mech" id="byMech"></div>
  </div>
  <div class="seclabel">RTK shell savings &mdash; RTK's own measured savings on shell commands via <code>rtk gain</code> &middot; <b>live since you opened this page</b></div>
  <div class="panel">
    <h2>RTK shell savings</h2>
    <div class="cards" id="rtkCards"></div>
  </div>
  <div class="seclabel">session activity &mdash; built-in tools (Read / Edit / Bash &hellip;) captured by hooks &middot; <b>live sessions only</b> &middot; not token savings</div>
  <div class="panel">
    <h2>activity</h2>
    <div class="cards" id="actCards"></div>
    <canvas id="actChart" style="margin-top:10px"></canvas>
    <div class="rate" id="actRate" style="color:var(--warn)">&mdash;</div>
    <div id="byCat" style="margin-top:10px"></div>
  </div>
</main>
<footer id="footer">—</footer>
<script>
const hist=[];           // {t, saved, bytes}
const savedSeries=[], bytesSeries=[];
const histAct=[], actSeries=[];   // {t, ev} for session-activity rate
const MAXPTS=60;
// "Live since the page loaded": every poll scopes savings + activity to sessions
// active at/after this instant, so previous sessions never show. Refresh to reset.
const SINCE=Math.floor(Date.now()/1000);
const SINCE_LABEL=new Date(SINCE*1000).toLocaleTimeString();
// RTK plane baseline: `rtk gain` is cumulative/all-time, so snapshot its counters
// on the first poll and render deltas — "live since you opened this page".
let rtkBase=null;

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
function spark(id,series,color){
  const c=document.getElementById(id), dpr=window.devicePixelRatio||1;
  const w=c.clientWidth, h=c.clientHeight;
  c.width=w*dpr; c.height=h*dpr;
  const x=c.getContext('2d'); x.scale(dpr,dpr); x.clearRect(0,0,w,h);
  if(series.length<2) return;
  const max=Math.max(1,...series), n=series.length;
  const px=i=>i/(n-1)*w, py=v=>h-4-(v/max)*(h-8);
  x.beginPath();
  series.forEach((v,i)=>{const X=px(i),Y=py(v); i?x.lineTo(X,Y):x.moveTo(X,Y);});
  x.lineTo(w,h); x.lineTo(0,h); x.closePath();
  x.fillStyle=color+'22'; x.fill();
  x.beginPath();
  series.forEach((v,i)=>{const X=px(i),Y=py(v); i?x.lineTo(X,Y):x.moveTo(X,Y);});
  x.strokeStyle=color; x.lineWidth=1.5; x.stroke();
}
function card(k,v,sub){
  return `<div class="card"><div class="k">${k}</div><div class="v">${v}</div>`+
    (sub?`<div class="sub">${sub}</div>`:'')+`</div>`;
}
function setStale(){
  document.getElementById('dot').classList.add('stale');
  document.getElementById('status').textContent='disconnected — retrying';
}
async function tick(){
  let d;
  try{ d=await (await fetch('/api/stats?since='+SINCE,{cache:'no-store'})).json(); }
  catch(e){ setStale(); return; }
  document.getElementById('dot').classList.remove('stale');
  document.getElementById('status').textContent='live · since '+SINCE_LABEL+(d.session?(' · session '+d.session):'')+' · '+(d.activity&&d.activity.sessions||0)+' live session(s)';
  document.getElementById('savedTop').textContent=humanCount(d.tokens_saved_est)+' tokens saved';

  const now=Date.now()/1000;
  // MCP plane (cards + this rate chart) reads MCP-only savings; RTK shell savings
  // have their own plane below, so a `rtk sync` doesn't spike this as MCP savings.
  const savedMcp=(d.tokens_saved_mcp!==undefined?d.tokens_saved_mcp:d.tokens_saved_est);
  hist.push({t:now,saved:savedMcp,bytes:d.bytes_returned});
  if(hist.length>1){
    const a=hist[hist.length-2], b=hist[hist.length-1], dt=Math.max(0.001,b.t-a.t);
    savedSeries.push(Math.max(0,(b.saved-a.saved)/dt*60));
    bytesSeries.push(Math.max(0,(b.bytes-a.bytes)/dt*60));
    if(savedSeries.length>MAXPTS) savedSeries.shift();
    if(bytesSeries.length>MAXPTS) bytesSeries.shift();
  }
  // windowed averages over the whole history
  const first=hist[0], last=hist[hist.length-1], span=Math.max(0.001,last.t-first.t);
  const savedPerMin=(last.saved-first.saved)/span*60;
  const bytesPerMin=(last.bytes-first.bytes)/span*60;

  document.getElementById('cards').innerHTML=
    card('ops total',d.ops.toLocaleString(),`${d.errors} err · ${d.timeouts} timeout`)+
    card('raw bytes in',humanBytes(d.raw_bytes_in))+
    card('bytes returned',humanBytes(d.bytes_returned))+
    card('tokens saved',humanCount(savedMcp),'~'+savedMcp.toLocaleString()+' · MCP tools only')+
    card('offloaded',d.offloaded_ops+' ops',humanBytes(d.offloaded_bytes)+' to store')+
    card('lock wait',d.lock_wait_ms+' ms');

  spark('savedChart',savedSeries,'#4cc4b0');
  spark('bytesChart',bytesSeries,'#4cc4b0');
  document.getElementById('savedRate').textContent=humanCount(Math.round(savedPerMin))+' tok/min';
  document.getElementById('bytesRate').textContent=humanBytes(Math.round(bytesPerMin))+'/min';

  const maxRaw=Math.max(1,...d.by_tool.map(t=>t.raw));
  document.querySelector('#byTool tbody').innerHTML=d.by_tool.map(t=>{
    const w=Math.round(t.raw/maxRaw*60);
    return `<tr><td>${t.tool}</td><td>${t.ops.toLocaleString()}</td>`+
      `<td><span class="bar" style="width:${w}px"></span> ${humanBytes(t.raw)}</td>`+
      `<td>${humanBytes(t.returned)}</td><td>${t.saved.toLocaleString()}</td></tr>`;
  }).join('')||'<tr><td colspan="5" style="color:var(--dim)">no ops yet</td></tr>';

  document.getElementById('byMech').innerHTML=d.by_mechanism.map(m=>
    `<span>${m.mechanism} <b>${m.ops}</b> ops · <b>${humanCount(m.saved)}</b> tok</span>`
  ).join('')||'<span style="color:var(--dim)">—</span>';

  // RTK shell savings — RTK's own numbers from `rtk gain`, shown live since you
  // opened this page (baseline captured on the first poll; rtk gain is cumulative).
  const r=d.rtk||{installed:false};
  if(r.installed){
    if(!rtkBase) rtkBase={commands:r.total_commands||0,saved:r.total_saved||0,input:r.total_input||0};
    const dCmd=Math.max(0,(r.total_commands||0)-rtkBase.commands);
    const dSaved=Math.max(0,(r.total_saved||0)-rtkBase.saved);
    const dInput=Math.max(0,(r.total_input||0)-rtkBase.input);
    const pct=dInput>0?(dSaved/dInput*100):0;
    document.getElementById('rtkCards').innerHTML=
      card('commands',dCmd.toLocaleString(),'since you opened this page')+
      card('tokens saved',humanCount(dSaved),'~'+dSaved.toLocaleString()+' · this session')+
      card('avg savings',pct.toFixed(1)+'%',dCmd?('over '+dCmd.toLocaleString()+' cmds'):'no rtk commands yet');
    // Top-line total reflects BOTH planes: MCP tool savings + RTK shell savings
    // (this session). Without this it showed MCP-only (often 0) while RTK saved 1000s.
    document.getElementById('savedTop').textContent=humanCount((d.tokens_saved_mcp||0)+dSaved)+' tokens saved';
  } else {
    document.getElementById('rtkCards').innerHTML=card('RTK not installed','—','run ctxforge rtk install');
  }

  // session activity (built-in tools, via hooks) — the "first plane"
  const a=d.activity||{total_events:0,sessions:0,by_category:[],last_ts:null};
  histAct.push({t:now,ev:a.total_events});
  if(histAct.length>1){
    const x=histAct[histAct.length-2], y=histAct[histAct.length-1], dt=Math.max(0.001,y.t-x.t);
    actSeries.push(Math.max(0,(y.ev-x.ev)/dt*60));
    if(actSeries.length>MAXPTS) actSeries.shift();
  }
  const af=histAct[0], al=histAct[histAct.length-1], asp=Math.max(0.001,al.t-af.t);
  const evPerMin=(al.ev-af.ev)/asp*60;
  document.getElementById('actCards').innerHTML=
    card('events total',(a.total_events||0).toLocaleString(),(a.sessions||0)+' session(s)')+
    card('last activity',a.last_ts?new Date(a.last_ts*1000).toLocaleTimeString():'—');
  spark('actChart',actSeries,'#e0a458');
  document.getElementById('actRate').textContent=Math.round(evPerMin)+' events/min';
  const maxCat=Math.max(1,...a.by_category.map(c=>c.count));
  document.getElementById('byCat').innerHTML=a.by_category.map(c=>{
    const w=Math.round(c.count/maxCat*120);
    return `<div class="catrow"><span class="catname">${c.category}</span>`+
      `<span class="bar" style="width:${w}px;background:var(--warn)"></span>`+
      `<span class="catn">${c.count}</span></div>`;
  }).join('')||'<span style="color:var(--dim)">no activity captured yet</span>';

  document.getElementById('footer').textContent=
    `store ${humanBytes(d.store_size)} · index chunks ${d.index_chunks} · `+
    `graph ${d.graph_nodes} nodes / ${d.graph_edges} edges · updated ${d.ts}`;
}
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
        OpLog::open(dir.path())
            .start("ctx_execute", json!({}))
            .finish(8000, 100, Some("a".into()), "ok", "", None);
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
        assert_eq!(v["by_tool"][0]["tool"], json!("ctx_execute"));
        // Session-activity block reflects the seeded hook event.
        assert_eq!(v["activity"]["total_events"], json!(1));
        assert_eq!(v["activity"]["by_category"][0]["category"], json!("file"));
        // The RTK plane (third plane) is always present; its `installed` flag is a
        // bool whose value depends on machine state, so assert structure only.
        assert!(v["rtk"].is_object(), "snapshot must carry an rtk object");
        assert!(v["rtk"]["installed"].is_boolean(), "rtk.installed is a bool");

        let (s2, ct2, body2) = route("/", dir.path(), None);
        assert_eq!(s2, 200);
        assert!(ct2.contains("html"));
        assert!(body2.contains("ctxforge"));
        assert!(body2.contains("/api/stats"));
        // The RTK shell-savings panel markup is baked into the self-contained page.
        assert!(body2.contains("RTK shell savings"));
        assert!(body2.contains("rtkCards"));

        let (s3, _, _) = route("/nope", dir.path(), None);
        assert_eq!(s3, 404);
    }

    #[test]
    fn tcp_roundtrip_serves_json() {
        let dir = tempdir().unwrap();
        OpLog::open(dir.path())
            .start("ctx_search", json!({}))
            .finish(10, 10, None, "ok", "", None);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let dirp = dir.path().to_path_buf();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle(stream, &dirp, None);
        });

        let mut c = TcpStream::connect(addr).unwrap();
        c.write_all(b"GET /api/stats HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut resp = String::new();
        c.read_to_string(&mut resp).unwrap();
        server.join().unwrap();

        assert!(resp.contains("200 OK"), "got: {resp}");
        assert!(resp.contains("application/json"));
        assert!(resp.contains("\"ops\":1"));
    }
}
