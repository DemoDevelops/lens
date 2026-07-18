//! Sparklines + full-view charts — the web dashboard's charts row and its
//! full-view `#fullcharts` group, as ratatui widgets.
//!
//! [`charts_row`] (both views) draws the two per-minute sparklines ("tokens
//! saved/min", "bytes returned/min") as braille [`Chart`] lines, titled with
//! the current rate (last cumulative bucket ÷ window minutes, exactly as the
//! web headline computes it). [`fullcharts`] (full view only) draws the four
//! `#fullcharts` panels in the web's 2×2 grid order: the cumulative
//! saved-tokens line, then saved-by-tool / ops-by-tool / compression-per-tool
//! as top-12 horizontal bars.
//!
//! The bar rows are hand-built styled [`Span`]s (a label, a block-glyph track,
//! a value) rather than a `BarChart`, because the compression bar stacks two
//! segments — returned (accent) + saved (dim) — inside one track, which a
//! `BarChart` bar can't express, and the web's `.hbar` is exactly that
//! label/track/value shape. Titles use `Block::title_top` (not the deprecated
//! `Block::title`) so the panel stays `-D warnings` clean.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Block, Borders, Chart, Dataset, GraphType, Paragraph};
use ratatui::Frame;

use serde_json::Value;

use super::model;
use super::theme::Palette;
use super::App;

// ---------------------------------------------------------------------------
// charts row — the two per-minute sparklines (both views)
// ---------------------------------------------------------------------------

/// "tokens saved/min" + "bytes returned/min" sparklines, 50/50 side by side.
/// The line is the per-bucket delta series ([`App::saved_series`] /
/// [`App::bytes_series`], falling back to a fresh diff of the raw buckets when
/// the run loop hasn't filled them yet); the title carries the current
/// per-minute rate as the web computes it.
pub(crate) fn charts_row(f: &mut Frame, area: Rect, app: &App) {
    let cols =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(area);
    let win_min = window_minutes(&app.snapshot);

    let saved = series_or_fallback(&app.saved_series, &app.snapshot, "saved_buckets");
    let saved_rate = per_min_rate(&app.snapshot, "saved_buckets", win_min);
    let saved_title = rate_title(
        "tokens saved/min ",
        format!("{} tok/min", model::human_count(saved_rate)),
        app.palette,
    );
    spark_chart(f, cols[0], &saved, saved_title, app.palette);

    let bytes = series_or_fallback(&app.bytes_series, &app.snapshot, "bytes_buckets");
    let bytes_rate = per_min_rate(&app.snapshot, "bytes_buckets", win_min);
    let bytes_title = rate_title(
        "bytes returned/min ",
        format!("{}/min", model::human_bytes(bytes_rate)),
        app.palette,
    );
    spark_chart(f, cols[1], &bytes, bytes_title, app.palette);
}

/// A chart title: a dim label followed by the accent, bold rate figure.
fn rate_title(label: &'static str, rate: String, pal: Palette) -> Line<'static> {
    Line::from(vec![
        Span::styled(label, Style::default().fg(pal.dim)),
        Span::styled(
            rate,
            Style::default().fg(pal.accent).add_modifier(Modifier::BOLD),
        ),
    ])
}

/// Render one braille line chart with a 0-baseline y-axis (a spike from zero
/// reads as a spike, matching the web's rate sparklines). Empty / single-point
/// / all-zero series draw a flat baseline instead of panicking.
fn spark_chart(f: &mut Frame, area: Rect, series: &[u64], title: Line<'static>, pal: Palette) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(pal.line))
        .title_top(title);
    let points = line_points(series);
    let x_max = (points.len() - 1).max(1) as f64;
    let y_max = series.iter().copied().max().unwrap_or(0).max(1) as f64;
    let datasets = vec![Dataset::default()
        .marker(Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(pal.accent))
        .data(&points)];
    let chart = Chart::new(datasets)
        .block(block)
        .x_axis(Axis::default().bounds([0.0, x_max]))
        .y_axis(Axis::default().bounds([0.0, y_max]));
    f.render_widget(chart, area);
}

/// `(x, y)` points for a chart line: index → value, or a two-point flat
/// baseline when there is nothing to plot (fewer than two samples).
fn line_points(series: &[u64]) -> Vec<(f64, f64)> {
    if series.len() < 2 {
        return vec![(0.0, 0.0), (1.0, 0.0)];
    }
    series
        .iter()
        .enumerate()
        .map(|(i, &v)| (i as f64, v as f64))
        .collect()
}

/// Window length in minutes (`window_end − window_start`), floored at one
/// second, matching the web `winMin`.
fn window_minutes(snap: &Value) -> f64 {
    let start = model::geti(snap, "window_start");
    let end = model::geti(snap, "window_end");
    ((end - start) as f64 / 60.0).max(1.0 / 60.0)
}

/// Per-minute rate for a cumulative bucket key: the last (total) bucket value
/// divided by the window length in minutes, as the web headline computes it.
fn per_min_rate(snap: &Value, key: &str, win_min: f64) -> u64 {
    let last = model::buckets(snap, key).last().copied().unwrap_or(0);
    (last as f64 / win_min).round().max(0.0) as u64
}

/// The cached delta series if the run loop filled it, else a fresh diff of the
/// raw cumulative buckets, so a first-frame render still draws a curve.
fn series_or_fallback(series: &[u64], snap: &Value, key: &str) -> Vec<u64> {
    if series.is_empty() {
        model::series(snap, key, 60)
    } else {
        series.to_vec()
    }
}

// ---------------------------------------------------------------------------
// fullcharts — cumulative line + three top-12 horizontal-bar panels (full view)
// ---------------------------------------------------------------------------

/// The four `#fullcharts` panels in the web's 2×2 grid order: cumulative
/// saved-tokens line (top-left), saved-by-tool bars (top-right), ops-by-tool
/// bars (bottom-left), compression-per-tool stacked bars (bottom-right).
pub(crate) fn fullcharts(f: &mut Frame, area: Rect, app: &App) {
    let rows =
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).split(area);
    let top =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(rows[0]);
    let bottom =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(rows[1]);

    cumulative_chart(f, top[0], app);
    saved_by_tool(f, top[1], app);
    ops_by_tool(f, bottom[0], app);
    comp_by_tool(f, bottom[1], app);
}

/// Cumulative saved-tokens line: the raw (un-diffed) `saved_buckets`, y-axis
/// auto-scaled to the series min..max so the shape shows.
fn cumulative_chart(f: &mut Frame, area: Rect, app: &App) {
    let pal = app.palette;
    let cumulative = model::buckets(&app.snapshot, "saved_buckets");
    let points = line_points(
        &cumulative
            .iter()
            .map(|&v| v.max(0) as u64)
            .collect::<Vec<u64>>(),
    );
    let x_max = (points.len() - 1).max(1) as f64;
    let (y_lo, y_hi) = autoscale(&cumulative);
    let datasets = vec![Dataset::default()
        .marker(Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(pal.accent))
        .data(&points)];
    let chart = Chart::new(datasets)
        .block(titled_block("tokens saved (cumulative)", pal))
        .x_axis(Axis::default().bounds([0.0, x_max]))
        .y_axis(Axis::default().bounds([y_lo, y_hi]));
    f.render_widget(chart, area);
}

/// Auto-scale bounds for the cumulative chart: `[min, max]`, widened to a unit
/// band when the series is flat or empty so a constant / absent series renders
/// a centered line instead of dividing by a zero-height range.
fn autoscale(cumulative: &[i64]) -> (f64, f64) {
    let min = cumulative.iter().copied().min().unwrap_or(0);
    let max = cumulative.iter().copied().max().unwrap_or(0);
    if min == max {
        (min as f64 - 1.0, max as f64 + 1.0)
    } else {
        (min as f64, max as f64)
    }
}

/// saved-by-tool: top-12 tools by saved tokens, bar ∝ saved / max.
fn saved_by_tool(f: &mut Frame, area: Rect, app: &App) {
    let pal = app.palette;
    let tools = top_tools(&app.snapshot, "saved", 12);
    let max = tools.iter().map(|t| t.1).max().unwrap_or(0).max(1);
    let inner_w = area.width.saturating_sub(2) as usize;
    let lines = tools
        .iter()
        .map(|(name, saved)| {
            bar_line(
                name,
                *saved as f64 / max as f64,
                &model::human_count(*saved),
                inner_w,
                pal,
            )
        })
        .collect();
    render_bar_panel(f, area, "saved tokens by tool", lines, "no savings yet", pal);
}

/// ops-by-tool: top-12 tools by call count, bar ∝ ops / max.
fn ops_by_tool(f: &mut Frame, area: Rect, app: &App) {
    let pal = app.palette;
    let tools = top_tools(&app.snapshot, "ops", 12);
    let max = tools.iter().map(|t| t.1).max().unwrap_or(0).max(1);
    let inner_w = area.width.saturating_sub(2) as usize;
    let lines = tools
        .iter()
        .map(|(name, ops)| {
            bar_line(
                name,
                *ops as f64 / max as f64,
                &model::human_count(*ops),
                inner_w,
                pal,
            )
        })
        .collect();
    render_bar_panel(f, area, "tool usage (ops)", lines, "no calls yet", pal);
}

/// compression-per-tool: top-12 tools by raw bytes; each track stacks the
/// returned fraction (accent) then the saved fraction (dim), value = save%.
fn comp_by_tool(f: &mut Frame, area: Rect, app: &App) {
    let pal = app.palette;
    let tools = top_tools_raw(&app.snapshot, 12);
    let max_raw = tools.iter().map(|t| t.1).max().unwrap_or(0).max(1);
    let inner_w = area.width.saturating_sub(2) as usize;
    let lines = tools
        .iter()
        .map(|(name, raw, returned)| comp_line(name, *raw, *returned, max_raw, inner_w, pal))
        .collect();
    render_bar_panel(
        f,
        area,
        "compression per tool",
        lines,
        "no offloading tool calls yet",
        pal,
    );
}

/// Top-`n` tools by a positive integer key (`saved` / `ops`), descending.
fn top_tools(snap: &Value, key: &str, n: usize) -> Vec<(String, u64)> {
    let arr = model::by_tool(snap).as_array().map(Vec::as_slice).unwrap_or(&[]);
    let mut tools: Vec<(String, u64)> = arr
        .iter()
        .filter_map(|t| {
            let val = t.get(key).and_then(Value::as_i64).unwrap_or(0);
            (val > 0).then(|| (tool_name(t), val as u64))
        })
        .collect();
    tools.sort_by_key(|t| std::cmp::Reverse(t.1));
    tools.truncate(n);
    tools
}

/// Top-`n` tools by raw bytes, descending, carrying `(name, raw, returned)` for
/// the stacked compression bars.
fn top_tools_raw(snap: &Value, n: usize) -> Vec<(String, u64, u64)> {
    let arr = model::by_tool(snap).as_array().map(Vec::as_slice).unwrap_or(&[]);
    let mut tools: Vec<(String, u64, u64)> = arr
        .iter()
        .filter_map(|t| {
            let raw = t.get("raw").and_then(Value::as_i64).unwrap_or(0);
            (raw > 0).then(|| {
                let returned = t
                    .get("returned")
                    .and_then(Value::as_i64)
                    .unwrap_or(0)
                    .max(0) as u64;
                (tool_name(t), raw as u64, returned)
            })
        })
        .collect();
    tools.sort_by_key(|t| std::cmp::Reverse(t.1));
    tools.truncate(n);
    tools
}

/// A tool row's display name (`tool` field), or `?`.
fn tool_name(t: &Value) -> String {
    t.get("tool")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_string()
}

/// Column widths for one bar row given the panel inner width: `(label, track,
/// value)`.
fn bar_widths(inner_w: usize) -> (usize, usize, usize) {
    let value_w = 7;
    let label_w = (inner_w / 3).clamp(6, 16);
    let track_w = inner_w.saturating_sub(label_w + value_w + 2).max(1);
    (label_w, track_w, value_w)
}

/// One `label ███░ value` bar row: `frac` of the track filled in accent, the
/// remainder blank.
fn bar_line(name: &str, frac: f64, value: &str, inner_w: usize, pal: Palette) -> Line<'static> {
    let (label_w, track_w, value_w) = bar_widths(inner_w);
    let filled = (frac.clamp(0.0, 1.0) * track_w as f64).round() as usize;
    let filled = filled.min(track_w);
    Line::from(vec![
        Span::styled(fit_label(name, label_w), Style::default().fg(pal.dim)),
        Span::raw(" "),
        Span::styled("█".repeat(filled), Style::default().fg(pal.accent)),
        Span::raw(" ".repeat(track_w - filled)),
        Span::raw(" "),
        Span::styled(format!("{value:>value_w$}"), Style::default().fg(pal.ink)),
    ])
}

/// One stacked compression bar: returned fraction (accent) then saved fraction
/// (dim = line color) of the track, value = `save%` of raw.
fn comp_line(
    name: &str,
    raw: u64,
    returned: u64,
    max_raw: u64,
    inner_w: usize,
    pal: Palette,
) -> Line<'static> {
    let (label_w, track_w, value_w) = bar_widths(inner_w);
    let track = track_w as f64;
    let returned_cells = (returned as f64 / max_raw as f64 * track).round() as usize;
    let returned_cells = returned_cells.min(track_w);
    let saved = raw.saturating_sub(returned);
    let saved_cells = (saved as f64 / max_raw as f64 * track).round() as usize;
    let saved_cells = saved_cells.min(track_w - returned_cells);
    let blank = track_w - returned_cells - saved_cells;
    let pct = if raw > 0 {
        (saved as f64 / raw as f64 * 100.0).round() as i64
    } else {
        0
    };
    Line::from(vec![
        Span::styled(fit_label(name, label_w), Style::default().fg(pal.dim)),
        Span::raw(" "),
        Span::styled("█".repeat(returned_cells), Style::default().fg(pal.accent)),
        Span::styled("█".repeat(saved_cells), Style::default().fg(pal.line)),
        Span::raw(" ".repeat(blank)),
        Span::raw(" "),
        Span::styled(
            format!("{:>value_w$}", format!("{pct}%")),
            Style::default().fg(pal.ink),
        ),
    ])
}

/// Truncate (with `…`) or right-pad `s` to exactly `width` display columns.
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

/// A bordered panel with an accent title and dim borders.
fn titled_block(title: &str, pal: Palette) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(pal.line))
        .title_top(Span::styled(
            title.to_string(),
            Style::default().fg(pal.accent),
        ))
}

/// Wrap bar rows in a titled panel; show `empty_msg` when there are no rows.
/// Rows past the panel height are clipped by `Paragraph` (a terminal can't
/// always show all 12) — a taller area shows more with no code change.
fn render_bar_panel(
    f: &mut Frame,
    area: Rect,
    title: &str,
    mut lines: Vec<Line<'static>>,
    empty_msg: &str,
    pal: Palette,
) {
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            empty_msg.to_string(),
            Style::default().fg(pal.dim),
        )));
    }
    let paragraph = Paragraph::new(lines).block(titled_block(title, pal));
    f.render_widget(paragraph, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::{model, theme::Palette};
    use super::super::{App, RateMode, ThemeKind, View, Window};
    use ratatui::{backend::TestBackend, Terminal};
    use serde_json::json;

    fn app_with(snapshot: serde_json::Value, view: View) -> App {
        App {
            snapshot,
            theme: ThemeKind::Dark,
            palette: Palette::from(ThemeKind::Dark),
            view,
            rate_mode: RateMode::Actual,
            rate: 5.0,
            rt_seconds: 30.0,
            window: Window::All,
            scope_global: false,
            scope_label: "session".into(),
            projects: vec![],
            scope_idx: 0,
            tool_sel: 0,
            saved_series: vec![1, 2, 3, 4, 5],
            bytes_series: vec![5, 4, 3, 2, 1],
            event_series: vec![],
            dir: std::path::PathBuf::from("."),
            session: None,
            rtk_base: None,
            tz_offset: 0,
            interval: 1,
        }
    }

    fn render<F: Fn(&mut Frame, Rect, &App)>(app: &App, g: F) -> String {
        let mut t = Terminal::new(TestBackend::new(120, 40)).unwrap();
        t.draw(|f| {
            let a = f.area();
            g(f, a, app);
        })
        .unwrap();
        t.backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    fn seeded() -> serde_json::Value {
        json!({
            "by_tool": [
                {"tool": "lens_search", "ops": 10, "raw": 5000, "returned": 800, "saved": 1050},
                {"tool": "lens_map",    "ops": 4,  "raw": 2000, "returned": 400, "saved": 400}
            ],
            "saved_buckets": [0, 100, 250, 600, 1000],
            "bytes_buckets": [0, 50, 90, 140, 200],
            "window_start": 1000,
            "window_end": 1600
        })
    }

    // (a) A seeded snapshot renders through both entry points without panicking,
    //     and so do the degenerate empty / all-zero series (flat baselines).
    #[test]
    fn charts_row_and_fullcharts_render_without_panic() {
        let app = app_with(seeded(), View::Full);
        let _ = render(&app, charts_row);
        let _ = render(&app, fullcharts);

        let empty = app_with(json!({}), View::Full);
        let zeroed = App {
            saved_series: vec![],
            bytes_series: vec![],
            ..app_with(json!({}), View::Full)
        };
        let _ = render(&empty, charts_row);
        let _ = render(&empty, fullcharts);
        let _ = render(&zeroed, charts_row);
        let _ = render(&zeroed, fullcharts);
    }

    // (b) charts_row titles the saved sparkline with its per-min rate, and
    //     fullcharts labels a seeded tool's bar + titles the cumulative chart.
    #[test]
    fn charts_row_shows_saved_and_fullcharts_shows_tool_bars() {
        let app = app_with(seeded(), View::Full);

        let row = render(&app, charts_row);
        assert!(row.contains("saved"), "charts row titles the saved sparkline");
        assert!(row.contains("tok/min"), "charts row shows the per-min rate");

        let full = render(&app, fullcharts);
        assert!(
            full.contains("lens_search"),
            "fullcharts labels a seeded tool bar"
        );
        assert!(
            full.contains("cumulative"),
            "fullcharts titles the cumulative chart"
        );
    }

    // (c) Pure bucket-math: per-bucket deltas of a cumulative series, and the
    //     cumulative chart's auto-scale (real range passes through; flat/empty
    //     widens to a unit band so ratatui never divides by a zero-height range).
    #[test]
    fn diffs_and_autoscale_math() {
        assert_eq!(model::diffs(&[0, 3, 3, 10]), vec![3, 0, 7]);
        assert_eq!(autoscale(&[0, 3, 3, 10]), (0.0, 10.0));
        assert_eq!(autoscale(&[5, 5, 5]), (4.0, 6.0));
        assert_eq!(autoscale(&[]), (-1.0, 1.0));
    }
}
