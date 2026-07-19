//! Session summary and the footer (the web row2, activity, and footer,
//! condensed).

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::{layout::Rect, Frame};

use super::model;
use super::App;

/// `mechanism 1op · 2op · ...`, plain (no per-item styling) — used both to
/// render the line and, via [`super::tables::wrap_line_count`], to predict
/// how many rows it wraps to at a given width so the frame can be sized to
/// fit it instead of cutting it off mid-word.
fn mechanism_summary(app: &App) -> String {
    let mech_items = model::by_mechanism(&app.snapshot)
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    if mech_items.is_empty() {
        return "\u{2014}".to_string();
    }
    mech_items
        .iter()
        .map(|m| {
            let name = m["mechanism"].as_str().unwrap_or("?");
            let ops = m["ops"].as_i64().unwrap_or(0);
            format!("{name} {ops}")
        })
        .collect::<Vec<_>>()
        .join(" \u{b7} ")
}

/// Fixed-width label column so `mechanism`/`rtk`/`activity` read as a
/// key:value list instead of three differently-indented sentences.
const LABEL_WIDTH: usize = 10;

fn labeled(label: &str, style: Style) -> Span<'static> {
    Span::styled(format!("{label:<LABEL_WIDTH$}"), style)
}

/// Mechanism mix, RTK shell savings, and session activity as three labeled,
/// wrapped lines — content only, no border; `chrome::frame_session_value`
/// owns the surrounding frame. The mechanism line wraps (instead of the old
/// single unwrapped line, which just got cut off mid-word past the panel's
/// width) since its item count varies with how many mechanisms have fired.
pub(crate) fn session_content(f: &mut Frame, area: Rect, app: &App) {
    let p = &app.palette;
    let ink = Style::new().fg(p.ink);
    let label = Style::new().fg(p.dim).add_modifier(Modifier::BOLD);

    let mech_line = Line::from(vec![labeled("mechanism", label), Span::styled(mechanism_summary(app), ink)]);

    let r = model::rtk(&app.snapshot);
    let installed = r["installed"].as_bool() == Some(true);
    let rtk_text = if installed {
        let cmds = r["total_commands"].as_i64().unwrap_or(0);
        let saved = model::human_count(r["total_saved"].as_i64().unwrap_or(0).max(0) as u64);
        format!("{cmds} cmds \u{b7} {saved} tok saved")
    } else {
        "not installed".to_string()
    };
    let rtk_style = if installed { ink } else { Style::new().fg(p.dim) };
    let rtk_line = Line::from(vec![labeled("rtk", label), Span::styled(rtk_text, rtk_style)]);

    let a = model::activity(&app.snapshot);
    let total_events = a["total_events"].as_i64().unwrap_or(0);
    let sessions = a["sessions"].as_i64().unwrap_or(0);
    let activity_line = Line::from(vec![
        labeled("activity", label),
        Span::styled(format!("{total_events} events \u{b7} {sessions} sessions"), ink),
    ]);

    f.render_widget(
        Paragraph::new(vec![mech_line, rtk_line, activity_line]).wrap(Wrap { trim: true }),
        area,
    );
}

/// The row count [`session_content`] draws at `col_width` columns, so
/// [`super::chrome::session_value_height`] can size the shared frame to fit
/// the mechanism line's real wrap instead of guessing a fixed height.
pub(crate) fn session_line_count(app: &App, col_width: u16) -> usize {
    let text = format!("{:<LABEL_WIDTH$}{}", "mechanism", mechanism_summary(app));
    let mech_lines = super::tables::wrap_line_count(&text, col_width.max(1) as usize).max(1);
    mech_lines + 2 // rtk + activity, each one line
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

    /// Seeds every key `session_box` and `footer` read: `by_mechanism` (array of
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
    fn session_box_renders_mechanism_rtk_and_activity() {
        let app = app_with(snap());
        let out = render(&app, session_content);
        assert!(out.contains("darkroom"), "mechanism chip name missing: {out}");
        assert!(out.contains("77"), "rtk cmds count missing: {out}");
        assert!(out.contains("314"), "activity events count missing: {out}");
        assert!(out.contains("8 sessions"), "activity sessions count missing: {out}");
    }

    #[test]
    fn session_box_renders_rtk_not_installed() {
        let mut s = snap();
        s["rtk"] = json!({"installed": false});
        let app = app_with(s);
        let out = render(&app, session_content);
        assert!(
            out.contains("not installed"),
            "not-installed message missing: {out}"
        );
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
