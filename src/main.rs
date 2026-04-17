mod tui;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use varro::{Document, FileSystemType, SearchOptions, Varro};

const MAIN_BRANCH: &str = "main";
const INDEX_FIELD: &str = "message";
const LAST_FILE: &str = ".last";
/// Varro compactor threshold (`Varro::with_min_segment_size`); default in the library is 64 MiB.
const VARRO_MIN_SEGMENT_SIZE: usize = 512 * 1024 * 1024;
/// Varro flusher auto-flush threshold (`Varro::with_max_buffer_size`); default in the library is 50 MiB.
const VARRO_MAX_BUFFER_SIZE: usize = 512 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum CommitSort {
    #[default]
    Date,
    Score,
}

#[derive(Parser)]
#[command(name = "git-varro", about = "Search commit messages on the main branch with Varro")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Hybrid BM25 + vector search on commit messages (updates the index first)
    Search {
        /// Words or phrase to find in commit messages (not raw VQL)
        query: String,
        /// Print matches to stdout (score order) instead of opening the TUI
        #[arg(long = "no-tui", short = 'n')]
        no_tui: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Search { query, no_tui } => run_search(&query, no_tui),
    }
}

pub(crate) fn short_sha(full: &str) -> &str {
    if full.len() <= 7 {
        full
    } else {
        &full[..7]
    }
}

pub(crate) fn first_line(message: &str) -> &str {
    message.lines().next().unwrap_or("").trim_end()
}

fn run_search(query: &str, no_tui: bool) -> Result<()> {
    let query = query.trim();
    if query.is_empty() {
        bail!("search query is empty");
    }

    let repo = find_git_root(&std::env::current_dir()?).context("not inside a git repository")?;
    let varro_dir = repo.join(".varro");
    fs::create_dir_all(&varro_dir).with_context(|| format!("create {}", varro_dir.display()))?;

    verify_main_exists(&repo)?;

    let main_tip = git_trimmed(&repo, &["rev-parse", MAIN_BRANCH])?;

    sync_index(&repo, &varro_dir, &main_tip)?;

    let engine = Varro::new(&varro_dir, FileSystemType::Local)
        .with_context(|| format!("open Varro index at {}", varro_dir.display()))?
        .with_min_segment_size(VARRO_MIN_SEGMENT_SIZE)
        .with_max_buffer_size(VARRO_MAX_BUFFER_SIZE);

    let vql = hybrid_message_vql(query);
    let opts = SearchOptions::new().with_include_documents(true);
    let mut best: HashMap<String, (Document, f64)> = HashMap::new();
    for (doc, score) in engine.search(vql, Some(opts)) {
        let id = doc.id();
        let replace = match best.get(&id) {
            None => true,
            Some((_, s)) => score > *s,
        };
        if replace {
            best.insert(id, (doc, score));
        }
    }

    let mut results: Vec<_> = best.into_values().collect();
    results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    if results.is_empty() {
        println!("No matches.");
        return Ok(());
    }

    let hits: Vec<SearchHit> = results
        .into_iter()
        .map(|(doc, score)| SearchHit {
            full_sha: doc.id(),
            message: doc
                .get_field(INDEX_FIELD.into())
                .map(|f| f.contents())
                .unwrap_or_default(),
            score,
        })
        .collect();

    if no_tui {
        let mut rows = hits;
        rows.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.full_sha.cmp(&b.full_sha))
        });
        for h in rows {
            println!(
                "{}  {}  {:.4}",
                short_sha(&h.full_sha),
                first_line(&h.message),
                h.score
            );
        }
        return Ok(());
    }

    let date_ordered = order_hits_by_main_history(&repo, &hits)?;
    let mut score_ordered = hits;
    score_ordered.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.full_sha.cmp(&b.full_sha))
    });
    tui::run(repo, date_ordered, score_ordered)?;

    Ok(())
}

#[derive(Clone)]
pub(crate) struct SearchHit {
    pub full_sha: String,
    pub message: String,
    pub score: f64,
}

fn order_hits_by_main_history(repo: &Path, hits: &[SearchHit]) -> Result<Vec<SearchHit>> {
    let set: HashSet<String> = hits.iter().map(|h| h.full_sha.clone()).collect();
    let map: HashMap<String, SearchHit> = hits
        .iter()
        .map(|h| (h.full_sha.clone(), h.clone()))
        .collect();

    let mut out = Vec::new();
    for line in git_rev_list_newest_first(repo, MAIN_BRANCH)? {
        if set.contains(&line) {
            if let Some(hit) = map.get(&line) {
                out.push(hit.clone());
            }
        }
    }

    for h in hits {
        if !out.iter().any(|x| x.full_sha == h.full_sha) {
            out.push(h.clone());
        }
    }

    Ok(out)
}

fn git_rev_list_newest_first(repo: &Path, revision: &str) -> Result<Vec<String>> {
    let out = git_output(repo, &["rev-list", revision])?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect())
}

/// Varro VQL uses single-quoted literals for arbitrary text (double quotes are not valid there).
/// BM25 on `message` OR vector similarity on `message`, same phrase for both sides.
fn hybrid_message_vql(user_query: &str) -> String {
    let safe = user_query.replace('\'', " ");
    format!("message:'{safe}' | ~message:'{safe}'")
}

fn sync_index(repo: &Path, varro_dir: &Path, main_tip: &str) -> Result<()> {
    let last_path = varro_dir.join(LAST_FILE);
    let last_sha = match fs::read_to_string(&last_path) {
        Ok(s) => {
            let s = s.trim();
            if s.is_empty() {
                None
            } else {
                Some(s.to_string())
            }
        }
        Err(_) => None,
    };

    let need_full = match &last_sha {
        None => true,
        Some(last) if last == main_tip => false,
        Some(last) => !is_ancestor(repo, last, MAIN_BRANCH)?,
    };

    if need_full {
        if last_sha.is_some() {
            eprintln!("git-varro: indexed tip is not an ancestor of {MAIN_BRANCH}; rebuilding index.");
        }
        fs::remove_dir_all(varro_dir).ok();
        fs::create_dir_all(varro_dir)
            .with_context(|| format!("create {}", varro_dir.display()))?;
        let commits = git_rev_list(repo, MAIN_BRANCH)?;
        index_commits(repo, varro_dir, &commits)?;
        write_last(varro_dir, main_tip)?;
        return Ok(());
    }

    if last_sha.as_deref() == Some(main_tip) {
        return Ok(());
    }

    let last = last_sha.as_deref().expect("checked");
    let range = format!("{last}..{MAIN_BRANCH}");
    let commits = git_rev_list(repo, &range)?;
    if !commits.is_empty() {
        index_commits(repo, varro_dir, &commits)?;
    }
    write_last(varro_dir, main_tip)?;
    Ok(())
}

fn index_commits(repo: &Path, varro_dir: &Path, commits: &[String]) -> Result<()> {
    let commits: Vec<String> = commits
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if commits.is_empty() {
        return Ok(());
    }

    let engine = Varro::new(varro_dir, FileSystemType::Local)
        .with_context(|| format!("open Varro index at {}", varro_dir.display()))?
        .with_min_segment_size(VARRO_MIN_SEGMENT_SIZE)
        .with_max_buffer_size(VARRO_MAX_BUFFER_SIZE);

    let total = commits.len() as u64;
    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{wide_bar:.cyan/blue}] {pos}/{len} commits — {msg} (eta {eta})",
        )
        .context("progress bar template")?
        .progress_chars("#>-"),
    );
    pb.set_message("queued into Varro");

    let index_one = |sha: &String| -> Result<()> {
        let full_sha = git_trimmed(repo, &["rev-parse", sha.as_str()])?;
        let message = git_commit_message(repo, &full_sha)?;
        let mut doc = Document::new(full_sha.clone());
        doc.add_field(INDEX_FIELD.into(), message, true);
        engine
            .index(doc)
            .with_context(|| format!("index commit {full_sha}"))?;
        pb.inc(1);
        Ok(())
    };

    let result: Result<()> = commits.par_iter().try_for_each(index_one);
    if result.is_err() {
        pb.abandon();
    }
    result?;

    pb.finish_with_message("indexed");
    engine.flush().context("flush Varro index")?;
    Ok(())
}

fn write_last(varro_dir: &Path, sha: &str) -> Result<()> {
    let path = varro_dir.join(LAST_FILE);
    fs::write(&path, format!("{sha}\n")).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn git_commit_message(repo: &Path, sha: &str) -> Result<String> {
    git_output(repo, &["show", "-s", "--format=%B", sha])
}

fn git_rev_list(repo: &Path, range: &str) -> Result<Vec<String>> {
    let out = git_output(repo, &["rev-list", "--reverse", range])?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect())
}

fn is_ancestor(repo: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .status()
        .context("run git merge-base --is-ancestor")?;
    Ok(status.success())
}

fn verify_main_exists(repo: &Path) -> Result<()> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", MAIN_BRANCH])
        .output()
        .context("run git rev-parse --verify main")?;

    if out.status.success() {
        return Ok(());
    }

    bail!(
        "branch `{MAIN_BRANCH}` not found in this repository (git-varro only indexes `{MAIN_BRANCH}`).\n{}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
}

fn git_trimmed(repo: &Path, args: &[&str]) -> Result<String> {
    let s = git_output(repo, args)?;
    let s = s.trim();
    if s.is_empty() {
        bail!("git {:?} produced empty output", args);
    }
    Ok(s.to_string())
}

fn git_output(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .with_context(|| format!("spawn git {:?}", args))?;

    if !out.status.success() {
        bail!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut cur = start;
    loop {
        if cur.join(".git").exists() {
            return Some(cur.to_path_buf());
        }
        cur = cur.parent()?;
    }
}
