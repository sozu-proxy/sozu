//! Theme + glyph mode for `sozu top`.
//!
//! Week-2 scope is intentionally narrow: a single hard-coded `Skin` with
//! Okabe-Ito categorical defaults + Viridis-shaped continuous ramps and three
//! glyph modes (Braille / Block / TTY-ASCII). The `--skin <name>` /
//! `SOZU_TOP_SKIN` lookup, capability auto-detection (`COLORTERM=truecolor`,
//! `tput colors`, `LANG`, `TERM=linux/dumb`), and TOML-encoded user skins land
//! in week 3 once the renderer is in place.

use ratatui::style::{Color, Modifier, Style};

use crate::cli::TopGlyphs;

/// Categorical palette + accent colours used across every pane. Defaults to
/// Okabe-Ito categorical for cluster colour assignment + a Viridis-shaped
/// continuous ramp for sparkline gradients. The colour-blind safe choice is
/// the hard-coded fallback; a `--skin` override only swaps the palette, never
/// overrides the structural rules (red/green is never the only signal —
/// glyphs `▲ ▼ ●` carry the redundant cue).
#[derive(Debug, Clone)]
pub struct Skin {
    /// Primary foreground for headings, focused tab, big-text numerals.
    pub primary: Color,
    /// Secondary foreground for status text, function-key labels.
    pub secondary: Color,
    /// Accent colour for sortable column headers + selected row.
    pub accent: Color,
    /// Cool tint for "all is well" sparkline tails (low values).
    pub cool: Color,
    /// Warm tint for "elevated" sparkline tails (mid-high values, no alert).
    pub warm: Color,
    /// Hot tint for sparkline alert peaks.
    pub hot: Color,
    /// Dim grey for tab labels not in focus, rule lines.
    pub muted: Color,
    /// Categorical palette assigned in cluster-table-row order. Cycles when
    /// the cluster count exceeds the palette length. Okabe-Ito 7-colour set
    /// extended with two Viridis points at the warm end for cluster counts
    /// above 7.
    pub categorical: &'static [Color],
}

impl Skin {
    /// Hard-coded default skin. Replaced by `Skin::from_toml` lookup in week 3.
    pub const fn default_dark() -> Self {
        Self {
            primary: Color::Rgb(232, 232, 240),
            secondary: Color::Rgb(180, 184, 192),
            accent: Color::Rgb(86, 192, 240),
            cool: Color::Rgb(57, 173, 152),
            warm: Color::Rgb(245, 191, 79),
            hot: Color::Rgb(232, 84, 90),
            muted: Color::Rgb(96, 100, 112),
            categorical: OKABE_ITO_PLUS,
        }
    }

    /// Style for the focused tab label in the numbered tab row.
    pub fn tab_focused(&self) -> Style {
        Style::default()
            .fg(self.primary)
            .bg(self.accent)
            .add_modifier(Modifier::BOLD)
    }

    /// Style for unfocused tab labels.
    pub fn tab_unfocused(&self) -> Style {
        Style::default().fg(self.muted)
    }

    /// Style for sparkline gradient at a given normalised position
    /// (`pos` in `[0.0, 1.0]`). Low → cool, mid → warm, high → hot. Pure
    /// function so the renderer can call it per-bar.
    pub fn spark_color(&self, pos: f32) -> Color {
        if pos < 0.5 {
            self.cool
        } else if pos < 0.85 {
            self.warm
        } else {
            self.hot
        }
    }

    /// Style applied to a cluster row when its sparkline has crossed the
    /// "critical" threshold (e.g. 5xx ratio > threshold). Stronger signal
    /// than `warm` and combines with the row's pulse marker.
    pub fn row_critical(&self) -> Style {
        Style::default().fg(self.hot).add_modifier(Modifier::BOLD)
    }

    /// Background tint for a row whose subject just disappeared (cluster
    /// or backend went away). Hot foreground + muted background so the
    /// row remains readable while still catching the eye.
    pub fn pulse_hot(&self) -> Style {
        Style::default()
            .fg(self.hot)
            .bg(self.muted)
            .add_modifier(Modifier::BOLD)
    }

    /// Background tint for a row whose subject just appeared (new cluster
    /// rolled out). Lower-priority cue than `pulse_hot`.
    pub fn pulse_cool(&self) -> Style {
        Style::default()
            .fg(self.cool)
            .bg(self.muted)
            .add_modifier(Modifier::BOLD)
    }

    /// Style for the function-key bar at the bottom of the screen.
    pub fn fkey_label(&self) -> Style {
        Style::default()
            .fg(self.primary)
            .bg(self.muted)
            .add_modifier(Modifier::BOLD)
    }

    pub fn fkey_action(&self) -> Style {
        Style::default().fg(self.secondary)
    }
}

/// Okabe-Ito 7-colour categorical palette + 2 Viridis high-end points to
/// extend headroom for >7 clusters in the heatmap. Each `Color::Rgb` value
/// is colour-blind safe in isolation; pairs are distinguishable across the
/// three common dichromatic types per Okabe-Ito's original 2002 paper.
const OKABE_ITO_PLUS: &[Color] = &[
    Color::Rgb(0, 158, 115),   // bluish green
    Color::Rgb(86, 180, 233),  // sky blue
    Color::Rgb(213, 94, 0),    // vermilion
    Color::Rgb(204, 121, 167), // reddish purple
    Color::Rgb(240, 228, 66),  // yellow
    Color::Rgb(0, 114, 178),   // blue
    Color::Rgb(230, 159, 0),   // orange
    // Viridis high end — gives extra differentiation when palette wraps.
    Color::Rgb(247, 209, 60),
    Color::Rgb(94, 201, 97),
];

/// Resolved glyph mode for sparklines and bar fills. `TopGlyphs` from clap
/// is the user override; `GlyphMode::resolve` collapses `None` to a default
/// (`Block`) until the auto-detect cascade lands in week 3.
#[derive(Debug, Clone, Copy)]
pub enum GlyphMode {
    /// Highest-density Unicode Braille mosaics; lifts each bar with sub-cell
    /// resolution. Default once auto-detect lands and the terminal reports
    /// Unicode-capable locale + an adequate font.
    Braille,
    /// Plain Unicode block elements (`▁▂▃▄▅▆▇█`). Broadest Unicode terminal
    /// compatibility; the safe v1 default.
    Block,
    /// 7-bit ASCII fallback for `linux`/`dumb` TERMs and serial consoles.
    Tty,
}

impl GlyphMode {
    /// Collapse the optional clap override to a concrete mode. Auto-detect
    /// (when `override_` is `None`) currently picks `Block`; the proper
    /// `LANG`/`TERM` cascade lands in week 3 alongside the skin loader.
    pub fn resolve(override_: Option<TopGlyphs>) -> Self {
        match override_ {
            Some(TopGlyphs::Braille) => Self::Braille,
            Some(TopGlyphs::Block) => Self::Block,
            Some(TopGlyphs::Tty) => Self::Tty,
            None => Self::Block,
        }
    }

    /// Eight-step ramp character set for sparkline cells. The ratatui
    /// `Sparkline` widget pulls these via `BarSet`; we expose the strings
    /// directly so panes that build their own column heatmap can match.
    pub fn ramp(self) -> &'static [&'static str] {
        match self {
            Self::Braille => &["⠀", "⡀", "⣀", "⣄", "⣤", "⣦", "⣶", "⣷", "⣿"],
            Self::Block => &[" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"],
            Self::Tty => &[" ", ".", ":", "-", "=", "+", "*", "#", "@"],
        }
    }
}

/// Status glyphs that double the colour signal so the colour-blind cue is
/// always backed up by a shape. `▲` rising, `▼` falling, `●` steady.
pub const GLYPH_RISING: &str = "▲";
pub const GLYPH_FALLING: &str = "▼";
pub const GLYPH_STEADY: &str = "●";
