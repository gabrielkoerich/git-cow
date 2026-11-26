//! `git cow cleanup` and `git cow tree`: find worktrees and branches whose work already landed

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use git_cow::git;

use crate::{is_template, other};

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Merged,
    Squashed,
    Gone,
    Active,
}

impl State {
    fn label(self) -> &'static str {
        match self {
            State::Merged => "merged",
            State::Squashed => "squashed",
            State::Gone => "gone",
            State::Active => "active",
        }
    }

    fn landed(self) -> bool {
        matches!(self, State::Merged | State::Squashed)
    }
}

struct Worktree {
    path: PathBuf,
    branch: Option<String>,
    head: String,
    locked: bool,
}

// Every linked worktree of `repo`, without the main checkout, which `git worktree list` puts first
fn linked_worktrees(repo: &Path) -> io::Result<Vec<Worktree>> {
    let out = git(repo, &["worktree", "list", "--porcelain"])?;
    let mut list = Vec::new();
    for block in out.split("\n\n").skip(1) {
        let mut wt = Worktree {
            path: PathBuf::new(),
            branch: None,
            head: String::new(),
            locked: false,
        };
        for line in block.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                wt.path = PathBuf::from(p);
            } else if let Some(h) = line.strip_prefix("HEAD ") {
                wt.head = h.to_string();
            } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
                wt.branch = Some(b.to_string());
            } else if line == "locked" || line.starts_with("locked ") {
                wt.locked = true;
            }
        }
        if !wt.path.as_os_str().is_empty() && !is_template(repo, &wt.path) {
            list.push(wt);
        }
    }
    Ok(list)
}

// The branch work lands on: the one named, or the remote's default, or a local main
fn base(repo: &Path, named: Option<&str>) -> io::Result<String> {
    let candidates: Vec<&str> = match named {
        Some(b) => vec![b],
        None => vec![
            "origin/HEAD",
            "origin/main",
            "origin/master",
            "main",
            "master",
        ],
    };
    candidates
        .into_iter()
        .find(|b| {
            git(
                repo,
                &["rev-parse", "--verify", "-q", &format!("{b}^{{commit}}")],
            )
            .is_ok()
        })
        .map(String::from)
        .ok_or_else(|| {
            other("no branch to compare with, name one: git cow cleanup <branch>".into())
        })
}

// git-extras' git-delete-squashed-branches check: a commit with `rev`'s tree on its merge base is
// squash-merged when `git cherry` finds the same patch in `base`
fn state(repo: &Path, base: &str, rev: &str, branch: Option<&str>) -> State {
    if git(repo, &["merge-base", "--is-ancestor", rev, base]).is_ok() {
        return State::Merged;
    }
    let squashed = (|| {
        let merge_base = git(repo, &["merge-base", base, rev]).ok()?;
        let tree = git(repo, &["rev-parse", &format!("{rev}^{{tree}}")]).ok()?;
        // unsigned, or a signing agent would ask once per branch
        let probe = git(
            repo,
            &[
                "commit-tree",
                "--no-gpg-sign",
                &tree,
                "-p",
                &merge_base,
                "-m",
                "_",
            ],
        )
        .ok()?;
        Some(git(repo, &["cherry", base, &probe]).ok()?.starts_with('-'))
    })();
    if squashed == Some(true) {
        return State::Squashed;
    }
    let gone = branch.is_some_and(|b| {
        git(
            repo,
            &[
                "for-each-ref",
                "--format=%(upstream:track)",
                &format!("refs/heads/{b}"),
            ],
        )
        .is_ok_and(|t| t == "[gone]")
    });
    if gone { State::Gone } else { State::Active }
}

fn has_changes(path: &Path) -> bool {
    path.exists() && git(path, &["status", "--porcelain"]).is_ok_and(|s| !s.is_empty())
}

enum Action {
    Remove,
    Keep(&'static str),
}

struct Item {
    path: Option<PathBuf>,
    branch: Option<String>,
    state: State,
    action: Action,
    forced: Force,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Force {
    No,
    Yes,
}

fn plan(repo: &Path, base: &str, force: Force) -> io::Result<Vec<Item>> {
    let worktrees = linked_worktrees(repo)?;
    let mut items = Vec::new();
    for wt in &worktrees {
        let rev = wt.branch.as_deref().unwrap_or(&wt.head);
        let state = state(repo, base, rev, wt.branch.as_deref());
        let changes = has_changes(&wt.path);
        let action = if !state.landed() {
            Action::Keep("")
        } else if wt.locked {
            Action::Keep("locked")
        } else if changes && force == Force::No {
            Action::Keep("has changes")
        } else {
            Action::Remove
        };
        let forced = if changes || !wt.path.exists() {
            Force::Yes
        } else {
            Force::No
        };
        items.push(Item {
            path: Some(wt.path.clone()),
            branch: wt.branch.clone(),
            state,
            action,
            forced,
        });
    }
    // Branches with no worktree, never one checked out anywhere or the base itself
    let main_branch = git(repo, &["symbolic-ref", "-q", "--short", "HEAD"]).ok();
    let base_local = base.strip_prefix("origin/").unwrap_or(base);
    let refs = git(
        repo,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads/"],
    )?;
    for b in refs.lines() {
        let checked_out = worktrees.iter().any(|w| w.branch.as_deref() == Some(b))
            || main_branch.as_deref() == Some(b);
        if checked_out || b == base_local || b == base {
            continue;
        }
        let state = state(repo, base, b, Some(b));
        items.push(Item {
            path: None,
            branch: Some(b.to_string()),
            state,
            action: if state.landed() {
                Action::Remove
            } else {
                Action::Keep("")
            },
            forced: Force::No,
        });
    }
    Ok(items)
}

fn line(item: &Item) -> String {
    let verb = match (&item.action, &item.path) {
        (Action::Remove, Some(_)) => "remove".to_string(),
        (Action::Remove, None) => "delete".to_string(),
        (Action::Keep(""), _) => "keep".to_string(),
        (Action::Keep(why), _) => format!("keep ({why})"),
    };
    let branch = item.branch.as_deref().unwrap_or("(detached)");
    let path = item
        .path
        .as_ref()
        .map_or_else(String::new, |p| p.display().to_string());
    format!("{verb:<18} {:<9} {branch}  {path}", item.state.label())
        .trim_end()
        .to_string()
}

struct Mode {
    dry_run: bool,
    yes: bool,
}

/// Remove worktrees and delete branches whose work is in the base, after showing the plan
pub(crate) fn cleanup(args: &[String]) -> io::Result<()> {
    let repo = std::env::current_dir()?;
    let mut mode = Mode {
        dry_run: false,
        yes: false,
    };
    let mut force = Force::No;
    let mut named = None;
    for a in args {
        match a.as_str() {
            "-n" | "--dry-run" => mode.dry_run = true,
            "-y" | "--yes" => mode.yes = true,
            "-f" | "--force" => force = Force::Yes,
            s if s.starts_with('-') => return Err(other(format!("unknown option {s}"))),
            s if named.is_none() => named = Some(s),
            _ => return Err(other("give at most one branch to compare with".into())),
        }
    }
    let base = base(&repo, named)?;
    let items = plan(&repo, &base, force)?;
    for item in &items {
        println!("{}", line(item));
    }
    let removals: Vec<&Item> = items
        .iter()
        .filter(|i| matches!(i.action, Action::Remove))
        .collect();
    let worktrees = removals.iter().filter(|i| i.path.is_some()).count();
    let branches = removals.iter().filter(|i| i.branch.is_some()).count();
    if removals.is_empty() {
        println!("nothing to clean up against {base}");
        return Ok(());
    }
    println!("{worktrees} worktrees and {branches} branches to remove, against {base}");
    if mode.dry_run {
        return Ok(());
    }
    if !mode.yes {
        if !io::stdin().is_terminal() {
            println!("pass --yes to remove them");
            return Ok(());
        }
        print!("remove them? [y/N] ");
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().lock().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            return Ok(());
        }
    }
    let mut failed = 0;
    for item in removals {
        if let Some(path) = &item.path {
            let p = path.to_string_lossy();
            let mut cmd = vec!["worktree", "remove"];
            if item.forced == Force::Yes {
                cmd.push("--force");
            }
            cmd.push(&p);
            if let Err(e) = git(&repo, &cmd) {
                eprintln!("git-cow: kept {p}: {e}");
                failed += 1;
                continue;
            }
            println!("removed {p}");
        }
        if let Some(b) = &item.branch {
            match git(&repo, &["branch", "-D", b]) {
                Ok(_) => println!("deleted {b}"),
                Err(e) => {
                    eprintln!("git-cow: kept branch {b}: {e}");
                    failed += 1;
                }
            }
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
        if d.join(".git").is_dir() {
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
        [d] => PathBuf::from(d),
        _ => return Err(other("give one folder".into())),
    };
    for repo in repositories(&std::fs::canonicalize(&dir)?) {
        let Ok(worktrees) = linked_worktrees(&repo) else {
            continue;
        };
        if worktrees.is_empty() {
            continue;
        }
        let base = base(&repo, None).ok();
        let states: Vec<Option<State>> = worktrees
            .iter()
            .map(|w| {
                let rev = w.branch.as_deref().unwrap_or(&w.head);
                base.as_deref()
                    .map(|b| state(&repo, b, rev, w.branch.as_deref()))
            })
            .collect();
        let landed = states.iter().flatten().filter(|s| s.landed()).count();
        let n = worktrees.len();
        let noun = if n == 1 { "worktree" } else { "worktrees" };
        println!(
            "{}  ({n} {noun}, {landed} merged or squashed)",
            repo.display()
        );
        for (wt, state) in worktrees.iter().zip(states) {
            let label = state.map_or("?", State::label);
            let branch = wt.branch.as_deref().unwrap_or("(detached)");
            let changes = if has_changes(&wt.path) {
                "  (has changes)"
            } else {
                ""
            };
            println!("  {label:<9} {branch}  {}{changes}", wt.path.display());
        }
    }
    Ok(())
}
