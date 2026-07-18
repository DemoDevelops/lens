//! By-mechanism chips, RTK shell savings, session activity, and the footer
//! (the web row2 + activity + footer). Stubs in T1; T7 renders the real
//! widgets and carries its own `TestBackend` tests.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Sparkline, Wrap};
use ratatui::Frame;

use super::model;
use super::App;

/// "by mechanism" chips: `name <ops>op·<saved>tok` per mechanism.
pub(crate) fn mechanism(f: &mut Frame, area: Rect, app: &App) {
    let p = &app.palette;
    let items = model::by_mechanism(&app.snapshot)
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);

    let mut spans = Vec::new();
    if items.is_empty() {
        spans.push(Span::styled("—", Style::new().fg(p.dim)));
    } else {
        for (i, m) in items.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" · ", Style::new().fg(p.dim)));
            }
            let name = m["mechanism"].as_str().unwrap_or("?");
            let ops = m["ops"].as_i64().unwrap_or(0);
            let saved = model::human_count(m["saved"].as_i64().unwrap_or(0).max(0) as u64);
            spans.push(Span::styled(name.to_string(), Style::new().fg(p.accent)));
            spans.push(Span::styled(
                format!(" {ops}op·{saved}tok"),
                Style::new().fg(p.dim),
            ));
        }
    }

    let block = Block::bordered()
        .border_style(Style::new().fg(p.line))
        .title(Span::styled("by mechanism", Style::new().fg(p.ink)));
    let para = Paragraph::new(Line::from(spans))
        .block(block)
        .wrap(Wrap { trim: true });
    f.render_widget(para, area);
}

/// RTK's own measured shell savings (already rebased to the delta since this
/// view opened by the run loop's `rebase_rtk`).
pub(crate) fn rtk(f: &mut Frame, area: Rect, app: &App) {
    let p = &app.palette;
    let r = model::rtk(&app.snapshot);

    let lines: Vec<Line> = if r["installed"].as_bool() == Some(true) {
        let cmds = r["total_commands"].as_i64().unwrap_or(0);
        let saved = model::human_count(r["total_saved"].as_i64().unwrap_or(0).max(0) as u64);
        let pct = r["avg_savings_pct"].as_f64().unwrap_or(0.0);
        vec![
            Line::styled(
                format!("cmds {cmds} · saved {saved}tok · avg {pct:.1}%"),
                Style::new().fg(p.ink),
            ),
            Line::styled("since opened", Style::new().fg(p.dim)),
        ]
    } else {
        vec![Line::styled(
            "not installed — run lens rtk install",
            Style::new().fg(p.dim),
        )]
    };

    let block = Block::bordered()
        .border_style(Style::new().fg(p.line))
        .title(Span::styled("RTK shell savings", Style::new().fg(p.ink)));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// Session activity: events/sessions/last-ts line + events/min sparkline +
/// per-category chips.
pub(crate) fn activity(f: &mut Frame, area: Rect, app: &App) {
    let p = &app.palette;
    let a = model::activity(&app.snapshot);

    let block = Block::bordered()
        .border_style(Style::new().fg(p.line))
        .title(Span::styled("session activity", Style::new().fg(p.ink)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = Layout::vertical([
        Constraint::Length(1), // events · sessions · last
        Constraint::Length(1), // events/min sparkline
        Constraint::Min(1),    // by-category chips
    ])
    .split(inner);

    let total_events = a["total_events"].as_i64().unwrap_or(0);
    let sessions = a["sessions"].as_i64().unwrap_or(0);
    let last = a["last_ts"]
        .as_i64()
        .map(crate::obs::iso8601_secs)
        .unwrap_or_else(|| "—".to_string());
    let header = Paragraph::new(Line::styled(
        format!("events {total_events} · sessions {sessions} · last {last}"),
        Style::new().fg(p.ink),
    ));
    f.render_widget(header, rows[0]);

    let series = if app.event_series.is_empty() {
        model::series(&app.snapshot, "event_buckets", 60)
    } else {
        app.event_series.clone()
    };
    let spark = Sparkline::default()
        .data(&series)
        .style(Style::new().fg(p.warn));
    f.render_widget(spark, rows[1]);

    let cats = a["by_category"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let cat_line = if cats.is_empty() {
        Line::styled("no activity captured yet", Style::new().fg(p.dim))
    } else {
        let mut spans = Vec::new();
        for (i, c) in cats.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" · ", Style::new().fg(p.dim)));
            }
            let name = c["category"].as_str().unwrap_or("?");
            let count = c["count"].as_i64().unwrap_or(0);
            spans.push(Span::styled(
                format!("{name} {count}"),
                Style::new().fg(p.ink),
            ));
        }
        Line::from(spans)
    };
    f.render_widget(Paragraph::new(cat_line).wrap(Wrap { trim: true }), rows[2]);
}

/// `store <size> · index <n> · graph <n>n/<n>e · updated <ts>`.
pub(crate) fn footer(f: &mut Frame, area: Rect, app: &App) {
    let p = &app.palette;
    let snap = &app.snapshot;
    let store = model::human_bytes(model::store_size(snap));
    let index_chunks = model::geti(snap, "index_chunks");
    let graph_nodes = model::geti(snap, "graph_nodes");
    let graph_edges = model::geti(snap, "graph_edges");
    let ts = snap["ts"].as_str().unwrap_or("—");
    let line = Line::styled(
        format!(
            "store {store} · index {index_chunks} · graph {graph_nodes}n/{graph_edges}e · updated {ts}"
        ),
        Style::new().fg(p.dim),
    );
    f.render_widget(Paragraph::new(line), area);
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
            event_series: vec![1, 2, 3, 2, 1],
            dir: std::path::PathBuf::from("."),
            session: None,
            rtk_base: None,
            tz_offset: 0,
            interval: 1,
        }
    }

    fn render<F: Fn(&mut ratatui::Frame, ratatui::layout::Rect, &App)>(app: &App, g: F) -> String {
        let mut t = Terminal::new(TestBackend::new(120, 12)).unwrap();
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

    /// Seeds every key the four panels read: `by_mechanism` (array of
    /// `{mechanism,ops,saved}`), `rtk` (`installed`/`total_commands`/
    /// `total_saved`/`avg_savings_pct`), `activity` (`total_events`/`sessions`/
    /// `last_ts`/`by_category` array of `{category,count}`), and the top-level
    /// footer keys `store_size`/`index_chunks`/`graph_nodes`/`graph_edges`/`ts`.
    fn snap() -> serde_json::Value {
        json!({
            "by_mechanism": [
                {"mechanism": "darkroom", "ops": 42, "saved": 12345},
                {"mechanism": "recall", "ops": 3, "saved": 100},
            ],
            "rtk": {
                "installed": true,
                "total_commands": 77,
                "total_saved": 5000,
                "total_input": 9000,
                "avg_savings_pct": 55.5,
            },
            "activity": {
                "total_events": 314,
                "sessions": 8,
                "last_ts": 1_700_000_000,
                "by_category": [
                    {"category": "Read", "count": 20},
                    {"category": "Edit", "count": 5},
                ],
            },
            "store_size": 2_097_152u64,
            "index_chunks": 123,
            "graph_nodes": 456,
            "graph_edges": 789,
            "ts": "2026-07-18T00:00:00.000Z",
        })
    }

    #[test]
    fn mechanism_renders_chip_name() {
        let app = app_with(snap());
        let out = render(&app, mechanism);
        assert!(out.contains("darkroom"), "chip name missing: {out}");
    }

    #[test]
    fn rtk_renders_installed_line() {
        let app = app_with(snap());
        let out = render(&app, rtk);
        assert!(out.contains("cmds"), "rtk cmds label missing: {out}");
        assert!(out.contains("77"), "total_commands missing: {out}");
    }

    #[test]
    fn rtk_renders_not_installed() {
        let mut s = snap();
        s["rtk"] = json!({"installed": false});
        let app = app_with(s);
        let out = render(&app, rtk);
        assert!(
            out.contains("not installed"),
            "not-installed message missing: {out}"
        );
    }

    #[test]
    fn activity_renders_event_and_session_counts() {
        let app = app_with(snap());
        let out = render(&app, activity);
        assert!(out.contains("314"), "events count missing: {out}");
        assert!(out.contains("sessions 8"), "sessions count missing: {out}");
    }

    #[test]
    fn footer_renders_store_and_graph_figures() {
        let app = app_with(snap());
        let out = render(&app, footer);
        assert!(out.contains("2.0 MB"), "store figure missing: {out}");
        assert!(out.contains("456"), "graph nodes missing: {out}");
        assert!(out.contains("789"), "graph edges missing: {out}");
    }
}
