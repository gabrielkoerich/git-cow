---
name: git-cow
description: >
  Create, list, warm, and remove Git worktrees with git cow only. A worktree
  you create goes in ~/.worktrees/<project>/. A worktree Claude Code already
  created under <project>/.claude/worktrees uses the same git cow commands.
  Use this for every worktree or isolated branch directory. Never call
  git worktree yourself. The git-worktrees skill follows this one for a
  feature branch and a pull request. A GitHub issue uses
  skills/github-issue-worktree/SKILL.md in this repository. Use when the
  user runs /git-cow.
---

# Worktrees

Call `git cow` only. Run it from a checkout of the repository. Never run `git worktree`.

A feature branch and a pull request are the git-worktrees skill. A GitHub issue is `skills/github-issue-worktree/SKILL.md` in this repository. Both follow this skill for the worktree.

`git cow` is the `git-cow` binary. Git runs it when `~/.cargo/bin` is on `PATH`. If `git cow` is not a command, install it:

```bash
cargo install git-cow
export PATH="$HOME/.cargo/bin:$PATH"
```

Stop if the install fails. Tell the user.

If the disk cannot clone files, or the repository has submodules, `git cow` still does the work. You still call `git cow` only.

## Where it goes

A worktree you create goes in `~/.worktrees/<project>/`.

Set `branch` to the branch name first. `<project>` is the main checkout's directory name. `<leaf>` is `branch` after the last `/`. `feat/login` goes to `~/.worktrees/<project>/login`. The main checkout is `dirname` of the common git dir, so this stays correct when you are already inside a worktree.

```bash
common=$(git rev-parse --path-format=absolute --git-common-dir)
project=$(basename "$(dirname "$common")")
leaf=${branch##*/}
root=$(git config --get cow.root || true)
root=${root:-$HOME/.worktrees}
root=${root/#\~/$HOME}
case "$root" in
  *'{repo}'*) path="${root//\{repo\}/$project}/$leaf" ;;
  *) path="$root/$project/$leaf" ;;
esac
```

`cow.root` replaces `~/.worktrees` when it is set. Expand a leading `~/`. A value that contains `{repo}` already names the project folder, so append only `/<leaf>`.

Claude Code keeps its own worktrees in the main checkout at `.claude/worktrees/<name>`, on branch `worktree-<name>`. That path is `"$(dirname "$common")/.claude/worktrees/<name>"`. The WorktreeCreate hook runs `git cow add` there. Do not create that directory yourself. When `git cow list` shows one, or your current directory is already inside one, keep that path for every later `git cow` command.

## Create

A branch can be checked out in one worktree. List first. Reuse a row that already has the branch, whichever directory it is in:

```bash
git cow list
```

Each line is `<path><tab><branch>`.

Start a new branch at the current `HEAD`. Pass the path from above. The command prints the worktree path. `cd` there.

```bash
git cow add -b "$branch" "$path"
```

To start from another commit, fetch it, then pass it after the path. A pathless command has no place for a commit.

```bash
git fetch origin "$base"
git cow add -b "$branch" "$path" "origin/$base"
```

Check out a branch that already exists. The path comes first. One argument is a path, not a branch name.

```bash
git cow add "$path" "$branch"
```

Pass a path outside `~/.worktrees/<project>/` only when the user named one, or when the branch is already under `<project>/.claude/worktrees`. If the path exists, stop and tell the user. Do not add a numeric suffix.

`-b` is the only flag `add` handles. Any other flag skips the clone.

## Remove

Go to the main checkout first. Use the path from `git cow list`. Give that worktree's build output to the extra checkout, then remove the worktree:

```bash
git cow remove --warm "$path"
```

This deletes the branch when it is merged. It keeps an unmerged branch and prints `kept branch`. Use `--force` only after the user agrees to discard uncommitted changes in that worktree.

Leave a worktree under `<project>/.claude/worktrees` in place, unless the user asked to remove that one. Claude Code removes its own through the WorktreeRemove hook.

`git cow warm "$path"` updates the extra checkout and leaves the worktree in place. Run it when the next worktree should start from this build and this worktree stays.

## Leave the extra checkout alone

git-cow keeps one extra checkout next to each directory of worktrees, at `<parent>/.git-cow/<project>`, on `origin/HEAD`.

For the default directory, that is `~/.worktrees/<project>/.git-cow/<project>`. For Claude Code, that is `<project>/.claude/worktrees/.git-cow/<project>`.

`git cow list` hides these. `git cow remove` refuses them, and refuses the main checkout. Leave them in place. The next `git cow add` in that directory depends on its extra checkout.

## What gets shared

Ignored directories such as `target/` and `node_modules` are cloned into the new worktree. Loose ignored files such as `.env` stay out.

A hidden directory is cloned only when it is on the share list. The built-in names are `.uv-cache`, `.next`, `.turbo`, `.nuxt`, `.svelte-kit`, `.parcel-cache`, `.gradle`, `.mypy_cache`, `.ruff_cache`, and `.pytest_cache`. Add one with `git cow share <name>`. The name starts with `.` and contains no slash. Share `.venv` only when that virtualenv was created relocatable, because it stores absolute paths.
