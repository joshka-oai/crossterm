//! Mouse scrolling demo with a scrollable buffer.
//!
//! This example makes it easy to compare how terminals emit mouse wheel events by
//! showing a large wrapped buffer, counting raw scroll events, and surfacing the
//! current scroll position in a fixed status bar. The intent is diagnostic rather
//! than polished UI, so the behavior stays close to the raw event stream.
//!
//! Keys: `q`/`Esc` quits, `1`/`3` change scroll step, `d` toggles the debug pane,
//! `r` resets counters and calibration, arrows scroll line-by-line.
//!
//! cargo run --example mouse-scroll

use std::borrow::Cow;
use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
    MouseEvent, MouseEventKind,
};
use crossterm::style::{Attribute, Color, Print, SetAttribute, SetForegroundColor};
use crossterm::terminal::{
    self, disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use crossterm::{execute, queue, SynchronizedUpdate};
use textwrap::wrap;

const LIPSUM: &str = include_str!("mouse-scroll-lipsum.txt");
const CONTENT_MIN_WIDTH: usize = 20;
const LOG_MIN_WIDTH: usize = 24;
const LOG_MAX_WIDTH: usize = 40;
const LOG_GAP: usize = 1;
const MAX_LOG_EVENTS: usize = 200;
const WHEEL_BURST_TIMEOUT: Duration = Duration::from_millis(120);
const HELP_LINES: [&str; 5] = [
    "Keys:",
    "  q/Esc  quit",
    "  1/3    step",
    "  d      debug",
    "  r      reset",
];

fn main() -> io::Result<()> {
    enable_raw_mode()?;

    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture, Hide)?;

    let result = App::new(LIPSUM).and_then(|mut app| app.run(&mut stdout));

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
    wheel_last_event: Option<Instant>,
    wheel_current_burst: u32,
    wheel_last_burst: u32,
    wheel_burst_samples: u32,
    wheel_events_total: u32,
    wheel_burst_color: Color,
    width: u16,
    height: u16,
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

#[derive(Clone, Debug)]
struct LogEntry {
    text: String,
    color: Option<Color>,
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
            wheel_last_event: None,
            wheel_current_burst: 0,
            wheel_last_burst: 0,
            wheel_burst_samples: 0,
            wheel_events_total: 0,
            wheel_burst_color: Color::Blue,
            width,
            height,
        })
    }

    /// Drive the main event loop until a quit action occurs.
    ///
    /// This method renders on each iteration and blocks for input events. It exits on
    /// `q`, `Esc`, or `Ctrl+C`, and otherwise mutates internal state based on input.
    fn run(&mut self, stdout: &mut io::Stdout) -> io::Result<()> {
        loop {
            self.render(stdout)?;
            let action = match event::read()? {
                Event::Key(key) => self.handle_key(key),
                Event::Mouse(mouse) => self.handle_mouse(mouse),
                Event::Resize(width, height) => self.handle_resize(width, height),
                _ => self.handle_other(),
            };

            if matches!(action, AppAction::Quit) {
                return Ok(());
            }
        }
    }

    /// Handle a keyboard event and report whether the app should exit.
    ///
    /// Arrow keys scroll line-by-line, while `q`, `Esc`, and `Ctrl+C` terminate the loop.
    /// Use `1` or `3` to change the scroll step, `d` to toggle the debug pane, and `r`
    /// to reset scroll counters plus calibration state.
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
                self.record_scroll_event();
                self.record_mouse_event(&mouse, true);
                self.scroll_up_events += 1;
                self.scroll_up();
            }
            MouseEventKind::ScrollDown => {
                self.record_scroll_event();
                self.record_mouse_event(&mouse, true);
                self.scroll_down_events += 1;
                self.scroll_down();
            }
            _ => self.record_mouse_event(&mouse, false),
        }

        AppAction::Continue
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

    /// Toggle whether the debug pane is shown and rewrap content to the new width.
    fn toggle_debug(&mut self) {
        self.show_debug = !self.show_debug;
        self.rewrap();
    }

    /// Clear scroll counters, mouse events, and calibration state.
    fn reset_stats(&mut self) {
        self.scroll_up_events = 0;
        self.scroll_down_events = 0;
        self.mouse_log.clear();
        self.reset_calibration();
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
        for line in HELP_LINES.iter().take(view_height) {
            let mut help = line.to_string();
            help.truncate(layout.log_width);
            queue!(stdout, MoveTo(log_x, row as u16), Print(help))?;
            row += 1;
        }

        if row < view_height {
            row += 1;
        }

        for line in self
            .debug_lines()
            .into_iter()
            .take(view_height.saturating_sub(row))
        {
            let mut header = line;
            header.truncate(layout.log_width);
            queue!(stdout, MoveTo(log_x, row as u16), Print(header))?;
            row += 1;
        }

        if row < view_height {
            let border_width = layout.log_width + LOG_GAP;
            queue!(
                stdout,
                MoveTo(separator_x, row as u16),
                Print("├"),
                Print("─".repeat(border_width))
            )?;
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

    /// Build the debug pane header lines, including calibration hints.
    ///
    /// The header is split across multiple lines to keep the log pane readable.
    fn debug_lines(&self) -> Vec<String> {
        let last_burst = if self.wheel_last_burst == 0 {
            "--".to_string()
        } else {
            self.wheel_last_burst.to_string()
        };
        vec![
            format!("Mouse events ({})", self.mouse_log.len()),
            format!("Cal {}", self.calibration_label()),
            format!("Last {}", last_burst),
        ]
    }

    /// Record a mouse event in the log, keeping only the most recent entries.
    ///
    /// The stored strings are intentionally compact to maximize the visible history.
    fn record_mouse_event(&mut self, mouse: &MouseEvent, is_scroll: bool) {
        if matches!(mouse.kind, MouseEventKind::Moved) {
            return;
        }
        let entry = LogEntry {
            text: format!(
                "{:?} ({}, {}) {:?}",
                mouse.kind, mouse.column, mouse.row, mouse.modifiers
            ),
            color: if is_scroll {
                Some(self.wheel_burst_color)
            } else {
                None
            },
        };
        self.mouse_log.push_front(entry);
        if self.mouse_log.len() > MAX_LOG_EVENTS {
            self.mouse_log.pop_back();
        }
    }

    /// Record a scroll wheel event for calibration.
    ///
    /// Scroll events arriving close together are treated as a single wheel notch. When
    /// the gap exceeds the timeout, the previous burst is finalized and used for the
    /// moving average.
    fn record_scroll_event(&mut self) {
        let now = Instant::now();
        if let Some(last_event) = self.wheel_last_event {
            if now.duration_since(last_event) > WHEEL_BURST_TIMEOUT {
                self.finalize_wheel_burst();
            }
        }

        if self.wheel_current_burst == 0 {
            self.start_wheel_burst();
        }
        self.wheel_current_burst = self.wheel_current_burst.saturating_add(1);
        self.wheel_last_event = Some(now);
    }

    /// Finalize the current scroll burst into the calibration counters.
    fn finalize_wheel_burst(&mut self) {
        if self.wheel_current_burst == 0 {
            return;
        }

        self.wheel_last_burst = self.wheel_current_burst;
        self.wheel_burst_samples = self.wheel_burst_samples.saturating_add(1);
        self.wheel_events_total = self
            .wheel_events_total
            .saturating_add(self.wheel_current_burst);
        self.wheel_current_burst = 0;
    }

    /// Reset all calibration counters and pending burst state.
    fn reset_calibration(&mut self) {
        self.wheel_last_event = None;
        self.wheel_current_burst = 0;
        self.wheel_last_burst = 0;
        self.wheel_burst_samples = 0;
        self.wheel_events_total = 0;
        self.wheel_burst_color = Color::Blue;
    }

    /// Start a new wheel burst and toggle its visual marker.
    fn start_wheel_burst(&mut self) {
        self.wheel_burst_color = match self.wheel_burst_color {
            Color::Blue => Color::Cyan,
            _ => Color::Blue,
        };
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
        match self.calibrated_events_per_wheel() {
            Some(value) => format!("{value:.1} ev/w"),
            None => "--".to_string(),
        }
    }

    /// Compute the average number of events per wheel notch.
    fn calibrated_events_per_wheel(&self) -> Option<f64> {
        if self.wheel_burst_samples == 0 {
            return None;
        }

        Some(self.wheel_events_total as f64 / self.wheel_burst_samples as f64)
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
