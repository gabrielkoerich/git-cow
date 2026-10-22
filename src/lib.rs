//! Git worktrees cloned from a per-repository template checkout, sharing disk blocks until written.
//! macOS clones whole directories with clonefile(2), Linux uses `cp --reflink=always` (FICLONE)

use std::collections::HashMap;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{LazyLock, Mutex};

/// Hidden directories that hold only build caches, so they are shared like any other ignored
/// directory. Every other path with a hidden component is tool or agent config and stays out
pub const DEFAULT_SHARE: &[&str] = &[
    ".uv-cache",
    ".next",
    ".turbo",
    ".nuxt",
    ".svelte-kit",
    ".parcel-cache",
    ".gradle",
    ".mypy_cache",
    ".ruff_cache",
    ".pytest_cache",
];

// Clone support per (source dir, destination dir), probed once per process
static PROBES: LazyLock<Mutex<HashMap<(PathBuf, PathBuf), bool>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn err(msg: String) -> io::Error {
    io::Error::other(msg)
}

/// What to share and how often to refresh the template from the checkout
#[derive(Clone, Debug)]
pub struct Options {
    /// Hidden directories shared like any other ignored directory
    pub share: Vec<String>,
    /// [`add_with`] warms the template from the checkout at most this often, zero means every time
    pub warm_interval: std::time::Duration,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            share: DEFAULT_SHARE.iter().map(|s| s.to_string()).collect(),
            warm_interval: std::time::Duration::from_secs(3600),
        }
    }
}

// Clone `src`, a file or a whole directory, to `dst_dir/<its name>` in one call
#[cfg(target_os = "macos")]
fn clone_into(src: &Path, dst_dir: &Path) -> io::Result<()> {
    use std::ffi::{CString, c_char, c_int};
    use std::os::unix::ffi::OsStrExt;
    unsafe extern "C" {
        fn clonefile(src: *const c_char, dst: *const c_char, flags: u32) -> c_int;
    }
    const CLONE_NOFOLLOW: u32 = 1;
    let name = src
        .file_name()
        .ok_or_else(|| err(format!("{} has no name", src.display())))?;
    let s = CString::new(src.as_os_str().as_bytes()).map_err(io::Error::other)?;
    let d = CString::new(dst_dir.join(name).as_os_str().as_bytes()).map_err(io::Error::other)?;
    if unsafe { clonefile(s.as_ptr(), d.as_ptr(), CLONE_NOFOLLOW) } != 0 {
        let e = io::Error::last_os_error();
        return Err(err(format!("clonefile {}: {e}", src.display())));
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn clone_into(src: &Path, dst_dir: &Path) -> io::Result<()> {
    // fails instead of copying when the volume cannot clone
    let out = Command::new("cp")
        .args(["-a", "--reflink=always"])
        .arg(src)
        .arg(dst_dir)
        .output()?;
    if !out.status.success() {
        return Err(err(format!(
            "cp {}: {}",
            src.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

/// Runs git in `dir` and returns its trimmed stdout
pub fn git(dir: &Path, args: &[&str]) -> io::Result<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output()?;
    if !out.status.success() {
        return Err(err(format!(
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Whether a file in `src_dir` clones into `dst_dir`: both on one volume that supports it.
/// Probed with one small file, since filesystem names miss mount options like XFS without reflink
pub fn can_clone(src_dir: &Path, dst_dir: &Path) -> bool {
    let key = (src_dir.to_path_buf(), dst_dir.to_path_buf());
    if let Some(known) = PROBES.lock().unwrap().get(&key) {
        return *known;
    }
    let id = std::process::id();
    let src = src_dir.join(format!(".cow-probe-{id}"));
    let into = dst_dir.join(format!(".cow-probe-dir-{id}"));
    let ok = fs::write(&src, b"x").is_ok()
        && fs::create_dir_all(&into).is_ok()
        && clone_into(&src, &into).is_ok();
    let _ = fs::remove_file(&src);
    let _ = fs::remove_dir_all(&into);
    PROBES.lock().unwrap().insert(key, ok);
    ok
}

// Held while a template changes, so a clone never sees it half updated, across processes too
fn lock_template(template: &Path) -> io::Result<File> {
    let name = template.file_name().unwrap_or_default().to_string_lossy();
    let file = File::create(template.with_file_name(format!(".{name}.lock")))?;
    file.lock()?;
    Ok(file)
}

// Create the template checkout, or move it to `start`
fn refresh_template(repo: &Path, template: &Path, start: &str) -> io::Result<()> {
    if template.join(".git").exists()
        && git(template, &["checkout", "--detach", "--force", start]).is_ok()
    {
        return Ok(());
    }
    let _ = fs::remove_dir_all(template);
    let _ = git(repo, &["worktree", "prune"]);
    if clone_checkout(repo, template, start).is_ok() {
        return Ok(());
    }
    let _ = fs::remove_dir_all(template);
    let _ = git(repo, &["worktree", "prune"]);
    git(
        repo,
        &[
            "worktree",
            "add",
            "--detach",
            &template.to_string_lossy(),
            start,
        ],
    )?;
    Ok(())
}

// Make the extra checkout by cloning the user's checkout, so only files that differ from `start`
// take disk. Edits, untracked and ignored files are cleaned out, the warm-up adds build output back
fn clone_checkout(repo: &Path, template: &Path, start: &str) -> io::Result<()> {
    let top = PathBuf::from(git(repo, &["rev-parse", "--show-toplevel"])?);
    let parent = template
        .parent()
        .ok_or_else(|| err("template has no parent".into()))?;
    if !can_clone(&top, parent) {
        return Err(err("checkout and template cannot share blocks".into()));
    }
    git(
        repo,
        &[
            "worktree",
            "add",
            "--no-checkout",
            "--detach",
            &template.to_string_lossy(),
            start,
        ],
    )?;
    for name in git(&top, &["ls-tree", "--name-only", "HEAD"])?.lines() {
        let src = top.join(name);
        if src.symlink_metadata().is_ok() {
            clone_into(&src, template)?;
        }
    }
    let src_git = git(&top, &["rev-parse", "--absolute-git-dir"])?;
    let tpl_git = git(template, &["rev-parse", "--absolute-git-dir"])?;
    fs::copy(
        Path::new(&src_git).join("index"),
        Path::new(&tpl_git).join("index"),
    )?;
    let _ = git(template, &["update-index", "-q", "--refresh"]);
    git(template, &["reset", "--hard", "-q"])?;
    git(template, &["clean", "-fdxq"])?;
    Ok(())
}

// Gitignored directories of a checkout at any depth, which hold whatever its tooling produced.
// A hidden component must be in `share`, anything else hidden is tool or agent config
fn ignored_dirs(src: &Path, share: &[String]) -> io::Result<Vec<String>> {
    let out = git(
        src,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
        ],
    )?;
    Ok(out
        .lines()
        .filter_map(|l| l.strip_suffix('/'))
        .filter(|n| {
            !n.is_empty()
                && n.split('/')
                    .all(|c| !c.starts_with('.') || share.iter().any(|s| s == c))
        })
        .map(String::from)
        .collect())
}

// Swap a clone of each ignored directory of `src` into the template. Loose ignored files stay
// out, so local secrets such as .env never reach a worktree
fn warm_template(src: &Path, template: &Path, share: &[String]) -> io::Result<()> {
    let name = template.file_name().unwrap_or_default().to_string_lossy();
    let staging = template.with_file_name(format!(".{name}.warm"));
    for dir in ignored_dirs(src, share)? {
        // A directory the template commit tracks is not build output
        if !git(template, &["ls-files", "--", &dir])
            .unwrap_or_default()
            .is_empty()
        {
            continue;
        }
        let _ = fs::remove_dir_all(&staging);
        fs::create_dir_all(&staging)?;
        // One directory that cannot be cloned only costs that directory's sharing
        let _ = (|| {
            clone_into(&src.join(&dir), &staging)?;
            let dst = template.join(&dir);
            let _ = fs::remove_dir_all(&dst);
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)?;
            }
            let base = Path::new(&dir).file_name().unwrap_or_default();
            fs::rename(staging.join(base), dst)
        })();
        let _ = fs::remove_dir_all(&staging);
    }
    Ok(())
}

/// Refresh an existing `template` with the gitignored directories of `src`, a checkout or a
/// worktree whose work merged. Shares the hidden directories in [`DEFAULT_SHARE`]
pub fn warm(src: &Path, template: &Path) -> io::Result<()> {
    warm_with(src, template, &Options::default())
}

/// [`warm`] with your own [`Options`]. It always warms, the interval only applies to [`add_with`]
pub fn warm_with(src: &Path, template: &Path, opts: &Options) -> io::Result<()> {
    if !template.join(".git").exists() {
        return Err(err(format!("no template at {}", template.display())));
    }
    let _lock = lock_template(template)?;
    warm_template(src, template, &opts.share)?;
    stamp(template)
}

// Marks when the template was last warmed, so add_with can skip a warm-up it did recently
fn stamp_path(template: &Path) -> PathBuf {
    let name = template.file_name().unwrap_or_default().to_string_lossy();
    template.with_file_name(format!(".{name}.warmed"))
}

fn stamp(template: &Path) -> io::Result<()> {
    fs::write(stamp_path(template), b"")
}

fn warmed_within(template: &Path, interval: std::time::Duration) -> bool {
    fs::metadata(stamp_path(template))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < interval)
}

fn populate(
    repo: &Path,
    template: &Path,
    wt: &Path,
    new_branch: Option<&str>,
    commitish: Option<&str>,
) -> io::Result<()> {
    // --no-checkout creates the gitdir entry and the `.git` file, so git state stays per worktree
    let wt_s = wt.to_string_lossy();
    let mut args = vec!["worktree", "add", "--no-checkout"];
    if let Some(b) = new_branch {
        args.extend(["-b", b]);
    }
    args.push(&wt_s);
    args.extend(commitish);
    git(repo, &args)?;

    for entry in fs::read_dir(template)? {
        let entry = entry?;
        if entry.file_name() == ".git" {
            continue;
        }
        clone_into(&entry.path(), wt)?;
    }

    // The template index describes the cloned files. Refresh its stat data for the new inodes,
    // then reset only the files where the worktree's commit differs from the template's
    let tpl_git = git(template, &["rev-parse", "--absolute-git-dir"])?;
    let wt_git = git(wt, &["rev-parse", "--absolute-git-dir"])?;
    fs::copy(
        Path::new(&tpl_git).join("index"),
        Path::new(&wt_git).join("index"),
    )?;
    let _ = git(wt, &["update-index", "-q", "--refresh"]);
    git(wt, &["reset", "--hard", "-q"])?;
    Ok(())
}

/// Create `wt` by cloning `template`, refreshed to `template_start` and warmed from `repo` first.
/// `new_branch` and `commitish` are `git worktree add`'s; on error nothing is left behind
pub fn add(
    repo: &Path,
    template: &Path,
    template_start: &str,
    wt: &Path,
    new_branch: Option<&str>,
    commitish: Option<&str>,
) -> io::Result<()> {
    add_with(
        repo,
        template,
        template_start,
        wt,
        new_branch,
        commitish,
        &Options::default(),
    )
}

/// [`add`] with your own [`Options`]
pub fn add_with(
    repo: &Path,
    template: &Path,
    template_start: &str,
    wt: &Path,
    new_branch: Option<&str>,
    commitish: Option<&str>,
    opts: &Options,
) -> io::Result<()> {
    let wt_parent = wt
        .parent()
        .ok_or_else(|| err("worktree has no parent".into()))?;
    let tpl_parent = template
        .parent()
        .ok_or_else(|| err("template has no parent".into()))?;
    fs::create_dir_all(wt_parent)?;
    fs::create_dir_all(tpl_parent)?;
    if !can_clone(tpl_parent, wt_parent) {
        return Err(err("volume cannot clone files".into()));
    }

    let _lock = lock_template(template)?;
    refresh_template(repo, template, template_start)?;
    if template.join(".gitmodules").exists() {
        // A cloned submodule `.git` file would point into the template's gitdir
        return Err(err("submodules are not supported".into()));
    }
    // Re-cloning big caches on every add costs seconds, and a template an hour old only means a
    // little more rebuilding. Bare repositories have no ignored files, so nothing changes for them
    if opts.warm_interval.is_zero() || !warmed_within(template, opts.warm_interval) {
        let _ = warm_template(repo, template, &opts.share);
        let _ = stamp(template);
    }

    // Only a branch this call creates may be removed on failure, never one that was there before
    let ours = new_branch
        .filter(|b| git(repo, &["rev-parse", "--verify", &format!("refs/heads/{b}")]).is_err());
    let res = populate(repo, template, wt, new_branch, commitish);
    if res.is_err() {
        let _ = git(
            repo,
            &["worktree", "remove", "--force", &wt.to_string_lossy()],
        );
        let _ = fs::remove_dir_all(wt);
        let _ = git(repo, &["worktree", "prune"]);
        if let Some(b) = ours {
            let _ = git(repo, &["branch", "-D", b]);
        }
    }
    res
}
// cowbench c1
// cowbench c2
