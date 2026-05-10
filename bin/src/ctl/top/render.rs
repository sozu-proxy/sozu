//! Render loop for `sozu top`. Synchronous (no tokio): the UI thread owns
//! the terminal, polls crossterm events with `event::poll(timeout)`, and
//! drains snapshot + event channels between input ticks.
//!
//! Frame cap: 30 fps. Data ticks fire as snapshots arrive on the
//! collector channel. Synchronized output (DEC mode 2026 via
//! `BeginSynchronizedUpdate` / `EndSynchronizedUpdate`) wraps each frame
//! so tmux + iTerm2 see a single atomic paint instead of per-cell flicker.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as CtEvent, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, poll, read,
};
use crossterm::execute;
use crossterm::terminal::{
    BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
    disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Tabs};
use tui_big_text::{BigText, PixelSize};
use tui_input::backend::crossterm::EventHandler;

use super::app::{ActiveTab, App};
use super::panes;
use super::theme::{GlyphMode, Skin};
use super::transport::{CertsSnapshot, ListenersSnapshot, Snapshot, TopEvent};

/// Cap the redraw rate at 30 fps regardless of how often new snapshots /
/// events arrive. Higher rates only burn CPU on tmux + non-Sixel
/// terminals; 33 ms is the documented btop-style upper bound.
const RENDER_INTERVAL: Duration = Duration::from_millis(33);

pub struct RenderConfig {
    pub mouse: bool,
    pub tick_once: bool,
    pub snapshot_frames: Option<u32>,
    /// Optional `--skin <name>` override, threaded through from clap so
    /// the renderer can call `Skin::resolve` once at startup. `None`
    /// resolves to the built-in default unless `SOZU_TOP_SKIN` overrides.
    pub skin: Option<String>,
    /// Optional `--glyphs` clap override. `None` runs `GlyphMode::resolve`
    /// auto-detect against `TERM` / `LC_ALL` / `LC_CTYPE` / `LANG`.
    pub glyphs: Option<crate::cli::TopGlyphs>,
}

/// Drive the TUI to completion. Returns when the user quits, the data
/// channels close, or `tick_once` / `snapshot_frames` exhausts.
pub fn run(
    cfg: RenderConfig,
    snapshots: Receiver<Snapshot>,
    events: Receiver<TopEvent>,
    listeners: Receiver<ListenersSnapshot>,
    certs: Receiver<CertsSnapshot>,
) -> io::Result<()> {
    // SIGINT/SIGTERM handler: flips a shared flag the loop checks every
    // tick. The terminal restore happens via `RawModeGuard::Drop` regardless
    // of how we exit (clean quit, panic, or signal-driven exit).
    let signal_quit = Arc::new(AtomicBool::new(false));
    let _ = ctrlc::try_set_handler({
        let signal_quit = Arc::clone(&signal_quit);
        move || signal_quit.store(true, Ordering::SeqCst)
    });

    let mut app = App::new();
    let (skin, skin_status) = Skin::resolve(cfg.skin.as_deref());
    let glyphs = GlyphMode::resolve(cfg.glyphs);
    app.glyphs = glyphs;
    if let Some(msg) = skin_status {
        // Surface the diagnostic in the status bar so the operator sees
        // *why* their override didn't take effect (typo'd name, parse
        // error, etc.) rather than silently rendering with the default.
        app.status = msg;
    }

    let _guard = RawModeGuard::install(cfg.mouse)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut last_render = Instant::now() - RENDER_INTERVAL;
    let mut frames_drawn: u32 = 0;
    let snapshot_frames_target = cfg.snapshot_frames;

    loop {
        if signal_quit.load(Ordering::SeqCst) || app.should_quit {
            break;
        }

        // Drain snapshots: keep the freshest one (the channel is
        // bounded(1), so at most a handful are buffered).
        while let Ok(snap) = snapshots.try_recv() {
            app.ingest_snapshot(&snap);
        }
        while let Ok(ev) = events.try_recv() {
            app.ingest_event(ev);
        }
        while let Ok(listeners) = listeners.try_recv() {
            app.ingest_listeners(listeners);
        }
        while let Ok(certs) = certs.try_recv() {
            app.ingest_certs(certs);
        }

        // Poll for input or sleep until the next render tick. The timeout
        // is whichever is sooner: the next render or 50 ms (so we drain
        // channels at least 20 times per second when the user is idle).
        let now = Instant::now();
        let next_render = last_render + RENDER_INTERVAL;
        let timeout = next_render
            .saturating_duration_since(now)
            .min(Duration::from_millis(50));

        if poll(timeout)? {
            match read()? {
                CtEvent::Key(key) if key.kind == KeyEventKind::Press => {
                    handle_key(&mut app, key);
                }
                CtEvent::Resize(_, _) => {
                    // No-op: ratatui re-queries the size on the next draw.
                }
                _ => {}
            }
        }

        // Frame cap: only redraw if RENDER_INTERVAL has elapsed since the
        // last paint. Synchronized output wraps the draw to give tmux a
        // single atomic frame.
        if last_render.elapsed() >= RENDER_INTERVAL {
            // Age each active pulse before the paint so the tint fades
            // one frame at a time. Skipping this would freeze pulses on
            // screen between snapshots.
            app.tick_pulses();
            execute!(io::stdout(), BeginSynchronizedUpdate)?;
            terminal.draw(|f| draw(f, &app, &skin))?;
            execute!(io::stdout(), EndSynchronizedUpdate)?;
            last_render = Instant::now();
            frames_drawn += 1;

            if cfg.tick_once && frames_drawn >= 1 {
                break;
            }
            if let Some(target) = snapshot_frames_target {
                if frames_drawn >= target {
                    break;
                }
            }
        }
    }
    Ok(())
}

fn handle_key(app: &mut App, key: KeyEvent) {
    // Palette mode swallows almost every key so the operator can type
    // command text freely. Only Enter / Escape / Ctrl-C escape back to
    // the normal handler.
    if app.palette_open {
        match key.code {
            KeyCode::Enter => app.apply_palette(),
            KeyCode::Esc => app.cancel_palette(),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.cancel_palette()
            }
            _ => {
                // Forward editing keys (backspace, arrows, character input)
                // to the tui-input widget so it maintains its own cursor.
                app.palette_input.handle_event(&CtEvent::Key(key));
            }
        }
        return;
    }
    match key.code {
        KeyCode::Char(':') => app.open_palette(),
        KeyCode::Char('q') | KeyCode::Char('Q') => app.should_quit = true,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true
        }
        KeyCode::F(10) => app.should_quit = true,
        KeyCode::Char('?') | KeyCode::F(1) => app.help_visible = !app.help_visible,
        KeyCode::Tab => app.active_tab = app.active_tab.cycle(true),
        KeyCode::BackTab => app.active_tab = app.active_tab.cycle(false),
        KeyCode::Char(c @ '1'..='7') => {
            if let Some(tab) = ActiveTab::from_digit(c.to_digit(10).unwrap_or(0) as u8) {
                app.active_tab = tab;
            }
        }
        // CLUSTERS sort cycle / reverse; mirror procs / k9s muscle memory.
        KeyCode::Char('s') if app.active_tab == ActiveTab::Clusters => {
            app.cluster_sort = app.cluster_sort.cycle();
        }
        KeyCode::Char('S') if app.active_tab == ActiveTab::Clusters => {
            app.cluster_sort_reverse = !app.cluster_sort_reverse;
        }
        KeyCode::Char('s') if app.active_tab == ActiveTab::Backends => {
            app.backend_sort = app.backend_sort.cycle();
        }
        KeyCode::Char('S') if app.active_tab == ActiveTab::Backends => {
            app.backend_sort_reverse = !app.backend_sort_reverse;
        }
        _ => {}
    }
}

fn draw(f: &mut ratatui::Frame<'_>, app: &App, skin: &Skin) {
    let area = f.area();
    let alert = app.thresholds.critical_message(&app.overview);
    let constraints: Vec<Constraint> = match alert {
        Some(_) => vec![
            Constraint::Length(3), // tabs row
            Constraint::Length(5), // big-text alert banner
            Constraint::Min(8),    // active pane
            Constraint::Length(1), // function-key bar
        ],
        None => vec![
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(1),
        ],
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    draw_tabs(f, chunks[0], app, skin);
    if let Some(headline) = alert {
        draw_alert(f, chunks[1], skin, headline);
        draw_pane(f, chunks[2], app, skin);
        draw_fkey_bar(f, chunks[3], app, skin);
    } else {
        draw_pane(f, chunks[1], app, skin);
        draw_fkey_bar(f, chunks[2], app, skin);
    }
}

fn draw_alert(f: &mut ratatui::Frame<'_>, area: Rect, skin: &Skin, headline: &str) {
    // Two-column layout: big-text headline on the left, narrow context
    // strip on the right with the headline label so screen-readers /
    // tmux-buffer scrollers still get a copyable string.
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
        .split(area);
    let big = BigText::builder()
        .pixel_size(PixelSize::Quadrant)
        .style(Style::default().fg(skin.hot).add_modifier(Modifier::BOLD))
        .lines(vec![Line::from(headline.to_owned())])
        .build();
    f.render_widget(big, cols[0]);
    let side = Paragraph::new(vec![
        Line::from(Span::styled(
            "ALERT",
            Style::default().fg(skin.hot).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            headline.to_owned(),
            Style::default().fg(skin.primary),
        )),
        Line::from(Span::styled(
            "see OVERVIEW for context",
            Style::default().fg(skin.secondary),
        )),
    ])
    .alignment(Alignment::Left);
    f.render_widget(side, cols[1]);
}

fn draw_tabs(f: &mut ratatui::Frame<'_>, area: Rect, app: &App, skin: &Skin) {
    let titles: Vec<Line<'_>> = ActiveTab::ALL
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let n = i + 1;
            Line::from(vec![Span::styled(
                format!(" {n} {} ", t.label()),
                if *t == app.active_tab {
                    skin.tab_focused()
                } else {
                    skin.tab_unfocused()
                },
            )])
        })
        .collect();
    let selected = ActiveTab::ALL
        .iter()
        .position(|t| *t == app.active_tab)
        .unwrap_or(0);
    let title = format!(
        " sōzu top · {} ",
        app.last_snapshot_at
            .map(|_| "live".to_owned())
            .unwrap_or_else(|| "no snapshot yet".into())
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(title)
        .style(Style::default().fg(skin.muted));
    let tabs = Tabs::new(titles)
        .select(selected)
        .block(block)
        .divider(Span::raw(" "));
    f.render_widget(tabs, area);
}

fn draw_pane(f: &mut ratatui::Frame<'_>, area: Rect, app: &App, skin: &Skin) {
    match app.active_tab {
        ActiveTab::Overview => panes::overview::render(f, area, app, skin),
        ActiveTab::Clusters => panes::clusters::render(f, area, app, skin),
        ActiveTab::Backends => panes::backends::render(f, area, app, skin),
        ActiveTab::Listeners => panes::listeners::render(f, area, app, skin),
        ActiveTab::Certs => panes::certs::render(f, area, app, skin),
        ActiveTab::H2 => panes::h2::render(f, area, app, skin),
        ActiveTab::Events => panes::events::render(f, area, app, skin),
    }
}

fn draw_fkey_bar(f: &mut ratatui::Frame<'_>, area: Rect, app: &App, skin: &Skin) {
    // Palette mode replaces the F-key bar with a one-line input so the
    // operator types `:cluster` / `:backend` / … inline. Drop back to
    // the htop-style strip otherwise.
    if app.palette_open {
        draw_palette(f, area, app, skin);
        return;
    }
    // htop-style F-key strip: alternating label/action so muscle memory
    // works without reading the keys explicitly. The actions wired today
    // are F1 Help, F10 Quit, Tab cycle, `:` palette; the rest reserve
    // their slots so future panes can plug in without re-laying out.
    let bindings: &[(&str, &str)] = &[
        ("F1", "Help"),
        ("F2", "Theme"),
        ("F3", "Find"),
        ("F4", "Filter"),
        ("F5", "Pause"),
        ("F6", "Sort"),
        ("F7", "Detail-"),
        ("F8", "Detail+"),
        ("F9", "Config"),
        ("F10", "Quit"),
    ];
    let mut spans: Vec<Span<'_>> = Vec::new();
    for (k, a) in bindings {
        spans.push(Span::styled(format!(" {k} "), skin.fkey_label()));
        spans.push(Span::styled(format!(" {a} "), skin.fkey_action()));
    }
    spans.push(Span::raw("  "));
    if let Some(err) = app.palette_error.as_ref() {
        spans.push(Span::styled(
            format!(" {err} "),
            Style::default().fg(skin.hot).add_modifier(Modifier::BOLD),
        ));
    } else {
        spans.push(Span::styled(
            " : palette ",
            Style::default()
                .fg(skin.accent)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" sort: {} ", app.cluster_sort.label()),
            Style::default()
                .fg(skin.accent)
                .add_modifier(Modifier::BOLD),
        ));
    }
    let para = Paragraph::new(Line::from(spans)).alignment(Alignment::Left);
    f.render_widget(para, area);
}

fn draw_palette(f: &mut ratatui::Frame<'_>, area: Rect, app: &App, skin: &Skin) {
    // Single-line `:cmd_here_` input. Prefixed `:` is implicit (the
    // operator presses `:` to enter palette mode, so the rendered text
    // does NOT include the colon — `apply_palette` strips any leading
    // colon defensively so paste-from-clipboard still works).
    let value = app.palette_input.value();
    let line = Line::from(vec![
        Span::styled(
            " :",
            Style::default()
                .fg(skin.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            value.to_owned(),
            Style::default()
                .fg(skin.primary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "_  ", // poor man's cursor; ratatui doesn't render the OS cursor
            Style::default()
                .fg(skin.accent)
                .add_modifier(Modifier::SLOW_BLINK),
        ),
        Span::styled(
            "Enter apply · Esc cancel · :cluster :backend :listener :cert :h2 :event :help :quit",
            Style::default().fg(skin.secondary),
        ),
    ]);
    f.render_widget(Paragraph::new(line).alignment(Alignment::Left), area);
}

/// RAII guard that restores the terminal on drop, panic, or signal exit.
/// Combines: `enable_raw_mode`, `EnterAlternateScreen`, optional
/// `EnableMouseCapture`, and cursor hide. Drop reverses the same sequence
/// so a panic mid-render doesn't leave the user's shell in raw mode with
/// the cursor hidden.
struct RawModeGuard {
    mouse: bool,
}

impl RawModeGuard {
    fn install(mouse: bool) -> io::Result<Self> {
        enable_raw_mode()?;
        let mut out = io::stdout();
        execute!(out, EnterAlternateScreen, Hide)?;
        if mouse {
            execute!(out, EnableMouseCapture)?;
        }
        Ok(Self { mouse })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let mut out = io::stdout();
        if self.mouse {
            let _ = execute!(out, DisableMouseCapture);
        }
        let _ = execute!(out, Show, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}
