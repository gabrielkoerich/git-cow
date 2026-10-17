use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

const USAGE: &str = "usage: git cow add [-b <branch>] [<path>] [<commit-ish>]
       git cow list
       git cow remove [--warm] [--force] <worktree>   (or rm)
       git cow warm <worktree>
       git cow share [--global] [<dir>...]

add     creates a worktree by cloning a template checkout, falling back to git worktree add.
        Without <path> it goes to <cow.root>/<repo>/<last part of the branch name>
list    lists the worktrees, without the templates
remove  removes a worktree and its branch when merged. --warm hands its build output to the
        template first, --force removes it with uncommitted changes. Never removes a template
warm    refreshes the template with a worktree's gitignored directories, run it after a merge
share   lists the hidden directories shared with every worktree, or adds to them in git config.
        Ignored directories are shared, but a hidden one only when listed, the rest is tool config

git config ({repo} in a path is replaced by the repository's name):
  cow.root          where worktrees go when <path> is left out (default ~/.worktrees)
  cow.template      the template checkout (default <worktree's parent>/.git-cow/<repo>)
  cow.share         a hidden directory to share on top of the built-in caches, one per entry
  cow.warmInterval  how often add refreshes the template from the checkout: 0, 30m, 1h (default 1h)
GIT_COW=0 disables cloning";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let res = match args.first().map(String::as_str) {
        Some("add") => return add(&args[1..]),
        Some("list") if args.len() == 1 => list(),
        Some("remove" | "rm") => remove(&args[1..]),
        Some("warm") if args.len() == 2 => warm(Path::new(&args[1])),
        Some("share") => share(&args[1..]),
        _ => return usage(),
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("git-cow: {e}");
            ExitCode::FAILURE
        }
    }
}

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}

fn other(msg: String) -> std::io::Error {
    std::io::Error::other(msg)
}

// A path from git config, with ~/ and {repo} expanded
fn config_path(repo: &Path, key: &str) -> Option<PathBuf> {
    let v = git_cow::git(repo, &["config", "--get", key]).ok()?;
    let v = match repo_name(repo) {
        Ok(name) => v.replace("{repo}", &name),
        Err(_) => v,
    };
    let home = std::env::var("HOME").unwrap_or_default();
    Some(match v.strip_prefix("~/") {
        Some(rest) => Path::new(&home).join(rest),
        None => PathBuf::from(v),
    })
}

// "0", "90" (seconds), "30m", "1h"
fn parse_interval(v: &str) -> Option<Duration> {
    let v = v.trim();
    let (n, unit) = match v.strip_suffix('h') {
        Some(n) => (n, 3600),
        None => match v.strip_suffix('m') {
            Some(n) => (n, 60),
            None => (v.strip_suffix('s').unwrap_or(v), 1),
        },
    };
    n.trim()
        .parse::<u64>()
        .ok()
        .map(|n| Duration::from_secs(n * unit))
}

// The built-in caches plus every cow.share entry, and the warm interval
fn options(repo: &Path) -> git_cow::Options {
    let mut opts = git_cow::Options::default();
    let extra = git_cow::git(repo, &["config", "--get-all", "cow.share"]).unwrap_or_default();
    for d in extra.lines().map(str::trim).filter(|d| !d.is_empty()) {
        if !opts.share.iter().any(|l| l == d) {
            opts.share.push(d.to_string());
        }
    }
    if let Some(i) = git_cow::git(repo, &["config", "--get", "cow.warmInterval"])
        .ok()
        .and_then(|v| parse_interval(&v))
    {
        opts.warm_interval = i;
    }
    opts
}

fn share(args: &[String]) -> std::io::Result<()> {
    let repo = std::env::current_dir()?;
    let global = args.iter().any(|a| a == "--global");
    let have = options(&repo).share;
    for d in args.iter().filter(|a| *a != "--global") {
        if !d.starts_with('.') || d.contains('/') {
            return Err(other(format!(
                "{d}: give a hidden directory name like .venv, other ignored directories are shared already"
            )));
        }
        if have.iter().any(|h| h == d) {
            continue;
        }
        let scope = if global { "--global" } else { "--local" };
        git_cow::git(&repo, &["config", scope, "--add", "cow.share", d])?;
    }
    for d in options(&repo).share {
        println!("{d}");
    }
    Ok(())
}

// The repository's name: its directory, or a bare repository's name without `.git`
fn repo_name(dir: &Path) -> std::io::Result<String> {
    let common = git_cow::git(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let common = Path::new(&common);
    let named = if common.file_name().is_some_and(|n| n == ".git") {
        common.parent().unwrap_or(common)
    } else {
        common
    };
    let name = named.file_name().unwrap_or_default().to_string_lossy();
    Ok(name.strip_suffix(".git").unwrap_or(&name).to_string())
}

fn template_for(repo: &Path, wt: &Path) -> std::io::Result<PathBuf> {
    if let Some(t) = config_path(repo, "cow.template") {
        return Ok(t);
    }
    let parent = wt.parent().unwrap_or(Path::new("."));
    Ok(parent.join(".git-cow").join(repo_name(repo)?))
}

fn is_template(repo: &Path, path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == ".git-cow")
        || config_path(repo, "cow.template").is_some_and(|t| t == path)
}

fn add(args: &[String]) -> ExitCode {
    let repo = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut new_branch = None;
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-b" => match it.next() {
                Some(b) => new_branch = Some(b.as_str()),
                None => return usage(),
            },
            // any other option is git's to handle
            s if s.starts_with('-') => return plain(args, "an option git-cow does not handle"),
            s => rest.push(s.to_string()),
        }
    }
    if rest.len() > 2 {
        return usage();
    }
    if rest.is_empty() {
        let (Some(b), Ok(name)) = (new_branch, repo_name(&repo)) else {
            return usage();
        };
        let leaf = b.rsplit('/').next().unwrap_or(b);
        let path = match git_cow::git(&repo, &["config", "--get", "cow.root"]) {
            // a root with {repo} already names the repository's folder
            Ok(r) if r.contains("{repo}") => config_path(&repo, "cow.root")
                .unwrap_or_default()
                .join(leaf),
            _ => config_path(&repo, "cow.root")
                .unwrap_or_else(|| {
                    Path::new(&std::env::var("HOME").unwrap_or_default()).join(".worktrees")
                })
                .join(name)
                .join(leaf),
        };
        rest.push(path.to_string_lossy().into_owned());
    }
    // what git worktree add gets if cloning is off or fails, with the path filled in
    let mut git_args: Vec<String> = new_branch
        .map(|b| vec!["-b".into(), b.into()])
        .unwrap_or_default();
    git_args.extend(rest.iter().cloned());
    if std::env::var("GIT_COW").is_ok_and(|v| v == "0") {
        return plain(&git_args, "GIT_COW=0");
    }

    let res = std::path::absolute(&rest[0]).and_then(|wt| {
        let template = template_for(&repo, &wt)?;
        // the template follows the default branch, where most worktrees start from
        let start = if git_cow::git(&repo, &["rev-parse", "--verify", "-q", "origin/HEAD"]).is_ok()
        {
            "origin/HEAD"
        } else {
            "HEAD"
        };
        git_cow::add_with(
            &repo,
            &template,
            start,
            &wt,
            new_branch,
            rest.get(1).map(String::as_str),
            &options(&repo),
        )?;
        println!("{}", wt.display());
        Ok(())
    });
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => plain(&git_args, &e.to_string()),
    }
}

fn plain(args: &[String], why: &str) -> ExitCode {
    eprintln!("git-cow: {why}, using git worktree add");
    let ok = Command::new("git")
        .args(["worktree", "add"])
        .args(args)
        .status()
        .is_ok_and(|s| s.success());
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

// (path, branch) of every worktree, the main checkout first
fn worktrees(repo: &Path) -> std::io::Result<Vec<(PathBuf, String)>> {
    let out = git_cow::git(repo, &["worktree", "list", "--porcelain"])?;
    let mut list = Vec::new();
    for block in out.split("\n\n") {
        let mut path = None;
        let mut branch = "(detached)".to_string();
        for line in block.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(p));
            } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
                branch = b.to_string();
            } else if line == "bare" {
                branch = "(bare)".into();
            }
        }
        if let Some(p) = path {
            list.push((p, branch));
        }
    }
    Ok(list)
}

fn list() -> std::io::Result<()> {
    let repo = std::env::current_dir()?;
    for (path, branch) in worktrees(&repo)? {
        if !is_template(&repo, &path) {
            println!("{}\t{branch}", path.display());
        }
    }
    Ok(())
}

fn remove(args: &[String]) -> std::io::Result<()> {
    let repo = std::env::current_dir()?;
    let warm_first = args.iter().any(|a| a == "--warm");
    let force = args.iter().any(|a| a == "--force");
    let paths: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    let [path] = paths.as_slice() else {
        return Err(other("give one worktree path".into()));
    };
    // git lists real paths, macOS temp folders are /var behind a /private/var symlink
    let wt = std::fs::canonicalize(path)?;
    let all = worktrees(&repo)?;
    let Some((_, branch)) = all.iter().find(|(p, _)| *p == wt) else {
        return Err(other(format!(
            "{} is not a worktree of this repository",
            wt.display()
        )));
    };
    if is_template(&repo, &wt) {
        return Err(other(format!(
            "{} is a git-cow template, not removed",
            wt.display()
        )));
    }
    if all.first().is_some_and(|(p, _)| *p == wt) {
        return Err(other("that is the main checkout, not removed".into()));
    }
    if warm_first {
        let template = template_for(&repo, &wt)?;
        git_cow::warm_with(&wt, &template, &options(&repo))?;
    }
    let wt_s = wt.to_string_lossy();
    let mut cmd = vec!["worktree", "remove"];
    if force {
        cmd.push("--force");
    }
    cmd.push(&wt_s);
    git_cow::git(&repo, &cmd)?;
    // -d deletes only a merged branch, an unmerged one stays with a note
    if !branch.starts_with('(') && git_cow::git(&repo, &["branch", "-d", branch]).is_err() {
        eprintln!("git-cow: kept branch {branch}, it is not merged");
    }
    Ok(())
}

fn warm(wt: &Path) -> std::io::Result<()> {
    let wt = std::path::absolute(wt)?;
    let template = template_for(&wt, &wt)?;
    git_cow::warm_with(&wt, &template, &options(&wt))
}
