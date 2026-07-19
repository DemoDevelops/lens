//! `lens dashboard --tui` — the terminal renderer of the same snapshot the web
//! dashboard polls, as a ratatui widget app behind the default-on `tui` cargo
//! feature (`--no-default-features` builds the lean MCP server without it).
//!
//! One file per panel group (`header`, `tables`, `charts`, `value`, `misc`),
//! each panel a `fn(&mut Frame, Rect, &App)` reading the one [`App::snapshot`]
//! from `stats::snapshot_json_since` — parity with the web is structural: both
//! render the same snapshot, so a new dimension added to the producer shows in
//! both. The keys the panels read are listed in `stats::SNAPSHOT_DIMENSIONS`;
//! the parity tripwire in `dashboard.rs` scans the concatenated `tui/` sources
//! for each of them (`model.rs` names every key).
//!
//! [`Window`] and [`ThemeKind`] compile with the feature OFF too — the
//! `dashboard.rs` flag parsing references them unconditionally; the featureless
//! [`run`] bails with a rebuild hint instead of drawing.

#[cfg(feature = "tui")]
pub(crate) mod theme;

#[cfg(feature = "tui")]
pub(crate) mod model;

#[cfg(feature = "tui")]
pub(crate) mod header;

#[cfg(feature = "tui")]
pub(crate) mod tables;

#[cfg(feature = "tui")]
pub(crate) mod chrome;

#[cfg(feature = "tui")]
pub(crate) mod charts;

#[cfg(feature = "tui")]
pub(crate) mod value;

#[cfg(feature = "tui")]
pub(crate) mod misc;

use std::path::PathBuf;

use anyhow::Result;

#[cfg(feature = "tui")]
use std::io::IsTerminal;
#[cfg(feature = "tui")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "tui")]
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(feature = "tui")]
use ratatui::crossterm::event::{self, Event, KeyCode, KeyModifiers};
#[cfg(feature = "tui")]
use ratatui::Frame;
#[cfg(feature = "tui")]
use serde_json::Value;

#[cfg(feature = "tui")]
use super::stats::snapshot_json_since;
#[cfg(feature = "tui")]
use theme::Palette;

// ---------------------------------------------------------------------------
// Time window (--since / --today) + theme kind — always compiled, feature or
// not: dashboard.rs flag parsing references these unconditionally.
// ---------------------------------------------------------------------------

/// A TUI time-window spec. Resolved to a `since` cutoff each tick, so relative windows
/// ("last 1h") slide and "today" stays correct across midnight.
#[derive(Clone)]
pub enum Window {
    All,
    Today,
    Last(u64), // seconds back from now
}

impl Window {
    /// Parse a `--since` spec: `all`, `today`, or a duration like `15m`/`1h`/`3h`/`2d`.
    pub fn parse(spec: &str) -> Option<Window> {
        match spec.trim() {
            "all" => Some(Window::All),
            "today" => Some(Window::Today),
            s => parse_duration(s).map(Window::Last),
        }
    }
    /// The `since` cutoff (unix secs) at `now`, given the local TZ offset (for `today`'s
    /// midnight). `None` = all time.
    #[cfg_attr(not(feature = "tui"), allow(dead_code))]
    fn since(&self, now: i64, tz_offset: i64) -> Option<i64> {
        match self {
            Window::All => None,
            Window::Today => {
                let local = now + tz_offset;
                Some(local - local.rem_euclid(86_400) - tz_offset)
            }
            Window::Last(secs) => Some(now - *secs as i64),
        }
    }
    /// Header label for the active window.
    #[cfg_attr(not(feature = "tui"), allow(dead_code))]
    pub(crate) fn label(&self) -> String {
        match self {
            Window::All => "all time".to_string(),
            Window::Today => "today".to_string(),
            Window::Last(secs) => format!("last {}", human_dur(*secs)),
        }
    }
}

/// Parse `45s` / `15m` / `90m` / `1h` / `3h` / `2d` to seconds.
fn parse_duration(s: &str) -> Option<u64> {
    let i = s.find(|c: char| !c.is_ascii_digit())?;
    if i == 0 {
        return None;
    }
    let n: u64 = s[..i].parse().ok()?;
    let mult = match &s[i..] {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => return None,
    };
    n.checked_mul(mult)
}

/// Compact duration label: `45m` / `2h` / `3d`.
#[cfg_attr(not(feature = "tui"), allow(dead_code))]
fn human_dur(secs: u64) -> String {
    if secs >= 86_400 && secs.is_multiple_of(86_400) {
        format!("{}d", secs / 86_400)
    } else if secs >= 3600 && secs.is_multiple_of(3600) {
        format!("{}h", secs / 3600)
    } else {
        format!("{}m", secs.max(60) / 60)
    }
}

/// Local UTC offset in seconds via `date +%z` (`+HHMM`/`-HHMM`); 0 if unavailable.
#[cfg_attr(not(feature = "tui"), allow(dead_code))]
fn local_offset_secs() -> i64 {
    if let Ok(out) = std::process::Command::new("date").arg("+%z").output() {
        if let Ok(s) = String::from_utf8(out.stdout) {
            let s = s.trim();
            if s.len() == 5 {
                let sign = if s.starts_with('-') { -1 } else { 1 };
                if let (Ok(h), Ok(m)) = (s[1..3].parse::<i64>(), s[3..5].parse::<i64>()) {
                    return sign * (h * 3600 + m * 60);
                }
            }
        }
    }
    0
}

/// The two `--theme` palettes. `Dark` is the cool default, mirroring the web dashboard's
/// default dark theme; `Seventies` is the warm retro scheme.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ThemeKind {
    Dark,
    Seventies,
}

impl ThemeKind {
    /// Parse a `--theme` value: `dark`, or `70s`/`seventies`/`retro`.
    pub fn parse(s: &str) -> Option<ThemeKind> {
        match s.trim().to_lowercase().as_str() {
            "dark" => Some(ThemeKind::Dark),
            "70s" | "seventies" | "retro" => Some(ThemeKind::Seventies),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// UI state — the frozen panel contract (wave-2 tasks compile against this)
// ---------------------------------------------------------------------------

/// Width below which the auto view collapses to [`View::Mini`] (mirrors the
/// web's mini/full toggle); `--mini`/`--full` override.
#[cfg(feature = "tui")]
const MINI_MAX: usize = 56;

/// Layout density: `Full` shows every panel (fullcharts included); `Mini` is
/// the compact layout for narrow terminals.
#[cfg(feature = "tui")]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum View {
    Mini,
    Full,
}

/// Cost basis for the `$` headline: real per-model spend (`Actual`, the web's
/// "Actual Usage" mode) or one model's `$/M` input rate.
#[cfg(feature = "tui")]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RateMode {
    Actual,
    Fable,
    Opus,
    Sonnet,
    Haiku,
}

/// The single UI state object. Panels READ these fields; T2's `run()` constructs
/// and mutates it. T2 may add private locals in `run()`, and may add fields here,
/// but must NEVER rename or remove a field listed here (the panels compile
/// against them).
#[cfg(feature = "tui")]
pub(crate) struct App {
    pub(crate) snapshot: Value, // fully-prepared per-tick snapshot (scope_global/window_label injected, rtk rebased)
    pub(crate) theme: ThemeKind,
    pub(crate) palette: Palette,
    pub(crate) view: View,
    pub(crate) rate_mode: RateMode,
    pub(crate) rate: f64, // $/M input for the current model
    pub(crate) rt_seconds: f64,
    pub(crate) window: Window,
    pub(crate) scope_global: bool,
    pub(crate) scope_label: String, // display label for the active scope
    pub(crate) projects: Vec<String>, // scope cycle targets (global + these)
    pub(crate) scope_idx: usize,
    pub(crate) tool_sel: usize,        // selected tools-table row
    pub(crate) saved_series: Vec<u64>, // per-bucket deltas (60) from saved_buckets
    pub(crate) bytes_series: Vec<u64>, // per-bucket deltas (60) from bytes_buckets
    pub(crate) event_series: Vec<u64>, // per-bucket deltas (60) from event_buckets
    // loop-internal (T2 owns; frozen here so App is the whole state):
    pub(crate) dir: PathBuf,
    pub(crate) session: Option<String>,
    pub(crate) rtk_base: Option<(i64, i64, i64)>,
    pub(crate) tz_offset: i64,
    pub(crate) interval: u64,
}

// ---------------------------------------------------------------------------
// Entry point — same signature with the feature on or off, so the dashboard.rs
// call site needs no cfg
// ---------------------------------------------------------------------------

/// Built without the `tui` feature: explain how to get the terminal dashboard
/// back instead of drawing.
#[cfg(not(feature = "tui"))]
#[allow(clippy::too_many_arguments)]
pub fn run(
    _dir: PathBuf,
    _session: Option<String>,
    _scope_global: bool,
    _interval: u64,
    _window: Window,
    _rate: f64,
    _rt_seconds: f64,
    _theme: ThemeKind,
    _force_view: Option<bool>,
) -> Result<()> {
    anyhow::bail!(
        "built without the `tui` feature; use `lens dashboard` (web) or rebuild with --features tui"
    )
}

// ---------------------------------------------------------------------------
// Signal-safe interrupt: SIGINT/SIGTERM set this flag so the loop can leave the
// alternate screen (`ratatui::restore`) instead of stranding a frozen frame on a
// kill. `AtomicBool::store` is async-signal-safe.
// ---------------------------------------------------------------------------

#[cfg(feature = "tui")]
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

#[cfg(feature = "tui")]
extern "C" fn handle_interrupt(_sig: i32) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// Run the live terminal dashboard: build one [`App`] (first snapshot via
/// [`tick_refresh`]), install SIGINT/SIGTERM handlers, then loop — redraw every
/// iteration, block up to `interval` seconds for a key, and re-read the snapshot
/// when the interval elapses — until `q`/Ctrl-C or a caught signal, restoring the
/// terminal on the way out. Not a tty (pipe/CI): render one frame into a
/// `TestBackend` and return, so `--tui | cat` exits 0 without touching raw mode.
#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
pub fn run(
    dir: PathBuf,
    session: Option<String>,
    scope_global: bool,
    interval: u64,
    window: Window,
    rate: f64,
    rt_seconds: f64,
    theme: ThemeKind,
    force_view: Option<bool>,
) -> Result<()> {
    let tz_offset = local_offset_secs();
    let view = match force_view {
        Some(true) => View::Full,
        Some(false) => View::Mini,
        // Auto: by terminal width, like the old renderer's `w < MINI_MAX` collapse.
        None if (term_cols() as usize) < MINI_MAX => View::Mini,
        None => View::Full,
    };
    // Scope cycle targets. There is no project enumerator (no `obs/session.rs`),
    // so the only resolvable non-global scope is the launch repo — a single entry
    // making `s` a clean global<->repo toggle through `(idx+1) % (projects+1)`.
    // The repo names itself from the data dir's parent (`<repo>/.lens`).
    let launch_repo = dir
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "this repo".to_string());
    let scope_idx = if scope_global { 0 } else { 1 };
    let scope_label = if scope_global {
        "global".to_string()
    } else {
        launch_repo.clone()
    };
    let mut app = App {
        snapshot: Value::Null, // populated by the first tick_refresh below
        theme,
        palette: Palette::from(theme),
        view,
        rate_mode: RateMode::Actual,
        rate,
        rt_seconds,
        window,
        scope_global,
        scope_label,
        projects: vec![launch_repo],
        scope_idx,
        tool_sel: 0,
        saved_series: Vec::new(),
        bytes_series: Vec::new(),
        event_series: Vec::new(),
        dir,
        session,
        rtk_base: None,
        tz_offset,
        interval: interval.max(1),
    };
    tick_refresh(&mut app)?; // first snapshot at the launch scope/window

    if !std::io::stdout().is_terminal() {
        // Not a terminal (pipe/CI): crossterm errors enabling raw mode, so render
        // one frame into a TestBackend and discard it — `--tui` exits 0 cleanly.
        let backend = ratatui::backend::TestBackend::new(term_cols(), term_rows().unwrap_or(40));
        let mut terminal = ratatui::Terminal::new(backend)?;
        terminal.draw(|f| draw(f, &app))?;
        return Ok(());
    }

    // Catch SIGINT/SIGTERM so a kill restores the terminal instead of leaving it
    // in raw mode on the alternate screen.
    unsafe {
        libc::signal(libc::SIGINT, handle_interrupt as *const () as usize);
        libc::signal(libc::SIGTERM, handle_interrupt as *const () as usize);
    }

    let mut terminal = ratatui::init(); // raw mode + alternate screen + panic hook
    let mut last_tick = Instant::now();

    // Background refresh plumbing: `compute_tick` is the slow part (observed
    // ~2.8s scanning transcripts across every Claude config dir), so it never
    // runs on this thread past the first frame — spawning it and picking up
    // the result over a channel keeps every keypress redrawing immediately
    // regardless of how long the scan takes. `generation` tags each spawn so
    // a `w`/`s`/`r` change mid-scan doesn't get clobbered when an older,
    // now-stale scan (for the previous window/scope) finishes later.
    let (tx, rx) = std::sync::mpsc::channel::<(u64, TickData)>();
    let mut generation: u64 = 0;
    let mut refresh_in_flight = false;
    let spawn_refresh = |app: &App, gen: u64, tx: std::sync::mpsc::Sender<(u64, TickData)>| {
        let dir = app.dir.clone();
        let session = app.session.clone();
        let scope_global = app.scope_global;
        let window = app.window.clone();
        let tz_offset = app.tz_offset;
        let rtk_base = app.rtk_base;
        std::thread::spawn(move || {
            let data = compute_tick(dir, session, scope_global, window, tz_offset, rtk_base);
            let _ = tx.send((gen, data));
        });
    };

    // No `?` past this point: every exit path must fall through to
    // `ratatui::restore()`, so errors break the loop into `res` instead.
    let res: Result<()> = loop {
        if INTERRUPTED.load(Ordering::SeqCst) {
            break Ok(());
        }
        // Apply whatever background refresh has landed since the last frame;
        // discard a result whose generation is behind the latest spawn (a
        // stale scan for a window/scope the user has since changed away from).
        while let Ok((gen, data)) = rx.try_recv() {
            if gen == generation {
                apply_tick(&mut app, data);
                refresh_in_flight = false;
            }
        }
        if let Err(e) = terminal.draw(|f| draw(f, &app)) {
            break Err(e.into());
        }
        let timeout = Duration::from_secs(app.interval.max(1));
        match event::poll(timeout) {
            Ok(true) => match event::read() {
                // Some terminals report both a press and a release per keystroke;
                // acting on both double-fires every key (e.g. `w` skips two window
                // presets per tap). Only presses drive state.
                Ok(Event::Key(k)) if k.kind == event::KeyEventKind::Press => {
                    if on_key(&mut app, k) {
                        break Ok(());
                    }
                    // `w`/`s`/`r` change what the snapshot must contain; `on_key`
                    // is pure state, so re-read here — always spawns, even over an
                    // in-flight scan, so a deliberate setting change is never
                    // delayed behind a slow periodic refresh for the old settings.
                    if matches!(
                        k.code,
                        KeyCode::Char('w') | KeyCode::Char('s') | KeyCode::Char('r')
                    ) {
                        generation += 1;
                        refresh_in_flight = true;
                        spawn_refresh(&app, generation, tx.clone());
                        last_tick = Instant::now();
                    }
                }
                Ok(_) => {}
                Err(e) => break Err(e.into()),
            },
            Ok(false) => {}
            // `poll` interrupted by a signal (or a transient error): re-check the
            // interrupt flag, otherwise fall through to the periodic tick.
            Err(_) => {
                if INTERRUPTED.load(Ordering::SeqCst) {
                    break Ok(());
                }
            }
        }
        // Periodic refresh: skip while one's already in flight instead of piling
        // up concurrent scans every time the interval elapses without a result
        // back yet (the scan routinely takes longer than the default interval).
        if last_tick.elapsed() >= timeout && !refresh_in_flight {
            generation += 1;
            refresh_in_flight = true;
            spawn_refresh(&app, generation, tx.clone());
            last_tick = Instant::now();
        }
    };
    ratatui::restore();
    res
}

/// Apply one key press to `app`'s UI state, returning `true` to quit. Pure state
/// (no snapshot IO) so `run` re-reads the snapshot itself after a `w`/`s`/`r`
/// key, and a test can assert every transition without a terminal.
///
/// - `q` / Ctrl-C → quit.
/// - `t` → theme Dark<->Seventies (palette follows).
/// - `v` → view Mini<->Full.
/// - `w` → window ring: live/15m/1h/3h/today/all.
/// - `s` → scope: global <-> each project.
/// - `r` → rate basis: Actual → Fable → Opus → Sonnet → Haiku → Actual.
/// - Down/`j` / Up/`k` → move the tools-table selection down/up.
#[cfg(feature = "tui")]
fn on_key(app: &mut App, key: event::KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('q') => return true,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return true,
        KeyCode::Char('t') => {
            app.theme = match app.theme {
                ThemeKind::Dark => ThemeKind::Seventies,
                ThemeKind::Seventies => ThemeKind::Dark,
            };
            app.palette = Palette::from(app.theme);
        }
        KeyCode::Char('v') => {
            app.view = match app.view {
                View::Mini => View::Full,
                View::Full => View::Mini,
            };
        }
        KeyCode::Char('w') => {
            // web live / 15m / 1h / 3h / today / all
            let presets = [
                Window::Last(60),
                Window::Last(900),
                Window::Last(3600),
                Window::Last(10800),
                Window::Today,
                Window::All,
            ];
            let cur = match &app.window {
                Window::Last(60) => 0,
                Window::Last(900) => 1,
                Window::Last(3600) => 2,
                Window::Last(10800) => 3,
                Window::Today => 4,
                Window::All => 5,
                // An off-preset `--since`: the next `w` enters the ring at live.
                Window::Last(_) => 5,
            };
            app.window = presets[(cur + 1) % presets.len()].clone();
        }
        KeyCode::Char('s') => {
            app.scope_idx = (app.scope_idx + 1) % (app.projects.len() + 1);
            app.scope_global = app.scope_idx == 0;
            app.scope_label = if app.scope_global {
                "global".to_string()
            } else {
                app.projects
                    .get(app.scope_idx - 1)
                    .cloned()
                    .unwrap_or_else(|| "this repo".to_string())
            };
        }
        KeyCode::Char('r') => {
            app.rate_mode = match app.rate_mode {
                RateMode::Actual => RateMode::Fable,
                RateMode::Fable => RateMode::Opus,
                RateMode::Opus => RateMode::Sonnet,
                RateMode::Sonnet => RateMode::Haiku,
                RateMode::Haiku => RateMode::Actual,
            };
            // A model mode reprices off the shared per-Mtok input rate; Actual
            // keeps the launch `--rate` (real per-model spend, not one rate).
            let model = match app.rate_mode {
                RateMode::Fable => Some("fable"),
                RateMode::Opus => Some("opus"),
                RateMode::Sonnet => Some("sonnet"),
                RateMode::Haiku => Some("haiku"),
                RateMode::Actual => None,
            };
            if let Some(m) = model {
                app.rate = crate::obs::pricing::price_for(m).input;
            }
        }
        // Down/`j` moves toward the bottom of the list (higher index); Up/`k`
        // moves back toward the top. `j`/`k` alias the arrows for terminals
        // (or multiplexers) that intercept arrow keys for their own navigation.
        KeyCode::Down | KeyCode::Char('j') => app.tool_sel = app.tool_sel.saturating_add(1),
        KeyCode::Up | KeyCode::Char('k') => app.tool_sel = app.tool_sel.saturating_sub(1),
        _ => {}
    }
    false
}

/// One completed tick's worth of data — what [`compute_tick`] produces and
/// [`run`]'s loop applies to `app` once it arrives, whether computed inline
/// (the first frame) or on a background thread (every later refresh).
#[cfg(feature = "tui")]
struct TickData {
    snapshot: Value,
    saved_series: Vec<u64>,
    bytes_series: Vec<u64>,
    event_series: Vec<u64>,
    rtk_base: Option<(i64, i64, i64)>,
}

/// The actual per-tick work: resolve scope/window to a snapshot and its
/// derived series. Pure function of owned inputs (no `&App`) so it can run on
/// a background thread — this is the slow part (`snapshot_json_since` scans
/// the op-log *and* every Claude Code transcript file under every config dir,
/// observed at ~2.8s against a real multi-account history), and running it on
/// the main thread blocked every keypress for its duration.
#[cfg(feature = "tui")]
fn compute_tick(
    dir: PathBuf,
    session: Option<String>,
    scope_global: bool,
    window: Window,
    tz_offset: i64,
    mut rtk_base: Option<(i64, i64, i64)>,
) -> TickData {
    // --global reads the machine-global mirror (cross-repo, no session filter);
    // otherwise the launch repo/session — also the fallback for every non-global
    // scope, since there is no per-project data-dir resolver.
    let (d, sess) = if scope_global {
        match crate::rtk::home_root() {
            Some(home) => (home, None),
            None => (dir.clone(), session.clone()),
        }
    } else {
        (dir.clone(), session.clone())
    };
    // Resolve the window each tick so "last 1h" slides and "today" stays correct.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let since = window.since(now, tz_offset);
    let mut snap = snapshot_json_since(&d, sess.as_deref(), since, None);
    // Surface scope + window to the panels (the snapshot can't know them).
    snap["scope_global"] = Value::Bool(scope_global);
    snap["window_label"] = Value::String(window.label());
    // RTK gain is cumulative; show the delta since launch (base captured tick 1).
    rebase_rtk(&mut snap, &mut rtk_base);
    let saved_series = model::series(&snap, "saved_buckets", 60);
    let bytes_series = model::series(&snap, "bytes_buckets", 60);
    let event_series = model::series(&snap, "event_buckets", 60);
    TickData {
        snapshot: snap,
        saved_series,
        bytes_series,
        event_series,
        rtk_base,
    }
}

/// Apply a completed [`TickData`] to `app`.
#[cfg(feature = "tui")]
fn apply_tick(app: &mut App, data: TickData) {
    app.snapshot = data.snapshot;
    app.saved_series = data.saved_series;
    app.bytes_series = data.bytes_series;
    app.event_series = data.event_series;
    app.rtk_base = data.rtk_base;
}

/// Synchronous tick, run inline for the very first frame (before the loop
/// starts, and for the non-tty one-frame fallback) where there's nothing yet
/// to keep responsive.
#[cfg(feature = "tui")]
fn tick_refresh(app: &mut App) -> Result<()> {
    let data = compute_tick(
        app.dir.clone(),
        app.session.clone(),
        app.scope_global,
        app.window.clone(),
        app.tz_offset,
        app.rtk_base,
    );
    apply_tick(app, data);
    Ok(())
}

/// Split the frame into panel areas: header, stat strip, the tools table
/// (sized to its real row count, not a fixed floor), then in full view two
/// more rows of small boxes (sparklines, then session|value), a spacer, then
/// the footer. Every panel is sized to its own content; the spacer (not any
/// panel) absorbs leftover space on a tall terminal, so the footer settles at
/// the bottom like a status bar instead of any box stretching into a mostly
/// empty void.
#[cfg(feature = "tui")]
fn draw(f: &mut Frame, app: &App) {
    use ratatui::layout::{Constraint, Layout};
    let full = app.view == View::Full;
    let term_width = f.area().width;
    let frame_a_height = chrome::overview_tools_height(term_width, app);

    let mut rows = vec![
        // title + live dot + control strip: bare, above every framed panel.
        Constraint::Length(2),
        // overview + tools + info, one frame: sized to its content so it
        // never balloons into a mostly-empty box on a tall terminal.
        Constraint::Length(frame_a_height),
    ];
    if full {
        rows.push(Constraint::Length(9)); // sparklines row
        rows.push(Constraint::Length(chrome::session_value_height(term_width, app))); // session | value, one frame
    } else {
        rows.push(Constraint::Length(6)); // mini: sparklines only
    }
    rows.push(Constraint::Min(0)); // spacer: absorbs leftover space, not any panel
    rows.push(Constraint::Length(1)); // footer
    let areas = Layout::vertical(rows).split(f.area());

    header::header_top(f, areas[0], app);
    chrome::frame_overview_tools(f, areas[1], app);
    charts::charts_row(f, areas[2], app);

    if full {
        chrome::frame_session_value(f, areas[3], app);
    }

    misc::footer(f, areas[areas.len() - 1], app);
}

// ---------------------------------------------------------------------------
// Snapshot-prep + terminal-size helpers shared by run()'s ticks (T2 reuses)
// ---------------------------------------------------------------------------

/// Rewrite the snapshot's `rtk` totals to the delta since the first tick (the
/// `base`), matching the web dashboard's first-poll baseline. No-op when RTK is not
/// installed. `avg_savings_pct` is recomputed from the windowed input/saved delta.
#[cfg(feature = "tui")]
fn rebase_rtk(snap: &mut Value, base: &mut Option<(i64, i64, i64)>) {
    if snap["rtk"]["installed"].as_bool() != Some(true) {
        return;
    }
    let cur = (
        snap["rtk"]["total_commands"].as_i64().unwrap_or(0),
        snap["rtk"]["total_saved"].as_i64().unwrap_or(0),
        snap["rtk"]["total_input"].as_i64().unwrap_or(0),
    );
    let (bc, bs, bi) = *base.get_or_insert(cur);
    let d_saved = (cur.1 - bs).max(0);
    let d_input = (cur.2 - bi).max(0);
    let pct = if d_input > 0 {
        d_saved as f64 / d_input as f64 * 100.0
    } else {
        0.0
    };
    snap["rtk"]["total_commands"] = Value::from((cur.0 - bc).max(0));
    snap["rtk"]["total_saved"] = Value::from(d_saved);
    snap["rtk"]["avg_savings_pct"] = Value::from(pct);
}

/// Terminal width in columns: `stty size` ("rows cols"), else `$COLUMNS`, else 80.
#[cfg(feature = "tui")]
fn term_cols() -> u16 {
    if let Ok(out) = std::process::Command::new("stty")
        .arg("size")
        .stdin(std::process::Stdio::inherit())
        .output()
    {
        if let Ok(s) = String::from_utf8(out.stdout) {
            if let Some(cols) = s
                .split_whitespace()
                .nth(1)
                .and_then(|c| c.parse::<u16>().ok())
            {
                if cols > 0 {
                    return cols;
                }
            }
        }
    }
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse::<u16>().ok())
        .filter(|&c| c > 0)
        .unwrap_or(80)
}

/// Terminal height in rows: `stty size` ("rows cols"), else `$LINES`, else `None`
/// (stay unclipped) when the height genuinely can't be determined — e.g. output
/// isn't a real tty, where clipping would silently drop captured content.
#[cfg(feature = "tui")]
fn term_rows() -> Option<u16> {
    if let Ok(out) = std::process::Command::new("stty")
        .arg("size")
        .stdin(std::process::Stdio::inherit())
        .output()
    {
        if let Ok(s) = String::from_utf8(out.stdout) {
            if let Some(rows) = s
                .split_whitespace()
                .next()
                .and_then(|r| r.parse::<u16>().ok())
            {
                if rows > 0 {
                    return Some(rows);
                }
            }
        }
    }
    std::env::var("LINES")
        .ok()
        .and_then(|r| r.parse::<u16>().ok())
        .filter(|&r| r > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_parse_and_resolve() {
        assert!(matches!(Window::parse("all"), Some(Window::All)));
        assert!(matches!(Window::parse("today"), Some(Window::Today)));
        assert!(matches!(Window::parse("1h"), Some(Window::Last(3600))));
        assert!(matches!(Window::parse("90m"), Some(Window::Last(5400))));
        assert!(matches!(Window::parse("2d"), Some(Window::Last(172_800))));
        assert!(Window::parse("bogus").is_none());
        assert!(Window::parse("h").is_none());

        let now = 1_000_000_000;
        assert_eq!(Window::All.since(now, 0), None);
        assert_eq!(Window::Last(3600).since(now, 0), Some(now - 3600));
        // today at UTC: midnight = now − (now mod 86400), always within the last day.
        let mid = Window::Today.since(now, 0).unwrap();
        assert_eq!(mid, now - now.rem_euclid(86_400));
        assert!(now - mid < 86_400);
        // a positive TZ offset moves the local-midnight boundary.
        assert_ne!(Window::Today.since(now, 3600), Window::Today.since(now, 0));

        assert_eq!(Window::Today.label(), "today");
        assert_eq!(Window::Last(3600).label(), "last 1h");
        assert_eq!(Window::Last(5400).label(), "last 90m");
        assert_eq!(Window::All.label(), "all time");
    }

    #[test]
    fn theme_kind_parses_both_palettes() {
        // The ThemeKind half of the old `theme_switches_palette` test; the palette
        // (color) assertions live in `theme::tests` now that SGR codes are gone.
        assert!(matches!(ThemeKind::parse("dark"), Some(ThemeKind::Dark)));
        assert!(matches!(
            ThemeKind::parse("70s"),
            Some(ThemeKind::Seventies)
        ));
        assert!(matches!(
            ThemeKind::parse("SEVENTIES"),
            Some(ThemeKind::Seventies)
        ));
        assert!(ThemeKind::parse("blue").is_none());
    }

    /// A fully-populated [`App`] for the pure `on_key` transition test — no
    /// terminal and no snapshot IO required.
    #[cfg(feature = "tui")]
    fn key_test_app() -> App {
        App {
            snapshot: Value::Null,
            theme: ThemeKind::Dark,
            palette: Palette::from(ThemeKind::Dark),
            view: View::Full,
            rate_mode: RateMode::Actual,
            rate: 3.0,
            rt_seconds: 4.0,
            window: Window::Last(60),
            scope_global: true,
            scope_label: "global".to_string(),
            projects: vec!["repo".to_string()],
            scope_idx: 0,
            tool_sel: 0,
            saved_series: Vec::new(),
            bytes_series: Vec::new(),
            event_series: Vec::new(),
            dir: PathBuf::from("/tmp/lens-tui-test/.lens"),
            session: None,
            rtk_base: None,
            tz_offset: 0,
            interval: 2,
        }
    }

    #[cfg(feature = "tui")]
    fn press(code: KeyCode) -> event::KeyEvent {
        event::KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// `on_key` is pure state, so every documented transition is assertable
    /// without a terminal. Covers the T2 predicate keys (t/v/w/r/q) plus Ctrl-C,
    /// scope, and the tool selector.
    #[cfg(feature = "tui")]
    #[test]
    fn on_key_transitions() {
        let mut app = key_test_app();

        // `t` flips theme Dark<->Seventies and syncs the palette.
        assert!(!on_key(&mut app, press(KeyCode::Char('t'))));
        assert!(matches!(app.theme, ThemeKind::Seventies));
        assert_eq!(
            app.palette.accent,
            Palette::from(ThemeKind::Seventies).accent
        );
        assert!(!on_key(&mut app, press(KeyCode::Char('t'))));
        assert!(matches!(app.theme, ThemeKind::Dark));

        // `v` flips view Full<->Mini.
        assert!(matches!(app.view, View::Full));
        assert!(!on_key(&mut app, press(KeyCode::Char('v'))));
        assert!(matches!(app.view, View::Mini));

        // `w` advances the window preset ring (Window isn't PartialEq, so compare
        // labels): live → 15m.
        assert_eq!(app.window.label(), "last 1m");
        assert!(!on_key(&mut app, press(KeyCode::Char('w'))));
        assert_eq!(app.window.label(), "last 15m");

        // `r` advances the rate mode and reprices off the shared model table.
        assert!(matches!(app.rate_mode, RateMode::Actual));
        assert!(!on_key(&mut app, press(KeyCode::Char('r'))));
        assert!(matches!(app.rate_mode, RateMode::Fable));
        let fable_input = crate::obs::pricing::price_for("fable").input;
        assert!((app.rate - fable_input).abs() < f64::EPSILON);

        // Down/`j` move the selection toward higher indices; Up/`k` move it back
        // toward 0, saturating there instead of wrapping/going negative.
        assert!(!on_key(&mut app, press(KeyCode::Down)));
        assert_eq!(app.tool_sel, 1);
        assert!(!on_key(&mut app, press(KeyCode::Char('j'))));
        assert_eq!(app.tool_sel, 2);
        assert!(!on_key(&mut app, press(KeyCode::Up)));
        assert_eq!(app.tool_sel, 1);
        assert!(!on_key(&mut app, press(KeyCode::Char('k'))));
        assert!(!on_key(&mut app, press(KeyCode::Char('k'))));
        assert_eq!(app.tool_sel, 0);

        // `s` toggles global<->repo (idx 0 <-> 1) and relabels.
        assert!(app.scope_global);
        assert!(!on_key(&mut app, press(KeyCode::Char('s'))));
        assert!(!app.scope_global);
        assert_eq!(app.scope_label, "repo");

        // `q` and Ctrl-C both quit.
        assert!(on_key(&mut app, press(KeyCode::Char('q'))));
        assert!(on_key(
            &mut app,
            event::KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
        ));
    }

    /// A snapshot from real op-log activity (3 canonical tools fire), so the
    /// full-frame smoke test below exercises every panel with realistic data
    /// instead of an empty snapshot. Ports `seeded_snap` from the old renderer
    /// (`git show master:src/obs/tui.rs`). `snapshot_json` also pulls
    /// `actual_usage` from real Claude Code transcripts (`claude_config_dirs()`
    /// unions `$CLAUDE_CONFIG_DIR`/`$XDG_CONFIG_HOME/claude`/`~/.claude`), so all
    /// three must point at an empty temp dir or this scans the developer's real
    /// session history — same isolation idiom as `usage.rs`'s `with_fixture`.
    #[cfg(feature = "tui")]
    fn seeded_snap() -> Value {
        let _g = crate::rtk::env_test_lock();
        let prev_cfg = std::env::var_os("CLAUDE_CONFIG_DIR");
        let prev_home = std::env::var_os("HOME");
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let empty = tempfile::tempdir().unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", empty.path());
        std::env::set_var("HOME", empty.path());
        std::env::set_var("XDG_CONFIG_HOME", empty.path());

        let dir = tempfile::tempdir().unwrap();
        let log = crate::obs::OpLog::open(dir.path());
        log.start("lens_run", serde_json::json!({})).finish(
            8000,
            100,
            Some("a".into()),
            "ok",
            "",
            None,
        );
        log.start("lens_search", serde_json::json!({}))
            .finish(50, 50, None, "ok", "", None);
        log.start("lens_symbol", serde_json::json!({}))
            .finish(40, 40, None, "ok", "", None);
        let snap = crate::obs::stats::snapshot_json(dir.path(), None);

        match prev_cfg {
            Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }
        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match prev_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        snap
    }

    /// A fully-populated [`App`] wrapping [`seeded_snap`] for the full-frame
    /// smoke test, at whichever `view` the test wants to exercise.
    #[cfg(feature = "tui")]
    fn smoke_app(view: View) -> App {
        App {
            snapshot: seeded_snap(),
            theme: ThemeKind::Dark,
            palette: Palette::from(ThemeKind::Dark),
            view,
            rate_mode: RateMode::Actual,
            rate: 5.0,
            rt_seconds: 4.0,
            window: Window::All,
            scope_global: true,
            scope_label: "global".to_string(),
            projects: vec!["repo".to_string()],
            scope_idx: 0,
            tool_sel: 0,
            saved_series: Vec::new(),
            bytes_series: Vec::new(),
            event_series: Vec::new(),
            dir: PathBuf::from("/tmp/lens-tui-smoke/.lens"),
            session: None,
            rtk_base: None,
            tz_offset: 0,
            interval: 2,
        }
    }

    /// Render `app` through the real [`draw`] entry point at `(width, height)`
    /// and flatten the `TestBackend` buffer into one string — enough for
    /// `contains` assertions; not line-broken.
    #[cfg(feature = "tui")]
    fn render_full(app: &App, width: u16, height: u16) -> String {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    /// Every panel wires into `draw` without panicking: a 120×50 Full-view frame
    /// shows representative content from the header (`saved`), the tools table
    /// (`lens_run`), and the footer (`store`). The same snapshot also renders at
    /// a cramped 80×24, Full and Mini — small terminals degrade, they don't crash.
    #[cfg(feature = "tui")]
    #[test]
    fn full_frame_renders_every_panel() {
        let full = smoke_app(View::Full);
        let frame = render_full(&full, 120, 50);
        assert!(frame.contains("saved"), "header money headline");
        assert!(frame.contains("lens_run"), "canonical tool in tools table");
        assert!(frame.contains("store"), "footer store figure");

        let _ = render_full(&full, 80, 24);
        let _ = render_full(&smoke_app(View::Mini), 80, 24);
    }
}
