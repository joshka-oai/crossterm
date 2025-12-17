//! Mouse scrolling demo with a scrollable buffer.
//!
//! cargo run --example mouse-scroll

use std::io::{self, Write};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseEventKind,
};
use crossterm::style::{Attribute, Print, SetAttribute};
use crossterm::terminal::{
    self, disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use crossterm::{execute, queue};
use textwrap::{wrap, Options};

const LIPSUM: &str = include_str!("mouse-scroll-lipsum.txt");

struct App {
    text: &'static str,
    lines: Vec<String>,
    scroll_offset: usize,
    scroll_up_events: u64,
    scroll_down_events: u64,
    width: u16,
    height: u16,
}

impl App {
    fn new(text: &'static str) -> io::Result<Self> {
        let (width, height) = terminal::size()?;
        let lines = wrap_paragraphs(text, width as usize);
        Ok(Self {
            text,
            lines,
            scroll_offset: 0,
            scroll_up_events: 0,
            scroll_down_events: 0,
            width,
            height,
        })
    }

    fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        self.lines = wrap_paragraphs(self.text, width as usize);
        self.clamp_scroll();
    }

    fn view_height(&self) -> usize {
        self.height.saturating_sub(1) as usize
    }

    fn max_scroll(&self) -> usize {
        self.lines.len().saturating_sub(self.view_height())
    }

    fn clamp_scroll(&mut self) {
        let max_scroll = self.max_scroll();
        if self.scroll_offset > max_scroll {
            self.scroll_offset = max_scroll;
        }
    }

    fn scroll_up(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(1);
    }

    fn scroll_down(&mut self) {
        let max_scroll = self.max_scroll();
        if self.scroll_offset < max_scroll {
            self.scroll_offset += 1;
        }
    }

    fn render(&self, stdout: &mut io::Stdout) -> io::Result<()> {
        let width = self.width as usize;
        let view_height = self.view_height();

        queue!(stdout, MoveTo(0, 0), Clear(ClearType::All))?;

        for row in 0..view_height {
            let line_index = self.scroll_offset + row;
            let line = if line_index < self.lines.len() {
                fit_to_width(&self.lines[line_index], width)
            } else {
                " ".repeat(width)
            };

            queue!(stdout, MoveTo(0, row as u16), Print(line))?;
        }

        if self.height > 0 {
            let status = self.status_line(width);
            queue!(
                stdout,
                MoveTo(0, self.height.saturating_sub(1)),
                SetAttribute(Attribute::Reverse),
                Print(status),
                SetAttribute(Attribute::Reset)
            )?;
        }

        stdout.flush()
    }

    fn status_line(&self, width: usize) -> String {
        if width == 0 {
            return String::new();
        }

        let total = self.lines.len();
        let view_height = self.view_height();
        let start = if total == 0 { 0 } else { self.scroll_offset + 1 };
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
            "Lines {}-{} / {} | Scroll {}% | Wheel up {} down {} | q/Esc to quit",
            start, end, total, percent, self.scroll_up_events, self.scroll_down_events
        );
        fit_to_width(&status, width)
    }
}

fn wrap_paragraphs(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let options = Options::new(width).break_words(false);

    let paragraphs: Vec<&str> = text.split("\n\n").collect();
    for (index, paragraph) in paragraphs.iter().enumerate() {
        if paragraph.trim().is_empty() {
            lines.push(String::new());
        } else {
            for line in wrap(paragraph, &options) {
                lines.push(line.into_owned());
            }
        }

        if index + 1 < paragraphs.len() {
            lines.push(String::new());
        }
    }

    if lines.is_empty() {
        lines.push(String::new());
    }

    lines
}

fn fit_to_width(line: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }

    if line.len() >= width {
        let mut truncated = line.to_string();
        truncated.truncate(width);
        truncated
    } else {
        let mut padded = String::with_capacity(width);
        padded.push_str(line);
        padded.push_str(&" ".repeat(width - line.len()));
        padded
    }
}

fn run_app(stdout: &mut io::Stdout, app: &mut App) -> io::Result<()> {
    loop {
        app.render(stdout)?;
        match event::read()? {
            Event::Key(key) if key.code == KeyCode::Esc => return Ok(()),
            Event::Key(key) if key.code == KeyCode::Char('q') => return Ok(()),
            Event::Key(key)
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                return Ok(())
            }
            Event::Key(key) if key.code == KeyCode::Up => app.scroll_up(),
            Event::Key(key) if key.code == KeyCode::Down => app.scroll_down(),
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => {
                    app.scroll_up_events += 1;
                    app.scroll_up();
                }
                MouseEventKind::ScrollDown => {
                    app.scroll_down_events += 1;
                    app.scroll_down();
                }
                _ => {}
            },
            Event::Resize(width, height) => app.resize(width, height),
            _ => {}
        }
    }
}

fn main() -> io::Result<()> {
    enable_raw_mode()?;

    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture, Hide)?;

    let result = (|| {
        let mut app = App::new(LIPSUM)?;
        run_app(&mut stdout, &mut app)
    })();

    execute!(stdout, Show, DisableMouseCapture, LeaveAlternateScreen)?;
    disable_raw_mode()?;

    if let Err(error) = result {
        eprintln!("Error: {error}");
    }

    Ok(())
}
