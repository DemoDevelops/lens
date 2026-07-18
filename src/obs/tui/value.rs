//! Applied-value panel: totals strip, per-dimension rows, and the Actual-Usage
//! per-model table (the web `renderApplied`). Stub in T1; T6 renders the real
//! widgets and carries its own `TestBackend` tests.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table};
use ratatui::Frame;

use super::{model, App, RateMode};

/// Estimated value applied to this scope's ops (never folded into the measured
/// `$` headline); per-model table when `app.rate_mode` is `Actual`.
///
/// Layout: a totals strip (two `Line`s of labeled chips), then a middle region
/// — the per-dimension "lens tools" breakdown, beside a per-model table when the
/// rate picker is on `Actual` (its picker equivalent) — then the note caption.
/// Field reads mirror the web `renderApplied` and the old ANSI `applied_value_lines`.
pub(crate) fn applied_value(f: &mut Frame, area: Rect, app: &App) {
    let p = app.palette;
    let lbl = Style::default().fg(p.dim);
    let dimst = Style::default().fg(p.dim);
    let val = Style::default().fg(p.accent);
    let big = Style::default().fg(p.accent).add_modifier(Modifier::BOLD);
    let ink = Style::default().fg(p.ink);

    let av = model::applied_value(&app.snapshot);
    let gi = |k: &str| av[k].as_i64().unwrap_or(0).max(0) as u64;
    let measured = gi("measured_tokens");
    let counter = gi("est_counterfactual_tokens");
    let total = gi("est_total_tokens");
    let rts = av["round_trips_avoided"].as_f64().unwrap_or(0.0);
    let est_value = model::money(total as f64 * app.rate / 1e6);

    // Real per-model transcript mix, minus synthetic/unnamed rows. The totals'
    // `spent`/`turns` and the per-model table both read from it.
    let models = model::actual_usage(&app.snapshot)
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|m| {
                    m["model"]
                        .as_str()
                        .is_some_and(|s| !s.is_empty() && s != "<synthetic>")
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let tot_turns: i64 = models.iter().map(|m| m["turns"].as_i64().unwrap_or(0)).sum();
    let tot_spent: f64 = models
        .iter()
        .map(|m| m["consumed_usd"].as_f64().unwrap_or(0.0))
        .sum();

    // Totals strip: two lines of `label value [basis]` chips, styled from the palette.
    let chip = move |label: &'static str, value: Span<'static>, basis: Option<String>| {
        let mut v = vec![Span::styled(format!("{label} "), lbl), value];
        if let Some(b) = basis {
            v.push(Span::styled(format!(" {b}"), dimst));
        }
        v
    };
    let join = move |chips: Vec<Vec<Span<'static>>>| {
        let mut out: Vec<Span<'static>> = Vec::new();
        for (i, c) in chips.into_iter().enumerate() {
            if i > 0 {
                out.push(Span::styled("   ", dimst));
            }
            out.extend(c);
        }
        Line::from(out)
    };
    let line_a = join(vec![
        chip(
            "measured saved",
            Span::styled(format!("{} tok", model::human_count(measured)), val),
            None,
        ),
        chip(
            "est. counterfactual",
            Span::styled(format!("+{} tok", model::human_count(counter)), val),
            None,
        ),
        chip(
            "est. total avoided",
            Span::styled(format!("{} tok", model::human_count(total)), big),
            None,
        ),
        chip(
            "est. value",
            Span::styled(est_value, big),
            Some(format!("@ ${}/M", app.rate)),
        ),
    ]);
    let line_b = join(vec![
        chip(
            "round-trips avoided",
            Span::styled(format!("~{}", rts.round() as i64), val),
            None,
        ),
        chip(
            "time saved",
            Span::styled(model::human_time(rts * app.rt_seconds), big),
            Some(format!("@ {}s/round-trip", app.rt_seconds)),
        ),
        chip("spent", Span::styled(model::money(tot_spent), val), None),
        chip("turns", Span::styled(tot_turns.to_string(), val), None),
    ]);

    // Per-dimension "lens tools" breakdown (always shown).
    let mut dim_lines: Vec<Line<'static>> = vec![Line::from(Span::styled("lens tools", dimst))];
    if let Some(rs) = av["rows"].as_array() {
        for r in rs {
            let d = r["dimension"].as_str().unwrap_or("");
            let ops = r["ops"].as_i64().unwrap_or(0);
            let et = r["est_tokens"].as_i64().unwrap_or(0);
            let rt = r["round_trips"].as_f64().unwrap_or(0.0);
            let mut parts: Vec<String> = Vec::new();
            if et > 0 {
                parts.push(format!("~{} tok", model::human_count(et as u64)));
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
            let name_style = if ops == 0 { dimst } else { ink };
            dim_lines.push(Line::from(vec![
                Span::styled(format!("{d} "), name_style),
                Span::styled(format!("×{ops}  "), dimst),
                Span::styled(parts.join(", "), dimst),
            ]));
        }
    }

    let chunks = Layout::vertical([
        Constraint::Length(2), // totals strip
        Constraint::Min(1),    // dimensions | per-model
        Constraint::Length(1), // note caption
    ])
    .split(area);
    f.render_widget(Paragraph::new(vec![line_a, line_b]), chunks[0]);

    if app.rate_mode == RateMode::Actual {
        // Actual Usage picker: dimensions left, per-model table right.
        let cols = Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(chunks[1]);
        f.render_widget(Paragraph::new(dim_lines), cols[0]);
        let right = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(cols[1]);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled("per model", dimst))),
            right[0],
        );
        if models.is_empty() {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled("no usage in window", dimst))),
                right[1],
            );
        } else {
            let header = Row::new(
                ["model", "turns", "saved", "value", "time", "spent"]
                    .into_iter()
                    .map(|h| Cell::from(Span::styled(h, dimst))),
            );
            let body: Vec<Row> = models
                .iter()
                .map(|m| {
                    let turns = m["turns"].as_i64().unwrap_or(0);
                    let share = if tot_turns > 0 {
                        turns as f64 / tot_turns as f64
                    } else {
                        0.0
                    };
                    let saved_tok = m["saved_tokens"].as_f64().unwrap_or(0.0).max(0.0) as u64;
                    let saved_usd = m["saved_usd"].as_f64().unwrap_or(0.0);
                    let spent = m["consumed_usd"].as_f64().unwrap_or(0.0);
                    let time = model::human_time(rts * share * app.rt_seconds);
                    let label = model::model_label(m["model"].as_str().unwrap_or("?"));
                    let row = Row::new(vec![
                        Cell::from(Span::styled(label, ink)),
                        Cell::from(Span::styled(turns.to_string(), ink)),
                        Cell::from(Span::styled(
                            format!("{} tok", model::human_count(saved_tok)),
                            val,
                        )),
                        Cell::from(Span::styled(model::money(saved_usd), val)),
                        Cell::from(Span::styled(time, ink)),
                        Cell::from(Span::styled(model::money(spent), val)),
                    ]);
                    if turns == 0 {
                        row.style(dimst)
                    } else {
                        row
                    }
                })
                .collect();
            let widths = [
                Constraint::Length(11),
                Constraint::Length(7),
                Constraint::Length(11),
                Constraint::Length(9),
                Constraint::Length(11),
                Constraint::Length(9),
            ];
            f.render_widget(
                Table::new(body, widths).header(header).column_spacing(1),
                right[1],
            );
        }
    } else {
        // Model-rate picker: per-dimension rows only, no per-model table.
        f.render_widget(Paragraph::new(dim_lines), chunks[1]);
    }

    let note = av["note"].as_str().unwrap_or("");
    let model_name = av["model"].as_str().unwrap_or("");
    let caption = if model_name.is_empty() {
        note.to_string()
    } else {
        format!("{note} · {model_name}")
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(caption, dimst))),
        chunks[2],
    );
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
            applied_value(f, a, app);
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
        // A per-model figure: Opus's saved-value column ($0.432).
        assert!(buf.contains("$0.432"), "per-model saved value cell");
        // Spent total ($1.66) excludes the synthetic/empty rows; their $9.00 never renders.
        assert!(buf.contains("$1.66"), "spent total excludes synthetic + empty");
        assert!(!buf.contains("$9.00"), "synthetic row filtered out");
    }

    #[test]
    fn model_mode_hides_per_model_table() {
        let app = app_with(snapshot(), RateMode::Opus);
        let buf = render(&app);
        // Per-dimension rows render (the "lens tools" breakdown).
        assert!(buf.contains("darkroom"), "per-dimension row present");
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
