//! `git cow cleanup` and `git cow tree`: find worktrees and branches whose work already landed

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use git_cow::REPO_ENV;

use crate::{is_template, other};

// Ahead of every git call here: no fsmonitor program from a repository's own config, and no index
// refresh written back, so reading a repository never runs or changes anything in it
const QUIET: &[&str] = &["-c", "core.fsmonitor=false", "--no-optional-locks"];

// Diffs compared byte for byte, without a diff program or textconv filter a repository may set
const DIFF: &[&str] = &[
    "--no-ext-diff",
    "--no-textconv",
    "--binary",
    "--full-index",
    "--no-color",
];

fn git_cmd(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    for var in REPO_ENV {
        cmd.env_remove(var);
    }
    cmd.arg("-C").arg(dir).args(QUIET);
    cmd
}

fn run(dir: &Path, args: &[&str]) -> io::Result<String> {
    let out = git_cmd(dir).args(args).stderr(Stdio::piped()).output()?;
    if !out.status.success() {
        return Err(other(format!(
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

// `git patch-id --verbatim` of what `args` prints, fed through a pipe so no byte is changed.
// Whitespace counts, so a later fix to indentation is never taken for the squash that landed
fn patch_ids(dir: &Path, args: &[&str], input: Option<String>) -> io::Result<String> {
    let mut producer = git_cmd(dir)
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let writer = match (input, producer.stdin.take()) {
        (Some(text), Some(mut stdin)) => {
            Some(std::thread::spawn(move || stdin.write_all(text.as_bytes())))
        }
        _ => None,
    };
    let diff = producer
        .stdout
        .take()
        .ok_or_else(|| other("git gave no output pipe".into()))?;
    let out = git_cmd(dir)
        .args(["patch-id", "--verbatim"])
        .stdin(diff)
        .stderr(Stdio::null())
        .output()?;
    if let Some(w) = writer {
        let _ = w.join();
    }
    if !producer.wait()?.success() || !out.status.success() {
        return Err(other(format!("git {args:?} | git patch-id failed")));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

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

// The main worktree, or the bare repository, and every linked worktree after it
fn worktrees(dir: &Path) -> io::Result<(Worktree, Vec<Worktree>)> {
    let out = run(dir, &["worktree", "list", "--porcelain"])?;
    let mut all = out.split("\n\n").map(|block| {
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
        wt
    });
    let main = all
        .next()
        .ok_or_else(|| other("git lists no worktree".into()))?;
    Ok((main, all.collect()))
}

struct Base {
    name: String,
    oid: String,
    // the local branch the base stands for, which cleanup never deletes
    local: Option<String>,
}

// The branch work lands on: the one named, or the remote's default, or a local main. A remote's
// HEAD is followed to the branch it names, and the plan prints that name
fn base(repo: &Path, named: Option<&str>) -> io::Result<Base> {
    let candidates: Vec<String> = match named {
        Some(b) => vec![
            run(repo, &["rev-parse", "--symbolic-full-name", b])
                .ok()
                .filter(|f| !f.is_empty())
                .unwrap_or_else(|| b.to_string()),
        ],
        None => [
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
            "refs/remotes/origin/master",
            "refs/heads/main",
            "refs/heads/master",
        ]
        .map(String::from)
        .to_vec(),
    };
    for c in candidates {
        let full = run(repo, &["symbolic-ref", "-q", &c]).unwrap_or(c);
        let Ok(oid) = run(
            repo,
            &["rev-parse", "--verify", "-q", &format!("{full}^{{commit}}")],
        ) else {
            continue;
        };
        let local = if let Some(rest) = full.strip_prefix("refs/remotes/") {
            rest.split_once('/').map(|(_, b)| b.to_string())
        } else {
            full.strip_prefix("refs/heads/").map(String::from)
        };
        let name = full
            .strip_prefix("refs/remotes/")
            .or_else(|| full.strip_prefix("refs/heads/"))
            .unwrap_or(&full)
            .to_string();
        return Ok(Base { name, oid, local });
    }
    Err(other(
        "no branch to compare with, name one: git cow cleanup <branch>".into(),
    ))
}

// The patch id of everything `rev` changed since its merge base with the base, and the base
// commits after that merge base. None for an ancestor, or a rev that changes nothing in total
fn squash_probe(repo: &Path, base: &str, rev: &str) -> Option<(String, String)> {
    if run(repo, &["merge-base", "--is-ancestor", rev, base]).is_ok() {
        return None;
    }
    let mb = run(repo, &["merge-base", base, rev]).ok()?;
    let mut args = vec!["diff"];
    args.extend(DIFF);
    args.extend([mb.as_str(), rev]);
    let own = patch_ids(repo, &args, None).ok()?;
    let own = own.split_whitespace().next()?.to_string();
    let commits = run(repo, &["rev-list", "--no-merges", base, &format!("^{mb}")]).ok()?;
    Some((own, commits))
}

// Patch ids of `commits`, all in one `git log`. An empty commit has none, so it matches nothing
fn commit_patch_ids(repo: &Path, commits: &HashSet<&str>) -> HashMap<String, String> {
    let mut by_commit = HashMap::new();
    if commits.is_empty() {
        return by_commit;
    }
    let mut args = vec![
        "log",
        "-p",
        "--no-walk=unsorted",
        "--stdin",
        "--format=commit %H",
    ];
    args.extend(DIFF);
    let input = commits.iter().fold(String::new(), |mut list, c| {
        list.push_str(c);
        list.push('\n');
        list
    });
    if let Ok(out) = patch_ids(repo, &args, Some(input)) {
        for line in out.lines() {
            if let Some((pid, commit)) = line.split_once(' ') {
                by_commit.insert(commit.to_string(), pid.to_string());
            }
        }
    }
    by_commit
}

// Merged is an ancestor of the base. Squashed is git-extras' check with whitespace counted: the
// whole change since the merge base has the patch id of one base commit after it
fn states(repo: &Path, base: &Base, revs: &[(String, Option<String>)]) -> Vec<State> {
    let probes: Vec<Option<(String, String)>> = revs
        .iter()
        .map(|(rev, _)| squash_probe(repo, &base.oid, rev))
        .collect();
    let wanted: HashSet<&str> = probes
        .iter()
        .flatten()
        .flat_map(|(_, commits)| commits.lines())
        .collect();
    let by_commit = commit_patch_ids(repo, &wanted);
    revs.iter()
        .zip(&probes)
        .map(|((rev, branch), probe)| {
            if run(repo, &["merge-base", "--is-ancestor", rev, &base.oid]).is_ok() {
                return State::Merged;
            }
            let squashed = probe.as_ref().is_some_and(|(own, commits)| {
                commits
                    .lines()
                    .any(|c| by_commit.get(c).is_some_and(|pid| pid == own))
            });
            if squashed {
                return State::Squashed;
            }
            let gone = branch.as_deref().is_some_and(|b| {
                run(
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
        })
        .collect()
}

// Edits, staged changes and every untracked file, whatever status.showUntrackedFiles says
fn has_changes(path: &Path) -> bool {
    path.exists()
        && run(
            path,
            &[
                "status",
                "--porcelain",
                "--untracked-files=all",
                "--ignore-submodules=none",
            ],
        )
        .is_ok_and(|s| !s.is_empty())
}

fn rev_of(w: &Worktree) -> (String, Option<String>) {
    let rev = w
        .branch
        .as_ref()
        .map_or_else(|| w.head.clone(), |b| format!("refs/heads/{b}"));
    (rev, w.branch.clone())
}

enum Action {
    Remove,
    Keep(&'static str),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Force {
    No,
    Yes,
}

struct Item {
    path: Option<PathBuf>,
    branch: Option<String>,
    // the commit the plan judged, so the run skips anything that moved since
    oid: String,
    state: State,
    action: Action,
    // whether removing this worktree also deletes its branch
    branch_too: bool,
    forced: Force,
}

struct Plan {
    repo: PathBuf,
    base: Base,
    items: Vec<Item>,
}

fn worktree_items(
    repo: &Path,
    base: &Base,
    linked: &[Worktree],
    protected: &HashSet<String>,
    here: &Path,
    force: Force,
) -> Vec<Item> {
    let revs: Vec<(String, Option<String>)> = linked.iter().map(rev_of).collect();
    let mut items = Vec::new();
    for (wt, state) in linked.iter().zip(states(repo, base, &revs)) {
        let changes = has_changes(&wt.path);
        let inside = here.starts_with(&wt.path)
            || std::fs::canonicalize(&wt.path).is_ok_and(|p| here.starts_with(p));
        let action = if !state.landed() {
            Action::Keep("")
        } else if wt.locked {
            Action::Keep("locked")
        } else if inside {
            Action::Keep("you are in it")
        } else if wt.branch.as_ref().is_some_and(|b| protected.contains(b)) {
            Action::Keep("base branch")
        } else if changes && force == Force::No {
            Action::Keep("has changes")
        } else {
            Action::Remove
        };
        items.push(Item {
            path: Some(wt.path.clone()),
            branch: wt.branch.clone(),
            oid: wt.head.clone(),
            state,
            action,
            branch_too: false,
            forced: if changes || !wt.path.exists() {
                Force::Yes
            } else {
                Force::No
            },
        });
    }
    // A branch goes with the last of its worktrees, and only when every one of them goes
    let mut last: HashMap<String, usize> = HashMap::new();
    for (i, item) in items.iter().enumerate() {
        if let Some(b) = &item.branch {
            last.insert(b.clone(), i);
        }
    }
    for (b, i) in &last {
        let all_go = items
            .iter()
            .filter(|it| it.branch.as_ref() == Some(b))
            .all(|it| matches!(it.action, Action::Remove));
        if all_go && !protected.contains(b) {
            items[*i].branch_too = true;
        }
    }
    items
}

fn plan(dir: &Path, named: Option<&str>, force: Force) -> io::Result<Plan> {
    let (main, linked) = worktrees(dir)?;
    // every call runs from the main worktree, so removing the one you stand in breaks nothing
    let repo = main.path.clone();
    let base = base(&repo, named)?;
    let here = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let mut protected: HashSet<String> = base.local.iter().cloned().collect();
    protected.extend(main.branch);
    protected.extend(run(&repo, &["symbolic-ref", "-q", "--short", "HEAD"]).ok());
    let linked: Vec<Worktree> = linked
        .into_iter()
        .filter(|w| !is_template(&repo, &w.path))
        .collect();
    let mut items = worktree_items(&repo, &base, &linked, &protected, &here, force);

    // Local branches with no worktree, never one checked out anywhere or the base
    let checked_out: HashSet<String> = linked.iter().filter_map(|w| w.branch.clone()).collect();
    let heads = run(
        &repo,
        &["for-each-ref", "--format=%(refname)", "refs/heads/"],
    )?;
    let mut only = Vec::new();
    for full in heads.lines() {
        let Some(b) = full.strip_prefix("refs/heads/") else {
            continue;
        };
        if checked_out.contains(b) || protected.contains(b) {
            continue;
        }
        let oid = run(&repo, &["rev-parse", "--verify", "-q", full])?;
        only.push((b.to_string(), oid));
    }
    let revs: Vec<(String, Option<String>)> = only
        .iter()
        .map(|(b, _)| (format!("refs/heads/{b}"), Some(b.clone())))
        .collect();
    for ((b, oid), state) in only.into_iter().zip(states(&repo, &base, &revs)) {
        items.push(Item {
            path: None,
            branch: Some(b),
            oid,
            state,
            action: if state.landed() {
                Action::Remove
            } else {
                Action::Keep("")
            },
            branch_too: true,
            forced: Force::No,
        });
    }
    Ok(Plan { repo, base, items })
}

// One tab separated line: what happens, how far the work landed, the branch, the worktree path
fn line(item: &Item) -> String {
    let verb = match (&item.action, &item.path) {
        (Action::Remove, Some(_)) if item.branch_too || item.branch.is_none() => {
            "remove".to_string()
        }
        (Action::Remove, Some(_)) => "remove (keeps branch)".to_string(),
        (Action::Remove, None) => "delete".to_string(),
        (Action::Keep(""), _) => "keep".to_string(),
        (Action::Keep(why), _) => format!("keep ({why})"),
    };
    let branch = item.branch.as_deref().unwrap_or("(detached)");
    let path = item
        .path
        .as_ref()
        .map_or_else(String::new, |p| p.display().to_string());
    format!("{verb}\t{}\t{branch}\t{path}", item.state.label())
        .trim_end()
        .to_string()
}

fn short(oid: &str) -> &str {
    oid.get(..12).unwrap_or(oid)
}

// Whether a worktree is still as the plan saw it: same commit, same branch, no new changes
fn unchanged(item: &Item, path: &Path) -> bool {
    if !path.exists() {
        return true;
    }
    let head = run(path, &["rev-parse", "HEAD"]).unwrap_or_default();
    let branch = run(path, &["symbolic-ref", "-q", "--short", "HEAD"]).ok();
    head == item.oid && branch == item.branch && (item.forced == Force::Yes || !has_changes(path))
}

// Delete the local `b` only if it still points at `oid` and no worktree has it checked out now.
// Only refs/heads changes, a remote is never touched
fn delete_branch(repo: &Path, b: &str, oid: &str) -> io::Result<()> {
    let (main, linked) = worktrees(repo)?;
    if std::iter::once(&main)
        .chain(&linked)
        .any(|w| w.branch.as_deref() == Some(b))
    {
        return Err(other(format!("{b} is checked out in a worktree")));
    }
    run(repo, &["update-ref", "-d", &format!("refs/heads/{b}"), oid])?;
    Ok(())
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

fn execute(repo: &Path, removals: &[&Item]) -> usize {
    let mut failed = 0;
    for item in removals {
        if let Some(path) = &item.path {
            let p = path.to_string_lossy();
            if !unchanged(item, path) {
                eprintln!("git-cow: kept {p}, it changed since the plan");
                failed += 1;
                continue;
            }
            let mut cmd = vec!["worktree", "remove"];
            if item.forced == Force::Yes {
                cmd.push("--force");
            }
            cmd.push(&p);
            if let Err(e) = run(repo, &cmd) {
                eprintln!("git-cow: kept {p}: {e}");
                failed += 1;
                continue;
            }
            println!("removed {p}");
        }
        if let (Some(b), true) = (&item.branch, item.branch_too) {
            match delete_branch(repo, b, &item.oid) {
                Ok(()) => println!(
                    "deleted {b} (was {}, git branch {b} {} restores it)",
                    short(&item.oid),
                    item.oid
                ),
                Err(e) => {
                    eprintln!("git-cow: kept branch {b}: {e}");
                    failed += 1;
                }
            }
        }
    }
    failed
}

/// Remove worktrees and delete local branches whose work is in the base, after showing the plan
pub(crate) fn cleanup(args: &[String]) -> io::Result<()> {
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
    let Plan { repo, base, items } = plan(&std::env::current_dir()?, named, force)?;
    for item in &items {
        println!("{}", line(item));
    }
    let removals: Vec<&Item> = items
        .iter()
        .filter(|i| matches!(i.action, Action::Remove))
        .collect();
    let against = format!("against {} ({})", base.name, short(&base.oid));
    if removals.is_empty() {
        println!("nothing to clean up {against}");
        return Ok(());
    }
    let worktrees = removals.iter().filter(|i| i.path.is_some()).count();
    let branches = removals.iter().filter(|i| i.branch_too).count();
    println!("{worktrees} worktrees and {branches} local branches to remove, {against}");
    if !confirm(&mode)? {
        return Ok(());
    }
    let failed = execute(&repo, &removals);
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

/// List the worktrees of every repository at or below a folder, with how far their work landed.
/// It runs git read-only in each repository it finds, with no fsmonitor and no index writes
pub(crate) fn tree(args: &[String]) -> io::Result<()> {
    let dir = match args {
        [] => std::env::current_dir()?,
        [d] if !d.starts_with('-') => PathBuf::from(d),
        _ => return Err(other("give one folder".into())),
    };
    for repo in repositories(&std::fs::canonicalize(&dir)?) {
        let Ok((_, linked)) = worktrees(&repo) else {
            continue;
        };
        let linked: Vec<Worktree> = linked
            .into_iter()
            .filter(|w| !is_template(&repo, &w.path))
            .collect();
        if linked.is_empty() {
            continue;
        }
        let revs: Vec<(String, Option<String>)> = linked.iter().map(rev_of).collect();
        let states: Vec<Option<State>> = base(&repo, None).map_or_else(
            |_| vec![None; linked.len()],
            |b| states(&repo, &b, &revs).into_iter().map(Some).collect(),
        );
        let landed = states.iter().flatten().filter(|s| s.landed()).count();
        let n = linked.len();
        let noun = if n == 1 { "worktree" } else { "worktrees" };
        println!(
            "{}  ({n} {noun}, {landed} merged or squashed)",
            repo.display()
        );
        for (wt, state) in linked.iter().zip(states) {
            let label = state.map_or("?", State::label);
            let branch = wt.branch.as_deref().unwrap_or("(detached)");
            let changes = if has_changes(&wt.path) {
                "\t(has changes)"
            } else {
                ""
            };
            println!("  {label}\t{branch}\t{}{changes}", wt.path.display());
        }
    }
    Ok(())
}
