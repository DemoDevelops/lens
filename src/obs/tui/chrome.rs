//! Frame chrome: hand-drawn horizontal/vertical rules that keep every
//! section's border touching its neighbors' instead of floating one column
//! off it. A ratatui [`Block`]'s corner glyphs only work for one independent
//! box; a divider or split that needs to read as *part of* that same frame
//! is built here as plain text overwriting the frame's own border cells —
//! not drawn one column inset from them, which is what made the old
//! per-panel `info` divider look glued-on rather than merged: it sat inside
//! the panel's already-inset `inner` rect, one cell to the right of the real
//! left border column, so `│` and `├` rendered as two separate glyphs
//! instead of one.
//!
//! [`frame_overview_tools`] and [`frame_session_value`] are the two unified
//! panels this composes: everything that used to be its own bordered box
//! (`overview`, `tools`, `info`, `session`, `value`) now shares one frame
//! with its group, connected by real T-junctions.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use super::{header, misc, tables, value, App};

/// One horizontal rule spanning `width` columns: `left_cap`/`right_cap` at
/// the ends (`├`/`┤` for a mid-divider, `└`/`┘` for a hand-drawn bottom
/// border), an optional ` label ` burned into the fill just past the left
/// cap.
fn rule(width: u16, left_cap: char, right_cap: char, label: &str, color: Color) -> Line<'static> {
    let w = width as usize;
    if w == 0 {
        return Line::default();
    }
    let mut chars = vec!['─'; w];
    chars[0] = left_cap;
    chars[w - 1] = right_cap;
    if !label.is_empty() && w > 2 {
        let text: Vec<char> = format!(" {label} ").chars().collect();
        for (i, c) in text.iter().enumerate() {
            let pos = 1 + i;
            if pos < w - 1 {
                chars[pos] = *c;
            }
        }
    }
    Line::styled(chars.into_iter().collect::<String>(), Style::default().fg(color))
}

/// A rule split into two labeled halves by one junction character at column
/// `split_x` (`┬` opens a vertical divider below this line, `┴` closes one
/// above it).
#[allow(clippy::too_many_arguments)]
fn split_rule(
    width: u16,
    split_x: u16,
    junction: char,
    left_cap: char,
    right_cap: char,
    left_label: &str,
    right_label: &str,
    color: Color,
) -> Line<'static> {
    let w = width as usize;
    if w == 0 {
        return Line::default();
    }
    let sx = (split_x as usize).min(w - 1);
    let mut chars = vec!['─'; w];
    chars[0] = left_cap;
    chars[w - 1] = right_cap;
    if sx > 0 {
        chars[sx] = junction;
    }
    let put = |chars: &mut [char], start: usize, end_excl: usize, label: &str| {
        if label.is_empty() {
            return;
        }
        let text: Vec<char> = format!(" {label} ").chars().collect();
        for (i, c) in text.iter().enumerate() {
            let pos = start + 1 + i;
            if pos < end_excl {
                chars[pos] = *c;
            }
        }
    };
    put(&mut chars, 0, sx, left_label);
    put(&mut chars, sx, w - 1, right_label);
    Line::styled(chars.into_iter().collect::<String>(), Style::default().fg(color))
}

/// A one-column-wide vertical rule: `height` rows of `│`.
fn vline(height: u16, color: Color) -> Paragraph<'static> {
    let lines: Vec<Line> = (0..height).map(|_| Line::styled("│", Style::default().fg(color))).collect();
    Paragraph::new(lines)
}

/// Render a full-width mid-rule at row `y` of `area` — `area`'s own x/width,
/// not an inset `inner`, so the `├`/`┤` ends land exactly on the frame's
/// border columns instead of one cell short of them.
fn render_rule(f: &mut Frame, area: Rect, y: u16, label: &str, color: Color) {
    f.render_widget(
        rule(area.width, '├', '┤', label, color),
        Rect { x: area.x, y, width: area.width, height: 1 },
    );
}

/// `overview` + `tools` + `info`, unified: one outer border; `tools`/`info`
/// are real T-junction dividers instead of separate boxes with a gap between.
pub(crate) fn frame_overview_tools(f: &mut Frame, area: Rect, app: &App) {
    let p = &app.palette;
    let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(p.dim));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let table_rows = tables::tools_row_count(app) as u16 + 1; // header + data
    let hint_lines = tables::max_hint_lines(inner.width) as u16;

    let rows = Layout::vertical([
        Constraint::Length(1), // overview headline
        Constraint::Length(2), // stat strip
        Constraint::Length(1), // "tools" rule
        Constraint::Length(table_rows),
        Constraint::Length(1), // "info" rule
        Constraint::Length(hint_lines),
    ])
    .split(inner);

    header::overview_line(f, rows[0], app);
    tables::stat_strip(f, rows[1], app);
    render_rule(f, area, rows[2].y, "tools", p.dim);
    tables::tools_rows(f, rows[3], app);
    render_rule(f, area, rows[4].y, "info", p.dim);
    tables::render_hint(f, rows[5], app);
}

/// The height [`frame_overview_tools`] draws at, so the caller can size its
/// `Constraint::Length` before layout instead of guessing (and, on a tall
/// terminal, leaving a stretch of dead space inside the box).
pub(crate) fn overview_tools_height(term_width: u16, app: &App) -> u16 {
    let inner_width = term_width.saturating_sub(2);
    let table_rows = tables::tools_row_count(app) as u16 + 1;
    let hint_lines = tables::max_hint_lines(inner_width) as u16;
    2 + 1 + 2 + 1 + table_rows + 1 + hint_lines // border(2) + headline + stat(2) + rule + table + rule + hint
}

/// `session` | `value`, unified: one frame split by a real vertical divider
/// with `┬`/`┴` junctions where it meets the top/bottom border, instead of
/// two separate boxes side by side with a gap between.
pub(crate) fn frame_session_value(f: &mut Frame, area: Rect, app: &App) {
    let p = &app.palette;
    if area.height < 3 {
        return;
    }
    let content = Rect {
        x: area.x + 1,
        y: area.y,
        width: area.width.saturating_sub(2),
        height: area.height,
    };
    let cols =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(1), Constraint::Fill(1)]).split(content);
    let split_x = cols[1].x - area.x;

    f.render_widget(
        split_rule(area.width, split_x, '┬', '┌', '┐', "session", "value", p.dim),
        Rect { x: area.x, y: area.y, width: area.width, height: 1 },
    );
    f.render_widget(
        split_rule(area.width, split_x, '┴', '└', '┘', "", "", p.dim),
        Rect { x: area.x, y: area.y + area.height - 1, width: area.width, height: 1 },
    );

    let mid_height = area.height - 2;
    if mid_height == 0 {
        return;
    }
    let mid_y = area.y + 1;
    f.render_widget(vline(mid_height, p.dim), Rect { x: area.x, y: mid_y, width: 1, height: mid_height });
    f.render_widget(
        vline(mid_height, p.dim),
        Rect { x: area.x + area.width - 1, y: mid_y, width: 1, height: mid_height },
    );
    f.render_widget(vline(mid_height, p.dim), Rect { x: cols[1].x, y: mid_y, width: 1, height: mid_height });

    let content_left = Rect { x: cols[0].x, y: mid_y, width: cols[0].width, height: mid_height };
    let content_right = Rect { x: cols[2].x, y: mid_y, width: cols[2].width, height: mid_height };
    misc::session_content(f, content_left, app);
    value::value_content(f, content_right, app);
}

/// The height [`frame_session_value`] draws at: the taller of the `session`/
/// `value` columns' real content, plus the two hand-drawn top/bottom rules.
pub(crate) fn session_value_height(term_width: u16, app: &App) -> u16 {
    let col_width = (term_width.saturating_sub(3) / 2).max(1);
    let session_lines = misc::session_line_count(app, col_width);
    let value_lines = value::value_line_count(app);
    session_lines.max(value_lines) as u16 + 2
}
