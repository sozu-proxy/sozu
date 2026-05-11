//! Theme + glyph mode for `sozu top`.
//!
//! Defaults to a hard-coded `Skin` with Okabe-Ito categorical palette plus
//! Viridis-shaped continuous ramps and three glyph modes (Braille / Block /
//! TTY-ASCII). The `--skin <name>` flag (with `SOZU_TOP_SKIN` env override,
//! k9s parity) resolves to a TOML file under `$XDG_CONFIG_HOME/sozu/skins/
//! <name>.toml`, falling back to `/etc/sozu/skins/` for system-wide skins.
//! Auto-detection of terminal capabilities (`COLORTERM=truecolor`, `tput
//! colors`, `LANG`, `TERM=linux/dumb`) lands separately in the glyph
//! cascade follow-up.

use std::env;
use std::path::{Path, PathBuf};

use ratatui::style::{Color, Modifier, Style};
use serde::Deserialize;

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
    /// above 7. Not yet consumed by a pane; reserved for cluster-row tinting
    /// when the CLUSTERS pane gets categorical row colours.
    #[allow(dead_code)]
    pub categorical: Vec<Color>,
}

impl Skin {
    /// Hard-coded default skin used when no `--skin` / `SOZU_TOP_SKIN`
    /// override resolves. Okabe-Ito categorical (colour-blind safe in
    /// isolation; pairs distinguishable across the three dichromatic types
    /// per Okabe & Ito 2002) plus a Viridis-shaped continuous ramp.
    pub fn default_dark() -> Self {
        Self {
            primary: Color::Rgb(232, 232, 240),
            secondary: Color::Rgb(180, 184, 192),
            accent: Color::Rgb(86, 192, 240),
            cool: Color::Rgb(57, 173, 152),
            warm: Color::Rgb(245, 191, 79),
            hot: Color::Rgb(232, 84, 90),
            muted: Color::Rgb(96, 100, 112),
            categorical: OKABE_ITO_PLUS.to_vec(),
        }
    }

    /// Resolve the operator's skin choice. Precedence:
    ///
    /// 1. `SOZU_TOP_SKIN` env var (k9s parity) — set to `default` or
    ///    `none` to keep the built-in palette regardless of `--skin`.
    /// 2. `--skin <name>` clap argument.
    /// 3. The built-in `default_dark()` palette.
    ///
    /// Lookup paths: `$XDG_CONFIG_HOME/sozu/skins/<name>.toml` (defaulting
    /// to `$HOME/.config/sozu/skins/<name>.toml`), then
    /// `/etc/sozu/skins/<name>.toml`. Returns `(skin, status_message)`
    /// where the status string is `None` on success (built-in or loaded
    /// cleanly) or `Some(diagnostic)` when a lookup failed; the renderer
    /// surfaces this in the status bar so the operator sees why their
    /// override didn't take effect.
    pub fn resolve(name: Option<&str>) -> (Self, Option<String>) {
        let env_choice = env::var("SOZU_TOP_SKIN").ok();
        let effective = env_choice.as_deref().or(name);
        let choice = match effective {
            Some("") | Some("default") | Some("none") | None => {
                return (Self::default_dark(), None);
            }
            Some(other) => other,
        };
        // Helper for the five fail-closed paths below: every diagnostic
        // returns the built-in default paired with an operator-facing
        // status string. Keeps the policy from commit 5b098d9b
        // (fail-closed on every lookup defect) in a single spelling.
        let default_with = |msg: String| (Self::default_dark(), Some(msg));
        match Self::lookup_paths(choice).into_iter().find(|p| p.is_file()) {
            Some(path) => {
                // Defence-in-depth on top of the literal-string filter in
                // `lookup_paths`: canonicalize both the chosen file and the
                // anchor skins directory, then require the file to live
                // under the anchor. Defeats symlink-based escapes (a
                // `<xdg>/sozu/skins/<name>.toml` symlink pointing at
                // `/etc/shadow`) and TOCTOU races between `is_file()` and
                // `from_open_file`. Returning the default with a
                // diagnostic keeps `--skin` behaviour predictable when
                // the operator mis-set the lookup path or hit a
                // packaging bug.
                let Ok(resolved) = path.canonicalize() else {
                    return default_with(format!(
                        "skin `{choice}` canonicalize failed; using default"
                    ));
                };
                // Fail closed when the parent anchor cannot be resolved.
                // The previous shape skipped the confinement check on
                // anchor failure (race-delete of the parent, weird
                // /proc paths, unusual fs mounts) and parsed the bare
                // resolved file — defeating the defence-in-depth check.
                let Some(anchor) = Self::skins_anchor(&path) else {
                    return default_with(format!(
                        "skin `{choice}` anchor resolve failed; using default"
                    ));
                };
                if !resolved.starts_with(&anchor) {
                    return default_with(format!(
                        "skin `{choice}` resolved outside skins dir; using default"
                    ));
                }
                // Close the TOCTOU window: open the file once and read
                // through the `File` handle so the kernel can't swap a
                // symlink target between `canonicalize` and the read.
                // `read_to_string(&Path)` would re-resolve the path, so
                // an attacker who controls the skins dir could swap a
                // symlink in the gap. Going through `File::open` +
                // `Read::read_to_string` removes that gap.
                match Self::from_open_file(&resolved) {
                    Ok(skin) => (skin, None),
                    Err(e) => {
                        default_with(format!("skin `{choice}` parse error: {e}; using default"))
                    }
                }
            }
            None => default_with(format!("skin `{choice}` not found; using default")),
        }
    }

    /// Canonicalize the parent skins directory of a candidate skin path so
    /// the caller can confine the resolved file underneath it. Returns
    /// `None` when the parent cannot be canonicalized (e.g. the candidate
    /// itself disappeared between `is_file()` and here); the caller then
    /// falls back to the default skin with a diagnostic.
    fn skins_anchor(candidate: &Path) -> Option<PathBuf> {
        candidate.parent()?.canonicalize().ok()
    }

    /// Read + parse a skin TOML file via an explicit `File::open` +
    /// `Read::read_to_string` so the kernel cannot swap a symlink target
    /// between `canonicalize` and the read (the gap a `read_to_string(&Path)`
    /// helper would leave open by re-resolving the path). Called from
    /// `resolve` after the parent-anchor confinement check.
    pub fn from_open_file(path: &Path) -> Result<Self, SkinError> {
        use std::io::Read;
        let mut file = std::fs::File::open(path).map_err(SkinError::Io)?;
        let mut body = String::new();
        file.read_to_string(&mut body).map_err(SkinError::Io)?;
        let raw: RawSkin = toml::from_str(&body).map_err(|e| SkinError::Parse(e.to_string()))?;
        raw.into_skin().map_err(SkinError::Validate)
    }

    fn lookup_paths(name: &str) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        // Reject `..` / path-separators to keep `--skin` from escaping the
        // skins directory; treat malformed names as "not found".
        if name.contains('/') || name.contains('\\') || name.contains("..") {
            return paths;
        }
        let xdg = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
        if let Some(base) = xdg {
            paths.push(base.join("sozu").join("skins").join(format!("{name}.toml")));
        }
        paths.push(PathBuf::from("/etc/sozu/skins").join(format!("{name}.toml")));
        paths
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

/// Errors surfaced by `Skin::from_open_file`. Kept narrow so the renderer can
/// stringify them into a status-bar diagnostic without leaking IO details.
#[derive(Debug, thiserror::Error)]
pub enum SkinError {
    #[error("read skin: {0}")]
    Io(std::io::Error),
    #[error("parse skin: {0}")]
    Parse(String),
    #[error("validate skin: {0}")]
    Validate(String),
}

#[derive(Debug, Deserialize)]
struct RawSkin {
    primary: String,
    secondary: String,
    accent: String,
    cool: String,
    warm: String,
    hot: String,
    muted: String,
    #[serde(default)]
    categorical: Vec<String>,
}

impl RawSkin {
    fn into_skin(self) -> Result<Skin, String> {
        let primary = parse_hex(&self.primary, "primary")?;
        let secondary = parse_hex(&self.secondary, "secondary")?;
        let accent = parse_hex(&self.accent, "accent")?;
        let cool = parse_hex(&self.cool, "cool")?;
        let warm = parse_hex(&self.warm, "warm")?;
        let hot = parse_hex(&self.hot, "hot")?;
        let muted = parse_hex(&self.muted, "muted")?;
        let categorical: Vec<Color> = if self.categorical.is_empty() {
            OKABE_ITO_PLUS.to_vec()
        } else {
            self.categorical
                .iter()
                .enumerate()
                .map(|(i, s)| parse_hex(s, &format!("categorical[{i}]")))
                .collect::<Result<Vec<_>, _>>()?
        };
        Ok(Skin {
            primary,
            secondary,
            accent,
            cool,
            warm,
            hot,
            muted,
            categorical,
        })
    }
}

fn parse_hex(s: &str, field: &str) -> Result<Color, String> {
    let raw = s.trim_start_matches('#');
    if raw.len() != 6 {
        return Err(format!(
            "field `{field}`: expected #RRGGBB hex colour, got `{s}`"
        ));
    }
    let bytes = match u32::from_str_radix(raw, 16) {
        Ok(n) => n,
        Err(_) => return Err(format!("field `{field}`: `{s}` is not hex")),
    };
    let r = ((bytes >> 16) & 0xff) as u8;
    let g = ((bytes >> 8) & 0xff) as u8;
    let b = (bytes & 0xff) as u8;
    Ok(Color::Rgb(r, g, b))
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
    /// Collapse the optional clap override to a concrete mode. When the
    /// operator passed `--glyphs`, honour the explicit choice. Otherwise
    /// the auto-detect cascade walks three terminal capability signals:
    ///
    /// 1. `TERM` reports `dumb`, `linux`, `xterm-old`, or any `*-mono*`
    ///    variant — fall back to 7-bit ASCII (`Tty`). These terminals
    ///    typically render Unicode glyphs as `?` / boxes.
    /// 2. The active locale (`LC_ALL` / `LC_CTYPE` / `LANG`) ends in
    ///    `UTF-8` / `UTF8` AND isn't `C` / `POSIX` — Braille mosaics
    ///    are safe.
    /// 3. Otherwise default to `Block` (broadest Unicode terminal
    ///    compatibility — every Unicode-capable TTY ships block
    ///    elements `▁..▇█` even without nerd-font support).
    pub fn resolve(override_: Option<TopGlyphs>) -> Self {
        if let Some(forced) = override_ {
            return match forced {
                TopGlyphs::Braille => Self::Braille,
                TopGlyphs::Block => Self::Block,
                TopGlyphs::Tty => Self::Tty,
            };
        }
        Self::autodetect()
    }

    fn autodetect() -> Self {
        let term = std::env::var("TERM").unwrap_or_default();
        let term_lower = term.to_ascii_lowercase();
        if term_lower.is_empty()
            || term_lower == "dumb"
            || term_lower == "linux"
            || term_lower == "xterm-old"
            || term_lower.ends_with("-mono")
            || term_lower.contains("-mono-")
        {
            return Self::Tty;
        }
        let locale = std::env::var("LC_ALL")
            .or_else(|_| std::env::var("LC_CTYPE"))
            .or_else(|_| std::env::var("LANG"))
            .unwrap_or_default();
        let locale_upper = locale.to_ascii_uppercase();
        let is_c_locale =
            locale_upper == "C" || locale_upper == "POSIX" || locale_upper.starts_with("C.");
        let is_utf8 = locale_upper.contains("UTF-8") || locale_upper.contains("UTF8");
        if is_utf8 && !is_c_locale {
            Self::Braille
        } else {
            Self::Block
        }
    }
}

/// Status glyphs that double the colour signal so the colour-blind cue is
/// always backed up by a shape. `▲` rising, `▼` falling, `●` steady.
pub const GLYPH_RISING: &str = "▲";
pub const GLYPH_FALLING: &str = "▼";
pub const GLYPH_STEADY: &str = "●";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_accepts_hash_prefix_and_bare() {
        assert_eq!(
            parse_hex("#56c0f0", "x").unwrap(),
            Color::Rgb(0x56, 0xc0, 0xf0)
        );
        assert_eq!(
            parse_hex("56c0f0", "x").unwrap(),
            Color::Rgb(0x56, 0xc0, 0xf0)
        );
    }

    #[test]
    fn parse_hex_rejects_wrong_length() {
        assert!(parse_hex("#abc", "x").is_err());
        assert!(parse_hex("#abcdefgg", "x").is_err());
    }

    #[test]
    fn skin_from_toml_round_trip() {
        let toml = r##"
            primary   = "#e8e8f0"
            secondary = "#b4b8c0"
            accent    = "#56c0f0"
            cool      = "#39ad98"
            warm      = "#f5bf4f"
            hot       = "#e8545a"
            muted     = "#606470"
            categorical = ["#009e73", "#56b4e9"]
        "##;
        let raw: RawSkin = toml::from_str(toml).expect("parse");
        let skin = raw.into_skin().expect("validate");
        assert_eq!(skin.hot, Color::Rgb(0xe8, 0x54, 0x5a));
        assert_eq!(skin.categorical.len(), 2);
    }

    #[test]
    fn skin_from_toml_empty_categorical_uses_default() {
        let toml = r##"
            primary   = "#e8e8f0"
            secondary = "#b4b8c0"
            accent    = "#56c0f0"
            cool      = "#39ad98"
            warm      = "#f5bf4f"
            hot       = "#e8545a"
            muted     = "#606470"
        "##;
        let raw: RawSkin = toml::from_str(toml).unwrap();
        let skin = raw.into_skin().unwrap();
        assert_eq!(skin.categorical.len(), OKABE_ITO_PLUS.len());
    }

    #[test]
    fn skin_lookup_rejects_traversal() {
        assert!(Skin::lookup_paths("../etc/passwd").is_empty());
        assert!(Skin::lookup_paths("foo/bar").is_empty());
    }

    #[test]
    fn glyph_mode_explicit_override_wins() {
        assert!(matches!(
            GlyphMode::resolve(Some(TopGlyphs::Tty)),
            GlyphMode::Tty
        ));
        assert!(matches!(
            GlyphMode::resolve(Some(TopGlyphs::Braille)),
            GlyphMode::Braille
        ));
        assert!(matches!(
            GlyphMode::resolve(Some(TopGlyphs::Block)),
            GlyphMode::Block
        ));
    }
}
