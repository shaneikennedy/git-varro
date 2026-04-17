# git-varro

Search **commit messages on `main`** in the current Git repo using the [Varro](https://github.com/shaneikennedy/varro) embedded search engine. The first run indexes every commit on `main` (SHA as document id, message as text); later runs only index new commits. The index lives in **`.varro/`** at the repo root.

## Requirements

- **Rust** (stable) and **Cargo**
- **`git`** on your `PATH`
- A branch named **`main`** (that is the only branch indexed)

## Install

From a clone of this repository:

```sh
cargo build --release
```

Put the binary on your `PATH`:

```sh
cp target/release/git-varro ~/.local/bin/   # or another directory on PATH
```

Alternatively:

```sh
cargo install --path .
```

Confirm:

```sh
git-varro --help
```

## Usage

From inside a Git repository:

```sh
git-varro search "your phrase"
```

This updates the index if needed, runs a hybrid BM25 + semantic search (Varro), then opens a **terminal UI**: commit list (short SHA + subject), full message on the right, **`t`** toggles sort between **date** (on `main`) and **match score**, **Enter** opens **`git show`** in your normal **pager** (colors, `q` to return to the UI), **`Q`** quits the app.

## Give your agents search super powers

Coding agents, scripts, and CI jobs do not need a TUI. Pass **`--no-tui`** or **`-n`** so `git-varro search` writes plain lines to **stdout** instead: each line is **short SHA**, **first line of the commit message**, and **score**, in **descending score order** (best match first). No colors or escape codes—easy to grep, pipe, or feed into a prompt.

```sh
git-varro search "regression in checkout" --no-tui
git-varro search "oauth" -n
```

Same indexing and search as the interactive flow; only the presentation changes.

## Notes

- The Varro dependency is pulled from **GitHub** (`master`); the resolved revision is pinned in **`Cargo.lock`**.
- First-time indexing can take a while on large histories (Varro + embeddings).
