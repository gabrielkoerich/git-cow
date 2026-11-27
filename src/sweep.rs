//! `git cow cleanup` and `git cow tree`: the plan, the question and the output around
//! `git_cow::cleanup`, which does the judging and the removing

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use git_cow::cleanup::{self, Action, Done, Item, Keep, Options, State};

use crate::other;

// One tab separated line: what happens, how far the work landed, the branch, the worktree path
fn line(item: &Item) -> String {
    let verb = match (item.action(), item.path()) {
        (Action::Remove, Some(_)) if item.discards_changes() => {
            "remove (discards changes)".to_string()
        }
        (Action::Remove, Some(_)) if item.deletes_branch() || item.branch().is_none() => {
            "remove".to_string()
        }
        (Action::Remove, Some(_)) => "remove (keeps branch)".to_string(),
        (Action::Remove, None) => "delete".to_string(),
        (Action::Keep(Keep::NotLanded), _) => "keep".to_string(),
        (Action::Keep(why), _) => format!("keep ({why})"),
        _ => "keep".to_string(),
    };
    let branch = item.branch().unwrap_or("(detached)");
    let path = item
        .path()
        .map_or_else(String::new, |p| p.display().to_string());
    format!("{verb}\t{}\t{branch}\t{path}", item.state().label())
        .trim_end()
        .to_string()
}

fn short(oid: &str) -> &str {
    oid.get(..12).unwrap_or(oid)
}

struct Mode {
    dry_run: bool,
    yes: bool,
}

fn confirm(mode: &Mode) -> io::Result<bool> {
    if mode.dry_run {
        return Ok(false);
    }
    if mode.yes {
        return Ok(true);
    }
    if !io::stdin().is_terminal() {
        println!("pass --yes to remove them");
        return Ok(false);
    }
    print!("remove them? [y/N] ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

fn warn_without_squash_check() {
    if !cleanup::squash_check_works() {
        eprintln!(
            "git-cow: this git is older than 2.40, so squash-merged branches show as active and stay"
        );
    }
}

/// Remove worktrees and delete local branches whose work is in the base, after showing the plan
pub(crate) fn cleanup(args: &[String]) -> io::Result<()> {
    let mut mode = Mode {
        dry_run: false,
        yes: false,
    };
    let mut opts = Options::default();
    for a in args {
        match a.as_str() {
            "-n" | "--dry-run" => mode.dry_run = true,
            "-y" | "--yes" => mode.yes = true,
            "-f" | "--force" => opts.force = true,
            s if s.starts_with('-') => return Err(other(format!("unknown option {s}"))),
            s if opts.base.is_none() => opts.base = Some(s.to_string()),
            _ => return Err(other("give at most one branch to compare with".into())),
        }
    }
    warn_without_squash_check();
    let plan = cleanup::plan(&std::env::current_dir()?, &opts)?;
    for item in plan.items() {
        println!("{}", line(item));
    }
    let removals: Vec<&Item> = plan
        .items()
        .iter()
        .filter(|i| matches!(i.action(), Action::Remove))
        .collect();
    let against = format!("against {} ({})", plan.base().name, short(&plan.base().oid));
    if removals.is_empty() {
        println!("nothing to clean up {against}");
        return Ok(());
    }
    let worktrees = removals.iter().filter(|i| i.path().is_some()).count();
    let branches = removals.iter().filter(|i| i.deletes_branch()).count();
    let wt_noun = if worktrees == 1 {
        "worktree"
    } else {
        "worktrees"
    };
    let br_noun = if branches == 1 { "branch" } else { "branches" };
    println!("{worktrees} {wt_noun} and {branches} local {br_noun} to remove, {against}");
    if !confirm(&mode)? {
        return Ok(());
    }
    let mut failed = 0;
    for done in cleanup::execute(&plan) {
        match done {
            Done::Removed(path) => println!("removed {}", path.display()),
            Done::Deleted {
                branch,
                oid,
                backup,
            } => println!(
                "deleted {branch} (was {}, git branch {branch} {backup} restores it)",
                short(&oid)
            ),
            Done::Failed { what, why } => {
                eprintln!("git-cow: kept {what}: {why}");
                failed += 1;
            }
            _ => {}
        }
    }
    if failed > 0 {
        return Err(other(format!("{failed} could not be removed")));
    }
    Ok(())
}

// Repositories at or below `dir`, not looking inside one or inside hidden and build folders
fn repositories(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), 0)];
    while let Some((d, depth)) = stack.pop() {
        let bare =
            d.join("HEAD").is_file() && d.join("objects").is_dir() && d.join("refs").is_dir();
        if d.join(".git").is_dir() || bare {
            found.push(d);
            continue;
        }
        if depth == 4 {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            if is_dir && !name.starts_with('.') && name != "node_modules" && name != "target" {
                stack.push((entry.path(), depth + 1));
            }
        }
    }
    found.sort();
    found
}

/// List the worktrees of every repository at or below a folder, with how far their work landed
pub(crate) fn tree(args: &[String]) -> io::Result<()> {
    let dir = match args {
        [] => std::env::current_dir()?,
        [d] if !d.starts_with('-') => PathBuf::from(d),
        _ => return Err(other("give one folder".into())),
    };
    warn_without_squash_check();
    for repo in repositories(&std::fs::canonicalize(&dir)?) {
        let Ok(worktrees) = cleanup::survey(&repo) else {
            continue;
        };
        if worktrees.is_empty() {
            continue;
        }
        let landed = worktrees
            .iter()
            .filter(|w| w.state.is_some_and(State::landed))
            .count();
        let n = worktrees.len();
        let noun = if n == 1 { "worktree" } else { "worktrees" };
        println!(
            "{}  ({n} {noun}, {landed} merged or squashed)",
            repo.display()
        );
        for wt in worktrees {
            let label = wt.state.map_or("?", State::label);
            let branch = wt.branch.as_deref().unwrap_or("(detached)");
            println!("  {label}\t{branch}\t{}", wt.path.display());
        }
    }
    Ok(())
}
