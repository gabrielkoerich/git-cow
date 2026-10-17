# git-cow

[![CI](https://github.com/gabrielkoerich/git-cow/actions/workflows/ci.yml/badge.svg)](https://github.com/gabrielkoerich/git-cow/actions/workflows/ci.yml) [![Release](https://img.shields.io/github/v/release/gabrielkoerich/git-cow)](https://github.com/gabrielkoerich/git-cow/releases/latest) [![crates.io](https://img.shields.io/crates/v/git-cow)](https://crates.io/crates/git-cow) [![docs.rs](https://img.shields.io/docsrs/git-cow)](https://docs.rs/git-cow) [![License](https://img.shields.io/crates/l/git-cow)](LICENSE)

`git worktree add` that clones files instead of copying them. Every new worktree is a copy-on-write clone of a template checkout, so `target/`, `node_modules` and other build output are shared on disk until a build rewrites them. A worktree that would cost gigabytes starts at a few megabytes and builds incrementally.

```bash
git cow add -b feat/login                 # ~/.worktrees/<repo>/login
git cow add -b feat/login ../login        # any path, like git worktree add
git cow list                              # your worktrees, without the templates
git cow remove --warm ~/.worktrees/<repo>/login   # after a merge: share its build output, then remove it
```

## Install

```bash
cargo install git-cow   # into ~/.cargo/bin
```

### The `git cow` subcommand

Git runs any executable named `git-<name>` on your `PATH` as `git <name>`. The install puts a `git-cow` binary on your `PATH`, so `git cow` works with no extra setup. Check it with:

```bash
command -v git-cow   # prints the binary's path
git cow              # prints the usage
```

If `git cow` says it is not a git command, `~/.cargo/bin` is not on your `PATH`: add `export PATH="$HOME/.cargo/bin:$PATH"` to your shell profile. To use another name, add an alias: `git config --global alias.wt '!git-cow'` makes `git wt add -b feat/x` work.

## Commands

| Command | What it does |
|---|---|
| `git cow add [-b <branch>] [<path>] [<commit-ish>]` | Creates a worktree by cloning the template. Same arguments as `git worktree add`, and without `<path>` it goes to `<root>/<repo>/<last part of the branch>`, where `<root>` is the `cow.root` setting (default `~/.worktrees`). Falls back to `git worktree add` when cloning is not possible |
| `git cow list` | Lists the worktrees, path and branch, without the templates |
| `git cow remove [--warm] [--force] <worktree>` (or `rm`) | Removes a worktree, then deletes its branch only if merged. `--warm` hands its build output to the template first, `--force` removes it with uncommitted changes. It refuses to remove a template or the main checkout |
| `git cow warm <worktree>` | Refreshes the template with a worktree's gitignored directories, without removing it |
| `git cow share [--global] [<dir>...]` | Lists the hidden directories that are shared, or adds to them |

A git-cow worktree is a normal git worktree, so `git worktree list`, `git worktree remove` and the rest work on it too. The difference is that `git worktree list` also shows the template, as a detached worktree, and removing that by hand only makes the next `add` recreate it.

## How it works

1. Keep one template per repository: a detached git worktree of your repository at `origin/HEAD`, so it shares your `.git` and costs no second copy of the history. New worktrees are cloned from it.
2. Clone the gitignored directories of your checkout into the template: build output and dependencies, never loose ignored files like `.env`. A hidden directory is shared only when it is on the share list, since the rest (`.claude`, `.codex`, `.aws`) is tool or agent config. This warm-up runs at most once per `cow.warmInterval`.
3. `git worktree add --no-checkout`, then clone every template entry except `.git` into it. macOS clones each whole directory with one `clonefile(2)` call, Linux uses `cp --reflink=always` (FICLONE).
4. Copy the template's index, refresh it, and `git reset --hard`, which rewrites only the files where your branch differs from the template.

If the volume cannot clone, or anything fails, it removes what it made and runs plain `git worktree add` with the same arguments.

## Where worktrees and templates go

The worktree goes where you say: the path you pass, or `cow.root` when you leave it out. The template goes to `<worktree's parent>/.git-cow/<repo>` unless `cow.template` names a place. So each tool keeps its own layout:

| Who | Worktrees | Template by default |
|---|---|---|
| `git cow add -b feat/x` | `~/.worktrees/<repo>/x` | `~/.worktrees/<repo>/.git-cow/<repo>` |
| A tool that passes its own path, such as Claude Code's `<repo>/.claude/worktrees/<name>` | that path | `<repo>/.claude/worktrees/.git-cow/<repo>` |
| A library caller, such as orch | its own path | the template path it passes |

Two templates of one repository cost almost no disk, since both are clones, but each is warmed separately. To give every tool one template per repository, point them all at the same place with `{repo}`, which stands for the repository's name:

```bash
git config --global cow.template '~/.orch/state/templates/{repo}'
```

The template, the worktrees and your checkout must be on one volume that clones.

## Configuration

| | Default | |
|---|---|---|
| `git config cow.root` | `~/.worktrees` | Where a worktree goes when the path is left out: `<root>/<repo>/<branch>`, or `<root>/<branch>` when the root contains `{repo}` |
| `git config cow.template` | `<worktree's parent>/.git-cow/<repo>` | The template checkout. `{repo}` is replaced by the repository's name |
| `git config cow.share` | | A hidden directory to share on top of the built-in caches, one entry each. `git cow share .venv` adds one, `git cow share` lists them |
| `git config cow.warmInterval` | `1h` | How often `add` refreshes the template from your checkout: `0` for every time, `30m`, `2h`. `git cow warm` always refreshes |
| `GIT_COW=0` | | Plain `git worktree add` |

Each setting works per repository, or for all of them with `git config --global`.

Hidden directories shared by default, all pure build caches: `.uv-cache`, `.next`, `.turbo`, `.nuxt`, `.svelte-kit`, `.parcel-cache`, `.gradle`, `.mypy_cache`, `.ruff_cache`, `.pytest_cache`. A virtualenv only works after a clone if it was created relocatable, since it stores absolute paths, so `.venv` is not on the list.

Cloning needs one volume that clones: APFS on macOS, XFS formatted with `reflink=1` or btrfs on Linux. ext4 and HFS+ always fall back.

## Library

```rust
git_cow::add(repo, template, "origin/main", worktree, Some("feat/x"), None)?;
git_cow::warm(merged_worktree, template)?;

let opts = git_cow::Options { share: vec![".venv".into()], ..Default::default() };
git_cow::add_with(repo, template, "origin/main", worktree, Some("feat/x"), None, &opts)?;
```

Synchronous and without dependencies. A lock file next to the template keeps concurrent clones, from threads or processes, from seeing it half updated.

## Benchmarks

One run each on an Apple Silicon Mac (APFS), against plain `git worktree add`. Disk is what the new worktree really adds, counted from bytes written after it was made, since `du` counts every clone as a full copy.

**orch** (Rust), then `cargo test --no-run` in the new worktree:

| | Create | Build | Disk |
|---|---|---|---|
| `git worktree add` | 0.3 s | 45.6 s | 1.5 GB |
| `git cow add` | 0.8 s | 31.7 s | 0.7 GB |

Cargo still rebuilds the repository's own crates in each worktree, the dependencies come from the template.

**bean** (Python, uv), then `uv sync` in the new worktree:

| | `uv sync` | Disk |
|---|---|---|
| `git worktree add`, a fresh uv cache in the worktree | 37.5 s | 2.8 GB |
| `git cow add`, `.uv-cache` shared from the template | 5.7 s | 0.02 GB |

## Costs

- A cloned file's blocks are copied the first time either side writes it.
- `git update-index --refresh` re-hashes every tracked file, since clones have new inodes. That grows with the number of tracked files, about 3 s on a repository with large tracked data.
- A big shared cache is cloned into every worktree, which takes time even when it takes no disk: about 4 s for a 6.7 GB uv cache.
- Between warm-ups the template can be up to `cow.warmInterval` behind your checkout, which costs a little extra building in new worktrees.
- Output that embeds absolute paths, such as virtualenvs, points at the template. Whether the tool recovers is up to the tool.
- A directory where everything is ignored is shared whole, hidden subdirectories included, because git reports it as one entry.
- Repositories with submodules always fall back.

## Origin

Extracted from [orch](https://github.com/gabrielkoerich/orch), where every agent task gets a worktree.

MIT licensed.
