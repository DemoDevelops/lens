//! Stat strip + tools adoption table (the web stat row + `by_tool` table).
//!
//! `stat_strip` mirrors the web `#strip` line (`dashboard.rs` `stat(...)` calls,
//! filled `877-888`); `tools_table` mirrors the merged `#tools` table (filled
//! `896-917`). Both read the same `by_tool`/`raw_bytes_in`/`bytes_returned`/etc.
//! fields the old ANSI renderer's `stats_strip`/`tool_table` read — only the
//! output changed, from box-drawing strings to ratatui widgets.

use ratatui::layout::{Alignment, Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;
use serde_json::Value;

use super::model;
use super::App;

/// One fixed hue per data column (ops/raw/ret/saved/save%/off/err/to), not
/// palette-derived — a small decorative categorical set (picked to read
/// clearly against either theme's near-black bg) so each column is
/// identifiable on sight instead of every header reading the same dim gray.
const HEADER_HUES: [Color; 8] = [
    Color::Rgb(0xe0, 0xa0, 0x5a), // ops: orange
    Color::Rgb(0xe0, 0xc8, 0x6e), // raw: yellow
    Color::Rgb(0x8c, 0xc8, 0x96), // ret: green
    Color::Rgb(0xb4, 0x96, 0xdc), // saved: lavender
    Color::Rgb(0xe6, 0x8c, 0xaf), // save%: pink
    Color::Rgb(0x78, 0xc8, 0xd2), // off: cyan
    Color::Rgb(0xe0, 0x82, 0x82), // err: soft red
    Color::Rgb(0xe6, 0xb4, 0x78), // to: soft amber
];

/// Canonical lens MCP tools, always shown in the tools table (dimmed at 0 ops) so a
/// dormant tool reads as unused, not absent. Mirrors `ADOPTION_TOOLS` in the web
/// `INDEX_HTML` (`dashboard.rs:514`) and the old `tui.rs` constant of the same name.
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

/// Hover description per tool (the web's native `title` tooltip), surfaced here as
/// the hint line under the selected tools-table row. Copied verbatim from
/// `TOOL_DESC` in `dashboard.rs:516-533`.
pub(crate) const TOOL_DESC: &[(&str, &str)] = &[
    ("lens_run", "Run code (python/js/ts/bash/ruby/go) in a darkroom subprocess; only stdout/stderr returns to context, not the data the script read. Large output is offloaded with a recall ref."),
    ("lens_run_file", "Analyze one file in the darkroom; your code gets the file path as its first CLI arg (sys.argv[1] / process.argv[2] / $1). Only what it prints returns; the file's bytes stay out of context."),
    ("lens_index", "Build a full-text index over a file or directory (respects .gitignore). Returns files indexed and chunk count; prerequisite for lens_search."),
    ("lens_search", "Run one or more BM25-ranked full-text queries in a single call. Returns the top snippets per query with path and relevance score; answers 'where is X mentioned'. For symbol defs/relationships use lens_symbol instead."),
    ("lens_map", "Parse the whole repo with tree-sitter into a symbol graph (functions, types, modules) and their relationships (calls, imports, contains). Run once per repo, then query with lens_symbol/lens_links/lens_path."),
    ("lens_symbol", "Find graph symbols by name substring (+ optional kind) and return each with its immediate connections: where a symbol lives and what directly touches it. Don't know the name? Use lens_find instead."),
    ("lens_find", "Find symbols by a natural-language query, ranked lexically by word overlap with symbol names. Use when you know what a symbol does but not its exact name; know the name? Use lens_symbol."),
    ("lens_links", "Return the local subgraph within N hops of a node id: a symbol's neighborhood or blast radius at a chosen depth. For one specific A-to-B connection use lens_path instead."),
    ("lens_path", "Find the shortest path between two symbols via BFS over graph edges: how A reaches B through the call/import chain. For a symbol's whole neighborhood use lens_links instead."),
    ("lens_recall", "Recover the full blob behind a retrieve_ref returned by another tool, reversing any truncation or offloading."),
    ("lens_skeleton", "Show a source file's structure cheaply: signatures, types, and nesting with executable bodies elided to '...'. Far fewer tokens than reading the whole file; full text is one lens_recall away. Use this instead of Read to see a file's shape; include_bodies returns chosen bodies inline."),
    ("lens_grep_ast", "Structural code search via a tree-sitter query (S-expression): matches syntax, not text, so it finds real calls without the false positives grep hits in comments or strings. Returns path:line matches. For plain-text search use lens_search instead."),
    ("lens_overview", "Token-budgeted repo overview: the most structurally important symbols (PageRank-ranked) with their callers and callees, as much as fits a token budget. A high-signal map of a codebase at fixed cost. For one file's structure use lens_skeleton instead."),
    ("lens_stats", "Report darkroom usage, estimated tokens saved, and current index/graph sizes for this repo."),
    ("lens_memory_record", "Save one durable note (a decision, constraint, rejected approach, or rule) that outlives the session and comes back on the next SessionStart; also lands in the search index."),
    ("lens_memory_query", "Pull back durable notes saved earlier: the whole running list, or the closest matches to a query when one is given."),
];

/// `TOOL_DESC[name]`, or a fallback for a historical/third-party tool recorded in the
/// data dir that isn't one of lens's current tools (mirrors the web's `||` fallback).
fn tool_desc(name: &str) -> &'static str {
    TOOL_DESC
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, d)| *d)
        .unwrap_or(
            "Historical or third-party tool recorded in this data dir; not a current lens tool.",
        )
}

/// The `by_tool` row for one tool, if it has fired at least once.
fn find_tool<'a>(by_tool: &'a Value, name: &str) -> Option<&'a Value> {
    by_tool
        .as_array()?
        .iter()
        .find(|t| t["tool"].as_str() == Some(name))
}

/// The op count for one tool, or 0 if it hasn't fired (or isn't in `by_tool` at all).
fn tool_ops(by_tool: &Value, name: &str) -> i64 {
    find_tool(by_tool, name)
        .and_then(|t| t["ops"].as_i64())
        .unwrap_or(0)
}

/// One-row strip: ops(err,to) · raw · ret · saved · save% · tools · offloaded · lock.
pub(crate) fn stat_strip(f: &mut Frame, area: Rect, app: &App) {
    let snap = &app.snapshot;
    let p = &app.palette;

    let raw = model::getu(snap, "raw_bytes_in");
    let ret = model::getu(snap, "bytes_returned");
    let overall_pct = if raw > 0 {
        ((raw as i64 - ret as i64) * 100 / raw as i64).max(0)
    } else {
        0
    };
    let by_tool = model::by_tool(snap);
    let fired = ADOPTION_TOOLS
        .iter()
        .filter(|name| tool_ops(by_tool, name) > 0)
        .count();
    let errors = model::geti(snap, "errors");
    let timeouts = model::geti(snap, "timeouts");

    let label = Style::default().fg(p.dim);
    let value = Style::default().fg(p.ink);
    let err_style = if errors > 0 {
        Style::default().fg(p.bad)
    } else {
        label
    };
    let to_style = if timeouts > 0 {
        Style::default().fg(p.warn)
    } else {
        label
    };

    // Two shorter lines instead of one 9-chip wall — each comfortably fits an
    // 80-col terminal instead of running off the right edge.
    let line_a = Line::from(vec![
        Span::styled("ops ", label),
        Span::styled(model::geti(snap, "ops").to_string(), value),
        Span::styled(" (", label),
        Span::styled(format!("{errors} err"), err_style),
        Span::styled(", ", label),
        Span::styled(format!("{timeouts} to"), to_style),
        Span::styled(")", label),
        Span::styled("  ·  ", label),
        Span::styled("raw ", label),
        Span::styled(model::human_bytes(raw), value),
        Span::styled("  ·  ", label),
        Span::styled("ret ", label),
        Span::styled(model::human_bytes(ret), value),
        Span::styled("  ·  ", label),
        Span::styled("save% ", label),
        Span::styled(format!("{overall_pct}%"), value),
    ]);
    let line_b = Line::from(vec![
        Span::styled("saved ", label),
        Span::styled(
            format!("{} tok", model::human_count(model::saved_mcp(snap))),
            value,
        ),
        Span::styled("  ·  ", label),
        Span::styled("tools ", label),
        Span::styled(format!("{fired}/{}", ADOPTION_TOOLS.len()), value),
        Span::styled("  ·  ", label),
        Span::styled("off ", label),
        Span::styled(
            format!(
                "{} ({})",
                model::geti(snap, "offloaded_ops"),
                model::human_bytes(model::getu(snap, "offloaded_bytes"))
            ),
            value,
        ),
        Span::styled("  ·  ", label),
        Span::styled("lock ", label),
        Span::styled(format!("{} ms", model::geti(snap, "lock_wait_ms")), value),
    ]);

    f.render_widget(Paragraph::new(vec![line_a, line_b]), area);
}

/// Right-pad/truncate (with `…`) `s` to exactly `width` display columns, so a
/// long third-party tool name can't push the table's fixed columns out of line.
fn fit_label(s: &str, width: usize) -> String {
    let count = s.chars().count();
    if count <= width {
        return format!("{s:<width$}");
    }
    if width == 0 {
        return String::new();
    }
    let mut truncated: String = s.chars().take(width - 1).collect();
    truncated.push('…');
    truncated
}

/// A right-aligned numeric cell in `style`.
pub(crate) fn num_cell(text: String, style: Style) -> Cell<'static> {
    Cell::from(Line::from(Span::styled(text, style)).alignment(Alignment::Right))
}

/// Greedy word-wrap line count for `text` at `width` columns — the same
/// algorithm `Paragraph`'s `Wrap` uses, so this predicts its actual height.
pub(crate) fn wrap_line_count(text: &str, width: usize) -> usize {
    if width == 0 {
        return 1;
    }
    let mut lines = 1usize;
    let mut col = 0usize;
    for word in text.split_whitespace() {
        let wlen = word.chars().count();
        if col == 0 {
            col = wlen;
        } else if col + 1 + wlen <= width {
            col += 1 + wlen;
        } else {
            lines += 1;
            col = wlen;
        }
    }
    lines.max(1)
}

/// The tallest the hint line ever needs to be at `width` columns: the longest
/// `"name: description"` in [`TOOL_DESC`], wrapped. A fixed worst-case instead
/// of measuring the current selection, so the panel's total height doesn't
/// jump around as the user browses tools with `j`/`k`.
pub(crate) fn max_hint_lines(width: u16) -> usize {
    TOOL_DESC
        .iter()
        .map(|(name, desc)| wrap_line_count(&format!("{name}: {desc}"), width as usize))
        .max()
        .unwrap_or(1)
}

/// The 10 canonical tools plus any extras that fired — the exact row count
/// [`tools_table`] will draw, so the caller can size the panel to its real
/// content instead of guessing a fixed floor (a tall terminal shouldn't turn a
/// 14-row table into a mostly-empty box).
pub(crate) fn tools_row_count(app: &App) -> usize {
    let by_tool = model::by_tool(&app.snapshot);
    let extras = by_tool
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|t| t["tool"].as_str())
                .filter(|n| !ADOPTION_TOOLS.contains(n))
                .count()
        })
        .unwrap_or(0);
    ADOPTION_TOOLS.len() + extras
}

/// Per-tool adoption table: the 10 canonical tools (dim at 0 ops) + extras that
/// fired; `app.tool_sel` highlights a row and surfaces its description. Columns
/// are plain right-aligned numbers (no in-cell bar) so they stay aligned at any
/// terminal width instead of silently overlapping. Content only — no border;
/// `chrome::frame_overview_tools` owns the surrounding frame and dividers.
pub(crate) fn tools_rows(f: &mut Frame, area: Rect, app: &App) {
    let snap = &app.snapshot;
    let p = &app.palette;
    let by_tool = model::by_tool(snap);
    let empty: Vec<Value> = Vec::new();
    let entries = by_tool.as_array().unwrap_or(&empty);

    // Canonical tools first (always present), then any extra tool names that fired
    // but aren't canonical — mirrors the web's `ADOPTION_TOOLS.concat(extra)`.
    let extras: Vec<String> = entries
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

    let sel = app.tool_sel.min(names.len().saturating_sub(1));

    let rows: Vec<Row> = names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let t = find_tool(by_tool, name);
            let present = t.is_some();
            let ops = t.and_then(|t| t["ops"].as_i64()).unwrap_or(0);
            let raw = t.and_then(|t| t["raw"].as_u64()).unwrap_or(0);
            let ret = t.and_then(|t| t["returned"].as_u64()).unwrap_or(0);
            let saved = t.and_then(|t| t["saved"].as_i64()).unwrap_or(0);
            let errs = t.and_then(|t| t["errors"].as_i64()).unwrap_or(0);
            let tos = t.and_then(|t| t["timeouts"].as_i64()).unwrap_or(0);
            let offc = t.and_then(|t| t["offloaded_ops"].as_i64()).unwrap_or(0);
            let offb = t.and_then(|t| t["offloaded_bytes"].as_u64()).unwrap_or(0);

            let status = if !present || ops == 0 {
                p.dim
            } else if errs != 0 {
                p.bad
            } else {
                p.accent
            };
            let ink = Style::default().fg(p.ink);
            let dim = Style::default().fg(p.dim);

            let raw_cell = if present { model::human_bytes(raw) } else { "—".into() };
            let ret_cell = if present { model::human_bytes(ret) } else { "—".into() };
            let saved_cell = if saved != 0 { model::human_count(saved.max(0) as u64) } else { "—".into() };
            let pct_cell = if raw > 0 {
                format!("{}%", (raw as i64 - ret as i64) * 100 / raw as i64)
            } else {
                "—".into()
            };
            let off_cell = if offc != 0 {
                format!("{offc}·{}", model::human_bytes(offb))
            } else {
                "—".into()
            };
            let err_style = if errs != 0 { Style::default().fg(p.bad) } else { dim };
            let to_style = if tos != 0 { Style::default().fg(p.warn) } else { dim };

            // Only the tool name inverts on selection, not the whole row — the
            // rest of the row keeps its normal styling so the row-level status
            // colors (dim/accent/bad) stay legible while browsing with j/k.
            let name_style = if i == sel {
                Style::default().fg(status).add_modifier(Modifier::REVERSED)
            } else {
                Style::default().fg(status)
            };
            Row::new(vec![
                Cell::from(Span::styled(fit_label(name, 16), name_style)),
                num_cell(ops.to_string(), ink),
                num_cell(raw_cell, ink),
                num_cell(ret_cell, ink),
                num_cell(saved_cell, ink),
                num_cell(pct_cell, ink),
                num_cell(off_cell, dim),
                num_cell(if errs != 0 { errs.to_string() } else { "—".into() }, err_style),
                num_cell(if tos != 0 { tos.to_string() } else { "—".into() }, to_style),
            ])
        })
        .collect();

    // A distinct hue per column (not one uniform dim gray) makes each column
    // instantly identifiable at a glance instead of requiring the reader to
    // trace back up from a data cell to figure out which header it's under.
    let bold = |c: Color| Style::default().fg(c).add_modifier(Modifier::BOLD);
    let header = Row::new(vec![
        Cell::from(Span::styled("tool", bold(p.ink))),
        num_cell("ops".into(), bold(HEADER_HUES[0])),
        num_cell("raw".into(), bold(HEADER_HUES[1])),
        num_cell("ret".into(), bold(HEADER_HUES[2])),
        num_cell("saved".into(), bold(HEADER_HUES[3])),
        num_cell("save%".into(), bold(HEADER_HUES[4])),
        num_cell("off".into(), bold(HEADER_HUES[5])),
        num_cell("err".into(), bold(HEADER_HUES[6])),
        num_cell("to".into(), bold(HEADER_HUES[7])),
    ]);

    let widths = [
        Constraint::Length(16),
        Constraint::Length(6),
        Constraint::Length(9),
        Constraint::Length(9),
        Constraint::Length(8),
        Constraint::Length(6),
        Constraint::Length(10),
        Constraint::Length(4),
        Constraint::Length(4),
    ];

    let table = Table::new(rows, widths).header(header).column_spacing(2);
    let mut table_state = TableState::default().with_selected(Some(sel));
    f.render_stateful_widget(table, area, &mut table_state);
}

/// The selected row's `name: description`, wrapped — the web's native `title`
/// tooltip, surfaced as text since a terminal has no hover. Sized to the
/// *longest* tool description at this width by [`max_hint_lines`], not a
/// fixed guess, so no description ever gets cut off. Content only; the "info"
/// divider above it is `chrome::frame_overview_tools`'s.
pub(crate) fn render_hint(f: &mut Frame, area: Rect, app: &App) {
    let p = &app.palette;
    let by_tool = model::by_tool(&app.snapshot);
    let empty: Vec<Value> = Vec::new();
    let entries = by_tool.as_array().unwrap_or(&empty);
    let extras: Vec<String> = entries
        .iter()
        .filter_map(|t| t["tool"].as_str())
        .filter(|n| !ADOPTION_TOOLS.contains(n))
        .map(|s| s.to_string())
        .collect();
    let names: Vec<String> = ADOPTION_TOOLS.iter().map(|s| s.to_string()).chain(extras).collect();
    let sel = app.tool_sel.min(names.len().saturating_sub(1));

    let sel_name = names.get(sel).map(String::as_str).unwrap_or("");
    let hint = Line::from(vec![
        Span::styled(
            format!("{sel_name}: "),
            Style::default().fg(p.ink).add_modifier(Modifier::BOLD),
        ),
        Span::styled(tool_desc(sel_name), Style::default().fg(p.dim)),
    ]);
    f.render_widget(Paragraph::new(hint).wrap(Wrap { trim: true }), area);
}

#[cfg(test)]
mod tests {
    use super::super::theme::Palette;
    use super::super::{App, RateMode, ThemeKind, View, Window};
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};
    use serde_json::json;

    fn app_with(snapshot: serde_json::Value) -> App {
        App {
            snapshot,
            theme: ThemeKind::Dark,
            palette: Palette::from(ThemeKind::Dark),
            view: View::Full,
            rate_mode: RateMode::Actual,
            rate: 5.0,
            rt_seconds: 30.0,
            window: Window::All,
            scope_global: false,
            scope_label: "session".into(),
            projects: vec![],
            scope_idx: 0,
            tool_sel: 0,
            saved_series: vec![],
            bytes_series: vec![],
            event_series: vec![],
            dir: std::path::PathBuf::from("."),
            session: None,
            rtk_base: None,
            tz_offset: 0,
            interval: 1,
        }
    }

    fn render_tools(app: &App) -> String {
        let mut t = Terminal::new(TestBackend::new(120, 30)).unwrap();
        t.draw(|f| {
            let a = f.area();
            tools_rows(f, a, app);
        })
        .unwrap();
        t.backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn tools_table_shows_canonical_tools_and_seeded_stats() {
        assert_eq!(ADOPTION_TOOLS.len(), 10);

        let snap = json!({
            "by_tool": [
                {
                    "tool": "lens_run",
                    "ops": 42,
                    "raw": 4000,
                    "returned": 1000,
                    "saved": 750,
                    "errors": 0,
                    "timeouts": 0,
                    "offloaded_ops": 1,
                    "offloaded_bytes": 500
                }
            ]
        });
        let app = app_with(snap);
        let out = render_tools(&app);

        for name in ADOPTION_TOOLS {
            assert!(
                out.contains(name),
                "canonical tool {name} missing from render"
            );
        }
        // lens_run: ops=42, save% = round((4000-1000)/4000*100) = 75%
        assert!(out.contains("42"), "seeded ops count not rendered");
        assert!(out.contains("75%"), "computed save% not rendered");
    }

    #[test]
    fn stat_strip_renders_labels_and_computed_values() {
        let snap = json!({
            "ops": 12,
            "errors": 2,
            "timeouts": 1,
            "raw_bytes_in": 4000,
            "bytes_returned": 1000,
            "tokens_saved_mcp": 5000,
            "by_tool": [],
            "offloaded_ops": 3,
            "offloaded_bytes": 900,
            "lock_wait_ms": 15
        });
        let app = app_with(snap);
        // Wide enough that the compact strip isn't truncated mid-item (it's one
        // un-wrapped line by design, like a terminal status bar).
        let mut t = Terminal::new(TestBackend::new(160, 5)).unwrap();
        t.draw(|f| {
            let a = f.area();
            stat_strip(f, a, &app);
        })
        .unwrap();
        let out: String = t
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();

        assert!(out.contains("ops"));
        assert!(out.contains("12"));
        assert!(out.contains("2 err"));
        assert!(out.contains("1 to"));
        // save% = round((4000-1000)/4000*100) = 75%
        assert!(out.contains("save% 75%"));
        assert!(out.contains("lock 15 ms"));
    }
}
