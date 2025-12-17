//! Mouse scrolling demo with a scrollable buffer.
//!
//! This example makes it easy to compare how terminals emit mouse wheel events by
//! showing a large wrapped buffer, counting raw scroll events, and surfacing the
//! current scroll position in a fixed status bar. The intent is diagnostic rather
//! than polished UI, so the behavior stays close to the raw event stream.
//!
//! Keys: `q`/`Esc` quits, `1`/`3` change scroll step, `a` toggles auto/manual timeout,
//! `[`/`]` adjust manual timeout, `t` toggles content (lipsum/design/source), `d` toggles
//! the debug pane, `r` resets counters and calibration, arrows scroll line-by-line.
//!
//! cargo run --example mouse-scroll

use std::borrow::Cow;
use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use futures::StreamExt;
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent, KeyModifiers,
    MouseEvent, MouseEventKind,
};
use crossterm::style::{
    Attribute, Color, Print, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::terminal::{
    self, disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use crossterm::{execute, queue, SynchronizedUpdate};
use textwrap::wrap;
use tokio::time::{interval, MissedTickBehavior};

const LIPSUM: &str = include_str!("mouse-scroll-lipsum.txt");
const SOURCE_CODE: &str = include_str!("mouse-scroll.rs");
const DESIGN_DOC: &str = include_str!("mouse-scroll-plan.md");
const CONTENT_MIN_WIDTH: usize = 20;
const LOG_MIN_WIDTH: usize = 26;
const LOG_MAX_WIDTH: usize = 46;
const LOG_GAP: usize = 1;
const MAX_LOG_EVENTS: usize = 200;
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const DEFAULT_BURST_TIMEOUT: Duration = Duration::from_millis(120);
const TIMEOUT_STEP: Duration = Duration::from_millis(10);
const GAP_SAMPLE_LIMIT: usize = 80;
const WHEEL_GAP_MAX: Duration = Duration::from_millis(5);
const WHEEL_MAX_DURATION: Duration = Duration::from_millis(150);
const WHEEL_MIN_COUNT: u32 = 4;
const WHEEL_MAX_COUNT: u32 = 20;
const TRACKPAD_GAP_MIN: Duration = Duration::from_millis(20);
const TRACKPAD_MIN_DURATION: Duration = Duration::from_millis(300);
const TRACKPAD_MIN_COUNT: u32 = 20;
const HELP_LINES: [&str; 8] = [
    "  q/Esc  quit",
    "  1/3    step",
    "  a      auto timeout",
    "  [/]    timeout -/+",
    "  t      content",
    "  d      debug",
    "  r      reset",
    "  arrows scroll",
];
const EXPLAIN_LINES: [&str; 7] = [
    "  Δt   gap from previous event",
    "  t    time since burst start",
    "  Active   current burst stats",
    "  Last     last closed burst",
    "  Gap      time between bursts",
    "  Input    wheel/trackpad guess",
    "  Burst    summary at close",
];
const LABEL_WIDTH: usize = 9;

#[tokio::main]
async fn main() -> io::Result<()> {
    enable_raw_mode()?;

    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture, Hide)?;

    let result = match App::new(LIPSUM) {
        Ok(mut app) => app.run(&mut stdout).await,
        Err(error) => Err(error),
    };

    execute!(stdout, Show, DisableMouseCapture, LeaveAlternateScreen)?;
    disable_raw_mode()?;

    if let Err(error) = result {
        eprintln!("Error: {error}");
    }

    Ok(())
}

struct App {
    text: &'static str,
    lines: Vec<Cow<'static, str>>,
    scroll_offset: usize,
    scroll_up_events: u64,
    scroll_down_events: u64,
    scroll_step: ScrollStep,
    mouse_log: VecDeque<LogEntry>,
    show_debug: bool,
    content_source: ContentSource,
    burst: Option<BurstState>,
    last_burst: Option<BurstSummary>,
    last_burst_end: Option<Instant>,
    last_burst_gap: Option<Duration>,
    burst_samples: u32,
    burst_events_total: u32,
    gap_samples: VecDeque<Duration>,
    timeout_mode: TimeoutMode,
    manual_timeout: Duration,
    next_burst_color: Color,
    width: u16,
    height: u16,
    terminal_info: TerminalInfo,
}

#[derive(Copy, Clone, Debug)]
struct Layout {
    content_width: usize,
    log_width: usize,
}

enum AppAction {
    Continue,
    Quit,
}

#[derive(Copy, Clone, Debug)]
enum ScrollStep {
    One,
    Three,
}

#[derive(Copy, Clone, Debug)]
enum ContentSource {
    Lipsum,
    DesignDoc,
    SourceCode,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum ScrollDirection {
    Up,
    Down,
}

#[derive(Copy, Clone, Debug)]
enum InputGuess {
    Wheel,
    Trackpad,
    Unknown,
}

#[derive(Clone, Debug)]
struct LogEntry {
    text: String,
    color: Option<Color>,
}

#[derive(Clone, Debug)]
struct BurstState {
    start: Instant,
    last: Instant,
    count: u32,
    direction: ScrollDirection,
    sum_delta: Duration,
    gap_from_prev: Option<Duration>,
    color: Color,
}

#[derive(Clone, Debug)]
struct BurstSummary {
    direction: ScrollDirection,
    count: u32,
    duration: Duration,
    avg_delta: Option<Duration>,
    gap_from_prev: Option<Duration>,
}

#[derive(Copy, Clone, Debug)]
enum TimeoutMode {
    Auto,
    Manual,
}

#[derive(Copy, Clone, Debug)]
enum Multiplexer {
    None,
    Tmux,
    Zellij,
}

#[derive(Clone, Debug)]
struct TerminalInfo {
    term: Option<String>,
    term_program: Option<String>,
    term_program_version: Option<String>,
    term_emulator: Option<String>,
    colorterm: Option<String>,
    tmux: Option<String>,
    tmux_term: Option<String>,
    zellij: Option<String>,
    zellij_session: Option<String>,
    multiplexer: Multiplexer,
}

impl App {
    /// Create a new app from a static text buffer.
    ///
    /// The input text is wrapped to the current terminal width immediately so the scroll
    /// offsets match the visible rows. The initial scroll position starts at the top with
    /// zeroed event counts.
    fn new(text: &'static str) -> io::Result<Self> {
        let (width, height) = terminal::size()?;
        let layout = Self::layout_for(width, true);
        let lines = wrap(text, layout.content_width);
        let mouse_log = VecDeque::with_capacity(MAX_LOG_EVENTS);
        Ok(Self {
            text,
            lines,
            scroll_offset: 0,
            scroll_up_events: 0,
            scroll_down_events: 0,
            scroll_step: ScrollStep::One,
            mouse_log,
            show_debug: true,
            content_source: ContentSource::Lipsum,
            burst: None,
            last_burst: None,
            last_burst_end: None,
            last_burst_gap: None,
            burst_samples: 0,
            burst_events_total: 0,
            gap_samples: VecDeque::with_capacity(GAP_SAMPLE_LIMIT),
            timeout_mode: TimeoutMode::Auto,
            manual_timeout: DEFAULT_BURST_TIMEOUT,
            next_burst_color: Color::Blue,
            width,
            height,
            terminal_info: TerminalInfo::detect(),
        })
    }

    /// Drive the main event loop until a quit action occurs.
    ///
    /// This runs a 60fps tick for rendering and listens for terminal events via
    /// `EventStream`. Rendering happens only when needed or while a burst is active.
    async fn run(&mut self, stdout: &mut io::Stdout) -> io::Result<()> {
        let mut events = EventStream::new();
        let mut ticker = interval(FRAME_INTERVAL);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut render_needed = true;

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let burst_active = self.update_burst_timeout();
                    if burst_active {
                        render_needed = true;
                    }
                    if render_needed {
                        self.render(stdout)?;
                        render_needed = false;
                    }
                }
                maybe_event = events.next() => {
                    let event = match maybe_event {
                        Some(Ok(event)) => event,
                        Some(Err(error)) => return Err(error),
                        None => return Ok(()),
                    };
                    let action = match event {
                        Event::Key(key) => self.handle_key(key),
                        Event::Mouse(mouse) => self.handle_mouse(mouse),
                        Event::Resize(width, height) => self.handle_resize(width, height),
                        _ => self.handle_other(),
                    };

                    if matches!(action, AppAction::Quit) {
                        return Ok(());
                    }

                    render_needed = true;
                }
            }
        }
    }

    /// Handle a keyboard event and report whether the app should exit.
    ///
    /// Arrow keys scroll line-by-line, while `q`, `Esc`, and `Ctrl+C` terminate the loop.
    /// Use `1` or `3` to change the scroll step, `a` to toggle auto/manual timeout,
    /// `[`/`]` to adjust the manual timeout, `t` to toggle the content source, `d` to
    /// toggle the debug pane, and `r` to reset scroll counters plus calibration state.
    fn handle_key(&mut self, key: KeyEvent) -> AppAction {
        match key.code {
            KeyCode::Esc => return AppAction::Quit,
            KeyCode::Char('q') => return AppAction::Quit,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return AppAction::Quit
            }
            _ => {}
        }

        match key.code {
            KeyCode::Up => self.scroll_up(),
            KeyCode::Down => self.scroll_down(),
            KeyCode::Char('1') => self.set_scroll_step(ScrollStep::One),
            KeyCode::Char('3') => self.set_scroll_step(ScrollStep::Three),
            KeyCode::Char('a') => self.toggle_timeout_mode(),
            KeyCode::Char('[') => self.adjust_manual_timeout(false),
            KeyCode::Char(']') => self.adjust_manual_timeout(true),
            KeyCode::Char('t') => self.toggle_content_source(),
            KeyCode::Char('d') => self.toggle_debug(),
            KeyCode::Char('r') => self.reset_stats(),
            _ => {}
        }

        AppAction::Continue
    }

    /// Handle a mouse event and return the next action.
    ///
    /// Only wheel events are counted and applied so raw scroll behavior is observable
    /// without extra acceleration or smoothing logic.
    fn handle_mouse(&mut self, mouse: MouseEvent) -> AppAction {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.handle_scroll_event(ScrollDirection::Up, &mouse);
                self.scroll_up_events += 1;
                self.scroll_up();
            }
            MouseEventKind::ScrollDown => {
                self.handle_scroll_event(ScrollDirection::Down, &mouse);
                self.scroll_down_events += 1;
                self.scroll_down();
            }
            _ => self.log_mouse_event(&mouse),
        }

        AppAction::Continue
    }

    /// Handle a scroll event and update burst timing state.
    ///
    /// A new burst starts on the first event or when direction changes, and an existing
    /// burst closes if the inter-event gap exceeds the current timeout.
    fn handle_scroll_event(&mut self, direction: ScrollDirection, mouse: &MouseEvent) {
        let now = Instant::now();
        let timeout = self.effective_timeout();
        let mut start_new = false;

        if let Some(burst) = &self.burst {
            let gap = now.duration_since(burst.last);
            if gap > timeout || burst.direction != direction {
                self.finalize_burst();
                start_new = true;
            }
        } else {
            start_new = true;
        }

        if start_new {
            self.start_burst(direction, now);
        }

        if let Some(burst) = &mut self.burst {
            let (delta, elapsed, color, should_record_gap) = {
                let should_record_gap = burst.count > 0;
                let delta = if should_record_gap {
                    now.duration_since(burst.last)
                } else {
                    Duration::from_millis(0)
                };
                if should_record_gap {
                    burst.sum_delta = burst.sum_delta.saturating_add(delta);
                }
                burst.last = now;
                burst.count = burst.count.saturating_add(1);

                let elapsed = now.duration_since(burst.start);
                let color = burst.color;
                (delta, elapsed, color, should_record_gap)
            };

            if should_record_gap {
                self.record_gap_sample(delta);
            }
            self.log_scroll_event(direction, mouse, delta, elapsed, color);
        }
    }

    /// Handle a terminal resize event.
    ///
    /// This re-wraps the static text to the new width and clamps the scroll offset to
    /// avoid pointing past the end of the buffer.
    fn handle_resize(&mut self, width: u16, height: u16) -> AppAction {
        self.resize(width, height);
        AppAction::Continue
    }

    /// Handle events that the demo does not currently care about.
    ///
    /// This keeps the event loop explicit and makes it easy to add instrumentation later.
    fn handle_other(&mut self) -> AppAction {
        AppAction::Continue
    }

    /// Resize the app's layout and wrap the text to the new width.
    ///
    /// The wrap width is always at least one column to avoid zero-width wrapping.
    fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        self.rewrap();
    }

    /// Scroll down by one line if the buffer allows.
    ///
    /// The maximum scroll position is derived from the wrapped line count.
    fn scroll_down(&mut self) {
        let step = self.scroll_step_lines();
        self.scroll_offset = self
            .scroll_offset
            .saturating_add(step)
            .min(self.max_scroll());
    }

    /// Scroll up by one line, saturating at the top.
    ///
    /// This is intentionally symmetric with scroll down for diagnostics.
    fn scroll_up(&mut self) {
        let step = self.scroll_step_lines();
        self.scroll_offset = self.scroll_offset.saturating_sub(step);
    }

    /// Clamp the scroll offset to the last valid position.
    ///
    /// This is used after resize or wrap updates to keep the view stable.
    fn clamp_scroll(&mut self) {
        self.scroll_offset = self.scroll_offset.min(self.max_scroll());
    }

    /// Set the scroll step mode for wheel and arrow navigation.
    ///
    /// This affects only how many lines each discrete scroll action moves.
    fn set_scroll_step(&mut self, step: ScrollStep) {
        self.scroll_step = step;
    }

    /// Toggle between auto and manual burst timeout modes.
    fn toggle_timeout_mode(&mut self) {
        self.timeout_mode = match self.timeout_mode {
            TimeoutMode::Auto => TimeoutMode::Manual,
            TimeoutMode::Manual => TimeoutMode::Auto,
        };
    }

    /// Adjust the manual timeout up or down by a fixed step.
    fn adjust_manual_timeout(&mut self, increase: bool) {
        let current_ms = self.manual_timeout.as_millis();
        let step_ms = TIMEOUT_STEP.as_millis();
        let next_ms = if increase {
            current_ms.saturating_add(step_ms)
        } else {
            current_ms.saturating_sub(step_ms)
        };
        self.manual_timeout = Duration::from_millis(next_ms as u64);
    }

    /// Toggle whether the debug pane is shown and rewrap content to the new width.
    fn toggle_debug(&mut self) {
        self.show_debug = !self.show_debug;
        self.rewrap();
    }

    /// Toggle between lipsum content and the design doc.
    fn toggle_content_source(&mut self) {
        self.content_source = match self.content_source {
            ContentSource::Lipsum => ContentSource::DesignDoc,
            ContentSource::DesignDoc => ContentSource::SourceCode,
            ContentSource::SourceCode => ContentSource::Lipsum,
        };
        self.text = match self.content_source {
            ContentSource::Lipsum => LIPSUM,
            ContentSource::DesignDoc => DESIGN_DOC,
            ContentSource::SourceCode => SOURCE_CODE,
        };
        self.rewrap();
    }

    /// Clear scroll counters, mouse events, and calibration state.
    fn reset_stats(&mut self) {
        self.scroll_up_events = 0;
        self.scroll_down_events = 0;
        self.mouse_log.clear();
        self.reset_burst_state();
    }

    /// Reset burst timing and calibration state.
    fn reset_burst_state(&mut self) {
        self.burst = None;
        self.last_burst = None;
        self.last_burst_end = None;
        self.last_burst_gap = None;
        self.burst_samples = 0;
        self.burst_events_total = 0;
        self.gap_samples.clear();
        self.next_burst_color = Color::Blue;
    }

    /// Rewrap the text to the active content width.
    fn rewrap(&mut self) {
        let layout = Self::layout_for(self.width, self.show_debug);
        self.lines = wrap(self.text, layout.content_width);
        self.clamp_scroll();
    }

    /// Render the current view into the terminal.
    ///
    /// Uses a synchronized update to reduce tearing in terminals that support it, and
    /// clears the full screen each frame for deterministic output. When there is enough
    /// horizontal space, a log pane shows the most recent mouse events.
    fn render(&self, stdout: &mut io::Stdout) -> io::Result<()> {
        stdout.sync_update(|stdout| -> io::Result<()> {
            let width = self.width as usize;
            let view_height = self.view_height();
            let layout = self.layout();

            queue!(stdout, MoveTo(0, 0), Clear(ClearType::All))?;

            for (row, line) in self
                .lines
                .iter()
                .skip(self.scroll_offset)
                .take(view_height)
                .enumerate()
            {
                queue!(stdout, MoveTo(0, row as u16), Print(line))?;
            }

            if self.height > 0 {
                let mut status = self.status_line();
                status.truncate(width);
                if status.len() < width {
                    status.push_str(&" ".repeat(width - status.len()));
                }
                queue!(
                    stdout,
                    MoveTo(0, self.height.saturating_sub(1)),
                    SetAttribute(Attribute::Reverse),
                    Print(status),
                    SetAttribute(Attribute::Reset)
                )?;
            }

            if layout.log_width > 0 {
                self.render_log(stdout, layout, view_height)?;
            }

            Ok(())
        })??;

        Ok(())
    }

    /// Render the mouse log pane if space permits.
    ///
    /// The log pane reserves a vertical separator, uses the first rows for help, then
    /// header rows, a border line, and finally recent mouse events.
    fn render_log(
        &self,
        stdout: &mut io::Stdout,
        layout: Layout,
        view_height: usize,
    ) -> io::Result<()> {
        if view_height == 0 {
            return Ok(());
        }

        let separator_x = layout.content_width as u16;
        let log_x = separator_x + LOG_GAP as u16;

        for row in 0..view_height {
            queue!(stdout, MoveTo(separator_x, row as u16), Print("│"))?;
        }

        let mut row = 0;
        if row < view_height {
            render_title(stdout, log_x, row as u16, layout.log_width, "Keys")?;
            row += 1;
        }
        for line in HELP_LINES.iter().take(view_height.saturating_sub(row)) {
            let mut help = line.to_string();
            help.truncate(layout.log_width);
            queue!(stdout, MoveTo(log_x, row as u16), Print(help))?;
            row += 1;
        }

        if row < view_height {
            render_separator(stdout, separator_x, row as u16, layout.log_width)?;
            row += 1;
        }

        let now = Instant::now();
        if row < view_height {
            render_title(stdout, log_x, row as u16, layout.log_width, "Stats")?;
            row += 1;
        }

        for line in self
            .debug_lines(now)
            .into_iter()
            .take(view_height.saturating_sub(row))
        {
            let mut header = line;
            header.truncate(layout.log_width);
            queue!(stdout, MoveTo(log_x, row as u16), Print(header))?;
            row += 1;
        }

        if row < view_height {
            render_separator(stdout, separator_x, row as u16, layout.log_width)?;
            row += 1;
        }

        if row < view_height {
            render_title(stdout, log_x, row as u16, layout.log_width, "Legend")?;
            row += 1;
        }
        let explain_rows = EXPLAIN_LINES.len().min(view_height.saturating_sub(row));
        for (offset, line) in EXPLAIN_LINES.iter().take(explain_rows).enumerate() {
            let mut text = line.to_string();
            text.truncate(layout.log_width);
            queue!(
                stdout,
                MoveTo(log_x, (row + offset) as u16),
                Print(text)
            )?;
        }
        row += explain_rows;

        if row < view_height {
            render_separator(stdout, separator_x, row as u16, layout.log_width)?;
            row += 1;
        }

        if row < view_height {
            render_title(stdout, log_x, row as u16, layout.log_width, "Events")?;
            row += 1;
        }
        let log_rows = view_height.saturating_sub(row);
        for (offset, entry) in self.mouse_log.iter().take(log_rows).enumerate() {
            let mut line = entry.text.clone();
            line.truncate(layout.log_width);
            queue!(
                stdout,
                MoveTo(log_x, (row + offset) as u16),
                SetAttribute(Attribute::Reset)
            )?;
            if let Some(color) = entry.color {
                queue!(stdout, SetForegroundColor(color))?;
            }
            queue!(stdout, Print(line), SetAttribute(Attribute::Reset))?;
        }

        Ok(())
    }

    /// Build the debug pane header lines, including burst timing details.
    ///
    /// The header is split across multiple lines to keep the log pane readable.
    fn debug_lines(&self, now: Instant) -> Vec<String> {
        let mut lines = Vec::new();
        lines.push(format!(
            "{}{}",
            pad_label("Mouse events"),
            format!("{:>4}", self.mouse_log.len())
        ));
        if let Some(burst) = &self.burst {
            let elapsed = now.duration_since(burst.start);
            let avg_delta = average_duration(burst.sum_delta, burst.count.saturating_sub(1));
            lines.push(format!(
                "{}{} {} ev {} avgΔ {}",
                pad_label("Active"),
                direction_label(burst.direction),
                fmt_count(burst.count),
                fmt_duration(elapsed),
                fmt_duration_opt(avg_delta)
            ));
        } else {
            lines.push(format!("{}--", pad_label("Active")));
        }

        if let Some(last) = &self.last_burst {
            lines.push(format!(
                "{}{} {} ev {} avgΔ {}",
                pad_label("Last"),
                direction_label(last.direction),
                fmt_count(last.count),
                fmt_duration(last.duration),
                fmt_duration_opt(last.avg_delta)
            ));
        } else {
            lines.push(format!("{}--", pad_label("Last")));
        }

        if let Some(gap) = self.last_burst_gap {
            lines.push(format!("{}{}", pad_label("Gap"), fmt_duration(gap)));
        } else {
            lines.push(format!("{}--", pad_label("Gap")));
        }

        lines.push(format!(
            "{}A:{} L:{}",
            pad_label("Input"),
            self.active_input_guess(now),
            self.last_input_guess()
        ));
        lines.push(format!(
            "{}{}",
            pad_label("Cal"),
            self.calibration_label()
        ));
        lines.push(format!(
            "{}{}",
            pad_label("Source"),
            self.content_source_label()
        ));
        let env_vars = self.terminal_info.env_kv();
        if env_vars.is_empty() {
            lines.push(format!("{}--", pad_label("Env")));
        } else {
            for (index, (key, value)) in env_vars.into_iter().enumerate() {
                let label = if index == 0 { "Env" } else { "" };
                lines.push(format!("{}{}={}", pad_label(label), key, value));
            }
        }
        lines.push(format!(
            "{}{}",
            pad_label("Mux"),
            self.terminal_info.mux_label()
        ));
        lines.push(format!(
            "{}{}",
            pad_label("Guess"),
            self.terminal_info.guess_label()
        ));
        lines.extend(self.timeout_lines());
        lines
    }

    /// Summarize the active burst as a coarse input guess.
    ///
    /// This returns `--` when no burst is active.
    fn active_input_guess(&self, now: Instant) -> &'static str {
        let Some(burst) = &self.burst else {
            return "--";
        };
        let duration = now.duration_since(burst.start);
        let avg_delta = average_duration(burst.sum_delta, burst.count.saturating_sub(1));
        input_guess_label(guess_input_kind(burst.count, duration, avg_delta))
    }

    /// Summarize the most recent completed burst as a coarse input guess.
    ///
    /// This returns `--` until at least one burst has completed.
    fn last_input_guess(&self) -> &'static str {
        let Some(summary) = &self.last_burst else {
            return "--";
        };
        input_guess_label(guess_input_kind(
            summary.count,
            summary.duration,
            summary.avg_delta,
        ))
    }

    /// Record a non-scroll mouse event in the log.
    fn log_mouse_event(&mut self, mouse: &MouseEvent) {
        if matches!(mouse.kind, MouseEventKind::Moved) {
            return;
        }
        let text = format!(
            "  {:?} ({}, {}) {:?}",
            mouse.kind, mouse.column, mouse.row, mouse.modifiers
        );
        self.push_log(text, None);
    }

    /// Record a scroll event log line with timing details.
    fn log_scroll_event(
        &mut self,
        direction: ScrollDirection,
        mouse: &MouseEvent,
        delta: Duration,
        elapsed: Duration,
        color: Color,
    ) {
        let text = format!(
            "  {} Δ{} t={} ({}, {}) {:?}",
            direction_label(direction),
            fmt_duration(delta),
            fmt_duration(elapsed),
            mouse.column,
            mouse.row,
            mouse.modifiers
        );
        self.push_log(text, Some(color));
    }

    /// Update the active burst timeout and finalize it when it expires.
    ///
    /// Returns true when a burst is active or has just closed.
    fn update_burst_timeout(&mut self) -> bool {
        let now = Instant::now();
        let should_finalize = match self.burst.as_ref() {
            Some(burst) => now.duration_since(burst.last) > self.effective_timeout(),
            None => return false,
        };

        if should_finalize {
            self.finalize_burst();
        }

        true
    }

    /// Start a new burst for the given direction.
    fn start_burst(&mut self, direction: ScrollDirection, now: Instant) {
        let color = self.next_burst_color();
        let gap_from_prev = self
            .last_burst_end
            .map(|end| now.saturating_duration_since(end));
        self.last_burst_gap = gap_from_prev;
        self.burst = Some(BurstState {
            start: now,
            last: now,
            count: 0,
            direction,
            sum_delta: Duration::from_millis(0),
            gap_from_prev,
            color,
        });
    }

    /// Finalize the active burst into summary statistics.
    fn finalize_burst(&mut self) {
        let Some(burst) = self.burst.take() else {
            return;
        };

        let duration = burst.last.duration_since(burst.start);
        let avg_delta = average_duration(burst.sum_delta, burst.count.saturating_sub(1));
        let summary = BurstSummary {
            direction: burst.direction,
            count: burst.count,
            duration,
            avg_delta,
            gap_from_prev: burst.gap_from_prev,
        };
        self.last_burst = Some(summary.clone());
        self.last_burst_end = Some(burst.last);
        self.burst_samples = self.burst_samples.saturating_add(1);
        self.burst_events_total = self.burst_events_total.saturating_add(burst.count);
        self.log_burst_summary(&summary, burst.color);
    }

    /// Record a burst summary line in the log.
    fn log_burst_summary(&mut self, summary: &BurstSummary, color: Color) {
        let gap = summary
            .gap_from_prev
            .map(fmt_duration)
            .unwrap_or_else(|| "--".to_string());
        let text = format!(
            "Burst {} {} ev {} avgΔ {} gap {}",
            direction_label(summary.direction),
            summary.count,
            fmt_duration(summary.duration),
            fmt_duration_opt(summary.avg_delta),
            gap
        );
        self.push_log(text, Some(color));
    }

    /// Push a log entry and trim the log to the maximum size.
    fn push_log(&mut self, text: String, color: Option<Color>) {
        self.mouse_log.push_front(LogEntry { text, color });
        if self.mouse_log.len() > MAX_LOG_EVENTS {
            self.mouse_log.pop_back();
        }
    }

    /// Compute the effective timeout in the active mode.
    fn effective_timeout(&self) -> Duration {
        match self.timeout_mode {
            TimeoutMode::Manual => self.manual_timeout,
            TimeoutMode::Auto => self
                .auto_timeout_stats()
                .map(|(_, _, timeout)| timeout)
                .unwrap_or(self.manual_timeout),
        }
    }

    /// Compute auto-timeout stats based on median + MAD.
    fn auto_timeout_stats(&self) -> Option<(Duration, Duration, Duration)> {
        if self.gap_samples.len() < 4 {
            return None;
        }

        let mut gaps: Vec<u128> = self.gap_samples.iter().map(|gap| gap.as_micros()).collect();
        let median_value = median(&mut gaps);
        let mut deviations: Vec<u128> = gaps
            .iter()
            .map(|gap| gap.abs_diff(median_value))
            .collect();
        let mad = median(&mut deviations);
        let min_us = 10_000;
        let effective_us = median_value.saturating_add(mad.saturating_mul(3)).max(min_us);
        let median = Duration::from_micros(median_value as u64);
        let mad = Duration::from_micros(mad as u64);
        let timeout = Duration::from_micros(effective_us as u64);
        Some((median, mad, timeout))
    }

    /// Record an inter-event gap for auto-timeout estimation.
    fn record_gap_sample(&mut self, gap: Duration) {
        self.gap_samples.push_back(gap);
        if self.gap_samples.len() > GAP_SAMPLE_LIMIT {
            self.gap_samples.pop_front();
        }
    }

    /// Alternate the color used to mark burst log entries.
    fn next_burst_color(&mut self) -> Color {
        let current = self.next_burst_color;
        self.next_burst_color = match current {
            Color::Blue => Color::Cyan,
            _ => Color::Blue,
        };
        current
    }

    /// Build the status bar string for the bottom row.
    ///
    /// The line indices are 1-based to match common UI expectations. The percentage is
    /// computed from the max scroll range so it remains stable across different sizes.
    fn status_line(&self) -> String {
        let total = self.lines.len();
        let view_height = self.view_height();
        let start = if total == 0 {
            0
        } else {
            self.scroll_offset + 1
        };
        let end = if total == 0 {
            0
        } else {
            (self.scroll_offset + view_height).min(total)
        };
        let max_scroll = self.max_scroll();
        let percent = if max_scroll == 0 {
            0
        } else {
            self.scroll_offset * 100 / max_scroll
        };

        let debug_state = if self.show_debug { "on" } else { "off" };
        let status = format!(
            "Lines {}-{} / {} | Scroll {}% | Step {} | Cal {} | Wheel up {} down {} | Debug {} | q/Esc to quit",
            start,
            end,
            total,
            percent,
            self.scroll_step_label(),
            self.calibration_label(),
            self.scroll_up_events,
            self.scroll_down_events,
            debug_state
        );
        status
    }

    /// Return a human-friendly calibration label.
    fn calibration_label(&self) -> String {
        match self.calibrated_events_per_burst() {
            Some(value) => format!("{value:.1} ev/b"),
            None => "--".to_string(),
        }
    }

    /// Compute the average number of events per burst.
    fn calibrated_events_per_burst(&self) -> Option<f64> {
        if self.burst_samples == 0 {
            return None;
        }

        Some(self.burst_events_total as f64 / self.burst_samples as f64)
    }

    /// Build timeout lines describing the active mode.
    fn timeout_lines(&self) -> Vec<String> {
        match self.timeout_mode {
            TimeoutMode::Manual => vec![
                format!("{}manual", pad_label("Timeout")),
                format!("{}{}", pad_label(""), fmt_duration(self.manual_timeout)),
                format!("{}--", pad_label("")),
            ],
            TimeoutMode::Auto => {
                if let Some((median, mad, timeout)) = self.auto_timeout_stats() {
                    vec![
                        format!("{}auto", pad_label("Timeout")),
                        format!("{}{}", pad_label(""), fmt_duration(timeout)),
                        format!(
                            "{}med {} mad {}",
                            pad_label(""),
                            fmt_duration(median),
                            fmt_duration(mad)
                        ),
                    ]
                } else {
                    vec![
                        format!("{}auto", pad_label("Timeout")),
                        format!(
                            "{}{} (no data)",
                            pad_label(""),
                            fmt_duration(self.manual_timeout)
                        ),
                        format!("{}med -- mad --", pad_label("")),
                    ]
                }
            }
        }
    }

    /// Return the number of lines to move per scroll action.
    fn scroll_step_lines(&self) -> usize {
        match self.scroll_step {
            ScrollStep::One => 1,
            ScrollStep::Three => 3,
        }
    }

    /// Return the user-facing label for the current scroll step.
    fn scroll_step_label(&self) -> &'static str {
        match self.scroll_step {
            ScrollStep::One => "1",
            ScrollStep::Three => "3",
        }
    }

    /// Return the user-facing label for the current content source.
    fn content_source_label(&self) -> &'static str {
        match self.content_source {
            ContentSource::Lipsum => "lipsum",
            ContentSource::DesignDoc => "design",
            ContentSource::SourceCode => "source",
        }
    }

    /// Compute the current layout for the available terminal width.
    ///
    /// A log pane is only enabled when both the content and log areas meet their minimum
    /// widths. The content width always stays at least one column.
    fn layout(&self) -> Layout {
        Self::layout_for(self.width, self.show_debug)
    }

    /// Compute the layout for a specific terminal width.
    ///
    /// This keeps a minimum content width and caps the log pane so it doesn't dominate
    /// narrow terminals.
    fn layout_for(width: u16, show_debug: bool) -> Layout {
        let total_width = width.max(1) as usize;
        if !show_debug {
            return Layout {
                content_width: total_width,
                log_width: 0,
            };
        }
        let min_total = CONTENT_MIN_WIDTH + LOG_MIN_WIDTH + LOG_GAP;
        if total_width < min_total {
            return Layout {
                content_width: total_width,
                log_width: 0,
            };
        }

        let log_width = (total_width / 3).min(LOG_MAX_WIDTH).max(LOG_MIN_WIDTH);
        let content_width = total_width.saturating_sub(log_width + LOG_GAP);
        if content_width < CONTENT_MIN_WIDTH {
            Layout {
                content_width: total_width,
                log_width: 0,
            }
        } else {
            Layout {
                content_width,
                log_width,
            }
        }
    }

    /// Compute the maximum scroll offset for the current viewport.
    ///
    /// This returns zero when the content fits entirely in view.
    fn max_scroll(&self) -> usize {
        self.lines.len().saturating_sub(self.view_height())
    }

    /// Return the number of rows available for content (excluding the status bar).
    ///
    /// The status bar consumes the final row, so the view height is one less than the
    /// terminal height when possible.
    fn view_height(&self) -> usize {
        self.height.saturating_sub(1) as usize
    }
}

impl TerminalInfo {
    fn detect() -> Self {
        let term = std::env::var("TERM").ok();
        let term_program = std::env::var("TERM_PROGRAM").ok();
        let term_program_version = std::env::var("TERM_PROGRAM_VERSION").ok();
        let term_emulator = std::env::var("TERMINAL_EMULATOR").ok();
        let colorterm = std::env::var("COLORTERM").ok();
        let tmux = std::env::var("TMUX").ok();
        let tmux_term = std::env::var("TMUX_TERM").ok();
        let zellij = std::env::var("ZELLIJ").ok();
        let zellij_session = std::env::var("ZELLIJ_SESSION_NAME").ok();
        let multiplexer = Self::multiplexer_from_env(&tmux, &zellij);

        Self {
            term,
            term_program,
            term_program_version,
            term_emulator,
            colorterm,
            tmux,
            tmux_term,
            zellij,
            zellij_session,
            multiplexer,
        }
    }

    fn mux_label(&self) -> &'static str {
        match self.multiplexer {
            Multiplexer::None => "none",
            Multiplexer::Tmux => "tmux",
            Multiplexer::Zellij => "zellij",
        }
    }

    fn env_kv(&self) -> Vec<(&'static str, String)> {
        let mut vars = Vec::new();
        if let Some(value) = &self.term {
            vars.push(("TERM", value.clone()));
        }
        if let Some(value) = &self.term_program {
            vars.push(("TERM_PROGRAM", value.clone()));
        }
        if let Some(value) = &self.term_program_version {
            vars.push(("TERM_PROGRAM_VERSION", value.clone()));
        }
        if let Some(value) = &self.term_emulator {
            vars.push(("TERMINAL_EMULATOR", value.clone()));
        }
        if let Some(value) = &self.colorterm {
            vars.push(("COLORTERM", value.clone()));
        }
        if let Some(value) = &self.tmux {
            vars.push(("TMUX", value.clone()));
        }
        if let Some(value) = &self.tmux_term {
            vars.push(("TMUX_TERM", value.clone()));
        }
        if let Some(value) = &self.zellij {
            vars.push(("ZELLIJ", value.clone()));
        }
        if let Some(value) = &self.zellij_session {
            vars.push(("ZELLIJ_SESSION_NAME", value.clone()));
        }
        vars
    }

    fn guess_label(&self) -> String {
        self.guess_terminal()
            .unwrap_or_else(|| "--".to_string())
    }

    fn guess_terminal(&self) -> Option<String> {
        if let Some(program) = &self.term_program {
            match program.as_str() {
                "Apple_Terminal" => return Some("Terminal.app".to_string()),
                "iTerm.app" => return Some("iTerm2".to_string()),
                _ => {
                    let program_lower = program.to_ascii_lowercase();
                    if program_lower.contains("wezterm") {
                        return Some("WezTerm".to_string());
                    }
                    if program_lower.contains("ghostty") {
                        return Some("Ghostty".to_string());
                    }
                }
            }
        }

        if let Some(emulator) = &self.term_emulator {
            let emulator_lower = emulator.to_ascii_lowercase();
            if emulator_lower.contains("ghostty") {
                return Some("Ghostty".to_string());
            }
            if emulator_lower.contains("wezterm") {
                return Some("WezTerm".to_string());
            }
        }

        if let Some(term) = &self.term {
            let term_lower = term.to_ascii_lowercase();
            if term_lower.contains("xterm-kitty") || term_lower.contains("kitty") {
                return Some("kitty".to_string());
            }
            if term_lower.contains("alacritty") {
                return Some("Alacritty".to_string());
            }
            if term_lower.contains("wezterm") {
                return Some("WezTerm".to_string());
            }
            if term_lower.contains("ghostty") {
                return Some("Ghostty".to_string());
            }
        }

        None
    }

    fn multiplexer_from_env(tmux: &Option<String>, zellij: &Option<String>) -> Multiplexer {
        if zellij.is_some() {
            Multiplexer::Zellij
        } else if tmux.is_some() {
            Multiplexer::Tmux
        } else {
            Multiplexer::None
        }
    }
}

fn direction_label(direction: ScrollDirection) -> &'static str {
    match direction {
        ScrollDirection::Up => "↑",
        ScrollDirection::Down => "↓",
    }
}

fn fmt_duration(duration: Duration) -> String {
    format!("{:.3}ms", duration.as_secs_f64() * 1000.0)
}

fn fmt_duration_opt(duration: Option<Duration>) -> String {
    duration
        .map(fmt_duration)
        .unwrap_or_else(|| "--".to_string())
}

fn pad_label(label: &str) -> String {
    format!("{label:<LABEL_WIDTH$} ", label = label, LABEL_WIDTH = LABEL_WIDTH)
}

fn fmt_count(count: u32) -> String {
    format!("{count:>4}")
}

fn average_duration(total: Duration, samples: u32) -> Option<Duration> {
    if samples == 0 {
        return None;
    }

    let avg_us = total.as_micros() / samples as u128;
    Some(Duration::from_micros(avg_us as u64))
}

fn guess_input_kind(count: u32, duration: Duration, avg_delta: Option<Duration>) -> InputGuess {
    let Some(avg_delta) = avg_delta else {
        return InputGuess::Unknown;
    };

    let is_wheel = avg_delta <= WHEEL_GAP_MAX
        && count >= WHEEL_MIN_COUNT
        && count <= WHEEL_MAX_COUNT
        && duration <= WHEEL_MAX_DURATION;
    if is_wheel {
        return InputGuess::Wheel;
    }

    let is_trackpad = avg_delta >= TRACKPAD_GAP_MIN
        || duration >= TRACKPAD_MIN_DURATION
        || count >= TRACKPAD_MIN_COUNT;
    if is_trackpad {
        return InputGuess::Trackpad;
    }

    InputGuess::Unknown
}

fn input_guess_label(guess: InputGuess) -> &'static str {
    match guess {
        InputGuess::Wheel => "wheel",
        InputGuess::Trackpad => "trackpad",
        InputGuess::Unknown => "unknown",
    }
}

fn median(values: &mut [u128]) -> u128 {
    values.sort_unstable();
    let mid = values.len() / 2;
    if values.len() % 2 == 1 {
        values[mid]
    } else {
        (values[mid - 1] + values[mid]) / 2
    }
}

fn render_separator(
    stdout: &mut io::Stdout,
    separator_x: u16,
    row: u16,
    log_width: usize,
) -> io::Result<()> {
    let border_width = log_width + LOG_GAP;
    let dash_count = border_width.saturating_sub(1);
    queue!(
        stdout,
        MoveTo(separator_x, row),
        Print("├"),
        Print("─".repeat(dash_count))
    )?;
    Ok(())
}

fn render_title(
    stdout: &mut io::Stdout,
    log_x: u16,
    row: u16,
    log_width: usize,
    title: &str,
) -> io::Result<()> {
    let line = center_text(title, log_width);
    queue!(
        stdout,
        MoveTo(log_x, row),
        SetBackgroundColor(Color::DarkGrey),
        SetForegroundColor(Color::White),
        Print(line),
        SetAttribute(Attribute::Reset)
    )?;
    Ok(())
}

fn center_text(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }

    let padded = format!(" {text} ");
    let trimmed: String = padded.chars().take(width).collect();
    let text_len = trimmed.chars().count();
    if text_len >= width {
        return trimmed;
    }

    let left = (width - text_len) / 2;
    let right = width - text_len - left;
    let mut out = String::with_capacity(width);
    out.push_str(&" ".repeat(left));
    out.push_str(&trimmed);
    out.push_str(&" ".repeat(right));
    out
}
