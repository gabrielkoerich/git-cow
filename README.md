# git-cow

[![CI](https://github.com/gabrielkoerich/git-cow/actions/workflows/ci.yml/badge.svg)](https://github.com/gabrielkoerich/git-cow/actions/workflows/ci.yml) [![Release](https://img.shields.io/github/v/release/gabrielkoerich/git-cow)](https://github.com/gabrielkoerich/git-cow/releases/latest) [![crates.io](https://img.shields.io/crates/v/git-cow)](https://crates.io/crates/git-cow) [![docs.rs](https://img.shields.io/docsrs/git-cow)](https://docs.rs/git-cow) [![License](https://img.shields.io/crates/l/git-cow)](LICENSE)

Git worktrees that share their build output. git-cow keeps one extra checkout of each repository and makes every new worktree a copy-on-write clone of it. So `target/`, `node_modules` and other ignored folders take no extra disk until a build changes them. A worktree that would cost gigabytes starts at a few megabytes, and its first build is incremental.

```bash
git cow add -b feat/login                 # ~/.worktrees/<repo>/login
git cow add -b feat/login ../login        # any path, like git worktree add
git cow list                              # your worktrees, without the extra checkout
git cow remove --warm ~/.worktrees/<repo>/login   # after a merge: give its build output to the extra checkout, then remove it
```

## Install

```bash
cargo install git-cow   # into ~/.cargo/bin
```

Git runs any program named `git-<name>` on your `PATH` as `git <name>`, so `git cow` works right after the install. If git says `cow` is not a git command, add `~/.cargo/bin` to your `PATH`:

```bash
export PATH="$HOME/.cargo/bin:$PATH"   # in your shell profile
```

For a different name, add an alias: `git config --global alias.wt '!git-cow'` makes `git wt add -b feat/x` work.

## Commands

| Command | What it does |
|---|---|
| `git cow add [-b <branch>] [<path>] [<commit-ish>]` | Creates a worktree. Same arguments as `git worktree add`, and the path is optional |
| `git cow list` | Lists your worktrees and their branches |
| `git cow remove [--warm] [--force] <worktree>` | Removes a worktree and deletes its branch if merged. `--warm` first gives its build output to the extra checkout, `--force` removes it with uncommitted changes. It never removes the extra checkout or your main checkout. Also `rm` |
| `git cow warm <worktree>` | Gives a worktree's build output to the extra checkout, and keeps the worktree |
| `git cow share [--global] [<dir>...]` | Lists the hidden folders that are shared, or adds one |

These are normal git worktrees, so `git worktree list` and `git worktree remove` work too. `git worktree list` also shows the extra checkout. If you remove it by hand, the next `add` makes a new one.

## How it works

1. git-cow keeps one extra checkout per repository, at `origin/HEAD`. It is a git worktree of your repository, so it uses your `.git` and the history is not stored twice. git-cow makes it by cloning your checkout and removing your edits and untracked files, so it takes disk only for the files that differ from `origin/HEAD`.
2. The extra checkout gets copy-on-write clones of your checkout's ignored folders, such as `target/` and `node_modules`. This runs at most once an hour by default (`cow.warmInterval`).
3. A new worktree is a copy-on-write clone of the extra checkout. macOS clones each folder in one `clonefile(2)` call. Linux uses `cp --reflink=always`.
4. Git then rewrites only the files where your branch differs from the extra checkout.

If cloning is not possible, git-cow cleans up and runs plain `git worktree add` with your arguments.

Loose ignored files such as `.env` are never copied. Hidden folders are copied only when they are on the share list, because most of them hold tool or agent settings (`.claude`, `.codex`, `.aws`). The list starts with these build caches: `.uv-cache`, `.next`, `.turbo`, `.nuxt`, `.svelte-kit`, `.parcel-cache`, `.gradle`, `.mypy_cache`, `.ruff_cache` and `.pytest_cache`. Add more with `git cow share <dir>`. A `.venv` only works after a clone if it was created relocatable, because it stores absolute paths, so it is not on the list.

## Configuration

All settings are git config, per repository or `--global`. In a path, `{repo}` becomes the repository's name.

| Setting | Default | What it sets |
|---|---|---|
| `cow.root` | `~/.worktrees` | Where `add` puts a worktree when you give no path: `<root>/<repo>/<branch>`. With `{repo}` in it, `<root>/<branch>` |
| `cow.base` | `<worktree's parent>/.git-cow/<repo>` | Where the extra checkout lives |
| `cow.share` | | One extra hidden folder to share, per entry |
| `cow.warmInterval` | `1h` | How often `add` refreshes the extra checkout from your checkout: `0` (always), `30m`, `2h`, `never` |
| `GIT_COW=0` (environment) | | Turns cloning off |

The extra checkout sits next to the worktree, so tools with different layouts each get their own: `~/.worktrees/<repo>/.git-cow/<repo>` for `git cow add`, `<repo>/.claude/worktrees/.git-cow/<repo>` for Claude Code. Another extra checkout takes almost no disk, but each is refreshed on its own. To share one extra checkout per repository across tools:

```bash
git config --global cow.base '~/.orch/state/templates/{repo}'
```

Your checkout, the extra checkout and the worktrees must sit on one volume that supports cloning: APFS on macOS, btrfs or XFS formatted with `reflink=1` on Linux. On ext4 and HFS+, git-cow falls back to `git worktree add`.

## Library

```rust
git_cow::add(repo, checkout, "origin/main", worktree, Some("feat/x"), None)?;
git_cow::warm(merged_worktree, checkout)?;

let opts = git_cow::Options { share: vec![".venv".into()], ..Default::default() };
git_cow::add_with(repo, checkout, "origin/main", worktree, Some("feat/x"), None, &opts)?;
```

`Options::from_repo(repo)` returns the options the CLI uses: the defaults plus `cow.share` and `cow.warmInterval` from git config.

The library is synchronous and has no dependencies. A lock file next to the extra checkout stops two clones, from threads or processes, from seeing it half updated.

## Benchmarks

One run each on an Apple Silicon Mac (APFS). Disk counts the bytes written after the worktree was made, because `du` counts every clone as a full copy.

orch (Rust), then `cargo test --no-run`:

| | Create | Build | Disk |
|---|---|---|---|
| `git worktree add` | 0.3 s | 45.6 s | 1.5 GB |
| `git cow add` | 0.8 s | 31.7 s | 0.7 GB |

The dependencies come from the extra checkout. Cargo still rebuilds the repository's own crates.

bean (Python), then `uv sync`:

| | `uv sync` | Disk |
|---|---|---|
| `git worktree add`, new uv cache | 37.5 s | 2.8 GB |
| `git cow add`, `.uv-cache` cloned from the extra checkout | 5.7 s | 0.02 GB |

## Costs

- A cloned file's blocks are copied the first time either side writes to it.
- Making the extra checkout the first time is slower than a plain checkout, about 30 s on a repository with large data folders, because the clone is cleaned before the build folders come back.
- Clones have new inodes, so git re-hashes every tracked file once. That took about 3 s on a repository with large tracked data.
- A big shared cache still takes time to clone: about 4 s for a 6.7 GB uv cache.
- The extra checkout can be up to an hour behind your checkout, so a new worktree may rebuild a little more.
- Tools that store absolute paths, such as virtualenvs, point at the extra checkout after a clone.
- A folder where every file is ignored is shared whole, hidden subfolders included, because git lists it as one entry.
- Repositories with submodules always fall back.

## Origin

Extracted from [orch](https://github.com/gabrielkoerich/orch), where every agent task gets a worktree.

MIT licensed.
