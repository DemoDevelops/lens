//! The two dashboard palettes as ratatui colors — the exact web hex from
//! `INDEX_HTML`'s CSS custom properties (`:root` = dark, `body.t70` = 70s), so
//! the terminal and the browser render the same scheme.

use ratatui::style::Color;

use super::ThemeKind;

/// Role colors for one theme, mirroring the web CSS variables `--bg --panel
/// --line --ink --dim --accent --warn --bad`. The fields are the frozen
/// contract every panel styles against.
#[derive(Clone, Copy)]
pub(crate) struct Palette {
    #[allow(dead_code)] // parity anchor: mirrors the web's `--bg` var; no panel needs its own bg fill
    pub bg: Color,
    #[allow(dead_code)] // parity anchor: mirrors the web's `--panel` var; no panel needs a distinct fill yet
    pub panel: Color,
    #[allow(dead_code)] // parity anchor: mirrors the web's `--line` var; the TUI borders use `dim` instead for terminal visibility (the web's near-bg hairline doesn't survive coarser terminal color rendering)
    pub line: Color,
    pub ink: Color,
    pub dim: Color,
    pub accent: Color,
    pub warn: Color,
    pub bad: Color,
}

impl From<ThemeKind> for Palette {
    /// `Palette::from(kind)` — hex-exact against the web palettes.
    fn from(kind: ThemeKind) -> Palette {
        match kind {
            ThemeKind::Dark => Palette {
                bg: Color::Rgb(0x0b, 0x0d, 0x10),
                panel: Color::Rgb(0x14, 0x18, 0x1d),
                line: Color::Rgb(0x22, 0x2a, 0x31),
                ink: Color::Rgb(0xe6, 0xed, 0xf3),
                dim: Color::Rgb(0x8b, 0x97, 0xa3),
                accent: Color::Rgb(0x4c, 0xc4, 0xb0),
                warn: Color::Rgb(0xe0, 0xa4, 0x58),
                bad: Color::Rgb(0xe0, 0x6c, 0x75),
            },
            ThemeKind::Seventies => Palette {
                bg: Color::Rgb(0x1a, 0x14, 0x10),
                panel: Color::Rgb(0x24, 0x1c, 0x15),
                line: Color::Rgb(0x3a, 0x2c, 0x1c),
                ink: Color::Rgb(0xf0, 0xe2, 0xc4),
                dim: Color::Rgb(0xb0, 0x9a, 0x72),
                accent: Color::Rgb(0xe0, 0xa9, 0x3c),
                warn: Color::Rgb(0xcc, 0x6a, 0x2a),
                bad: Color::Rgb(0xb0, 0x43, 0x1c),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obs::tui::ThemeKind;
    use ratatui::style::Color;

    /// Replaces the old `theme_switches_palette` SGR assertions: each kind maps
    /// to its exact web hex, and the two palettes really differ.
    #[test]
    fn palette_switches_with_theme_kind() {
        assert_eq!(
            Palette::from(ThemeKind::Dark).accent,
            Color::Rgb(0x4c, 0xc4, 0xb0),
            "dark accent is the web teal"
        );
        assert_eq!(
            Palette::from(ThemeKind::Seventies).accent,
            Color::Rgb(0xe0, 0xa9, 0x3c),
            "70s accent is the web harvest gold"
        );
        assert_ne!(
            Palette::from(ThemeKind::Dark).accent,
            Palette::from(ThemeKind::Seventies).accent
        );
    }
}
