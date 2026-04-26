mod tui;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use git2::{Repository, RevparseMode, Sort};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use varro::{
    CompactionOptions, Document, FileSystemType, FlushOptions, Options, SearchOptions,
    SemanticSearchOptions, Varro,
};

const MAIN_BRANCH: &str = "main";
const INDEX_FIELD: &str = "message";
const LAST_FILE: &str = ".last";
/// Varro compactor threshold (`Varro::with_min_segment_size`); default in the library is 64 MiB.
const VARRO_MIN_SEGMENT_SIZE: usize = 512 * 1024 * 1024;
const VARRO_MAX_BUFFER_SIZE: usize = 1024 * 1024;
/// Varro background compaction wake interval (`Varro::with_compaction_frequency`).
const VARRO_COMPACTION_FREQUENCY: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum CommitSort {
    #[default]
    Date,
    Score,
}

#[derive(Parser)]
#[command(
    name = "git-varro",
    about = "Search commit messages on the main branch with Varro"
)]
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
    let git = Repository::open(&repo).context("open repository with git2")?;
    let varro_dir = repo.join(".varro");
    fs::create_dir_all(&varro_dir).with_context(|| format!("create {}", varro_dir.display()))?;

    verify_main_branch(&git)?;

    let main_tip = resolve_revision_to_oid_hex(&git, MAIN_BRANCH)?;

    sync_index(&git, &varro_dir, &main_tip)?;
    let opts = Options {
        filesystem: FileSystemType::Local,
        compaction: CompactionOptions {
            min_segment_size: VARRO_MIN_SEGMENT_SIZE,
            compaction_frequency: VARRO_COMPACTION_FREQUENCY,
        },
        flush: FlushOptions {
            max_buffer_size: VARRO_MAX_BUFFER_SIZE,
        },
        semantic_search: SemanticSearchOptions::new(false),
    };

    let engine = Varro::new(&varro_dir, opts)
        .with_context(|| format!("open Varro index at {}", varro_dir.display()))?;

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

    let date_ordered = order_hits_by_main_history(&git, &hits)?;
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

fn order_hits_by_main_history(git: &Repository, hits: &[SearchHit]) -> Result<Vec<SearchHit>> {
    let set: HashSet<String> = hits.iter().map(|h| h.full_sha.clone()).collect();
    let map: HashMap<String, SearchHit> = hits
        .iter()
        .map(|h| (h.full_sha.clone(), h.clone()))
        .collect();

    let mut out = Vec::new();
    for oid_hex in git2_rev_list_newest_first(git, MAIN_BRANCH)? {
        if set.contains(&oid_hex) {
            if let Some(hit) = map.get(&oid_hex) {
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

/// Varro VQL uses single-quoted literals for arbitrary text (double quotes are not valid there).
/// BM25 on `message` OR vector similarity on `message`, same phrase for both sides.
fn hybrid_message_vql(user_query: &str) -> String {
    let safe = user_query.replace('\'', " ");
    format!("message:'{safe}' | ~message:'{safe}'")
}

fn sync_index(git: &Repository, varro_dir: &Path, main_tip: &str) -> Result<()> {
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
        Some(last) => !is_ancestor(git, last, MAIN_BRANCH)?,
    };

    if need_full {
        if last_sha.is_some() {
            eprintln!(
                "git-varro: indexed tip is not an ancestor of {MAIN_BRANCH}; rebuilding index."
            );
        }
        fs::remove_dir_all(varro_dir).ok();
        fs::create_dir_all(varro_dir).with_context(|| format!("create {}", varro_dir.display()))?;
        index_commits(git, varro_dir, MAIN_BRANCH)?;
        write_last(varro_dir, main_tip)?;
        return Ok(());
    }

    if last_sha.as_deref() == Some(main_tip) {
        return Ok(());
    }

    let last = last_sha.as_deref().expect("checked");
    let range = format!("{last}..{MAIN_BRANCH}");
    index_commits(git, varro_dir, &range)?;
    write_last(varro_dir, main_tip)?;
    Ok(())
}

/// `revision` is anything `git rev-parse` accepts for a walk (e.g. `main`, `abc..main`).
/// Commit messages are read via **git2** (libgit2); Varro indexing still runs in parallel.
fn index_commits(git: &Repository, varro_dir: &Path, revision: &str) -> Result<()> {
    let rows = git2_commit_messages_rows(git, revision)?;
    if rows.is_empty() {
        return Ok(());
    }

    let opts = Options {
        filesystem: FileSystemType::Local,
        compaction: CompactionOptions {
            min_segment_size: VARRO_MIN_SEGMENT_SIZE,
            compaction_frequency: VARRO_COMPACTION_FREQUENCY,
        },
        flush: FlushOptions {
            max_buffer_size: VARRO_MAX_BUFFER_SIZE,
        },
        semantic_search: SemanticSearchOptions::new(false),
    };

    let engine = Varro::new(varro_dir, opts)
        .with_context(|| format!("open Varro index at {}", varro_dir.display()))?;

    let total = rows.len() as u64;
    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{wide_bar:.cyan/blue}] {pos}/{len} commits — {msg}",
        )
        .context("progress bar template")?
        .progress_chars("#>-"),
    );
    pb.set_message("queued into Varro");

    let index_one = |(full_sha, message): &(String, String)| -> Result<()> {
        let mut doc = Document::new(full_sha.clone());
        doc.add_field(INDEX_FIELD.into(), message.clone(), true);
        engine
            .index(doc)
            .with_context(|| format!("index commit {full_sha}"))?;
        pb.inc(1);
        Ok(())
    };

    let result: Result<()> = rows.par_iter().try_for_each(index_one);
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

fn verify_main_branch(git: &Repository) -> Result<()> {
    if let Err(e) = git.resolve_reference_from_short_name(MAIN_BRANCH) {
        bail!(
            "branch `{MAIN_BRANCH}` not found in this repository (git-varro only indexes `{MAIN_BRANCH}`).\n{e}"
        );
    }
    Ok(())
}

fn resolve_revision_to_oid_hex(git: &Repository, spec: &str) -> Result<String> {
    Ok(git
        .revparse_single(spec)
        .with_context(|| format!("resolve revision {spec:?}"))?
        .peel_to_commit()
        .with_context(|| format!("revision {spec:?} is not a commit"))?
        .id()
        .to_string())
}

fn is_ancestor(git: &Repository, ancestor: &str, descendant: &str) -> Result<bool> {
    let a = git
        .revparse_single(ancestor)
        .with_context(|| format!("revparse ancestor {ancestor:?}"))?
        .id();
    let d = git
        .revparse_single(descendant)
        .with_context(|| format!("revparse descendant {descendant:?}"))?
        .id();
    git.graph_descendant_of(d, a)
        .context("git2 graph_descendant_of (merge-base --is-ancestor)")
}

/// Configure `revwalk` like `git rev-list` for `revision` (`main` or two-dot `A..B`).
fn git2_push_revspec_for_revwalk(
    git: &Repository,
    rw: &mut git2::Revwalk,
    revision: &str,
) -> Result<()> {
    if revision.contains("..") {
        let spec = git
            .revparse(revision)
            .with_context(|| format!("git2 revparse {revision:?}"))?;
        if spec.mode().contains(RevparseMode::MERGE_BASE) {
            bail!(
                "git-varro does not support three-dot revspecs ({revision}); use two-dot ranges only"
            );
        }
        let from = spec
            .from()
            .with_context(|| format!("revparse {revision:?}: missing left side of range"))?;
        let to = spec
            .to()
            .with_context(|| format!("revparse {revision:?}: missing right side of range"))?;
        rw.hide(from.id())?;
        rw.push(to.id())?;
    } else {
        rw.push(
            git.revparse_single(revision)
                .with_context(|| format!("git2 revparse_single {revision:?}"))?
                .peel_to_commit()
                .with_context(|| format!("revision {revision:?} is not a commit"))?
                .id(),
        )?;
    }
    Ok(())
}

/// Oldest commit first (same order as `git log --reverse` / previous `rev-list --reverse`).
fn git2_commits_oldest_first(git: &Repository, revision: &str) -> Result<Vec<git2::Oid>> {
    let mut rw = git.revwalk()?;
    rw.set_sorting(Sort::TOPOLOGICAL | Sort::TIME)
        .context("revwalk set_sorting")?;
    git2_push_revspec_for_revwalk(git, &mut rw, revision)?;
    let mut ids = Vec::new();
    for id in rw {
        ids.push(id?);
    }
    ids.reverse();
    Ok(ids)
}

fn git2_commit_messages_rows(git: &Repository, revision: &str) -> Result<Vec<(String, String)>> {
    let ids = git2_commits_oldest_first(git, revision)?;
    let mut rows = Vec::with_capacity(ids.len());
    for id in ids {
        let commit = git
            .find_commit(id)
            .with_context(|| format!("find_commit {}", id))?;
        let message = commit_message_raw_utf8(&commit)?;
        rows.push((id.to_string(), message));
    }
    Ok(rows)
}

/// Matches `git log` `%B` / raw commit message bytes as UTF-8 (strict).
fn commit_message_raw_utf8(commit: &git2::Commit) -> Result<String> {
    String::from_utf8(commit.message_raw_bytes().to_vec())
        .context("commit message is not valid UTF-8")
}

/// Same order as `git rev-list` (newest first) for one positive ref.
fn git2_rev_list_newest_first(git: &Repository, revision: &str) -> Result<Vec<String>> {
    let mut rw = git.revwalk()?;
    rw.set_sorting(Sort::TOPOLOGICAL | Sort::TIME)
        .context("revwalk set_sorting")?;
    rw.push(
        git.revparse_single(revision)
            .with_context(|| format!("git2 revparse_single {revision:?}"))?
            .peel_to_commit()
            .with_context(|| format!("revision {revision:?} is not a commit"))?
            .id(),
    )?;
    let mut out = Vec::new();
    for id in rw {
        out.push(id?.to_string());
    }
    Ok(out)
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
