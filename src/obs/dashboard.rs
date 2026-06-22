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
                eprintln!("lens dashboard: unknown flag '{other}'");
                eprintln!("usage: lens dashboard [--port <n>] [--host <addr>] [--session <id>]");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    let dir = data_dir();
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
            // scope=global reads the machine-global mirror under home_root(), totaling
            // every repo and launch profile; cross-repo, so it drops the session filter.
            let (d, sess) = match (query_param(query, "scope"), crate::rtk::home_root()) {
                (Some("global"), Some(home)) => (home, None),
                _ => (dir.to_path_buf(), session),
            };
            (
                200,
                "application/json",
                snapshot_json_since(&d, sess, since).to_string(),
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
<title>lens dashboard</title>
<style>
  :root{
    --bg:#0b0d10; --panel:#14181d; --line:#222a31; --ink:#e6edf3;
    --dim:#8b97a3; --accent:#4cc4b0; --warn:#e0a458; --bad:#e06c75;
  }
  *{box-sizing:border-box}
  body{margin:0;background:var(--bg);color:var(--ink);
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
  select#win,input#winAt{background:var(--panel);color:var(--ink);border:1px solid var(--line);
    border-radius:5px;font:10px ui-monospace,Menlo,monospace;padding:1px 4px}
  button#scope{background:var(--panel);color:var(--dim);border:1px solid var(--line);
    border-radius:5px;font:10px ui-monospace,Menlo,monospace;padding:1px 6px;cursor:pointer}
  button#scope.on{color:var(--accent);border-color:var(--accent)}
  .cost{cursor:pointer;display:flex;align-items:baseline;gap:6px;user-select:none}
  .cost .dollars{color:var(--accent);font-weight:700;font-size:15px;letter-spacing:.3px}
  .cost .basis{color:var(--dim);font-size:10px;border-bottom:1px dotted var(--line)}
  .cost:hover .basis{color:var(--ink)}
  .saved-top{color:var(--dim);font-size:10px}
  main{padding:7px 10px;display:grid;gap:7px}
  .panel{background:var(--panel);border:1px solid var(--line);border-radius:7px;padding:6px 9px}
  .panel h2{font-size:9px;color:var(--dim);text-transform:uppercase;letter-spacing:.5px;margin:0 0 4px}
  .seclabel{color:var(--dim);font-size:9px;letter-spacing:.3px;margin:2px 0 -3px}
  .strip{display:flex;flex-wrap:wrap;gap:3px 20px;align-items:baseline}
  .strip .st i{color:var(--dim);font-style:normal;font-size:9px;text-transform:uppercase;letter-spacing:.4px;margin-right:4px}
  .strip .st b{font-size:14px;font-weight:600}
  .charts{display:grid;grid-template-columns:1fr 1fr;gap:14px}
  .ch{display:flex;flex-direction:column}
  .ch>span{font-size:9px;color:var(--dim);text-transform:uppercase;letter-spacing:.4px;display:flex;justify-content:space-between}
  .ch>span b{color:var(--accent);font-size:11px}
  .ch canvas{width:100%;height:28px;display:block;margin-top:2px}
  .row2{display:grid;grid-template-columns:1fr 1fr;gap:7px}
  @media(max-width:560px){.row2{grid-template-columns:1fr}}
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
  .actline{display:flex;flex-wrap:wrap;gap:3px 20px;align-items:baseline}
  .actline i{color:var(--dim);font-style:normal;font-size:9px;text-transform:uppercase;letter-spacing:.4px;margin-right:4px}
  .actline b{font-size:13px;font-weight:600}
  .actrow{display:flex;align-items:center;gap:10px;margin-top:3px}
  .actrow canvas{flex:1;height:22px;display:block}
  .actrow .rate{color:var(--warn);font-size:10px;white-space:nowrap}
  .cats{display:flex;flex-wrap:wrap;gap:2px 13px;margin-top:5px;font-size:10px}
  .cats span{color:var(--dim)}
  .cats b{color:var(--warn)}
  footer{color:var(--dim);font-size:9px;padding:3px 10px 8px;text-align:center}
</style>
</head>
<body>
<header>
  <h1>lens</h1>
  <div class="live"><span class="dot" id="dot"></span><span id="status">connecting…</span></div>
  <select id="win" title="time window — how far back to scope savings + activity"></select>
  <input id="winAt" type="text" placeholder="2pm" size="6" title="custom start time, e.g. 2pm or 14:30 — Enter to apply">
  <button id="scope" title="toggle global view: total tokens across every repo and launch profile">this repo</button>
  <div class="grow"></div>
  <div class="cost" id="cost" title="estimated $ saved — click to switch model rate"><span class="dollars" id="dollars">$—</span><span class="basis" id="basis">@ —</span></div>
  <div class="saved-top" id="savedTop">— saved</div>
</header>
<main>
  <div class="panel strip" id="strip"></div>
  <div class="panel charts">
    <div class="ch"><span>tokens saved / min <b id="savedRate">—</b></span><canvas id="savedChart"></canvas></div>
    <div class="ch"><span>bytes returned / min <b id="bytesRate">—</b></span><canvas id="bytesChart"></canvas></div>
  </div>
  <div class="seclabel">by tool + tool adoption &middot; saved &asymp; input tokens avoided &middot; dim = no calls in window</div>
  <div class="panel">
    <table id="tools"><thead><tr>
      <th>tool</th><th>ops</th><th>raw</th><th>ret</th><th>saved~tok</th><th>save%</th><th>off</th><th>err</th><th>to</th>
    </tr></thead><tbody></tbody></table>
  </div>
  <div class="row2">
    <div class="panel"><h2>by mechanism</h2><div class="mech" id="byMech"></div></div>
    <div class="panel"><h2>RTK shell savings</h2><div class="mech" id="rtkCards"></div></div>
  </div>
  <div class="seclabel">session activity &middot; built-in tools (Read / Edit / Bash) via hooks &middot; not token savings</div>
  <div class="panel">
    <div class="actline" id="actLine"></div>
    <div class="actrow"><canvas id="actChart"></canvas><span class="rate" id="actRate">—</span></div>
    <div class="cats" id="byCat"></div>
  </div>
</main>
<footer id="footer">—</footer>
<script>
const hist=[];
// Canonical lens MCP tools, shown in the merged tool table even at 0 calls.
const ADOPTION_TOOLS=['lens_run','lens_run_file','lens_search','lens_index','lens_map','lens_recall','lens_symbol','lens_links','lens_path','lens_find'];
const savedSeries=[], bytesSeries=[];
const histAct=[], actSeries=[];
const MAXPTS=60;
let rtkBase=null;
let scope='repo';

// Time window: the backend /api/stats?since= cutoff scopes ops + session activity.
// "live" = since the page opened (default). Concrete clock times ("since 2:00 PM") and
// relative presets are built into the dropdown; a text field accepts arbitrary times.
// RTK savings can't honor an arbitrary cutoff (rtk gain is a cumulative counter), so
// that one plane stays "since you opened the page" regardless of the selector.
const PAGELOAD=Math.floor(Date.now()/1000);
let activeSince=0, winMode='all', atLabel='';
const winSel=document.getElementById('win'), winAt=document.getElementById('winAt');
function addOpt(label,val){const o=document.createElement('option');o.textContent=label;o.value=val;winSel.appendChild(o);}
(function buildWinOptions(){
  addOpt('live','live'); addOpt('last 15m','15m'); addOpt('last 1h','1h'); addOpt('last 3h','3h'); addOpt('today','today');
  // Concrete top-of-hour marks so "since 2pm" is a one-click pick, no fiddly time spinner.
  const h0=new Date().getHours();
  for(let h=h0; h>=Math.max(0,h0-7); h--){
    const d=new Date(); d.setHours(h,0,0,0);
    if(d.getTime()>Date.now()) continue;
    addOpt('since '+d.toLocaleTimeString([],{hour:'numeric',minute:'2-digit'}), 'at:'+Math.floor(d.getTime()/1000));
  }
  addOpt('all time','all'); addOpt('custom…','custom');
})();
winSel.value='all';
function presetSince(m){
  const now=Math.floor(Date.now()/1000);
  if(m==='all') return 0;
  if(m==='today'){const d=new Date();d.setHours(0,0,0,0);return Math.floor(d.getTime()/1000);}
  if(m==='3h') return now-10800;
  if(m==='1h') return now-3600;
  if(m==='15m') return now-900;
  return PAGELOAD; // live
}
// Lenient: "2pm" -> 14:00, "2:30pm" -> 14:30, "14:00"/"14" -> 14:00, "11am" -> 11:00.
function parseTime(s){
  s=(s||'').trim().toLowerCase(); if(!s) return null;
  const m=s.match(/^(\d{1,2})(?::(\d{2}))?\s*(am|pm)?$/); if(!m) return null;
  let h=+m[1]; const min=m[2]?+m[2]:0, ap=m[3];
  if(ap==='pm'&&h<12) h+=12; if(ap==='am'&&h===12) h=0;
  if(h>23||min>59) return null;
  const d=new Date(); d.setHours(h,min,0,0);
  let u=Math.floor(d.getTime()/1000); const now=Math.floor(Date.now()/1000);
  if(u>now) u-=86400; return u; // a future time means "earlier today" already passed -> yesterday
}
function winLabel(){
  const at=new Date(activeSince*1000).toLocaleTimeString();
  if(winMode==='all') return 'all time';
  if(winMode==='live') return 'live · since '+at;
  if(winMode==='today') return 'today · since '+at;
  if(winMode==='3h') return 'last 3h · since '+at;
  if(winMode==='1h') return 'last 1h · since '+at;
  if(winMode==='15m') return 'last 15m · since '+at;
  if(winMode==='at') return atLabel;
  return 'since '+at; // custom text
}
function resetSeries(){hist.length=0;savedSeries.length=0;bytesSeries.length=0;histAct.length=0;actSeries.length=0;}
function applyWin(){resetSeries();tick();}
winSel.addEventListener('change',function(){
  const v=this.value;
  winAt.style.display = v==='custom'?'':'none';
  if(v==='custom'){ winMode='custom'; const u=parseTime(winAt.value); if(u!=null){activeSince=u;applyWin();} winAt.focus(); return; }
  if(v.indexOf('at:')===0){ winMode='at'; atLabel=this.options[this.selectedIndex].text; activeSince=parseInt(v.slice(3),10); }
  else { winMode=v; activeSince=presetSince(v); }
  applyWin();
});
function commitCustom(){ if(winMode!=='custom') return; const u=parseTime(winAt.value); if(u!=null){activeSince=u;applyWin();} else { winAt.style.borderColor='var(--bad)'; setTimeout(function(){winAt.style.borderColor='';},800); } }
winAt.addEventListener('change',commitCustom);
winAt.addEventListener('keydown',function(e){if(e.key==='Enter')commitCustom();});
winAt.style.display='none';
const scopeBtn=document.getElementById('scope');
scopeBtn.addEventListener('click',function(){
  scope = scope==='repo' ? 'global' : 'repo';
  this.textContent = scope==='global' ? 'all repos' : 'this repo';
  this.classList.toggle('on', scope==='global');
  resetSeries(); tick();
});

// Cost estimate: "tokens saved" are context INPUT tokens you avoided sending, so price
// them at the input rate. Click the basis to cycle model; remembered in localStorage.
const RATES=[{m:'Opus 4.8',r:5},{m:'Sonnet 4.6',r:3},{m:'Haiku 4.5',r:1}];
let rateIdx=0;
try{const s=localStorage.getItem('lens_rate_model');const i=RATES.findIndex(x=>x.m===s);if(i>=0)rateIdx=i;}catch(e){}
let savedTotal=0;
function money(v){return '$'+(v>=1?v.toFixed(2):v>=0.01?v.toFixed(3):v.toFixed(4));}
function renderCost(){
  const x=RATES[rateIdx];
  document.getElementById('dollars').textContent=money(savedTotal*x.r/1e6)+' saved';
  document.getElementById('basis').textContent='@ $'+x.r+'/M · '+x.m+' in ▾';
}
document.getElementById('cost').addEventListener('click',function(){
  rateIdx=(rateIdx+1)%RATES.length;
  try{localStorage.setItem('lens_rate_model',RATES[rateIdx].m);}catch(e){}
  renderCost();
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
function spark(id,series,color){
  const c=document.getElementById(id), dpr=window.devicePixelRatio||1;
  const w=c.clientWidth, h=c.clientHeight;
  c.width=w*dpr; c.height=h*dpr;
  const x=c.getContext('2d'); x.scale(dpr,dpr); x.clearRect(0,0,w,h);
  if(series.length<2) return;
  const max=Math.max(1,...series), n=series.length;
  const px=i=>i/(n-1)*w, py=v=>h-3-(v/max)*(h-6);
  x.beginPath();
  series.forEach((v,i)=>{const X=px(i),Y=py(v); i?x.lineTo(X,Y):x.moveTo(X,Y);});
  x.lineTo(w,h); x.lineTo(0,h); x.closePath();
  x.fillStyle=color+'22'; x.fill();
  x.beginPath();
  series.forEach((v,i)=>{const X=px(i),Y=py(v); i?x.lineTo(X,Y):x.moveTo(X,Y);});
  x.strokeStyle=color; x.lineWidth=1.5; x.stroke();
}
function stat(k,v){return `<span class="st"><i>${k}</i><b>${v}</b></span>`;}
function setStale(){
  document.getElementById('dot').classList.add('stale');
  document.getElementById('status').textContent='disconnected — retrying';
}
async function tick(){
  let d;
  try{ d=await (await fetch('/api/stats?since='+activeSince+(scope==='global'?'&scope=global':''),{cache:'no-store'})).json(); }
  catch(e){ setStale(); return; }
  document.getElementById('dot').classList.remove('stale');
  document.getElementById('status').textContent=(scope==='global'?'GLOBAL · ':'')+winLabel()+(d.session?(' · '+d.session):'')+' · '+(d.activity&&d.activity.sessions||0)+' session(s)';

  const now=Date.now()/1000;
  const savedMcp=(d.tokens_saved_mcp!==undefined?d.tokens_saved_mcp:d.tokens_saved_est);
  hist.push({t:now,saved:savedMcp,bytes:d.bytes_returned});
  if(hist.length>1){
    const a=hist[hist.length-2], b=hist[hist.length-1], dt=Math.max(0.001,b.t-a.t);
    savedSeries.push(Math.max(0,(b.saved-a.saved)/dt*60));
    bytesSeries.push(Math.max(0,(b.bytes-a.bytes)/dt*60));
    if(savedSeries.length>MAXPTS) savedSeries.shift();
    if(bytesSeries.length>MAXPTS) bytesSeries.shift();
  }
  const first=hist[0], last=hist[hist.length-1], span=Math.max(0.001,last.t-first.t);
  const savedPerMin=(last.saved-first.saved)/span*60;
  const bytesPerMin=(last.bytes-first.bytes)/span*60;

  // Stat strip — the old six cards, now one compact line.
  const overallPct=d.raw_bytes_in>0?Math.round((d.raw_bytes_in-d.bytes_returned)/d.raw_bytes_in*100):0;
  const fired=ADOPTION_TOOLS.filter(n=>d.by_tool.some(t=>t.tool===n&&t.ops>0)).length;
  document.getElementById('strip').innerHTML=
    stat('ops',d.ops.toLocaleString()+' ('+d.errors+'e·'+d.timeouts+'t)')+
    stat('raw in',humanBytes(d.raw_bytes_in))+
    stat('returned',humanBytes(d.bytes_returned))+
    stat('saved',humanCount(savedMcp)+' tok')+
    stat('save%',overallPct+'%')+
    stat('ctx fired',fired+'/'+ADOPTION_TOOLS.length)+
    stat('offloaded',d.offloaded_ops+' ('+humanBytes(d.offloaded_bytes)+')')+
    stat('lock',d.lock_wait_ms+' ms');

  spark('savedChart',savedSeries,'#4cc4b0');
  spark('bytesChart',bytesSeries,'#4cc4b0');
  document.getElementById('savedRate').textContent=humanCount(Math.round(savedPerMin))+' tok/min';
  document.getElementById('bytesRate').textContent=humanBytes(Math.round(bytesPerMin))+'/min';

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
    return `<tr${ops?'':' class="z"'}><td>${name}</td><td>${ops.toLocaleString()}</td>`+
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
    document.getElementById('savedTop').textContent=humanCount((d.tokens_saved_mcp||0)+dSaved)+' tok';
    savedTotal=(d.tokens_saved_mcp||0)+dSaved; renderCost();
  } else {
    document.getElementById('rtkCards').innerHTML='<span class="dim2">not installed — run lens rtk install</span>';
    document.getElementById('savedTop').textContent=humanCount(savedMcp)+' tok';
    savedTotal=savedMcp; renderCost();
  }

  // session activity (built-in tools via hooks)
  const a=d.activity||{total_events:0,sessions:0,by_category:[],last_ts:null};
  histAct.push({t:now,ev:a.total_events});
  if(histAct.length>1){
    const x=histAct[histAct.length-2], y=histAct[histAct.length-1], dt=Math.max(0.001,y.t-x.t);
    actSeries.push(Math.max(0,(y.ev-x.ev)/dt*60));
    if(actSeries.length>MAXPTS) actSeries.shift();
  }
  const af=histAct[0], al=histAct[histAct.length-1], asp=Math.max(0.001,al.t-af.t);
  const evPerMin=(al.ev-af.ev)/asp*60;
  document.getElementById('actLine').innerHTML=
    `<span class="st"><i>events</i><b>${(a.total_events||0).toLocaleString()}</b></span>`+
    `<span class="st"><i>sessions</i><b>${a.sessions||0}</b></span>`+
    `<span class="st"><i>last</i><b>${a.last_ts?new Date(a.last_ts*1000).toLocaleTimeString():'—'}</b></span>`;
  spark('actChart',actSeries,'#e0a458');
  document.getElementById('actRate').textContent=Math.round(evPerMin)+' ev/min';
  document.getElementById('byCat').innerHTML=a.by_category.map(c=>
    `<span>${c.category} <b>${c.count}</b></span>`
  ).join('')||'<span class="dim2">no activity captured yet</span>';

  document.getElementById('footer').textContent=
    `store ${humanBytes(d.store_size)} · index ${d.index_chunks} · `+
    `graph ${d.graph_nodes}n/${d.graph_edges}e · updated ${d.ts}`;
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
}
