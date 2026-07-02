//! `lens dashboard --tui` — the terminal renderer of the same snapshot the web
//! dashboard polls.
//!
//! Zero new dependencies: box-drawing + Unicode block sparklines + ANSI SGR color,
//! nothing else. [`render_snapshot`] is a **pure** `Value -> String` (no terminal,
//! clock, or IO), so it unit-tests by handing it a snapshot; [`run`] is the refresh
//! loop, mirroring `lens stats --watch` (clear screen, render, flush, sleep). Cooked
//! mode throughout, so Ctrl-C exits clean with nothing to restore.
//!
//! Parity with the web is structural: both render the one snapshot from
//! `stats::snapshot_json_since`, so a new dimension added to the producer shows in
//! both. The keys this reads are listed in `stats::SNAPSHOT_DIMENSIONS`; the parity
//! tripwire in `dashboard.rs` scans this file's source for each of them.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::Value;

use super::stats::{human_bytes, human_count, snapshot_json_since};

/// Canonical lens MCP tools, always shown in the tool table (dimmed at 0 calls) so a
/// dormant tool reads as unused, not absent. Mirrors `ADOPTION_TOOLS` in the web
/// `INDEX_HTML`; the `tool_table_lists_canonical_tools` test keeps the two in step.
pub(crate) const ADOPTION_TOOLS: &[&str] = &[
    "lens_run",
    "lens_run_file",
    "lens_search",
    "lens_index",
    "lens_map",
    "lens_recall",
    "lens_symbol",
    "lens_links",
    "lens_path",
    "lens_find",
];

/// Width below which the layout collapses to the single-column mini view (mirrors
/// the web's mini/full toggle). At/above it, panels are framed with box-drawing.
const MINI_MAX: usize = 56;

const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

// ---------------------------------------------------------------------------
// Pure renderer
// ---------------------------------------------------------------------------

/// Render one snapshot to a terminal frame. Pure: the only inputs are the snapshot,
/// the target `width`, the `color` theme (palette + whether ANSI is emitted), the
/// `$/M`-token `rate` for the money headline, and `rt_seconds` (seconds per avoided
/// round-trip) for the applied-value time figure. `width < 56` ⇒ the mini layout.
pub fn render_snapshot(snap: &Value, width: u16, color: Theme, rate: f64, rt_seconds: f64) -> String {
    let w = (width as usize).clamp(24, 200);
    let mini = w < MINI_MAX;
    let inner = if mini { w } else { w.saturating_sub(4) };
    let mut out = String::new();

    out.push_str(&header(snap, w, mini, color, rate));
    out.push('\n');

    let section = |title: &str, lines: Vec<String>, o: &mut String| {
        if mini {
            o.push_str(&section_mini(title, &lines, w, color));
        } else {
            o.push_str(&panel(title, &lines, w, color));
        }
    };

    section("STATS", stats_strip(snap, inner), &mut out);
    section("THROUGHPUT", throughput(snap, inner, color), &mut out);
    section("TOOLS", tool_table(snap, inner, mini, color), &mut out);
    section("BY MECHANISM", mechanism_lines(snap, inner, color), &mut out);
    section("RTK SHELL", rtk_lines(snap, inner, color), &mut out);
    section("SESSION ACTIVITY", activity_lines(snap, inner, color), &mut out);
    section("APPLIED VALUE", applied_value_lines(snap, rt_seconds, color), &mut out);
    out.push_str(&footer(snap, w, color));
    out.push('\n');
    out
}

/// `$ saved · N tok` + a status line (scope / ops / sessions). The money basis is
/// `tokens_saved_mcp` (the measured MCP savings) priced at `rate` — never an
/// estimate, matching the web `$` headline at steady state.
fn header(snap: &Value, w: usize, mini: bool, color: Theme, rate: f64) -> String {
    let saved = saved_mcp(snap);
    let dollars = saved as f64 * rate / 1_000_000.0;
    let money = if dollars >= 1.0 {
        format!("${dollars:.2}")
    } else if dollars >= 0.01 {
        format!("${dollars:.3}")
    } else {
        format!("${dollars:.4}")
    };
    let title = bold(&gold("lens", color), color);
    let head = format!(
        "{title}  {} saved · {} tok",
        bold(&gold(&money, color), color),
        human_count(saved)
    );
    let ops = geti(snap, "ops");
    let sessions = snap["activity"]["sessions"].as_i64().unwrap_or(0);
    let scope = if snap["scope_global"].as_bool().unwrap_or(false) {
        "GLOBAL · "
    } else {
        ""
    };
    let window = snap["window_label"].as_str().unwrap_or("all time");
    let status = dim(
        &format!("{scope}{window} · {ops} ops · {sessions} session(s)"),
        color,
    );
    let head = clip(&head, w);
    let status = clip(&status, w);
    if mini {
        format!("{head}\n{status}")
    } else {
        format!("{head}\n{status}\n{}", dim(&"─".repeat(w), color))
    }
}

/// The compact stat strip: ops, bytes, savings, adoption, offload, lock.
fn stats_strip(snap: &Value, inner: usize) -> Vec<String> {
    let raw = getu(snap, "raw_bytes_in");
    let ret = getu(snap, "bytes_returned");
    let overall = if raw > 0 {
        ((raw as i64 - ret as i64) * 100 / raw as i64).max(0)
    } else {
        0
    };
    let fired = ADOPTION_TOOLS
        .iter()
        .filter(|n| tool_ops(snap, n) > 0)
        .count();
    let items = vec![
        format!(
            "ops {} ({} err, {} to)",
            geti(snap, "ops"),
            geti(snap, "errors"),
            geti(snap, "timeouts")
        ),
        format!("raw {}", human_bytes(raw)),
        format!("ret {}", human_bytes(ret)),
        format!("saved {} tok", human_count(saved_mcp(snap))),
        format!("save% {overall}%"),
        format!("tools {}/{}", fired, ADOPTION_TOOLS.len()),
        format!(
            "offloaded {} ({})",
            geti(snap, "offloaded_ops"),
            human_bytes(getu(snap, "offloaded_bytes"))
        ),
        format!("lock {} ms", geti(snap, "lock_wait_ms")),
    ];
    flow(&items, inner, "   ")
}

/// Two block sparklines: tokens saved / min and bytes returned / min, with the rate
/// derived from the snapshot's own window bounds (no clock).
fn throughput(snap: &Value, inner: usize, color: Theme) -> Vec<String> {
    let win_min = {
        let s = snap["window_start"].as_i64().unwrap_or(0);
        let e = snap["window_end"].as_i64().unwrap_or(0);
        ((e - s) as f64 / 60.0).max(1.0 / 60.0)
    };
    let cells = inner.saturating_sub(26).clamp(8, 48);
    let line = |label: &str, key: &str, fmt_rate: &dyn Fn(i64) -> String| -> String {
        let buckets = buckets(snap, key);
        let last = buckets.last().copied().unwrap_or(0);
        let rate = (last as f64 / win_min).round() as i64;
        format!(
            "{:<14}{}  {}",
            label,
            gold(&sparkline(&buckets, cells), color),
            dim(&fmt_rate(rate), color)
        )
    };
    vec![
        line(
            "saved/min",
            "saved_buckets",
            &|r| format!("{} tok/min", human_count(r.max(0) as u64)),
        ),
        line(
            "bytes/min",
            "bytes_buckets",
            &|r| format!("{}/min", human_bytes(r.max(0) as u64)),
        ),
    ]
}

/// The merged per-tool table: canonical tools (dim at 0 calls) + any extras that
/// fired. Columns: tool, ops, raw, ret, saved, save%. A `⚠` flags errors/timeouts.
fn tool_table(snap: &Value, _inner: usize, mini: bool, color: Theme) -> Vec<String> {
    let by_tool = snap["by_tool"].as_array().cloned().unwrap_or_default();
    let extras: Vec<String> = by_tool
        .iter()
        .filter_map(|t| t["tool"].as_str())
        .filter(|n| !ADOPTION_TOOLS.contains(n))
        .map(|s| s.to_string())
        .collect();
    let names: Vec<String> = ADOPTION_TOOLS
        .iter()
        .map(|s| s.to_string())
        .chain(extras)
        .collect();
    let get = |name: &str| by_tool.iter().find(|t| t["tool"].as_str() == Some(name)).cloned();

    let mut lines = Vec::new();
    if mini {
        // Narrow: tool + ops + saved only.
        for name in &names {
            let t = get(name);
            let ops = t.as_ref().map(|t| t["ops"].as_i64().unwrap_or(0)).unwrap_or(0);
            let saved = t.as_ref().map(|t| t["saved"].as_i64().unwrap_or(0)).unwrap_or(0);
            let row = format!("{name:<15}{ops:>5}  {saved:>8} tok");
            lines.push(if ops == 0 { dim(&row, color) } else { row });
        }
        return lines;
    }
    lines.push(dim(
        &format!(
            "{:<15}{:>5}{:>10}{:>10}{:>10}{:>6}",
            "tool", "ops", "raw", "ret", "saved", "save%"
        ),
        color,
    ));
    for name in &names {
        let t = get(name);
        let (ops, raw, ret, saved, errs, tos) = match &t {
            Some(t) => (
                t["ops"].as_i64().unwrap_or(0),
                t["raw"].as_u64().unwrap_or(0),
                t["returned"].as_u64().unwrap_or(0),
                t["saved"].as_i64().unwrap_or(0),
                t["errors"].as_i64().unwrap_or(0),
                t["timeouts"].as_i64().unwrap_or(0),
            ),
            None => (0, 0, 0, 0, 0, 0),
        };
        let pct = if raw > 0 {
            format!("{}%", (raw as i64 - ret as i64) * 100 / raw as i64)
        } else {
            "—".to_string()
        };
        let warn_flag = if errs + tos > 0 {
            warn(&format!(" ⚠{}", errs + tos), color)
        } else {
            String::new()
        };
        let row = format!(
            "{name:<15}{ops:>5}{:>10}{:>10}{:>10}{pct:>6}{warn_flag}",
            human_bytes(raw),
            human_bytes(ret),
            saved
        );
        lines.push(if ops == 0 { dim(&row, color) } else { row });
    }
    lines
}

/// `darkroom 3op·120tok · index 5op·80tok …`
fn mechanism_lines(snap: &Value, inner: usize, _color: Theme) -> Vec<String> {
    let items: Vec<String> = snap["by_mechanism"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|m| {
                    format!(
                        "{} {}op·{}tok",
                        m["mechanism"].as_str().unwrap_or("?"),
                        m["ops"].as_i64().unwrap_or(0),
                        human_count(m["saved"].as_i64().unwrap_or(0).max(0) as u64)
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    if items.is_empty() {
        return vec!["—".to_string()];
    }
    flow(&items, inner, " · ")
}

/// RTK's own measured shell savings (read straight from the snapshot's `rtk` plane).
fn rtk_lines(snap: &Value, _inner: usize, _color: Theme) -> Vec<String> {
    let r = &snap["rtk"];
    if r["installed"].as_bool() == Some(true) {
        vec![format!(
            "cmds {} · saved {} tok · avg {:.1}%",
            r["total_commands"].as_i64().unwrap_or(0),
            human_count(r["total_saved"].as_i64().unwrap_or(0).max(0) as u64),
            r["avg_savings_pct"].as_f64().unwrap_or(0.0),
        )]
    } else {
        vec!["not installed — run `lens rtk install`".to_string()]
    }
}

/// Session activity (built-in tools via hooks) + an event sparkline + categories.
fn activity_lines(snap: &Value, inner: usize, color: Theme) -> Vec<String> {
    let a = &snap["activity"];
    let cells = inner.saturating_sub(26).clamp(8, 48);
    let mut lines = vec![format!(
        "events {} · {} session(s)  {}",
        a["total_events"].as_i64().unwrap_or(0),
        a["sessions"].as_i64().unwrap_or(0),
        warn(&sparkline(&buckets(snap, "event_buckets"), cells), color),
    )];
    let cats: Vec<String> = a["by_category"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|c| {
                    format!(
                        "{} {}",
                        c["category"].as_str().unwrap_or("?"),
                        c["count"].as_i64().unwrap_or(0)
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    if cats.is_empty() {
        lines.push(dim("no activity captured yet", color));
    } else {
        lines.extend(flow(&cats, inner, " · "));
    }
    lines
}

/// The applied-value panel: benchmark per-op rates × this scope's live op counts.
/// Shows measured vs estimated tokens, round-trips avoided, and time saved
/// (round-trips × `rt_seconds`), then a per-dimension breakdown. Everything here is an
/// estimate (see the note caption); it never enters the measured `$` headline.
fn applied_value_lines(snap: &Value, rt_seconds: f64, color: Theme) -> Vec<String> {
    let av = &snap["applied_value"];
    let g = |k: &str| av[k].as_i64().unwrap_or(0).max(0) as u64;
    let rts = av["round_trips_avoided"].as_f64().unwrap_or(0.0);
    let mut lines = vec![
        dim(av["note"].as_str().unwrap_or(""), color),
        format!(
            "measured saved      {}",
            gold(&format!("{} tok", human_count(g("measured_tokens"))), color)
        ),
        format!("est. counterfactual +{} tok", human_count(g("est_counterfactual_tokens"))),
        format!(
            "est. total avoided  {}",
            bold(&gold(&format!("{} tok", human_count(g("est_total_tokens"))), color), color)
        ),
        format!(
            "round-trips avoided {}  {}",
            gold(&format!("~{rts:.0}"), color),
            dim("(navigation measured + 1/value-op)", color)
        ),
        format!(
            "time saved          {}  {}",
            bold(&gold(&human_time(rts * rt_seconds), color), color),
            dim(&format!("@ {rt_seconds:.0}s/round-trip"), color)
        ),
    ];
    if let Some(rows) = av["rows"].as_array() {
        lines.push(dim("by dimension", color));
        for r in rows {
            let d = r["dimension"].as_str().unwrap_or("");
            let ops = r["ops"].as_i64().unwrap_or(0);
            let et = r["est_tokens"].as_i64().unwrap_or(0);
            let rt = r["round_trips"].as_f64().unwrap_or(0.0);
            let mut parts: Vec<String> = Vec::new();
            if et > 0 {
                parts.push(format!("~{} tok", human_count(et as u64)));
            }
            if rt > 0.0 {
                parts.push(format!("~{rt:.1} rt"));
            }
            if matches!(d, "darkroom" | "skeleton") {
                parts.push("tok measured live".to_string());
            }
            if parts.is_empty() {
                parts.push("—".to_string());
            }
            let line = format!("  {d:<11} ×{ops}  {}", parts.join(", "));
            lines.push(if ops == 0 { dim(&line, color) } else { line });
        }
    }
    lines
}

/// Seconds as a compact human duration: `~Ns` / `~N.N min` / `~N.N h`.
fn human_time(secs: f64) -> String {
    if secs < 90.0 {
        format!("~{secs:.0}s")
    } else if secs < 5400.0 {
        format!("~{:.1} min", secs / 60.0)
    } else {
        format!("~{:.1} h", secs / 3600.0)
    }
}

/// `store X · index N · graph Nn/Me · updated <ts>`
fn footer(snap: &Value, w: usize, color: Theme) -> String {
    let rule = dim(&"─".repeat(w), color);
    let line = dim(
        &format!(
            "store {} · index {} · graph {}n/{}e · updated {}",
            human_bytes(getu(snap, "store_size")),
            geti(snap, "index_chunks"),
            geti(snap, "graph_nodes"),
            geti(snap, "graph_edges"),
            snap["ts"].as_str().unwrap_or("—"),
        ),
        color,
    );
    format!("{rule}\n{}", clip(&line, w))
}

// ---------------------------------------------------------------------------
// Small value helpers
// ---------------------------------------------------------------------------

fn geti(snap: &Value, k: &str) -> i64 {
    snap.get(k).and_then(|v| v.as_i64()).unwrap_or(0)
}
fn getu(snap: &Value, k: &str) -> u64 {
    snap.get(k).and_then(|v| v.as_u64()).unwrap_or(0)
}
fn saved_mcp(snap: &Value) -> u64 {
    snap.get("tokens_saved_mcp")
        .and_then(|v| v.as_i64())
        .or_else(|| snap.get("tokens_saved_est").and_then(|v| v.as_i64()))
        .unwrap_or(0)
        .max(0) as u64
}
fn tool_ops(snap: &Value, name: &str) -> i64 {
    snap["by_tool"]
        .as_array()
        .and_then(|a| a.iter().find(|t| t["tool"].as_str() == Some(name)))
        .and_then(|t| t["ops"].as_i64())
        .unwrap_or(0)
}
fn buckets(snap: &Value, key: &str) -> Vec<i64> {
    snap.get(key)
        .and_then(|v| v.as_array())
        .map(|a| a.iter().map(|x| x.as_i64().unwrap_or(0)).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Sparkline + ANSI-aware width/pad/clip + color
// ---------------------------------------------------------------------------

/// Map a **cumulative** bucket series to `cells` block chars: diff to per-bucket
/// deltas, resample to `cells`, scale each to `▁..█` by its fraction of the max.
/// Deterministic — the same input always yields the same string.
pub fn sparkline(cumulative: &[i64], cells: usize) -> String {
    if cells == 0 {
        return String::new();
    }
    let deltas: Vec<i64> = if cumulative.len() < 2 {
        cumulative.to_vec()
    } else {
        cumulative.windows(2).map(|w| (w[1] - w[0]).max(0)).collect()
    };
    let groups = resample(&deltas, cells);
    let max = groups.iter().copied().max().unwrap_or(0);
    if max == 0 {
        return BLOCKS[0].to_string().repeat(cells);
    }
    groups
        .iter()
        .map(|&v| {
            if v <= 0 {
                BLOCKS[0]
            } else {
                let idx = ((v as f64 / max as f64) * (BLOCKS.len() - 1) as f64).round() as usize;
                BLOCKS[idx.min(BLOCKS.len() - 1)]
            }
        })
        .collect()
}

/// Resample `deltas` to exactly `cells` buckets (sum within each source range;
/// nearest-neighbor stretch when `deltas` is shorter than `cells`).
fn resample(deltas: &[i64], cells: usize) -> Vec<i64> {
    let n = deltas.len();
    if n == 0 {
        return vec![0; cells];
    }
    (0..cells)
        .map(|i| {
            let lo = i * n / cells;
            let hi = ((i + 1) * n / cells).max(lo + 1).min(n);
            deltas[lo..hi].iter().copied().sum()
        })
        .collect()
}

/// Wrap `items` into lines no wider than `width`, joined by `sep`.
fn flow(items: &[String], width: usize, sep: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for it in items {
        if !cur.is_empty() && vis_width(&cur) + vis_width(sep) + vis_width(it) > width {
            lines.push(std::mem::take(&mut cur));
        }
        if cur.is_empty() {
            cur = it.clone();
        } else {
            cur.push_str(sep);
            cur.push_str(it);
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Frame `lines` in a box with `title` in the top border (full view).
fn panel(title: &str, lines: &[String], w: usize, color: Theme) -> String {
    let inner = w.saturating_sub(4).max(1);
    let head_plain = format!("┌─ {title} ");
    let fill = w.saturating_sub(vis_width(&head_plain) + 1);
    let mut out = String::new();
    out.push_str(&dim("┌─ ", color));
    out.push_str(&accent(title, color));
    out.push_str(&dim(&format!(" {}┐", "─".repeat(fill)), color));
    out.push('\n');
    for line in lines {
        out.push_str(&dim("│ ", color));
        out.push_str(&fit(line, inner, color));
        out.push_str(&dim(" │", color));
        out.push('\n');
    }
    out.push_str(&dim(&format!("└{}┘", "─".repeat(w.saturating_sub(2))), color));
    out.push('\n');
    out
}

/// A lightweight titled section without side borders (mini view).
fn section_mini(title: &str, lines: &[String], w: usize, color: Theme) -> String {
    let prefix = format!("── {title} ");
    let fill = w.saturating_sub(vis_width(&prefix));
    let mut out = format!("{}\n", dim(&format!("{prefix}{}", "─".repeat(fill)), color));
    for line in lines {
        out.push_str(&clip(line, w));
        out.push('\n');
    }
    out.push('\n');
    out
}

/// Visible column width of `s`, ignoring ANSI SGR escape sequences. Every non-escape
/// scalar counts as one column (the box/block/×/arrow glyphs used here are width-1).
fn vis_width(s: &str) -> usize {
    let mut w = 0;
    let mut in_esc = false;
    for c in s.chars() {
        if in_esc {
            if c.is_ascii_alphabetic() {
                in_esc = false;
            }
        } else if c == '\x1b' {
            in_esc = true;
        } else {
            w += 1;
        }
    }
    w
}

/// Truncate `s` to a visible width of `max`, skipping ANSI escapes; if truncation cut
/// inside a colored span, append a reset so the box border isn't tinted.
fn clip(s: &str, max: usize) -> String {
    if vis_width(s) <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    let mut in_esc = false;
    let mut saw_color = false;
    for c in s.chars() {
        if in_esc {
            out.push(c);
            if c.is_ascii_alphabetic() {
                in_esc = false;
            }
        } else if c == '\x1b' {
            in_esc = true;
            saw_color = true;
            out.push(c);
        } else {
            if w == max {
                break;
            }
            out.push(c);
            w += 1;
        }
    }
    if saw_color {
        out.push_str("\x1b[0m");
    }
    out
}

/// Clip then right-pad with spaces to exactly `width` visible columns.
fn fit(s: &str, width: usize, _color: Theme) -> String {
    let clipped = clip(s, width);
    let w = vis_width(&clipped);
    if w >= width {
        clipped
    } else {
        format!("{clipped}{}", " ".repeat(width - w))
    }
}

fn sgr(s: &str, code: &str, on: bool) -> String {
    if on {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// The color treatment threaded through the (pure) renderer: whether ANSI is emitted
/// at all (`on`, the old `color` bool, gated by `NO_COLOR`/TTY) and which `--theme`
/// palette to draw from. `Copy`, so it threads like the bool it replaces.
#[derive(Clone, Copy)]
pub struct Theme {
    on: bool,
    kind: ThemeKind,
}

/// The two `--theme` palettes. `Dark` is the cool default, mirroring the web dashboard's
/// default dark theme; `Seventies` is the warm retro scheme.
#[derive(Clone, Copy)]
pub enum ThemeKind {
    Dark,
    Seventies,
}

impl Theme {
    /// Color off (NO_COLOR or not a TTY): every role renders as plain text.
    pub const OFF: Theme = Theme { on: false, kind: ThemeKind::Dark };
    pub fn new(on: bool, kind: ThemeKind) -> Theme {
        Theme { on, kind }
    }
}

impl ThemeKind {
    /// Parse a `--theme` value: `dark`, or `70s`/`seventies`/`retro`.
    pub fn parse(s: &str) -> Option<ThemeKind> {
        match s.trim().to_lowercase().as_str() {
            "dark" => Some(ThemeKind::Dark),
            "70s" | "seventies" | "retro" => Some(ThemeKind::Seventies),
            _ => None,
        }
    }
}

// Role names, not literal hues: `accent` = panel titles/labels, `gold` = headline
// values, `warn` = attention, `dim` = borders/captions. Each role maps to an xterm-256
// code per palette — universally supported, still zero-dep, `NO_COLOR`-gated by `run`.
fn dim(s: &str, color: Theme) -> String {
    let code = match color.kind {
        ThemeKind::Seventies => "38;5;101", // muted khaki
        ThemeKind::Dark => "38;5;245",      // slate gray
    };
    sgr(s, code, color.on)
}
fn bold(s: &str, color: Theme) -> String {
    sgr(s, "1", color.on)
}
fn accent(s: &str, color: Theme) -> String {
    let code = match color.kind {
        ThemeKind::Seventies => "38;5;107", // avocado
        ThemeKind::Dark => "38;5;73",       // soft teal
    };
    sgr(s, code, color.on)
}
fn gold(s: &str, color: Theme) -> String {
    let code = match color.kind {
        ThemeKind::Seventies => "38;5;178", // harvest gold
        ThemeKind::Dark => "38;5;80",       // bright teal
    };
    sgr(s, code, color.on)
}
fn warn(s: &str, color: Theme) -> String {
    let code = match color.kind {
        ThemeKind::Seventies => "38;5;166", // burnt orange
        ThemeKind::Dark => "38;5;179",      // amber
    };
    sgr(s, code, color.on)
}

// ---------------------------------------------------------------------------
// Time window (--since / --today)
// ---------------------------------------------------------------------------

/// A TUI time-window spec. Resolved to a `since` cutoff each tick, so relative windows
/// ("last 1h") slide and "today" stays correct across midnight.
#[derive(Clone)]
pub enum Window {
    All,
    Today,
    Last(u64), // seconds back from now
}

impl Window {
    /// Parse a `--since` spec: `all`, `today`, or a duration like `15m`/`1h`/`3h`/`2d`.
    pub fn parse(spec: &str) -> Option<Window> {
        match spec.trim() {
            "all" => Some(Window::All),
            "today" => Some(Window::Today),
            s => parse_duration(s).map(Window::Last),
        }
    }
    /// The `since` cutoff (unix secs) at `now`, given the local TZ offset (for `today`'s
    /// midnight). `None` = all time.
    fn since(&self, now: i64, tz_offset: i64) -> Option<i64> {
        match self {
            Window::All => None,
            Window::Today => {
                let local = now + tz_offset;
                Some(local - local.rem_euclid(86_400) - tz_offset)
            }
            Window::Last(secs) => Some(now - *secs as i64),
        }
    }
    /// Header label for the active window.
    fn label(&self) -> String {
        match self {
            Window::All => "all time".to_string(),
            Window::Today => "today".to_string(),
            Window::Last(secs) => format!("last {}", human_dur(*secs)),
        }
    }
}

/// Parse `45s` / `15m` / `90m` / `1h` / `3h` / `2d` to seconds.
fn parse_duration(s: &str) -> Option<u64> {
    let i = s.find(|c: char| !c.is_ascii_digit())?;
    if i == 0 {
        return None;
    }
    let n: u64 = s[..i].parse().ok()?;
    let mult = match &s[i..] {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => return None,
    };
    n.checked_mul(mult)
}

/// Compact duration label: `45m` / `2h` / `3d`.
fn human_dur(secs: u64) -> String {
    if secs >= 86_400 && secs.is_multiple_of(86_400) {
        format!("{}d", secs / 86_400)
    } else if secs >= 3600 && secs.is_multiple_of(3600) {
        format!("{}h", secs / 3600)
    } else {
        format!("{}m", secs.max(60) / 60)
    }
}

/// Local UTC offset in seconds via `date +%z` (`+HHMM`/`-HHMM`); 0 if unavailable.
fn local_offset_secs() -> i64 {
    if let Ok(out) = std::process::Command::new("date").arg("+%z").output() {
        if let Ok(s) = String::from_utf8(out.stdout) {
            let s = s.trim();
            if s.len() == 5 {
                let sign = if s.starts_with('-') { -1 } else { 1 };
                if let (Ok(h), Ok(m)) = (s[1..3].parse::<i64>(), s[3..5].parse::<i64>()) {
                    return sign * (h * 3600 + m * 60);
                }
            }
        }
    }
    0
}

// ---------------------------------------------------------------------------
// Refresh loop
// ---------------------------------------------------------------------------

/// Run the live terminal dashboard: each tick reads the snapshot directly (no HTTP),
/// clears the screen, renders, and sleeps `interval` seconds — the `lens stats
/// --watch` model. `scope_global` reads the machine-global mirror (cross-repo, no
/// session filter), reusing the web's scope branch. `force_view`: `Some(true)` =
/// full, `Some(false)` = mini, `None` = auto by terminal width. Cooked mode, so
/// Ctrl-C exits cleanly with nothing to restore.
#[allow(clippy::too_many_arguments)]
pub fn run(
    dir: PathBuf,
    session: Option<String>,
    scope_global: bool,
    interval: u64,
    window: Window,
    rate: f64,
    rt_seconds: f64,
    theme_kind: ThemeKind,
    force_view: Option<bool>,
) -> Result<()> {
    let on = std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal();
    let color = Theme::new(on, theme_kind);
    let interval = interval.max(1);
    let tz_offset = local_offset_secs();
    let window_label = window.label();
    // RTK gain is a cumulative counter; like the web dashboard we show the delta
    // since this view launched, captured on the first tick, so both read "since
    // opened" rather than all-time.
    let mut rtk_base: Option<(i64, i64, i64)> = None;
    loop {
        let (d, sess) = if scope_global {
            match crate::rtk::home_root() {
                Some(home) => (home, None),
                None => (dir.clone(), session.clone()),
            }
        } else {
            (dir.clone(), session.clone())
        };
        // Resolve the window each tick so "last 1h" slides and "today" stays correct.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let since = window.since(now, tz_offset);
        let mut snap = snapshot_json_since(&d, sess.as_deref(), since, None);
        // Surface scope + window to the renderer (pure fn can't know them otherwise).
        snap["scope_global"] = Value::Bool(scope_global);
        snap["window_label"] = Value::String(window_label.clone());
        rebase_rtk(&mut snap, &mut rtk_base);
        let cols = term_cols();
        let width = match force_view {
            Some(false) => 50,             // --mini: compact single column
            Some(true) => cols.max(56),    // --full: framed layout at terminal width
            None => cols,                  // auto: mini/full by width
        };
        let frame = render_snapshot(&snap, width, color, rate, rt_seconds);
        print!("\x1b[2J\x1b[H{frame}");
        let _ = std::io::stdout().flush();
        std::thread::sleep(Duration::from_secs(interval));
    }
}

/// Rewrite the snapshot's `rtk` totals to the delta since the first tick (the
/// `base`), matching the web dashboard's first-poll baseline. No-op when RTK is not
/// installed. `avg_savings_pct` is recomputed from the windowed input/saved delta.
fn rebase_rtk(snap: &mut Value, base: &mut Option<(i64, i64, i64)>) {
    if snap["rtk"]["installed"].as_bool() != Some(true) {
        return;
    }
    let cur = (
        snap["rtk"]["total_commands"].as_i64().unwrap_or(0),
        snap["rtk"]["total_saved"].as_i64().unwrap_or(0),
        snap["rtk"]["total_input"].as_i64().unwrap_or(0),
    );
    let (bc, bs, bi) = *base.get_or_insert(cur);
    let d_saved = (cur.1 - bs).max(0);
    let d_input = (cur.2 - bi).max(0);
    let pct = if d_input > 0 {
        d_saved as f64 / d_input as f64 * 100.0
    } else {
        0.0
    };
    snap["rtk"]["total_commands"] = Value::from((cur.0 - bc).max(0));
    snap["rtk"]["total_saved"] = Value::from(d_saved);
    snap["rtk"]["avg_savings_pct"] = Value::from(pct);
}

/// Terminal width in columns: `stty size` ("rows cols"), else `$COLUMNS`, else 80.
fn term_cols() -> u16 {
    if let Ok(out) = std::process::Command::new("stty")
        .arg("size")
        .stdin(std::process::Stdio::inherit())
        .output()
    {
        if let Ok(s) = String::from_utf8(out.stdout) {
            if let Some(cols) = s.split_whitespace().nth(1).and_then(|c| c.parse::<u16>().ok()) {
                if cols > 0 {
                    return cols;
                }
            }
        }
    }
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse::<u16>().ok())
        .filter(|&c| c > 0)
        .unwrap_or(80)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obs::stats::snapshot_json;
    use crate::obs::OpLog;
    use tempfile::tempdir;

    /// Color-on theme for the ANSI-aware width tests (palette is irrelevant there).
    const ON: Theme = Theme { on: true, kind: ThemeKind::Seventies };

    /// A snapshot with one op in each of three value dimensions, so every panel has
    /// content (and the scorecard carries non-zero `your_ops`).
    fn seeded_snap() -> Value {
        let dir = tempdir().unwrap();
        let log = OpLog::open(dir.path());
        log.start("lens_run", serde_json::json!({}))
            .finish(8000, 100, Some("a".into()), "ok", "", None);
        log.start("lens_search", serde_json::json!({}))
            .finish(50, 50, None, "ok", "", None);
        log.start("lens_symbol", serde_json::json!({}))
            .finish(40, 40, None, "ok", "", None);
        snapshot_json(dir.path(), None)
    }

    #[test]
    fn sparkline_is_deterministic() {
        assert_eq!(sparkline(&[0, 1, 3, 6, 10], 4), "▃▅▆█");
        // Flat cumulative ⇒ all-low baseline, exact length.
        assert_eq!(sparkline(&[5, 5, 5, 5], 6), "▁▁▁▁▁▁");
        // Empty ⇒ low baseline of the requested width.
        assert_eq!(sparkline(&[], 3), "▁▁▁");
        // Same input twice ⇒ identical output (determinism).
        let a = sparkline(&[0, 2, 2, 9, 9, 20], 10);
        let b = sparkline(&[0, 2, 2, 9, 9, 20], 10);
        assert_eq!(a, b);
        assert_eq!(a.chars().count(), 10);
    }

    #[test]
    fn vis_width_skips_ansi() {
        assert_eq!(vis_width("abc"), 3);
        assert_eq!(vis_width("\x1b[2mabc\x1b[0m"), 3);
        assert_eq!(vis_width(&gold("hi", ON)), 2);
        // Box + block glyphs are width-1 each.
        assert_eq!(vis_width("█▁→×"), 4);
    }

    #[test]
    fn fit_pads_and_clips_to_exact_width() {
        assert_eq!(vis_width(&fit("ab", 5, Theme::OFF)), 5);
        assert_eq!(vis_width(&fit("abcdef", 4, Theme::OFF)), 4);
        // Colored content still fits exactly (ANSI bytes don't count).
        assert_eq!(vis_width(&fit(&gold("abcdef", ON), 4, Theme::OFF)), 4);
    }

    #[test]
    fn render_full_has_every_dimension() {
        let snap = seeded_snap();
        let out = render_snapshot(&snap, 80, Theme::OFF, 5.0, 4.0);
        // Headline + panels.
        assert!(out.contains("saved"), "money headline");
        assert!(out.contains("APPLIED VALUE"), "applied-value panel title");
        assert!(out.contains("BY MECHANISM"));
        assert!(out.contains("RTK SHELL"));
        assert!(out.contains("SESSION ACTIVITY"));
        // Canonical tools listed even when dormant.
        for t in ADOPTION_TOOLS {
            assert!(out.contains(t), "tool {t} missing from table");
        }
        // Applied-value panel: measured + estimated tokens, round-trips, time, dims.
        assert!(out.contains("measured saved"), "measured line");
        assert!(out.contains("est. total avoided"), "estimated total line");
        assert!(out.contains("round-trips avoided"), "round-trips line");
        assert!(out.contains("time saved"), "time-saved line");
        assert!(out.contains("@ 4s/round-trip"), "time basis exposed");
        assert!(out.contains("by dimension"));
        assert!(out.contains("navigation"), "per-dimension breakdown");
        assert!(out.contains("benchmark rates applied"), "applied-value note");
        // Footer.
        assert!(out.contains("store "));
        assert!(out.contains("graph "));
    }

    #[test]
    fn render_mini_keeps_dimensions_no_box() {
        let snap = seeded_snap();
        let out = render_snapshot(&snap, 40, Theme::OFF, 5.0, 4.0);
        // Same dimensions, terser. No box side glyph.
        assert!(out.contains("APPLIED VALUE"));
        assert!(!out.contains('│'), "mini view drops box sides");
        assert!(out.contains("darkroom"));
        assert!(out.contains("lens_run"));
        assert!(out.contains("time saved"));
        // Every rendered line stays within the width budget.
        for line in out.lines() {
            assert!(vis_width(line) <= 40, "mini line over width: {line:?}");
        }
    }

    #[test]
    fn full_lines_fit_the_frame() {
        let snap = seeded_snap();
        let out = render_snapshot(&snap, 80, ON, 5.0, 4.0);
        for line in out.lines() {
            assert!(vis_width(line) <= 80, "full line over width ({}): {line:?}", vis_width(line));
        }
    }

    #[test]
    fn money_scales_with_rate() {
        let dir = tempdir().unwrap();
        OpLog::open(dir.path())
            .start("lens_run", serde_json::json!({}))
            .finish(4_000_000, 0, Some("a".into()), "ok", "", None);
        let snap = snapshot_json(dir.path(), None);
        // 1,000,000 tokens saved @ $5/M ⇒ $5.00.
        let out = render_snapshot(&snap, 80, Theme::OFF, 5.0, 4.0);
        assert!(out.contains("$5.00 saved"), "expected $5.00 in: {}", out.lines().next().unwrap());
    }

    #[test]
    fn time_saved_scales_with_rt_seconds() {
        let dir = tempdir().unwrap();
        // 3 navigation ops ⇒ measured round-trips; time = round-trips × rt_seconds.
        let log = OpLog::open(dir.path());
        for _ in 0..3 {
            log.start("lens_symbol", serde_json::json!({})).finish(40, 40, None, "ok", "", None);
        }
        let snap = snapshot_json(dir.path(), None);
        let rts = snap["applied_value"]["round_trips_avoided"].as_f64().unwrap();
        assert!(rts > 6.0, "3 nav ops avoid >6 round-trips, got {rts}");
        // Doubling rt_seconds roughly doubles the rendered minutes; just assert both render.
        let a = render_snapshot(&snap, 80, Theme::OFF, 5.0, 4.0);
        let b = render_snapshot(&snap, 80, Theme::OFF, 5.0, 10.0);
        assert!(a.contains("@ 4s/round-trip") && b.contains("@ 10s/round-trip"));
    }

    #[test]
    fn tool_table_lists_canonical_tools() {
        // Parity with the web ADOPTION_TOOLS list: same ten names.
        assert_eq!(ADOPTION_TOOLS.len(), 10);
        for t in [
            "lens_run", "lens_run_file", "lens_search", "lens_index", "lens_map",
            "lens_recall", "lens_symbol", "lens_links", "lens_path", "lens_find",
        ] {
            assert!(ADOPTION_TOOLS.contains(&t), "{t} missing");
        }
    }

    #[test]
    fn window_parse_and_resolve() {
        assert!(matches!(Window::parse("all"), Some(Window::All)));
        assert!(matches!(Window::parse("today"), Some(Window::Today)));
        assert!(matches!(Window::parse("1h"), Some(Window::Last(3600))));
        assert!(matches!(Window::parse("90m"), Some(Window::Last(5400))));
        assert!(matches!(Window::parse("2d"), Some(Window::Last(172_800))));
        assert!(Window::parse("bogus").is_none());
        assert!(Window::parse("h").is_none());

        let now = 1_000_000_000;
        assert_eq!(Window::All.since(now, 0), None);
        assert_eq!(Window::Last(3600).since(now, 0), Some(now - 3600));
        // today at UTC: midnight = now − (now mod 86400), always within the last day.
        let mid = Window::Today.since(now, 0).unwrap();
        assert_eq!(mid, now - now.rem_euclid(86_400));
        assert!(now - mid < 86_400);
        // a positive TZ offset moves the local-midnight boundary.
        assert_ne!(Window::Today.since(now, 3600), Window::Today.since(now, 0));

        assert_eq!(Window::Today.label(), "today");
        assert_eq!(Window::Last(3600).label(), "last 1h");
        assert_eq!(Window::Last(5400).label(), "last 90m");
        assert_eq!(Window::All.label(), "all time");
    }

    #[test]
    fn theme_switches_palette() {
        // Same role, different SGR code per palette; off ⇒ plain text either way.
        let g70 = gold("x", Theme::new(true, ThemeKind::Seventies));
        let gdk = gold("x", Theme::new(true, ThemeKind::Dark));
        assert!(g70.contains("38;5;178"), "70s gold is harvest gold: {g70:?}");
        assert!(gdk.contains("38;5;80"), "dark gold is teal: {gdk:?}");
        assert_ne!(g70, gdk);
        assert_eq!(gold("x", Theme::OFF), "x");
        // Whole-frame switch: dark renders teal titles, 70s renders avocado.
        let snap = seeded_snap();
        let dark = render_snapshot(&snap, 80, Theme::new(true, ThemeKind::Dark), 5.0, 4.0);
        let seventies = render_snapshot(&snap, 80, Theme::new(true, ThemeKind::Seventies), 5.0, 4.0);
        assert!(dark.contains("38;5;73"), "dark theme uses teal titles");
        assert!(seventies.contains("38;5;107"), "70s theme uses avocado titles");
        // Parse accepts both spellings, rejects junk.
        assert!(matches!(ThemeKind::parse("dark"), Some(ThemeKind::Dark)));
        assert!(matches!(ThemeKind::parse("70s"), Some(ThemeKind::Seventies)));
        assert!(matches!(ThemeKind::parse("SEVENTIES"), Some(ThemeKind::Seventies)));
        assert!(ThemeKind::parse("blue").is_none());
    }
}
