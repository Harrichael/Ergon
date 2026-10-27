//! `ergon work -i`: pick tasks from `ergon ls` and start one session on them.
//! Space checks a row and Enter starts; with nothing checked, Enter takes the
//! highlighted row. A refusal stays in the picker, shown under the list.

use std::fs::{File, OpenOptions};
use std::path::Path;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use rusqlite::Connection;

use crate::claude::{self, Launch};
use crate::derived::Derived;
use crate::project::Project;
use crate::task::Tasks;
use crate::view;

pub struct Picker {
    /// Each `ls` row's slug and text.
    rows: Vec<(String, String)>,
    checked: Vec<bool>,
    cursor: usize,
    pub error: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Stay,
    Quit,
    Start(Vec<String>),
}

impl Picker {
    pub fn new(tasks: &Tasks, derived: &Derived) -> Picker {
        let rows: Vec<(String, String)> = view::ls_rows(tasks, derived, Default::default())
            .into_iter()
            .map(|(id, row)| {
                let text = anstream::adapter::strip_str(&row).to_string();
                (tasks.get(id).slug.clone(), text)
            })
            .collect();
        Picker {
            checked: vec![false; rows.len()],
            rows,
            cursor: 0,
            error: None,
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> Action {
        let n = self.rows.len();
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::Quit,
            KeyCode::Esc | KeyCode::Char('q') => Action::Quit,
            KeyCode::Char('j') | KeyCode::Down if n > 0 => {
                self.cursor = (self.cursor + 1) % n;
                Action::Stay
            }
            KeyCode::Char('k') | KeyCode::Up if n > 0 => {
                self.cursor = self.cursor.checked_sub(1).unwrap_or(n - 1);
                Action::Stay
            }
            KeyCode::Char(' ') if n > 0 => {
                self.checked[self.cursor] = !self.checked[self.cursor];
                self.error = None;
                Action::Stay
            }
            KeyCode::Enter if n > 0 => {
                let mut slugs: Vec<String> = self
                    .rows
                    .iter()
                    .zip(&self.checked)
                    .filter(|(_, c)| **c)
                    .map(|((slug, _), _)| slug.clone())
                    .collect();
                if slugs.is_empty() {
                    slugs.push(self.rows[self.cursor].0.clone());
                }
                Action::Start(slugs)
            }
            _ => Action::Stay,
        }
    }

    fn draw(&self, f: &mut ratatui::Frame, title: &str) {
        let error_height = if self.error.is_some() { 3 } else { 0 };
        let [list, error, help] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(error_height),
            Constraint::Length(1),
        ])
        .areas(f.area());
        let items: Vec<ListItem> = if self.rows.is_empty() {
            vec![ListItem::new("(nothing is ready to start)")]
        } else {
            self.rows
                .iter()
                .zip(&self.checked)
                .map(|((_, row), c)| {
                    ListItem::new(format!("{} {row}", if *c { "[x]" } else { "[ ]" }))
                })
                .collect()
        };
        let mut state = ListState::default();
        if !self.rows.is_empty() {
            state.select(Some(self.cursor));
        }
        let widget = List::new(items)
            .block(Block::default().title(title).borders(Borders::ALL))
            .highlight_style(Style::default().bg(Color::DarkGray))
            .highlight_symbol("> ");
        f.render_stateful_widget(widget, list, &mut state);
        if let Some(e) = &self.error {
            f.render_widget(
                Paragraph::new(Span::styled(e.as_str(), Style::default().fg(Color::Red)))
                    .wrap(Wrap { trim: true }),
                error,
            );
        }
        f.render_widget(
            Paragraph::new(Span::styled(
                "Space: check   Enter: start (the highlighted task if none is checked)   Esc: cancel",
                Style::default().fg(Color::DarkGray),
            )),
            help,
        );
    }
}

/// Runs the picker and prepares the session for what was picked, or `None`
/// if it was cancelled.
pub fn run(conn: &mut Connection, project: &Project, cwd: &Path) -> Result<Option<Launch>> {
    let tasks = Tasks::load(conn, project)?;
    let mut picker = Picker::new(&tasks, &Derived::compute(&tasks));
    let title = format!(" ergon work: {} ", project.name);
    with_tui(|terminal| {
        loop {
            terminal.draw(|f| picker.draw(f, &title))?;
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind == KeyEventKind::Release {
                continue;
            }
            match picker.key(key) {
                Action::Stay => {}
                Action::Quit => return Ok(None),
                Action::Start(slugs) => match claude::work(conn, project, &slugs, cwd) {
                    Ok(launch) => return Ok(Some(launch)),
                    Err(e) => picker.error = Some(format!("{e:#}")),
                },
            }
        }
    })
}

static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);
static PANIC_HOOK: Once = Once::new();

/// Runs `f` on the alternate screen of /dev/tty, restoring the terminal
/// afterwards, on errors and panics too.
fn with_tui<T>(f: impl FnOnce(&mut Terminal<CrosstermBackend<File>>) -> Result<T>) -> Result<T> {
    // The panic report prints before unwinding reaches the guard, so restore
    // first, or the report lands on the alternate screen and is lost.
    PANIC_HOOK.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if TUI_ACTIVE.load(Ordering::SeqCst) {
                restore_terminal();
            }
            prev(info);
        }));
    });
    let mut tty = OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .context("opening /dev/tty for the picker")?;
    crossterm::terminal::enable_raw_mode()?;
    TUI_ACTIVE.store(true, Ordering::SeqCst);
    let _guard = RestoreGuard;
    crossterm::execute!(tty, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(tty))?;
    f(&mut terminal)
}

fn restore_terminal() {
    let _ = crossterm::terminal::disable_raw_mode();
    if let Ok(mut tty) = OpenOptions::new().write(true).open("/dev/tty") {
        let _ = crossterm::execute!(tty, LeaveAlternateScreen, crossterm::cursor::Show);
    }
}

struct RestoreGuard;

impl Drop for RestoreGuard {
    fn drop(&mut self) {
        restore_terminal();
        TUI_ACTIVE.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops;
    use crate::{db, project};

    fn press(p: &mut Picker, code: KeyCode) -> Action {
        p.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    /// The picker lists what `ergon ls` does. Enter starts the checked
    /// tasks, or the highlighted one when none is checked.
    #[test]
    fn picks_checked_tasks_or_the_highlighted_one() {
        let mut conn = db::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let p = project::init(&mut conn, "demo", "Root", dir.path()).unwrap();
        let add = |conn: &mut Connection, intent: &str| {
            ops::add(conn, &p, None, intent, Default::default()).unwrap()
        };
        let (a, b, c) = (
            add(&mut conn, "A"),
            add(&mut conn, "B"),
            add(&mut conn, "C"),
        );
        let tasks = Tasks::load(&conn, &p).unwrap();
        let mut picker = Picker::new(&tasks, &Derived::compute(&tasks));
        assert!(picker.rows[0].1.starts_with(&format!("{a}  p-  todo")));

        assert_eq!(press(&mut picker, KeyCode::Down), Action::Stay);
        assert_eq!(
            press(&mut picker, KeyCode::Enter),
            Action::Start(vec![b.clone()])
        );

        press(&mut picker, KeyCode::Char(' '));
        press(&mut picker, KeyCode::Char('j'));
        press(&mut picker, KeyCode::Char(' '));
        assert_eq!(
            press(&mut picker, KeyCode::Enter),
            Action::Start(vec![b, c])
        );
        assert_eq!(press(&mut picker, KeyCode::Esc), Action::Quit);
    }
}
