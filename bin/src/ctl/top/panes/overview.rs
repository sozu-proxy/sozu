//! OVERVIEW pane — four sparklines (RPS, p99 latency, 5xx error %, slab
//! saturation) above a row of big numerals. The "blast factor" lives here:
//! it's the operator's first impression of the proxy's health.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, RenderDirection, Sparkline};

use super::super::app::App;
use super::super::theme::{GLYPH_FALLING, GLYPH_RISING, GLYPH_STEADY, Skin};

/// Render the four-sparkline OVERVIEW grid into `area`. The layout is
/// `2 lines big-numeral header` × `flex sparkline body` per cell.
pub fn render(f: &mut Frame<'_>, area: Rect, app: &App, skin: &Skin) {
    let bar_set = app.glyphs.sparkline_set();
    // 2x2 grid: top row RPS + p99, bottom row service-time + saturation.
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    let top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(outer[0]);
    let bot = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(outer[1]);

    render_cell(
        f,
        top[0],
        skin,
        "REQUESTS / SEC",
        &format_rps(&app.overview.rps),
        &subtitle_for_rps(app),
        &app.overview.rps.to_vec(),
        SparkScale::Auto,
        &bar_set,
    );
    render_cell(
        f,
        top[1],
        skin,
        "LATENCY p99 (ms)",
        &format_latency(&app.overview.latency_p99_ms),
        &subtitle_for_latency(app),
        &app.overview.latency_p99_ms.to_vec(),
        SparkScale::FixedMax(scale_for_latency(app)),
        &bar_set,
    );
    render_cell(
        f,
        bot[0],
        skin,
        "SERVICE TIME p99 (ms)",
        &format_latency(&app.overview.service_time_p99_ms),
        &subtitle_for_service_time(app),
        &app.overview.service_time_p99_ms.to_vec(),
        SparkScale::FixedMax(scale_for_service_time(app)),
        &bar_set,
    );
    render_cell(
        f,
        bot[1],
        skin,
        "SATURATION (%)",
        &format_pct_simple(&app.overview.saturation_pct),
        &subtitle_for_saturation(app),
        &app.overview.saturation_pct.to_vec(),
        SparkScale::FixedMax(100),
        &bar_set,
    );
}

enum SparkScale {
    Auto,
    FixedMax(u64),
}

fn render_cell(
    f: &mut Frame<'_>,
    area: Rect,
    skin: &Skin,
    title: &str,
    big: &str,
    subtitle: &str,
    samples: &[u64],
    scale: SparkScale,
    bar_set: &ratatui::symbols::bar::Set<'_>,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(format!(" {title} "))
        .style(Style::default().fg(skin.muted));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // big numeral
            Constraint::Length(1), // subtitle
            Constraint::Min(1),    // sparkline
        ])
        .split(inner);

    let big = Paragraph::new(Line::from(Span::styled(
        big.to_owned(),
        Style::default()
            .fg(skin.primary)
            .add_modifier(Modifier::BOLD),
    )));
    f.render_widget(big, chunks[0]);

    let sub = Paragraph::new(Line::from(Span::styled(
        subtitle.to_owned(),
        Style::default().fg(skin.secondary),
    )));
    f.render_widget(sub, chunks[1]);

    let max = match scale {
        SparkScale::Auto => samples.iter().copied().max().unwrap_or(1).max(1),
        SparkScale::FixedMax(m) => m.max(1),
    };
    // Recolour the sparkline based on the latest value's position in the
    // gradient. ratatui's Sparkline uses one style for the whole widget;
    // we approximate the "gradient at peak" by colouring the whole sparkline
    // at the position of its most recent sample. Per-bar gradient lands in
    // week 3 once the Canvas-backed renderer is in place.
    let last = samples.last().copied().unwrap_or(0);
    let pos = (last as f32 / max as f32).clamp(0.0, 1.0);
    // RightToLeft anchors the newest sample on the right edge so the
    // sparkline scrolls history leftward as new ticks arrive. The
    // default LeftToRight left-anchors and leaves the right side empty
    // until the ring reaches column-width samples — looks like the
    // graph is filling only half its pane on the operator's first
    // minute of monitoring.
    let spark = Sparkline::default()
        .data(samples)
        .max(max)
        .bar_set(bar_set.clone())
        .direction(RenderDirection::RightToLeft)
        .style(Style::default().fg(skin.spark_color(pos)));
    f.render_widget(spark, chunks[2]);
}

fn format_rps(ring: &super::super::app::SparkRing) -> String {
    match ring.last() {
        Some(v) => format!("{v} req/s"),
        None => "—".into(),
    }
}

fn format_latency(ring: &super::super::app::SparkRing) -> String {
    match ring.last() {
        Some(v) => format!("{v} ms"),
        None => "—".into(),
    }
}

fn subtitle_for_service_time(app: &App) -> String {
    let trend = trend_glyph(&app.overview.service_time_p99_ms);
    if app.overview.service_time_p99_ms.is_empty() {
        "no samples".into()
    } else {
        format!("sozu request-processing p99 · {trend}")
    }
}

fn scale_for_service_time(app: &App) -> u64 {
    // Anchor the sparkline's max at the latency threshold so service-time
    // spikes peg the cell the same way backend-latency spikes peg the
    // sibling cell. Floors at 50 ms so a quiet system doesn't make the
    // sparkline jitter on rounding noise.
    let threshold = app.thresholds.latency_p99_critical_ms.max(50.0) as u64;
    threshold.max(app.overview.service_time_p99_ms.max())
}

fn format_pct_simple(ring: &super::super::app::SparkRing) -> String {
    match ring.last() {
        Some(v) => format!("{v} %"),
        None => "—".into(),
    }
}

fn subtitle_for_rps(app: &App) -> String {
    let trend = trend_glyph(&app.overview.rps);
    format!(
        "{} client conns · {} active sessions · {trend} 60 s",
        app.overview.client_connections, app.overview.active_sessions,
    )
}

fn subtitle_for_latency(app: &App) -> String {
    let trend = trend_glyph(&app.overview.latency_p99_ms);
    if app.overview.latency_p99_ms.is_empty() {
        "no samples".into()
    } else {
        format!(
            "max p99 across clusters · {} ms threshold · {trend}",
            app.thresholds.latency_p99_critical_ms
        )
    }
}

fn subtitle_for_saturation(app: &App) -> String {
    let trend = trend_glyph(&app.overview.saturation_pct);
    format!(
        "slab/buffer; warn at {:.0} % · {trend}",
        app.thresholds.slab_critical_pct
    )
}

fn trend_glyph(ring: &super::super::app::SparkRing) -> &'static str {
    let last2: Vec<u64> = ring.samples().rev().take(2).copied().collect();
    if last2.len() < 2 {
        GLYPH_STEADY
    } else if last2[0] > last2[1] {
        GLYPH_RISING
    } else if last2[0] < last2[1] {
        GLYPH_FALLING
    } else {
        GLYPH_STEADY
    }
}

fn scale_for_latency(app: &App) -> u64 {
    // Anchor the sparkline's max at the threshold so a steady stream below
    // the threshold draws short bars and a spike above the threshold pegs
    // the cell. Floors at 100 ms so a slow startup with no traffic doesn't
    // make the cell jiggle.
    let threshold = app.thresholds.latency_p99_critical_ms.max(100.0) as u64;
    threshold.max(app.overview.latency_p99_ms.max())
}
