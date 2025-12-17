//! Mouse scrolling demo with a scrollable buffer.
//!
//! This example makes it easy to compare how terminals emit mouse wheel events by
//! showing a large wrapped buffer, counting raw scroll events, and surfacing the
//! current scroll position in a fixed status bar. The intent is diagnostic rather
//! than polished UI, so the behavior stays close to the raw event stream.
//!
//! cargo run --example mouse-scroll

use std::borrow::Cow;
use std::collections::VecDeque;
use std::io;

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
    MouseEvent, MouseEventKind,
};
use crossterm::style::{Attribute, Print, SetAttribute};
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
    mouse_log: VecDeque<String>,
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

impl App {
    /// Create a new app from a static text buffer.
    ///
    /// The input text is wrapped to the current terminal width immediately so the scroll
    /// offsets match the visible rows. The initial scroll position starts at the top with
    /// zeroed event counts.
    fn new(text: &'static str) -> io::Result<Self> {
        let (width, height) = terminal::size()?;
        let layout = Self::layout_for(width);
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
    /// Other keys are ignored so terminal repeat behavior can be observed cleanly.
    fn handle_key(&mut self, key: KeyEvent) -> AppAction {
        match key.code {
            KeyCode::Esc => AppAction::Quit,
            KeyCode::Char('q') => AppAction::Quit,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => AppAction::Quit,
            KeyCode::Up => {
                self.scroll_up();
                AppAction::Continue
            }
            KeyCode::Down => {
                self.scroll_down();
                AppAction::Continue
            }
            KeyCode::Char('1') => {
                self.set_scroll_step(ScrollStep::One);
                AppAction::Continue
            }
            KeyCode::Char('3') => {
                self.set_scroll_step(ScrollStep::Three);
                AppAction::Continue
            }
            _ => AppAction::Continue,
        }
    }

    /// Handle a mouse event and return the next action.
    ///
    /// Only wheel events are counted and applied so raw scroll behavior is observable
    /// without extra acceleration or smoothing logic.
    fn handle_mouse(&mut self, mouse: MouseEvent) -> AppAction {
        self.record_mouse_event(&mouse);
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.scroll_up_events += 1;
                self.scroll_up();
            }
            MouseEventKind::ScrollDown => {
                self.scroll_down_events += 1;
                self.scroll_down();
            }
            _ => {}
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
        let layout = Self::layout_for(width);
        self.lines = wrap(self.text, layout.content_width);
        self.clamp_scroll();
    }

    /// Scroll down by one line if the buffer allows.
    ///
    /// The maximum scroll position is derived from the wrapped line count.
    fn scroll_down(&mut self) {
        let step = self.scroll_step_lines();
        self.scroll_offset = self.scroll_offset.saturating_add(step).min(self.max_scroll());
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
    /// The log pane reserves a vertical separator and uses the first row as a header,
    /// with the remaining rows showing the most recent mouse events.
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

        let mut header = format!("Mouse events ({})", self.mouse_log.len());
        header.truncate(layout.log_width);
        queue!(stdout, MoveTo(log_x, 0), Print(header))?;

        let log_rows = view_height.saturating_sub(1);
        for (row, entry) in self.mouse_log.iter().take(log_rows).enumerate() {
            let mut line = entry.clone();
            line.truncate(layout.log_width);
            queue!(stdout, MoveTo(log_x, (row + 1) as u16), Print(line))?;
        }

        Ok(())
    }

    /// Record a mouse event in the log, keeping only the most recent entries.
    ///
    /// The stored strings are intentionally compact to maximize the visible history.
    fn record_mouse_event(&mut self, mouse: &MouseEvent) {
        if matches!(mouse.kind, MouseEventKind::Moved) {
            return;
        }
        let entry = format!(
            "{:?} ({}, {}) {:?}",
            mouse.kind, mouse.column, mouse.row, mouse.modifiers
        );
        self.mouse_log.push_front(entry);
        if self.mouse_log.len() > MAX_LOG_EVENTS {
            self.mouse_log.pop_back();
        }
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

        let status = format!(
            "Lines {}-{} / {} | Scroll {}% | Step {} | Wheel up {} down {} | q/Esc to quit",
            start,
            end,
            total,
            percent,
            self.scroll_step_label(),
            self.scroll_up_events,
            self.scroll_down_events
        );
        status
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
        Self::layout_for(self.width)
    }

    /// Compute the layout for a specific terminal width.
    ///
    /// This keeps a minimum content width and caps the log pane so it doesn't dominate
    /// narrow terminals.
    fn layout_for(width: u16) -> Layout {
        let total_width = width.max(1) as usize;
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
