//! Worktrees and local branches whose work already landed in a base branch, and their removal.
//! [`plan`] judges every worktree and branch, [`execute`] removes what the plan marks, and saves
//! each commit it lets go of under `refs/git-cow/deleted/` first

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{REPO_ENV, config_path};

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

fn err(msg: String) -> io::Error {
    io::Error::other(msg)
}

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
        return Err(err(format!(
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
        .ok_or_else(|| err("git gave no output pipe".into()))?;
    let out = git_cmd(dir)
        .args(["patch-id", "--verbatim"])
        .stdin(diff)
        .stderr(Stdio::null())
        .output()?;
    if let Some(w) = writer {
        let _ = w.join();
    }
    if !producer.wait()?.success() || !out.status.success() {
        return Err(err(format!("git {args:?} | git patch-id failed")));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// How far a worktree's or branch's work landed in the base
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum State {
    /// An ancestor of the base
    Merged,
    /// Squash-merged: one base commit made the whole change and nothing reverted it
    Squashed,
    /// Not in the base, and its upstream branch is gone
    Gone,
    /// Not in the base
    Active,
}

impl State {
    /// The word the plan prints
    pub fn label(self) -> &'static str {
        match self {
            State::Merged => "merged",
            State::Squashed => "squashed",
            State::Gone => "gone",
            State::Active => "active",
        }
    }

    /// Merged or squashed, so the work is in the base
    pub fn landed(self) -> bool {
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
        return Err(err(format!(
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
        .ok_or_else(|| err("git lists no worktree".into()))?;
    Ok((main, all.collect()))
}

/// The branch a plan compares with
#[derive(Debug)]
pub struct Base {
    /// The name it resolved to, such as `origin/main`
    pub name: String,
    /// Its commit when the plan was made
    pub oid: String,
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
    Err(err(
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

// A new empty folder in the repository's git dir, which only its owner can write. A folder that
// already exists is never reused, since git would read objects planted in it before the real ones
fn scratch_dir(common: &Path) -> Option<PathBuf> {
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    for _ in 0..8 {
        let n = COUNT.fetch_add(1, Ordering::Relaxed);
        let dir = common.join(format!("git-cow-merge-{}-{nanos}-{n}", std::process::id()));
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Some(dir),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(_) => return None,
        }
    }
    None
}

// The tree of a three way merge, written to a throwaway object folder over the repository's so the
// repository gets no new objects. None on a conflict
fn merge_tree(repo: &Path, merge_base: &str, ours: &str, theirs: &str) -> Option<String> {
    let common = run(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .ok()?;
    // --attr-source covers only the tree, info/attributes would still name a merge driver
    let info =
        std::fs::read_to_string(Path::new(&common).join("info/attributes")).unwrap_or_default();
    if info.contains("merge") {
        return None;
    }
    let scratch = scratch_dir(Path::new(&common))?;
    let merge_base = format!("--merge-base={merge_base}");
    let out = git_cmd(repo)
        .env("GIT_OBJECT_DIRECTORY", &scratch)
        .env(
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            Path::new(&common).join("objects"),
        )
        .args([
            NO_ATTRIBUTES,
            "-c",
            "core.attributesFile=/dev/null",
            "-c",
            "merge.default=text",
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

// A folder, or any folder below it, that is a repository of its own. Symlinks are not followed,
// since removing the worktree removes only the link
fn repository_below(dir: &Path) -> bool {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        if d.join(".git").exists() || (d.join("HEAD").is_file() && d.join("objects").is_dir()) {
            return true;
        }
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        stack.extend(
            entries
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.path()),
        );
    }
    false
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
    // a status that fails hides what is inside, so it counts as holding one
    !out.status.success()
        || String::from_utf8_lossy(&out.stdout)
            .split('\0')
            .filter_map(|e| e.strip_prefix("?? ").or_else(|| e.strip_prefix("!! ")))
            .filter(|p| p.ends_with('/'))
            .any(|p| repository_below(&path.join(p)))
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
fn own_state(repo: &Path, path: &Path, admin: Option<&PathBuf>) -> Option<Keep> {
    let Some(admin) = admin else {
        return Some(Keep::Unreadable);
    };
    let submodules = admin.join("modules").exists()
        || path.join(".gitmodules").exists()
        || run(path, &["ls-files", "-s"]).is_ok_and(|s| s.lines().any(|l| l.starts_with("160000")));
    if submodules {
        return Some(Keep::Submodules);
    }
    if path.exists() && holds_repository(path) {
        return Some(Keep::Repository);
    }
    if BUSY.iter().any(|f| admin.join(f).exists()) {
        return Some(Keep::Busy);
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
        return Some(Keep::OwnRefs);
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

/// What [`execute`] does with an item
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Action {
    /// Remove the worktree, or delete the branch with no worktree
    Remove,
    /// Leave it, for this reason
    Keep(Keep),
}

/// Why a plan keeps a worktree or branch
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Keep {
    /// Its work is not in the base
    NotLanded,
    /// `git worktree lock` holds it
    Locked,
    /// The `dir` the plan was made from is inside it
    Inside,
    /// Claude Code removes its own worktrees, and a session may still run in one
    ClaudeCode,
    /// It is on the base branch, the default branch, the main checkout's branch, or a branch a
    /// rebase or bisect works on
    BaseBranch,
    /// Its folder is gone, kept unless [`Options::force`]
    FolderMissing,
    /// It holds submodules
    Submodules,
    /// It holds another repository in an untracked or ignored folder
    Repository,
    /// A rebase, merge, bisect, cherry-pick or revert is in progress in it
    Busy,
    /// It has refs of its own under `refs/worktree/` or `refs/bisect/`
    OwnRefs,
    /// git cannot read its state
    Unreadable,
    /// It has edits or untracked files, kept unless [`Options::force`]
    Changes,
}

impl std::fmt::Display for Keep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Keep::NotLanded => "not landed",
            Keep::Locked => "locked",
            Keep::Inside => "you are in it",
            Keep::ClaudeCode => "Claude Code worktree",
            Keep::BaseBranch => "base branch",
            Keep::FolderMissing => "folder missing",
            Keep::Submodules => "has submodules",
            Keep::Repository => "holds a repository",
            Keep::Busy => "in a rebase, merge or bisect",
            Keep::OwnRefs => "has its own refs",
            Keep::Unreadable => "unreadable",
            Keep::Changes => "has changes",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Force {
    No,
    Yes,
}

/// What to compare with, and whether to remove worktrees with changes or a missing folder too
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// The branch work lands on, or the remote's default branch, then a local main or master
    pub base: Option<String>,
    /// Also remove landed worktrees with changes or a missing folder. Never a worktree holding
    /// a repository, submodules, refs of its own or a rebase
    pub force: bool,
}

/**
 * One worktree, or a local branch with no worktree, and what the plan does with it. Only [`plan`]
 * and [`plan_worktree`] make one, and a caller can read but never change it, so [`execute`] only
 * ever removes what the plan judged safe:
 *
 * ```compile_fail
 * # use git_cow::cleanup::{self, Action, Options};
 * # let plan = cleanup::plan(std::path::Path::new("."), &Options::default()).unwrap();
 * plan.items()[0].action = Action::Remove;
 * ```
 **/
#[derive(Debug)]
pub struct Item {
    path: Option<PathBuf>,
    branch: Option<String>,
    state: State,
    action: Action,
    // the commit and the changes the plan judged, so the run skips anything that moved since
    oid: String,
    changes: String,
    // whether removing this worktree also deletes its branch
    branch_too: bool,
    forced: Force,
    // the worktree's admin folder, whose HEAD reflog may hold commits no ref has
    admin: Option<PathBuf>,
}

impl Item {
    /// The worktree, or None for a branch with no worktree
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Its branch, or None for a detached worktree
    pub fn branch(&self) -> Option<&str> {
        self.branch.as_deref()
    }

    /// How far its work landed
    pub fn state(&self) -> State {
        self.state
    }

    /// What [`execute`] does with it
    pub fn action(&self) -> Action {
        self.action
    }

    /// The commit the plan judged
    pub fn oid(&self) -> &str {
        &self.oid
    }

    /// Whether removing it also deletes its branch, which it does only when every worktree on
    /// that branch goes
    pub fn deletes_branch(&self) -> bool {
        self.branch_too
    }

    /// Whether removing it drops edits or untracked files, which only [`Options::force`] allows
    pub fn discards_changes(&self) -> bool {
        !self.changes.is_empty()
    }
}

/// Every worktree and local branch [`plan`] judged
#[derive(Debug)]
pub struct Plan {
    repo: PathBuf,
    base: Base,
    items: Vec<Item>,
}

impl Plan {
    /// The main worktree or bare repository every git call runs in
    pub fn repo(&self) -> &Path {
        &self.repo
    }

    /// What the plan compared with
    pub fn base(&self) -> &Base {
        &self.base
    }

    /// One per worktree, then one per local branch with no worktree
    pub fn items(&self) -> &[Item] {
        &self.items
    }
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
            Action::Keep(Keep::NotLanded)
        } else if wt.locked {
            Action::Keep(Keep::Locked)
        } else if inside {
            Action::Keep(Keep::Inside)
        } else if claude_worktree(&wt.path) {
            Action::Keep(Keep::ClaudeCode)
        } else if wt.branch.as_ref().is_some_and(|b| protected.contains(b)) {
            Action::Keep(Keep::BaseBranch)
        } else if !wt.path.exists() && force == Force::No {
            Action::Keep(Keep::FolderMissing)
        } else if let Some(why) = own_state(repo, &wt.path, admin) {
            Action::Keep(why)
        } else if !changes.is_empty() && force == Force::No {
            Action::Keep(Keep::Changes)
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
            admin: admin.cloned(),
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

/**
 * Judge every worktree of the repository at `dir`, and every local branch with no worktree,
 * against the base. It changes nothing
 *
 * # Errors
 *
 * When `dir` is not in a repository, no base branch resolves, or git cannot list the branches
 **/
pub fn plan(dir: &Path, opts: &Options) -> io::Result<Plan> {
    judge(dir, opts, None)
}

/**
 * Judge only the worktree at `wt`, as a tool does with a worktree it made once its work landed.
 * The plan has that one item, and deletes its branch only if no other worktree has it
 *
 * # Errors
 *
 * When `wt` is not a worktree of the repository at `dir`, or is an extra checkout, or as [`plan`]
 **/
pub fn plan_worktree(dir: &Path, wt: &Path, opts: &Options) -> io::Result<Plan> {
    judge(dir, opts, Some(wt))
}

fn same_path(a: &Path, b: &Path) -> bool {
    a == b || std::fs::canonicalize(a).is_ok_and(|p| std::fs::canonicalize(b).ok() == Some(p))
}

fn judge(dir: &Path, opts: &Options, only: Option<&Path>) -> io::Result<Plan> {
    let named = opts.base.as_deref();
    let force = if opts.force { Force::Yes } else { Force::No };
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
    // Local branches with no worktree, never one checked out anywhere, an extra checkout too
    let checked_out: HashSet<String> = linked.iter().filter_map(|w| w.branch.clone()).collect();
    let mut linked: Vec<Worktree> = linked
        .into_iter()
        .filter(|w| !is_extra_checkout(&repo, &w.path))
        .collect();
    if let Some(wt) = only {
        let target = linked
            .iter()
            .find(|w| same_path(&w.path, wt))
            .ok_or_else(|| {
                err(format!(
                    "{} is not a worktree of this repository, or is an extra checkout",
                    wt.display()
                ))
            })?;
        // the others on its branch decide whether its branch may go
        let (path, branch) = (target.path.clone(), target.branch.clone());
        linked.retain(|w| w.path == path || (branch.is_some() && w.branch == branch));
        let mut items = worktree_items(&repo, &base, &linked, &protected, &here, force);
        items.retain(|i| i.path.as_ref() == Some(&path));
        if linked.len() > 1 {
            for item in &mut items {
                item.branch_too = false;
            }
        }
        return Ok(Plan { repo, base, items });
    }
    let mut items = worktree_items(&repo, &base, &linked, &protected, &here, force);

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
                Action::Keep(Keep::NotLanded)
            },
            branch_too: true,
            forced: Force::No,
            admin: None,
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

// Whether a worktree is still as the plan saw it: same commit, same branch, same changes
fn unchanged(item: &Item, path: &Path) -> bool {
    if !path.exists() {
        return true;
    }
    let head = run(path, &["rev-parse", "HEAD"]).unwrap_or_default();
    head == item.oid && head_branch(path) == item.branch && changes(path) == item.changes
}

// Where cleanup keeps every commit it lets go of, one folder per run, so gc never prunes them
const BACKUP: &str = "refs/git-cow/deleted";

// A worktree folder name as a ref name part
fn ref_part(name: &str) -> String {
    let clean: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_".contains(c) {
                c
            } else {
                '-'
            }
        })
        .collect();
    if clean.is_empty() {
        "worktree".into()
    } else {
        clean
    }
}

// Commits only the worktree's HEAD reflog holds, saved before git deletes that reflog with it
fn back_up_reflog(repo: &Path, admin: &Path, name: &str, stamp: &str) -> io::Result<()> {
    let admin = admin.to_string_lossy();
    let Ok(log) = run(
        repo,
        &["--git-dir", &admin, "reflog", "show", "--format=%H", "HEAD"],
    ) else {
        return Ok(());
    };
    let oids: HashSet<&str> = log.lines().filter(|l| !l.is_empty()).collect();
    if oids.is_empty() {
        return Ok(());
    }
    let mut args = vec!["rev-list"];
    args.extend(oids.iter());
    args.extend(["--not", "--branches", "--tags", "--remotes"]);
    let unreachable = run(repo, &args)?;
    let lost = unreachable.lines().filter(|o| oids.contains(o));
    for (i, oid) in lost.enumerate() {
        let backup = format!("{BACKUP}/{stamp}/worktree/{}/{i}", ref_part(name));
        // an empty old value makes update-ref refuse a ref that exists
        run(repo, &["update-ref", &backup, oid, ""])?;
    }
    Ok(())
}

// Delete the local `b` only if it still points at `oid` and no worktree has it checked out now,
// after saving it under the backup refs. Only refs/heads changes, a remote is never touched
fn delete_branch(repo: &Path, b: &str, oid: &str, stamp: &str) -> io::Result<String> {
    let (main, linked) = worktrees(repo)?;
    if std::iter::once(&main)
        .chain(&linked)
        .any(|w| w.branch.as_deref() == Some(b))
    {
        return Err(err(format!("{b} is checked out in a worktree")));
    }
    if busy_branches(repo).contains(b) {
        return Err(err(format!("a rebase or bisect works on {b}")));
    }
    let backup = format!("{BACKUP}/{stamp}/branch/{b}");
    run(repo, &["update-ref", &backup, oid, ""])?;
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
    Ok(backup)
}

/// What [`execute`] did
#[derive(Debug)]
#[non_exhaustive]
pub enum Done {
    /// Removed this worktree
    Removed(PathBuf),
    /// Deleted this local branch, after saving it as `backup`
    Deleted {
        /// The branch
        branch: String,
        /// The commit it pointed at
        oid: String,
        /// The ref that keeps it, `git branch <branch> <backup>` restores it
        backup: String,
    },
    /// Left this worktree or branch, for this reason
    Failed {
        /// The worktree path or branch name
        what: String,
        /// Why
        why: String,
    },
}

/**
 * Remove every worktree and delete every branch the plan marks [`Action::Remove`]. Each commit
 * it lets go of is saved under `refs/git-cow/deleted/<seconds>-<pid>-<call>/` first, and an item that moved since
 * the plan stays
 **/
pub fn execute(plan: &Plan) -> Vec<Done> {
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    let repo = &plan.repo;
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let call = CALLS.fetch_add(1, Ordering::Relaxed);
    let stamp = format!("{secs}-{}-{call}", std::process::id());
    let mut done = Vec::new();
    for (n, item) in plan
        .items
        .iter()
        .filter(|i| matches!(i.action, Action::Remove))
        .enumerate()
    {
        if let Some(path) = &item.path {
            let p = path.to_string_lossy().into_owned();
            let failed = |why: String| Done::Failed {
                what: p.clone(),
                why,
            };
            if !unchanged(item, path) {
                done.push(failed("it changed since the plan".into()));
                continue;
            }
            let leaf = path.file_name().unwrap_or_default().to_string_lossy();
            let name = format!("{n}-{leaf}");
            if let Some(admin) = &item.admin
                && let Err(e) = back_up_reflog(repo, admin, &name, &stamp)
            {
                done.push(failed(format!("its reflog could not be backed up: {e}")));
                continue;
            }
            let mut cmd = vec!["worktree", "remove"];
            if item.forced == Force::Yes {
                cmd.push("--force");
            }
            cmd.push(&p);
            if let Err(e) = run(repo, &cmd) {
                done.push(failed(e.to_string()));
                continue;
            }
            done.push(Done::Removed(path.clone()));
        }
        if let (Some(b), true) = (&item.branch, item.branch_too) {
            done.push(match delete_branch(repo, b, &item.oid, &stamp) {
                Ok(backup) => Done::Deleted {
                    branch: b.clone(),
                    oid: item.oid.clone(),
                    backup,
                },
                Err(e) => Done::Failed {
                    what: format!("branch {b}"),
                    why: e.to_string(),
                },
            });
        }
    }
    done
}

/// Whether this git can tell a squash merge: it needs `patch-id --verbatim` and `--attr-source`,
/// both in git 2.40. Without them squash-merged branches show as active and stay
pub fn squash_check_works() -> bool {
    git_cmd(&std::env::temp_dir())
        .args([NO_ATTRIBUTES, "patch-id", "--verbatim"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// A linked worktree and how far its work landed, None when no base branch resolves
#[derive(Debug)]
pub struct Surveyed {
    /// The worktree
    pub path: PathBuf,
    /// Its branch, or None when detached
    pub branch: Option<String>,
    /// How far its work landed
    pub state: Option<State>,
}

/**
 * Every linked worktree of the repository at `dir` but the extra checkouts, with how far its work
 * landed. It reads only commits and refs, never a worktree's files, so no filter or hook a
 * repository sets can run, which makes it fit for repositories you did not make
 *
 * # Errors
 *
 * When `dir` is not in a repository
 **/
pub fn survey(dir: &Path) -> io::Result<Vec<Surveyed>> {
    let (main, linked) = worktrees(dir)?;
    let linked: Vec<Worktree> = linked
        .into_iter()
        .filter(|w| !is_extra_checkout(&main.path, &w.path))
        .collect();
    let revs: Vec<(String, Option<String>)> = linked.iter().map(rev_of).collect();
    let states: Vec<Option<State>> = base(&main.path, None).map_or_else(
        |_| vec![None; linked.len()],
        |b| {
            states(&main.path, &b, &revs)
                .into_iter()
                .map(Some)
                .collect()
        },
    );
    Ok(linked
        .into_iter()
        .zip(states)
        .map(|(w, state)| Surveyed {
            path: w.path,
            branch: w.branch,
            state,
        })
        .collect())
}

/**
 * Whether `path` is a git-cow extra checkout of the repository at `repo`: under a `.git-cow`
 * folder, at `cow.base`, or with the lock file git-cow keeps beside each one. git lists real
 * paths while `cow.base` may go through a symlink, so both sides are resolved
 **/
pub fn is_extra_checkout(repo: &Path, path: &Path) -> bool {
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let locked = path.file_name().is_some_and(|n| {
        path.with_file_name(format!(".{}.lock", n.to_string_lossy()))
            .exists()
    });
    path.components().any(|c| c.as_os_str() == ".git-cow")
        || locked
        || config_path(repo, "cow.base")
            .or_else(|| config_path(repo, "cow.template"))
            .is_some_and(|t| real(&t) == real(path))
}
