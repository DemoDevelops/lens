//! Sparklines — the web dashboard's charts row, as ratatui widgets.
//!
//! [`charts_row`] draws the two per-minute sparklines ("tokens saved/min",
//! "bytes returned/min") as braille [`Chart`] lines, titled with the current
//! rate (last cumulative bucket ÷ window minutes, exactly as the web headline
//! computes it).

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Block, Borders, Chart, Dataset, GraphType};
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
        .border_style(Style::default().fg(pal.dim))
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

    // (a) A seeded snapshot renders without panicking, and so do the degenerate
    //     empty / all-zero series (flat baselines).
    #[test]
    fn charts_row_renders_without_panic() {
        let app = app_with(seeded(), View::Full);
        let _ = render(&app, charts_row);

        let empty = app_with(json!({}), View::Full);
        let zeroed = App {
            saved_series: vec![],
            bytes_series: vec![],
            ..app_with(json!({}), View::Full)
        };
        let _ = render(&empty, charts_row);
        let _ = render(&zeroed, charts_row);
    }

    // (b) charts_row titles the saved sparkline with its per-min rate.
    #[test]
    fn charts_row_shows_saved_rate() {
        let app = app_with(seeded(), View::Full);

        let row = render(&app, charts_row);
        assert!(row.contains("saved"), "charts row titles the saved sparkline");
        assert!(row.contains("tok/min"), "charts row shows the per-min rate");
    }

    // (c) Pure bucket-math: per-bucket deltas of a cumulative series.
    #[test]
    fn diffs_math() {
        assert_eq!(model::diffs(&[0, 3, 3, 10]), vec![3, 0, 7]);
    }
}
