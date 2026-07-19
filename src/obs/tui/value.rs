//! Applied-value panel: a compact totals line plus the Actual-Usage per-model
//! table (the web `renderApplied`, condensed). Content only — no border;
//! `chrome::frame_session_value` owns the surrounding frame.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table};
use ratatui::Frame;
use serde_json::Value;

use super::tables::num_cell;
use super::{model, App, RateMode};

/// Real per-model transcript mix, minus synthetic/unnamed rows. The totals'
/// `spent`/`turns` and the per-model table both read from this.
fn valid_models(snap: &Value) -> Vec<&Value> {
    model::actual_usage(snap)
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|m| m["model"].as_str().is_some_and(|s| !s.is_empty() && s != "<synthetic>"))
                .collect()
        })
        .unwrap_or_default()
}

/// Estimated value applied to this scope's ops (never folded into the measured
/// `$` headline): one totals line, then the per-model table when
/// `app.rate_mode` is `Actual`, else a `$/M` rate caption. The table's columns
/// fill the panel's real width instead of a fixed guess, so a wide `session |
/// value` split doesn't leave the model table crowded into a corner.
pub(crate) fn value_content(f: &mut Frame, area: Rect, app: &App) {
    let p = app.palette;
    let dimst = Style::default().fg(p.dim);
    let val = Style::default().fg(p.accent);
    let big = Style::default().fg(p.accent).add_modifier(Modifier::BOLD);
    let ink = Style::default().fg(p.ink);

    let av = model::applied_value(&app.snapshot);
    let total = av["est_total_tokens"].as_i64().unwrap_or(0).max(0) as u64;
    let rts = av["round_trips_avoided"].as_f64().unwrap_or(0.0);
    let est_value = model::money(total as f64 * app.rate / 1e6);

    let models = valid_models(&app.snapshot);
    let tot_turns: i64 = models.iter().map(|m| m["turns"].as_i64().unwrap_or(0)).sum();

    let totals = Line::from(vec![
        Span::styled(est_value, big),
        Span::styled(format!(" · {} tok", model::human_count(total)), val),
        Span::styled(format!(" · {} turns", tot_turns), dimst),
    ]);

    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(area);
    f.render_widget(Paragraph::new(totals), chunks[0]);

    if app.rate_mode == RateMode::Actual {
        if models.is_empty() {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled("no usage in window", dimst))),
                chunks[1],
            );
        } else {
            let header = Row::new(vec![
                Cell::from(Span::styled("model", dimst)),
                num_cell("turns".into(), dimst),
                num_cell("saved".into(), dimst),
                num_cell("spent".into(), dimst),
            ]);
            let body: Vec<Row> = models
                .iter()
                .map(|m| {
                    let turns = m["turns"].as_i64().unwrap_or(0);
                    let saved_tok = m["saved_tokens"].as_f64().unwrap_or(0.0).max(0.0) as u64;
                    let spent = m["consumed_usd"].as_f64().unwrap_or(0.0);
                    let label = model::model_label(m["model"].as_str().unwrap_or("?"));
                    let row = Row::new(vec![
                        Cell::from(Span::styled(label, ink)),
                        num_cell(turns.to_string(), ink),
                        num_cell(model::human_count(saved_tok), val),
                        num_cell(model::money(spent), val),
                    ]);
                    if turns == 0 {
                        row.style(dimst)
                    } else {
                        row
                    }
                })
                .collect();
            // `model` stays a tight fixed column (a wide Fill share here just
            // strands blank space after a short left-aligned label); the
            // numeric columns Fill the rest evenly so a wide `value` half
            // spreads its right-aligned figures out instead of leaving them
            // crowded against a narrow fixed table with empty space past it.
            let widths = [
                Constraint::Length(12),
                Constraint::Fill(1),
                Constraint::Fill(1),
                Constraint::Fill(1),
            ];
            f.render_widget(
                Table::new(body, widths).header(header).column_spacing(2),
                chunks[1],
            );
        }
    } else {
        let caption = format!(
            "rate ${}/M · time ~{}",
            app.rate,
            model::human_time(rts * app.rt_seconds)
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(caption, dimst))),
            chunks[1],
        );
    }
}

/// The row count [`value_content`] draws at, so
/// [`super::chrome::session_value_height`] can size the shared frame to fit
/// the per-model table instead of guessing a fixed height.
pub(crate) fn value_line_count(app: &App) -> usize {
    if app.rate_mode != RateMode::Actual {
        return 2; // totals + rate caption
    }
    let n = valid_models(&app.snapshot).len();
    if n == 0 {
        2 // totals + "no usage in window"
    } else {
        2 + n // totals + header + one row per model
    }
}

#[cfg(test)]
mod tests {
    use super::super::{model, theme::Palette};
    use super::super::{App, RateMode, ThemeKind, View, Window};
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};
    use serde_json::json;

    fn app_with(snapshot: serde_json::Value, rate_mode: RateMode) -> App {
        App {
            snapshot,
            theme: ThemeKind::Dark,
            palette: Palette::from(ThemeKind::Dark),
            view: View::Full,
            rate_mode,
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
        let mut t = Terminal::new(TestBackend::new(120, 40)).unwrap();
        t.draw(|f| {
            let a = f.area();
            value_content(f, a, app);
        })
        .unwrap();
        t.backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    fn snapshot() -> serde_json::Value {
        json!({
            "applied_value": {
                "measured_tokens": 123456,
                "est_counterfactual_tokens": 45000,
                "est_total_tokens": 168000,
                "round_trips_avoided": 12.0,
                "rows": [
                    {"dimension": "darkroom", "ops": 5, "est_tokens": 12000, "round_trips": 2.5, "source": "measured"},
                    {"dimension": "graph", "ops": 0, "est_tokens": 0, "round_trips": 0.0, "source": "floor"}
                ],
                "note": "estimated from per-op benchmark rates",
                "model": "bench-suite"
            },
            "actual_usage": [
                {"model": "claude-opus-4-8", "turns": 100, "saved_tokens": 123456, "saved_usd": 0.4321, "consumed_usd": 1.11},
                {"model": "claude-sonnet-5", "turns": 50, "saved_tokens": 65432, "saved_usd": 0.2109, "consumed_usd": 0.55},
                {"model": "<synthetic>", "turns": 9, "saved_tokens": 1, "saved_usd": 9.0, "consumed_usd": 9.0},
                {"model": "", "turns": 3, "saved_tokens": 1, "saved_usd": 3.0, "consumed_usd": 3.0}
            ]
        })
    }

    #[test]
    fn actual_mode_renders_per_model_table() {
        let app = app_with(snapshot(), RateMode::Actual);
        let buf = render(&app);
        // Both real model labels are present (the per-model table rendered).
        assert!(
            buf.contains(&model::model_label("claude-opus-4-8")),
            "Opus label"
        );
        assert!(
            buf.contains(&model::model_label("claude-sonnet-5")),
            "Sonnet label"
        );
        // Per-row spent figures render; the synthetic/empty rows are filtered out.
        assert!(buf.contains("$1.11"), "Opus spent cell");
        assert!(buf.contains("$0.55"), "Sonnet spent cell");
        assert!(!buf.contains("$9.00"), "synthetic row filtered out");
    }

    #[test]
    fn model_mode_hides_per_model_table() {
        let app = app_with(snapshot(), RateMode::Opus);
        let buf = render(&app);
        // A rate caption renders instead of the per-model table.
        assert!(buf.contains("rate $5/M"), "rate caption present");
        // The per-model table is absent, so no model labels appear.
        assert!(
            !buf.contains(&model::model_label("claude-opus-4-8")),
            "no per-model table in model mode"
        );
        assert!(
            !buf.contains(&model::model_label("claude-sonnet-5")),
            "no per-model table in model mode"
        );
    }
}
