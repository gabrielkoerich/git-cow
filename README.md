# git-cow

[![CI](https://github.com/gabrielkoerich/git-cow/actions/workflows/ci.yml/badge.svg)](https://github.com/gabrielkoerich/git-cow/actions/workflows/ci.yml) [![Release](https://img.shields.io/github/v/release/gabrielkoerich/git-cow)](https://github.com/gabrielkoerich/git-cow/releases/latest) [![crates.io](https://img.shields.io/crates/v/git-cow)](https://crates.io/crates/git-cow) [![docs.rs](https://img.shields.io/docsrs/git-cow)](https://docs.rs/git-cow) [![License](https://img.shields.io/crates/l/git-cow)](LICENSE)

`git worktree add` that clones files instead of copying them. Every new worktree is a copy-on-write clone of a template checkout, so `target/`, `node_modules` and other build output are shared on disk until a build rewrites them. A worktree that would cost gigabytes starts at a few megabytes and builds incrementally.

```bash
git cow add -b feat/login                 # ~/.worktrees/<repo>/login
git cow add -b feat/login ../login        # any path, like git worktree add
git cow warm ~/.worktrees/<repo>/login    # after a merge, share its build output with the next worktree
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

## How it works

1. Keep one template checkout per repository at `<worktree's parent>/.git-cow/<repo>`, moved to `origin/HEAD` before each clone.
2. Clone the gitignored directories of your checkout into the template: build output and dependencies, never loose ignored files like `.env`. A hidden directory is shared only when it is on the share list, since the rest (`.claude`, `.codex`, `.aws`) is tool or agent config.
3. `git worktree add --no-checkout`, then clone every template entry except `.git` into it. macOS clones each whole directory with one `clonefile(2)` call, Linux uses `cp --reflink=always` (FICLONE).
4. Copy the template's index, refresh it, and `git reset --hard`, which rewrites only the files where your branch differs from the template.

If the volume cannot clone, or anything fails, it removes what it made and runs plain `git worktree add` with the same arguments.

## Configuration

Hidden directories shared by default, all pure build caches: `.uv-cache`, `.next`, `.turbo`, `.nuxt`, `.svelte-kit`, `.parcel-cache`, `.gradle`, `.mypy_cache`, `.ruff_cache`, `.pytest_cache`. Add your own with `git cow share <dir>`, or `--global` for every repository. A virtualenv only works after a clone if it was created relocatable, since it stores absolute paths.


| | Default | |
|---|---|---|
| `git config cow.root` | `~/.worktrees` | Where a worktree goes when the path is left out: `<root>/<repo>/<last part of the branch>` |
| `git config cow.template` | `<worktree's parent>/.git-cow/<repo>` | The template checkout |
| `git config cow.share` | | A hidden directory to share on top of the built-in caches, one entry each. `git cow share .venv` adds one, `git cow share` lists them |
| `GIT_COW=0` | | Plain `git worktree add` |

The checkout, the template and the worktrees must be on one volume that clones: APFS on macOS, XFS formatted with `reflink=1` or btrfs on Linux. ext4 and HFS+ always fall back.

## Library

```rust
git_cow::add(repo, template, "origin/main", worktree, Some("feat/x"), None)?;
git_cow::warm(merged_worktree, template)?;
```

Synchronous and without dependencies. A lock file next to the template keeps concurrent clones, from threads or processes, from seeing it half updated.

## Costs

- A cloned file's blocks are copied the first time either side writes it.
- `git update-index --refresh` re-hashes every tracked file, since clones have new inodes. That grows with the number of tracked files.
- Output that embeds absolute paths, such as virtualenvs, points at the template. Whether the tool recovers is up to the tool.
- A directory where everything is ignored is shared whole, hidden subdirectories included, because git reports it as one entry.
- The template is one more checkout on disk and shows in `git worktree list`.
- Repositories with submodules always fall back.

## Origin

Extracted from [orch](https://github.com/gabrielkoerich/orch), where every agent task gets a worktree. Measured there on APFS: a warmed clone built in 55 to 72 s against 73 s for a plain worktree, and wrote about 1 GB less per task.

MIT licensed.
