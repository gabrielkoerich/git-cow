//! `git cow cleanup` and `git cow tree`: find worktrees and branches whose work already landed

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use git_cow::REPO_ENV;

use crate::{is_template, other};

// Ahead of every git call here: no fsmonitor or gpg program from a repository's own config, and
// no index refresh written back. `git status` can still run a clean filter, so only cleanup does
const QUIET: &[&str] = &[
    "-c",
    "core.fsmonitor=false",
    "-c",
    "log.showSignature=false",
    "--no-optional-locks",
];

// Files a rebase, merge, bisect, cherry-pick or revert in progress leaves in a worktree's gitdir
const BUSY: &[&str] = &[
    "rebase-merge",
    "rebase-apply",
    "MERGE_HEAD",
    "BISECT_LOG",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "sequencer",
];

// Reads attributes from the empty tree, so no merge driver or filter a repository names runs
const NO_ATTRIBUTES: &str = "--attr-source=4b825dc642cb6eb9a060e54bf8d69288fbee4904";

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
    // a partial clone would fetch a missing blob, through ssh or a credential helper
    cmd.env("GIT_NO_LAZY_FETCH", "1");
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

// The main worktree, or the bare repository, and every linked worktree after it. Read with -z, so
// a path holding a newline stays one path
fn worktrees(dir: &Path) -> io::Result<(Worktree, Vec<Worktree>)> {
    let out = git_cmd(dir)
        .args(["worktree", "list", "--porcelain", "-z"])
        .stderr(Stdio::piped())
        .output()?;
    if !out.status.success() {
        return Err(other(format!(
            "git worktree list: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut all = text.split("\0\0").filter(|b| !b.is_empty()).map(|block| {
        let mut wt = Worktree {
            path: PathBuf::new(),
            branch: None,
            head: String::new(),
            locked: false,
        };
        for line in block.split('\0') {
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
            remote_branch(repo, rest)
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

// The branch in `<remote>/<branch>`, taking the longest remote name that fits, since a remote name
// may hold a slash
fn remote_branch(repo: &Path, rest: &str) -> Option<String> {
    let remotes = run(repo, &["remote"]).unwrap_or_default();
    remotes
        .lines()
        .filter_map(|r| {
            rest.strip_prefix(r)
                .and_then(|b| b.strip_prefix('/'))
                .map(|b| (r.len(), b))
        })
        .max_by_key(|(n, _)| *n)
        .map(|(_, b)| b.to_string())
        .or_else(|| rest.split_once('/').map(|(_, b)| b.to_string()))
}

// In a partial clone reading a branch's blobs would fetch them, so only ancestry counts there
fn is_partial(repo: &Path) -> bool {
    run(repo, &["config", "--get", "extensions.partialClone"]).is_ok()
        || run(repo, &["config", "--get-regexp", r"^remote\..*\.promisor$"])
            .is_ok_and(|o| o.lines().any(|l| l.ends_with(" true")))
}

struct Probe {
    // patch ids of what the rev changed since the merge base, and of undoing it
    own: String,
    undo: String,
    // base commits after the merge base, children before parents
    commits: String,
    mb: String,
}

// None for an ancestor of the base, or a rev that changes nothing in total
fn squash_probe(repo: &Path, base: &str, rev: &str) -> Option<Probe> {
    if run(repo, &["merge-base", "--is-ancestor", rev, base]).is_ok() {
        return None;
    }
    let mb = run(repo, &["merge-base", base, rev]).ok()?;
    let pid = |from: &str, to: &str| {
        let mut args = vec!["diff"];
        args.extend(DIFF);
        args.extend([from, to]);
        let out = patch_ids(repo, &args, None).ok()?;
        out.split_whitespace().next().map(String::from)
    };
    let own = pid(&mb, rev)?;
    let undo = pid(rev, &mb)?;
    let commits = run(
        repo,
        &[
            "rev-list",
            "--no-merges",
            "--topo-order",
            base,
            &format!("^{mb}"),
        ],
    )
    .ok()?;
    Some(Probe {
        own,
        undo,
        commits,
        mb,
    })
}

// The tree of a three way merge, written to a throwaway object folder over the repository's so the
// repository gets no new objects. None on a conflict
fn merge_tree(repo: &Path, merge_base: &str, ours: &str, theirs: &str) -> Option<String> {
    let common = run(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .ok()?;
    let scratch = std::env::temp_dir().join(format!("git-cow-merge-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).ok()?;
    let merge_base = format!("--merge-base={merge_base}");
    let out = git_cmd(repo)
        .env("GIT_OBJECT_DIRECTORY", &scratch)
        .env(
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            Path::new(&common).join("objects"),
        )
        .args([
            NO_ATTRIBUTES,
            "merge-tree",
            "--write-tree",
            "--no-messages",
            &merge_base,
            ours,
            theirs,
        ])
        .stderr(Stdio::null())
        .output();
    let _ = std::fs::remove_dir_all(&scratch);
    let out = out.ok().filter(|o| o.status.success())?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .map(String::from)
}

// Replaying the rev's change onto `c`'s parent gives exactly `c`, so `c` is its squash and not the
// same patch made somewhere else in a file
fn is_its_squash(repo: &Path, mb: &str, c: &str, rev: &str) -> bool {
    let tree = run(repo, &["rev-parse", &format!("{c}^{{tree}}")]).ok();
    tree.is_some() && merge_tree(repo, mb, &format!("{c}^"), rev) == tree
}

// A base commit after the squash `c` reverts it: git revert names it, or the patch undoes the rev.
// An undo hidden in other work goes unseen, but the work stays in `c` in the base's history
fn reverted(
    repo: &Path,
    base: &str,
    c: &str,
    later: &[&str],
    undo: &str,
    by_commit: &HashMap<String, String>,
) -> bool {
    later
        .iter()
        .any(|l| by_commit.get(*l).map(String::as_str) == Some(undo))
        || run(
            repo,
            &[
                "log",
                "--format=%H",
                "--fixed-strings",
                &format!("--grep=This reverts commit {c}"),
                base,
                &format!("^{c}"),
            ],
        )
        .is_ok_and(|out| !out.is_empty())
}

// One base commit after the merge base is the rev's squash, replayed exactly, and no later base
// commit reverts it
fn squashed(
    repo: &Path,
    base: &str,
    rev: &str,
    probe: &Probe,
    by_commit: &HashMap<String, String>,
) -> bool {
    let commits: Vec<&str> = probe.commits.lines().collect();
    commits.iter().enumerate().any(|(i, c)| {
        by_commit.get(*c) == Some(&probe.own)
            && is_its_squash(repo, &probe.mb, c, rev)
            && !reverted(repo, base, c, &commits[..i], &probe.undo, by_commit)
    })
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

// Merged is an ancestor of the base. Squashed is git-extras' check with whitespace counted, made
// sure by replaying the squash, and not reverted since
fn states(repo: &Path, base: &Base, revs: &[(String, Option<String>)]) -> Vec<State> {
    let partial = is_partial(repo);
    let probes: Vec<Option<Probe>> = revs
        .iter()
        .map(|(rev, _)| {
            if partial {
                None
            } else {
                squash_probe(repo, &base.oid, rev)
            }
        })
        .collect();
    let wanted: HashSet<&str> = probes
        .iter()
        .flatten()
        .flat_map(|p| p.commits.lines())
        .collect();
    let by_commit = commit_patch_ids(repo, &wanted);
    revs.iter()
        .zip(&probes)
        .map(|((rev, branch), probe)| {
            if run(repo, &["merge-base", "--is-ancestor", rev, &base.oid]).is_ok() {
                return State::Merged;
            }
            if probe
                .as_ref()
                .is_some_and(|p| squashed(repo, &base.oid, rev, p, &by_commit))
            {
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

// Edits, staged changes and every untracked file, whatever status.showUntrackedFiles says, and
// files marked assume-unchanged or skip-worktree, whose edits git status hides. Empty when clean
fn changes(path: &Path) -> String {
    if !path.exists() {
        return String::new();
    }
    let mut seen = run(
        path,
        &[
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
    )
    .unwrap_or_else(|e| format!("unreadable: {e}"));
    if let Ok(files) = run(path, &["ls-files", "-v"]) {
        for l in files
            .lines()
            .filter(|l| l.starts_with(|c: char| c.is_ascii_lowercase() || c == 'S'))
        {
            seen.push('\n');
            seen.push_str(l);
        }
    }
    seen.trim().to_string()
}

// A folder, or one up to `depth` levels below it, that is a repository of its own
fn repository_below(dir: &Path, depth: u32) -> bool {
    if dir.join(".git").exists() || (dir.join("HEAD").is_file() && dir.join("objects").is_dir()) {
        return true;
    }
    depth > 0
        && std::fs::read_dir(dir).is_ok_and(|entries| {
            entries.flatten().any(|e| {
                e.file_type().is_ok_and(|t| t.is_dir()) && repository_below(&e.path(), depth - 1)
            })
        })
}

// An untracked or ignored folder holding a repository, whose history a remove takes with it
fn holds_repository(path: &Path) -> bool {
    let Ok(out) = git_cmd(path)
        .args([
            "status",
            "--porcelain",
            "-z",
            "--ignored=traditional",
            "--untracked-files=normal",
        ])
        .stderr(Stdio::null())
        .output()
    else {
        return true;
    };
    String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter_map(|e| e.strip_prefix("?? ").or_else(|| e.strip_prefix("!! ")))
        .filter(|p| p.ends_with('/'))
        .any(|p| repository_below(&path.join(p), 3))
}

// Each linked worktree's admin folder in the common git dir, keyed by the worktree path its
// `gitdir` file names
fn admin_dirs(repo: &Path) -> HashMap<PathBuf, PathBuf> {
    let mut map = HashMap::new();
    let Ok(common) = run(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ) else {
        return map;
    };
    if let Ok(entries) = std::fs::read_dir(Path::new(&common).join("worktrees")) {
        for e in entries.flatten() {
            if let Ok(gitfile) = std::fs::read_to_string(e.path().join("gitdir"))
                && let Some(wt) = Path::new(gitfile.trim()).parent()
            {
                map.insert(wt.to_path_buf(), e.path());
            }
        }
    }
    map
}

// What a worktree holds that git keeps nowhere else: submodules and other repositories inside it,
// refs of its own, a rebase, merge or bisect. Read from its admin folder, so a missing folder too
fn own_state(repo: &Path, path: &Path, admin: Option<&PathBuf>) -> Option<&'static str> {
    let Some(admin) = admin else {
        return Some("unreadable");
    };
    let submodules = admin.join("modules").exists()
        || path.join(".gitmodules").exists()
        || run(path, &["ls-files", "-s"]).is_ok_and(|s| s.lines().any(|l| l.starts_with("160000")));
    if submodules {
        return Some("has submodules");
    }
    if path.exists() && holds_repository(path) {
        return Some("holds a repository");
    }
    if BUSY.iter().any(|f| admin.join(f).exists()) {
        return Some("in a rebase, merge or bisect");
    }
    let admin = admin.to_string_lossy();
    let refs = run(
        repo,
        &[
            "--git-dir",
            &admin,
            "for-each-ref",
            "refs/worktree/",
            "refs/bisect/",
        ],
    );
    if refs.is_ok_and(|s| !s.is_empty()) {
        return Some("has its own refs");
    }
    None
}

// Claude Code removes its own worktrees, and a session may still run in one
fn claude_worktree(path: &Path) -> bool {
    let parts: Vec<_> = path
        .components()
        .map(std::path::Component::as_os_str)
        .collect();
    parts
        .windows(2)
        .any(|w| w[0] == ".claude" && w[1] == "worktrees")
}

// Branches a rebase or bisect in any worktree works on, which git lists as detached
fn busy_branches(repo: &Path) -> HashSet<String> {
    let mut busy = HashSet::new();
    let Ok(common) = run(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ) else {
        return busy;
    };
    let common = PathBuf::from(common);
    let mut gitdirs = vec![common.clone()];
    if let Ok(entries) = std::fs::read_dir(common.join("worktrees")) {
        gitdirs.extend(entries.flatten().map(|e| e.path()));
    }
    for g in gitdirs {
        for f in [
            "rebase-merge/head-name",
            "rebase-apply/head-name",
            "BISECT_START",
        ] {
            if let Ok(text) = std::fs::read_to_string(g.join(f)) {
                let name = text.trim();
                busy.insert(name.strip_prefix("refs/heads/").unwrap_or(name).to_string());
            }
        }
    }
    busy
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
    // the commit and the changes the plan judged, so the run skips anything that moved since
    oid: String,
    changes: String,
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
    let admins = admin_dirs(repo);
    let mut items = Vec::new();
    for (wt, state) in linked.iter().zip(states(repo, base, &revs)) {
        let changes = changes(&wt.path);
        let admin = admins.get(&wt.path).or_else(|| {
            let real = std::fs::canonicalize(&wt.path).ok()?;
            admins
                .iter()
                .find(|(p, _)| std::fs::canonicalize(p).is_ok_and(|q| q == real))
                .map(|(_, a)| a)
        });
        let inside = here.starts_with(&wt.path)
            || std::fs::canonicalize(&wt.path).is_ok_and(|p| here.starts_with(p));
        let action = if !state.landed() {
            Action::Keep("")
        } else if wt.locked {
            Action::Keep("locked")
        } else if inside {
            Action::Keep("you are in it")
        } else if claude_worktree(&wt.path) {
            Action::Keep("Claude Code worktree")
        } else if wt.branch.as_ref().is_some_and(|b| protected.contains(b)) {
            Action::Keep("base branch")
        } else if !wt.path.exists() && force == Force::No {
            Action::Keep("folder missing")
        } else if let Some(why) = own_state(repo, &wt.path, admin) {
            Action::Keep(why)
        } else if !changes.is_empty() && force == Force::No {
            Action::Keep("has changes")
        } else {
            Action::Remove
        };
        let forced = if !changes.is_empty() || !wt.path.exists() {
            Force::Yes
        } else {
            Force::No
        };
        items.push(Item {
            path: Some(wt.path.clone()),
            branch: wt.branch.clone(),
            changes,
            oid: wt.head.clone(),
            state,
            action,
            branch_too: false,
            forced,
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
    protected.extend(head_branch(&repo));
    if named.is_some() {
        protected.extend(base_of(&repo));
    }
    protected.extend(busy_branches(&repo));
    let linked: Vec<Worktree> = linked
        .into_iter()
        .filter(|w| !is_template(&repo, &w.path))
        .collect();
    let mut items = worktree_items(&repo, &base, &linked, &protected, &here, force);

    // Local branches with no worktree, never one checked out anywhere or the base
    let checked_out: HashSet<String> = linked.iter().filter_map(|w| w.branch.clone()).collect();
    // a symbolic ref such as master pointing at main is an alias, deleting it would delete main
    let heads = run(
        &repo,
        &[
            "for-each-ref",
            "--format=%(refname)%00%(symref)",
            "refs/heads/",
        ],
    )?;
    let mut only = Vec::new();
    for entry in heads.lines() {
        let (full, symref) = entry.split_once('\0').unwrap_or((entry, ""));
        let Some(b) = full.strip_prefix("refs/heads/") else {
            continue;
        };
        if !symref.is_empty() {
            continue;
        }
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
            changes: String::new(),
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

// The branch HEAD names in `dir`, from the full ref so a tag with the same name cannot shorten it
fn head_branch(dir: &Path) -> Option<String> {
    run(dir, &["symbolic-ref", "-q", "HEAD"])
        .ok()
        .and_then(|h| h.strip_prefix("refs/heads/").map(String::from))
}

// The local default branch, kept even when cleanup compares with another one
fn base_of(repo: &Path) -> Option<String> {
    base(repo, None).ok().and_then(|b| b.local)
}

// One tab separated line: what happens, how far the work landed, the branch, the worktree path
fn line(item: &Item) -> String {
    let verb = match (&item.action, &item.path) {
        (Action::Remove, Some(_)) if !item.changes.is_empty() => {
            "remove (discards changes)".to_string()
        }
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

// Whether a worktree is still as the plan saw it: same commit, same branch, same changes
fn unchanged(item: &Item, path: &Path) -> bool {
    if !path.exists() {
        return true;
    }
    let head = run(path, &["rev-parse", "HEAD"]).unwrap_or_default();
    head == item.oid && head_branch(path) == item.branch && changes(path) == item.changes
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
    if busy_branches(repo).contains(b) {
        return Err(other(format!("a rebase or bisect works on {b}")));
    }
    run(
        repo,
        &[
            "update-ref",
            "--no-deref",
            "-d",
            &format!("refs/heads/{b}"),
            oid,
        ],
    )?;
    // git branch -D drops these too
    let _ = run(
        repo,
        &["config", "--remove-section", &format!("branch.{b}")],
    );
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
    warn_without_squash_check();
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

// The squash check needs patch-id --verbatim and --attr-source, both in git 2.40. Older git cannot
// tell a squash merge, so those branches show as active and stay, which is safe but worth saying
fn warn_without_squash_check() {
    let works = git_cmd(Path::new("."))
        .args([NO_ATTRIBUTES, "patch-id", "--verbatim"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !works {
        eprintln!(
            "git-cow: this git is older than 2.40, so squash-merged branches show as active and stay"
        );
    }
}

/// List the worktrees of every repository at or below a folder, with how far their work landed.
/// It reads only commits and refs, never a worktree's files, so no filter or hook a repository
/// sets can run
pub(crate) fn tree(args: &[String]) -> io::Result<()> {
    let dir = match args {
        [] => std::env::current_dir()?,
        [d] if !d.starts_with('-') => PathBuf::from(d),
        _ => return Err(other("give one folder".into())),
    };
    warn_without_squash_check();
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
            println!("  {label}\t{branch}\t{}", wt.path.display());
        }
    }
    Ok(())
}
