use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::DefaultTerminal;

use crate::CommitSort;
use crate::SearchHit;
use crate::first_line;
use crate::short_sha;

struct App {
    repo: PathBuf,
    date_ordered: Vec<SearchHit>,
    score_ordered: Vec<SearchHit>,
    commit_sort: CommitSort,
    list_state: ListState,
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

/// Leave Ratatui, run `git show` with your normal pager and colors, then resume the TUI.
fn run_git_show_pager(terminal: &mut DefaultTerminal, repo: &Path, sha: &str) -> Result<()> {
    ratatui::restore();
    let result = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("show")
        .arg(sha)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();
    *terminal = ratatui::init();
    let status = result.context("spawn git show")?;
    if !status.success() {
        bail!("git show exited with status {status}");
    }
    Ok(())
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
    };

    loop {
        terminal
            .draw(|f| draw_browse(f, f.area(), &mut app))
            .context("draw TUI")?;

        match event::read().context("read keyboard")? {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
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
                    run_git_show_pager(terminal, &app.repo, &hit.full_sha)?;
                }
                _ => {}
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
        " repo: {}   ↑/↓/j/k: move   t: sort (date/relevance)   Enter: git show (pager)   Q: quit ",
        app.repo.display()
    );
    f.render_widget(
        Paragraph::new(help_text).style(Style::new().dark_gray()),
        help,
    );
}
