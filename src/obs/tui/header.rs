//! Header panel: title + live status, the window/scope/view/theme/rate control
//! strip (bare, above everything else, like a page title), and the `overview`
//! headline — the `$ saved`/time-saved/measured/classified figures the web
//! `INDEX_HTML` header shows across two lines, merged into one here. The
//! surrounding frame (border, stat strip placement) is [`super::chrome`]'s;
//! this file only renders the headline's content into the row it's given.

use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::model;
use super::{App, RateMode, ThemeKind, View};

/// Title · live dot + status · active control labels. Bare, no box — reads
/// as the page's own title bar, above the framed panels.
pub(crate) fn header_top(f: &mut Frame, area: Rect, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title + live dot
            Constraint::Length(1), // window/scope/view/theme/rate control strip
        ])
        .split(area);

    f.render_widget(title_line(app), rows[0]);
    f.render_widget(control_strip(app), rows[1]);
}

/// The overview headline, one line: `$ saved` · time saved · measured ·
/// classified. Previously two lines (a money headline, then a
/// measured/classified sub-line) with the applied-value time-saved figure
/// buried in the `value` panel and hidden outside rate-mode; merged per
/// request and surfaced here since it's the one number this dashboard never
/// otherwise shows front and center.
pub(crate) fn overview_line(f: &mut Frame, area: Rect, app: &App) {
    f.render_widget(Paragraph::new(money_line(app)), area);
}

/// `lens dashboard` in accent bold, plus a live/stale dot: accent when the
/// snapshot is fresh, bad when it looks stalled.
fn title_line(app: &App) -> Paragraph<'static> {
    let dot_color = if is_fresh(app) { app.palette.accent } else { app.palette.bad };
    let line = Line::from(vec![
        Span::styled(
            "lens dashboard",
            Style::default().fg(app.palette.accent).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled("\u{25cf}", Style::default().fg(dot_color)),
    ]);
    Paragraph::new(line)
}

/// A snapshot is fresh when its `ts` is within ~2x the poll interval of now;
/// a missing/unparseable `ts` renders fresh rather than flag a false stale.
fn is_fresh(app: &App) -> bool {
    let Some(ts) = app.snapshot.get("ts").and_then(|v| v.as_i64()) else {
        return true;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(ts);
    let budget = (app.interval.max(1) as i64) * 2;
    (now - ts).abs() <= budget
}

/// The active window/scope/view/theme/rate selections, each shown as
/// `[key] value`: the key dim (how to change it), the value accent bold
/// (what's active now).
fn control_strip(app: &App) -> Paragraph<'static> {
    let view_label = match app.view {
        View::Mini => "Mini",
        View::Full => "Full",
    };
    let theme_label = match app.theme {
        ThemeKind::Dark => "dark",
        ThemeKind::Seventies => "70s",
    };
    let rate_label = match app.rate_mode {
        RateMode::Actual => "Actual",
        RateMode::Fable => "Fable",
        RateMode::Opus => "Opus",
        RateMode::Sonnet => "Sonnet",
        RateMode::Haiku => "Haiku",
    };
    let controls = [
        ("w", app.window.label()),
        ("s", app.scope_label.clone()),
        ("v", view_label.to_string()),
        ("t", theme_label.to_string()),
        ("r", rate_label.to_string()),
    ];

    let dim = Style::default().fg(app.palette.dim);
    let accent = Style::default().fg(app.palette.accent).add_modifier(Modifier::BOLD);

    let mut spans = Vec::with_capacity(controls.len() * 3);
    for (i, (key, value)) in controls.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  \u{b7}  ", dim));
        }
        spans.push(Span::styled(format!("[{key}] "), dim));
        spans.push(Span::styled(value, accent));
    }
    Paragraph::new(Line::from(spans))
}

/// `$X saved · ~Ys time saved · A measured · ~B classified`. Mirrors the web
/// `renderCost`/`savedTop` math exactly: `savedTotal` = classified tokens
/// (`saved_mcp`) plus RTK's since-open delta (already rebased into the
/// snapshot by the run loop); dollars are the real per-model spend sum in
/// Actual mode, else `savedTotal * rate/1e6`. Never folds an applied-value
/// estimate into the `$` figure; only the time-saved figure comes from
/// `applied_value` (`round_trips_avoided * rt_seconds`, the same basis the
/// `value` panel's rate-mode caption uses).
fn money_line(app: &App) -> Line<'static> {
    let rtk_delta = if app.snapshot["rtk"]["installed"].as_bool() == Some(true) {
        app.snapshot["rtk"]["total_saved"].as_i64().unwrap_or(0)
    } else {
        0
    };
    let saved_total = model::saved_mcp(&app.snapshot) as i64 + rtk_delta;
    let dollars = match app.rate_mode {
        RateMode::Actual => model::actual_usage(&app.snapshot)
            .as_array()
            .map(|a| a.iter().filter_map(|m| m["saved_usd"].as_f64()).sum())
            .unwrap_or(0.0),
        _ => saved_total as f64 * app.rate / 1e6,
    };
    let rts = model::applied_value(&app.snapshot)["round_trips_avoided"]
        .as_f64()
        .unwrap_or(0.0);
    let time_saved = model::human_time(rts * app.rt_seconds);

    let accent = Style::default().fg(app.palette.accent).add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(app.palette.dim);
    Line::from(vec![
        Span::styled(format!("{} saved", model::money(dollars)), accent),
        Span::styled("  \u{b7}  ", dim),
        Span::styled(format!("{time_saved} time saved"), accent),
        Span::styled("  \u{b7}  ", dim),
        Span::styled(
            format!("{} measured", model::human_count(model::saved_measured_floor(&app.snapshot))),
            dim,
        ),
        Span::styled(" \u{b7} ", dim),
        Span::styled(format!("~{} classified", model::human_count(saved_total.max(0) as u64)), dim),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::theme::Palette;
    use super::super::{App, RateMode, ThemeKind, View, Window};
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

    fn render(app: &App) -> String {
        let mut t = Terminal::new(TestBackend::new(120, 12)).unwrap();
        t.draw(|f| {
            let [top, rest] =
                Layout::vertical([Constraint::Length(2), Constraint::Length(1)]).areas(f.area());
            header_top(f, top, app);
            overview_line(f, rest, app);
        })
        .unwrap();
        t.backend().buffer().content().iter().map(|c| c.symbol()).collect()
    }

    /// Actual-usage mode sums real per-model `saved_usd`; the sub-line reports
    /// the measured floor and the classified (mcp + RTK-since-open) total; the
    /// control strip shows the seeded view/theme/rate as their active labels.
    #[test]
    fn renders_money_headline_and_active_controls() {
        let snap = json!({
            "tokens_saved_mcp": 50_000,
            "tokens_saved_measured_floor": 20_000,
            "rtk": { "installed": true, "total_saved": 5_000 },
            "actual_usage": [
                { "saved_usd": 7.5 },
                { "saved_usd": 5.0 }
            ],
        });
        let app = app_with(snap);
        let out = render(&app);

        assert!(out.contains("lens dashboard"), "title: {out}");
        assert!(out.contains("saved"), "headline should read '... saved': {out}");
        assert!(out.contains("$12.50"), "actual_usage sum 7.5+5.0 -> $12.50: {out}");
        assert!(out.contains("20.0K"), "measured floor sub-line: {out}");
        assert!(out.contains("55.0K"), "classified total (mcp 50K + rtk delta 5K): {out}");
        assert!(out.contains("Full"), "active view label: {out}");
        assert!(out.contains("dark"), "active theme label: {out}");
        assert!(out.contains("Actual"), "active rate label: {out}");
        assert!(out.contains("session"), "active scope label: {out}");
    }

    /// Non-Actual rate modes price `savedTotal` (mcp + rtk-since-open) at the
    /// model's `$/M` rate instead of summing `actual_usage`.
    #[test]
    fn per_model_rate_prices_saved_total_not_actual_usage() {
        let snap = json!({
            "tokens_saved_mcp": 1_000_000,
            "tokens_saved_measured_floor": 0,
            "rtk": { "installed": false },
            "actual_usage": [{ "saved_usd": 999.0 }],
        });
        let mut app = app_with(snap);
        app.rate_mode = RateMode::Sonnet;
        app.rate = 3.0; // $3/M -> 1_000_000 tok * 3/1e6 = $3.00, not $999
        let out = render(&app);

        assert!(out.contains("$3.00"), "per-model rate ignores actual_usage: {out}");
        assert!(!out.contains("999"), "must not price from actual_usage in rate mode: {out}");
        assert!(out.contains("Sonnet"), "active rate label switches: {out}");
    }

    /// RTK not installed: the classified total is `tokens_saved_mcp` alone, no
    /// delta added. No `actual_usage` recorded yet -> `None` -> the `0.0`
    /// fallback, not a summed value.
    #[test]
    fn rtk_not_installed_excludes_delta() {
        let snap = json!({
            "tokens_saved_mcp": 42,
            "tokens_saved_measured_floor": 1,
            "rtk": { "installed": false, "total_saved": 999_999 },
        });
        let out = render(&app_with(snap));
        assert!(out.contains("$0.0000"), "no actual_usage yet -> $0.0000 fallback: {out}");
        assert!(out.contains('1'), "measured floor of 1: {out}");
    }
}
