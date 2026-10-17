use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const USAGE: &str = "usage: git cow add [-b <branch>] [<path>] [<commit-ish>]
       git cow warm <worktree>

add   creates a worktree by cloning a template checkout, falling back to git worktree add.
      Without <path> it goes to <cow.root>/<repo>/<last part of the branch name>
warm  refreshes the template with a worktree's gitignored directories, run it after a merge

git config:
  cow.root      where worktrees go when <path> is left out (default ~/.worktrees)
  cow.template  the template checkout (default <worktree's parent>/.git-cow/<repo>)
GIT_COW=0 disables cloning";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let res = match args.first().map(String::as_str) {
        Some("add") => return add(&args[1..]),
        Some("warm") if args.len() == 2 => warm(Path::new(&args[1])),
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

fn config(repo: &Path, key: &str) -> Option<PathBuf> {
    let v = git_cow::git(repo, &["config", "--get", key]).ok()?;
    let home = std::env::var("HOME").unwrap_or_default();
    Some(match v.strip_prefix("~/") {
        Some(rest) => Path::new(&home).join(rest),
        None => PathBuf::from(v),
    })
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
    if let Some(t) = config(repo, "cow.template") {
        return Ok(t);
    }
    let parent = wt.parent().unwrap_or(Path::new("."));
    Ok(parent.join(".git-cow").join(repo_name(repo)?))
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
        let root = config(&repo, "cow.root").unwrap_or_else(|| {
            Path::new(&std::env::var("HOME").unwrap_or_default()).join(".worktrees")
        });
        let leaf = b.rsplit('/').next().unwrap_or(b);
        rest.push(root.join(name).join(leaf).to_string_lossy().into_owned());
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
        git_cow::add(
            &repo,
            &template,
            start,
            &wt,
            new_branch,
            rest.get(1).map(String::as_str),
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

fn warm(wt: &Path) -> std::io::Result<()> {
    let wt = std::path::absolute(wt)?;
    let template = template_for(&wt, &wt)?;
    git_cow::warm(&wt, &template)
}
