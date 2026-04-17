use std::path::PathBuf;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::DefaultTerminal;

use crate::CommitSort;
use crate::SearchHit;
use crate::git_show_full;
use crate::short_sha;

enum Mode {
    Browse,
    Show {
        sha: String,
        lines: Vec<String>,
        scroll: usize,
    },
}

struct App {
    repo: PathBuf,
    date_ordered: Vec<SearchHit>,
    score_ordered: Vec<SearchHit>,
    commit_sort: CommitSort,
    list_state: ListState,
    mode: Mode,
}

impl App {
    fn list_len(&self) -> usize {
        match self.commit_sort {
            CommitSort::Date => self.date_ordered.len(),
            CommitSort::Score => self.score_ordered.len(),
        }
    }

    fn hits_slice(&self) -> &[SearchHit] {
        hits_for_sort(
            self.commit_sort,
            &self.date_ordered,
            &self.score_ordered,
        )
    }

    fn toggle_sort(&mut self) {
        let prev = self
            .hits_slice()
            .get(self.list_state.selected().unwrap_or(0))
            .map(|h| h.full_sha.clone());
        self.commit_sort = match self.commit_sort {
            CommitSort::Date => CommitSort::Score,
            CommitSort::Score => CommitSort::Date,
        };
        let list = self.hits_slice();
        let idx = prev
            .and_then(|s| list.iter().position(|h| h.full_sha == s))
            .unwrap_or(0);
        let idx = idx.min(list.len().saturating_sub(1));
        self.list_state.select(Some(idx));
    }
}

pub fn run(
    repo: PathBuf,
    date_ordered: Vec<SearchHit>,
    score_ordered: Vec<SearchHit>,
) -> Result<()> {
    let mut terminal = ratatui::init();
    let res = run_inner(&mut terminal, repo, date_ordered, score_ordered);
    ratatui::restore();
    res
}

/// Inner lines visible inside a `Block` with borders for height `h`.
fn bordered_inner_lines(h: u16) -> usize {
    h.saturating_sub(2) as usize
}

fn show_body_rect(area: Rect) -> Rect {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area)[0]
}

fn max_git_show_scroll(line_count: usize, area: Rect) -> usize {
    let body = show_body_rect(area);
    let visible = bordered_inner_lines(body.height).max(1);
    line_count.saturating_sub(visible)
}

fn run_inner(
    terminal: &mut DefaultTerminal,
    repo: PathBuf,
    date_ordered: Vec<SearchHit>,
    score_ordered: Vec<SearchHit>,
) -> Result<()> {
    let mut app = App {
        repo,
        date_ordered,
        score_ordered,
        commit_sort: CommitSort::Date,
        list_state: ListState::default().with_selected(Some(0)),
        mode: Mode::Browse,
    };

    loop {
        terminal
            .draw(|f| draw(f, &mut app))
            .context("draw TUI")?;

        match event::read().context("read keyboard")? {
            Event::Key(key) if key.kind == KeyEventKind::Press => match &mut app.mode {
                Mode::Browse => match key.code {
                    KeyCode::Char('Q') => return Ok(()),
                    KeyCode::Char('t') => {
                        app.toggle_sort();
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        let len = app.list_len();
                        select_prev(&mut app.list_state, len);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        let len = app.list_len();
                        select_next(&mut app.list_state, len);
                    }
                    KeyCode::Enter => {
                        let Some(i) = app.list_state.selected() else {
                            continue;
                        };
                        let Some(hit) = app.hits_slice().get(i) else {
                            continue;
                        };
                        let text = git_show_full(&app.repo, &hit.full_sha)
                            .context("git show")?;
                        let lines: Vec<String> =
                            text.lines().map(std::string::ToString::to_string).collect();
                        app.mode = Mode::Show {
                            sha: hit.full_sha.clone(),
                            lines,
                            scroll: 0,
                        };
                    }
                    _ => {}
                },
                Mode::Show {
                    lines,
                    scroll,
                    ..
                } => {
                    let (w, h) = terminal::size().unwrap_or((80, 24));
                    let max = max_git_show_scroll(lines.len(), Rect::new(0, 0, w, h));
                    let page = {
                        let body = show_body_rect(Rect::new(0, 0, w, h));
                        bordered_inner_lines(body.height).max(1)
                    };
                    match key.code {
                        KeyCode::Char('Q') => return Ok(()),
                        KeyCode::Esc | KeyCode::Char('q') => {
                            app.mode = Mode::Browse;
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            *scroll = scroll.saturating_sub(1);
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            *scroll = (*scroll + 1).min(max);
                        }
                        KeyCode::PageUp => {
                            *scroll = scroll.saturating_sub(page);
                        }
                        KeyCode::PageDown => {
                            *scroll = (*scroll + page).min(max);
                        }
                        _ => {}
                    }
                }
            },
            Event::Resize(_, _) => {}
            _ => {}
        }
    }
}

fn select_prev(state: &mut ListState, len: usize) {
    if len == 0 {
        return;
    }
    let i = state.selected().unwrap_or(0);
    let n = if i == 0 { len - 1 } else { i - 1 };
    state.select(Some(n));
}

fn select_next(state: &mut ListState, len: usize) {
    if len == 0 {
        return;
    }
    let i = state.selected().unwrap_or(0);
    let n = if i + 1 >= len { 0 } else { i + 1 };
    state.select(Some(n));
}

fn draw(f: &mut ratatui::Frame<'_>, app: &mut App) {
    if let Mode::Show { lines, scroll, .. } = &mut app.mode {
        let max = max_git_show_scroll(lines.len(), f.area());
        *scroll = (*scroll).min(max);
    }
    let area = f.area();
    match &app.mode {
        Mode::Browse => draw_browse(f, area, app),
        Mode::Show {
            sha,
            lines,
            scroll,
        } => draw_show(f, area, sha, lines, *scroll),
    }
}

fn first_line(message: &str) -> &str {
    message.lines().next().unwrap_or("").trim_end()
}

fn hits_for_sort<'a>(
    sort: CommitSort,
    date_ordered: &'a [SearchHit],
    score_ordered: &'a [SearchHit],
) -> &'a [SearchHit] {
    match sort {
        CommitSort::Date => date_ordered,
        CommitSort::Score => score_ordered,
    }
}

fn draw_browse(f: &mut ratatui::Frame<'_>, area: Rect, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);
    let body = chunks[0];
    let help = chunks[1];

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Min(28)])
        .split(body);

    let sel = app.list_state.selected().unwrap_or(0);
    let sort = app.commit_sort;
    let hits = hits_for_sort(sort, &app.date_ordered, &app.score_ordered);
    let items: Vec<ListItem> = hits
        .iter()
        .map(|h| {
            let subject = first_line(&h.message);
            ListItem::new(Text::from(vec![
                Line::from(format!("{}: {}", short_sha(&h.full_sha), subject)).white(),
                Line::from(format!("score {:.4}", h.score)).dark_gray(),
            ]))
        })
        .collect();
    let msg: String = hits
        .get(sel)
        .map(|h| h.message.clone())
        .unwrap_or_default();

    let list_title = match sort {
        CommitSort::Date => " commits (main, by date) ",
        CommitSort::Score => " commits (by relevance) ",
    };
    let list = List::new(items)
        .block(Block::bordered().title(list_title))
        .highlight_style(Style::new().reversed())
        .highlight_symbol("> ");

    f.render_stateful_widget(list, cols[0], &mut app.list_state);

    let right = Paragraph::new(msg.as_str())
        .wrap(Wrap { trim: true })
        .block(Block::bordered().title(" commit message "));
    f.render_widget(right, cols[1]);

    let help_text = format!(
        " repo: {}   ↑/↓/j/k: move   t: sort (date/relevance)   Enter: git show   Q: quit ",
        app.repo.display()
    );
    f.render_widget(
        Paragraph::new(help_text).style(Style::new().dark_gray()),
        help,
    );
}

fn draw_show(f: &mut ratatui::Frame<'_>, area: Rect, sha: &str, lines: &[String], scroll: usize) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);
    let body = chunks[0];
    let help = chunks[1];

    let visible = bordered_inner_lines(body.height).max(1);
    let max_scroll = lines.len().saturating_sub(visible);
    let scroll = scroll.min(max_scroll);
    let end = (scroll + visible).min(lines.len());
    let slice: Vec<Line> = lines[scroll..end]
        .iter()
        .map(|s| Line::from(s.as_str()))
        .collect();

    let title = format!(" git show {} ", short_sha(sha));
    let para = Paragraph::new(Text::from(slice)).block(Block::bordered().title(title));
    f.render_widget(para, body);

    let status = format!(
        " lines {}–{} of {}   ↑/↓/j/k PgUp/PgDn: scroll   q/Esc: back   Q: quit ",
        if lines.is_empty() {
            0
        } else {
            scroll + 1
        },
        if lines.is_empty() { 0 } else { end },
        lines.len()
    );
    f.render_widget(
        Paragraph::new(status).style(Style::new().dark_gray()),
        help,
    );
}
