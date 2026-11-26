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
    /// and `Duration::MAX` never
    pub warm_interval: std::time::Duration,
    /// How many times to pass `--force` to `git worktree add`, which counts it like `-f -f`. Any
    /// count also lets [`add_with`] replace [`Foreign::Files`], and never another kind
    pub force: u8,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            share: DEFAULT_SHARE.iter().map(|s| s.to_string()).collect(),
            warm_interval: std::time::Duration::from_secs(3600),
            force: 0,
        }
    }
}

impl Options {
    /// The defaults plus every `cow.share` entry and `cow.warmInterval` from `repo`'s git config
    pub fn from_repo(repo: &Path) -> Options {
        let mut opts = Options::default();
        let extra = git(repo, &["config", "--get-all", "cow.share"]).unwrap_or_default();
        for d in extra.lines().map(str::trim).filter(|d| !d.is_empty()) {
            if !opts.share.iter().any(|l| l == d) {
                opts.share.push(d.to_string());
            }
        }
        if let Some(i) = git(repo, &["config", "--get", "cow.warmInterval"])
            .ok()
            .and_then(|v| parse_interval(&v))
        {
            opts.warm_interval = i;
        }
        opts
    }
}

// "0", "90" (seconds), "30m", "1h", "never"
fn parse_interval(v: &str) -> Option<std::time::Duration> {
    let v = v.trim();
    if v == "never" {
        return Some(std::time::Duration::MAX);
    }
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
        .map(|n| std::time::Duration::from_secs(n * unit))
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
    probe(src_dir, dst_dir).unwrap_or(false)
}

// `can_clone`, with an error when the probe file cannot be written, such as no permission or a
// full disk, since the user can fix that and the volume may clone fine
fn probe(src_dir: &Path, dst_dir: &Path) -> io::Result<bool> {
    let key = (src_dir.to_path_buf(), dst_dir.to_path_buf());
    // Held through the probe, since threads probing at once would share its file names
    let mut probes = PROBES.lock().unwrap();
    if let Some(known) = probes.get(&key) {
        return Ok(*known);
    }
    let id = std::process::id();
    let src = src_dir.join(format!(".cow-probe-{id}"));
    let into = dst_dir.join(format!(".cow-probe-dir-{id}"));
    let res = fs::write(&src, b"x")
        .and_then(|()| fs::create_dir_all(&into))
        .map(|()| clone_into(&src, &into).is_ok());
    let _ = fs::remove_file(&src);
    let _ = fs::remove_dir_all(&into);
    if let Ok(ok) = res {
        probes.insert(key, ok);
    }
    res.map_err(|e| {
        err(format!(
            "cannot write in {} or {}: {e}",
            src_dir.display(),
            dst_dir.display()
        ))
    })
}

// A file next to the template, `.<name>.<ext>`
fn beside(template: &Path, ext: &str) -> PathBuf {
    let name = template.file_name().unwrap_or_default().to_string_lossy();
    template.with_file_name(format!(".{name}.{ext}"))
}

// Held while a template changes, so a clone never sees it half updated, across processes too
fn lock_template(template: &Path) -> io::Result<File> {
    let file = File::create(beside(template, "lock"))?;
    file.lock()?;
    Ok(file)
}

fn real(dir: &Path, args: &[&str]) -> Option<PathBuf> {
    git(dir, args).ok().and_then(|p| fs::canonicalize(p).ok())
}

fn remove_any(path: &Path) {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => {
            let _ = fs::remove_dir_all(path);
        }
        Ok(_) => {
            let _ = fs::remove_file(path);
        }
        Err(_) => {}
    }
}

// Move `template` aside, then delete it, so a delete cut short never leaves files at its path
fn discard(template: &Path) {
    let aside = beside(template, "trash");
    remove_any(&aside);
    if fs::rename(template, &aside).is_ok() {
        remove_any(&aside);
    } else {
        remove_any(template);
    }
}

// Every git-cow version takes this lock before it makes a template, so a path with the lock
// beside it holds what is left of git-cow's own template
fn made_by_git_cow(template: &Path) -> bool {
    beside(template, "lock").exists()
}

/// A template path that is not a git-cow extra checkout, inside the `io::Error` that [`add_with`],
/// [`warm_with`] and [`remove_template`] return. They leave it alone, and only [`Options::force`]
/// in [`add_with`] replaces [`Foreign::Files`]. The message names the path, the advice is the caller's
#[derive(Debug)]
#[non_exhaustive]
pub enum Foreign {
    /// A file, or a directory with files outside git
    Files(PathBuf),
    /// A main checkout, which git-cow would reset or delete
    Checkout(PathBuf),
    /// A worktree on this branch, while extra checkouts are always detached
    Branch(PathBuf, String),
    /// A git repository itself, bare or a `.git` directory
    Repository(PathBuf),
    /// A directory with files tracked by the checkout at the second path
    Inside(PathBuf, PathBuf),
    /// A worktree git cannot read, for this reason, that git-cow did not make
    Unreadable(PathBuf, String),
    /// A detached worktree with no git-cow lock beside it, or one in a rebase, merge or bisect
    Detached(PathBuf),
    /// A directory holding the checkout or repository at the second path
    Nested(PathBuf, PathBuf),
    /// The home folder, or a folder above it
    Home(PathBuf),
}

impl std::fmt::Display for Foreign {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let elsewhere = "so git-cow never uses it as its extra checkout";
        match self {
            Foreign::Files(p) => write!(
                f,
                "'{}' holds files and is not a git-cow extra checkout, so it is left alone",
                p.display()
            ),
            Foreign::Checkout(p) => write!(f, "'{}' is a main checkout, {elsewhere}", p.display()),
            Foreign::Branch(p, b) => write!(
                f,
                "'{}' is a worktree on branch {b}, {elsewhere}",
                p.display()
            ),
            Foreign::Repository(p) => {
                write!(f, "'{}' is a git repository, {elsewhere}", p.display())
            }
            Foreign::Inside(p, top) => write!(
                f,
                "'{}' holds files tracked by the checkout at '{}', {elsewhere}",
                p.display(),
                top.display()
            ),
            Foreign::Unreadable(p, why) => write!(
                f,
                "git cannot read the worktree at '{}' ({why}), {elsewhere}",
                p.display()
            ),
            Foreign::Detached(p) => write!(
                f,
                "'{}' is a worktree git-cow did not make or one in the middle of a rebase, merge or bisect, {elsewhere}",
                p.display()
            ),
            Foreign::Nested(p, inner) => write!(
                f,
                "'{}' holds the checkout or repository at '{}', {elsewhere}",
                p.display(),
                inner.display()
            ),
            Foreign::Home(p) => write!(
                f,
                "'{}' is or holds the home folder, {elsewhere}",
                p.display()
            ),
        }
    }
}

impl std::error::Error for Foreign {}

// What git-cow never made a template of. Templates are detached linked worktrees, of this or
// another repository with the same name. `made` is whether git-cow's lock sat beside the path
fn foreign(template: &Path, made: bool) -> Option<Foreign> {
    let path = template.to_path_buf();
    if !occupied(template) {
        return None;
    }
    if template.join("HEAD").is_file()
        && template.join("objects").is_dir()
        && template.join("refs").is_dir()
    {
        return Some(Foreign::Repository(path));
    }
    let dot = template.join(".git");
    // a template's `.git` is always a file, and only an empty `.git` directory is no repository
    if dot.is_dir() && occupied(&dot) {
        return Some(Foreign::Checkout(path));
    }
    if dot.is_file() {
        let gitdir = git(template, &["rev-parse", "--absolute-git-dir"]);
        let common = real(
            template,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        );
        match (gitdir, common) {
            (Ok(gitdir), Some(common))
                if real(template, &["rev-parse", "--show-toplevel"])
                    == fs::canonicalize(template).ok() =>
            {
                if fs::canonicalize(&gitdir).ok().as_ref() == Some(&common) {
                    return Some(Foreign::Checkout(path));
                }
                if let Ok(b) = git(template, &["symbolic-ref", "-q", "--short", "HEAD"]) {
                    return Some(Foreign::Branch(path, b));
                }
                // a detached HEAD is git-cow's only with its lock, and never while a user is mid-rebase
                let busy = ["rebase-merge", "rebase-apply", "MERGE_HEAD", "BISECT_LOG"]
                    .iter()
                    .any(|f| Path::new(&gitdir).join(f).exists());
                return (!made || busy).then_some(Foreign::Detached(path));
            }
            // what is left of git-cow's own template is rebuilt, as before
            (Err(_), _) if made => return None,
            (Err(e), _) => return Some(Foreign::Unreadable(path, e.to_string())),
            // a `.git` git reads as some other checkout's, so it is files inside that one
            _ => {}
        }
    }
    if git(template, &["ls-files"]).is_ok_and(|o| !o.is_empty()) {
        let top = real(template, &["rev-parse", "--show-toplevel"]).unwrap_or_default();
        return Some(Foreign::Inside(path, top));
    }
    if let Some(inner) = nested_repository(template) {
        return Some(Foreign::Nested(path, inner));
    }
    let home = std::env::var_os("HOME").and_then(|h| fs::canonicalize(h).ok());
    if let (Some(home), Ok(real)) = (home, fs::canonicalize(template))
        && home.starts_with(&real)
    {
        return Some(Foreign::Home(path));
    }
    if made {
        return None;
    }
    Some(Foreign::Files(path))
}

// The first checkout or repository below `dir`, without following symlinks. An empty `.git`
// directory holds nothing, so it does not count
fn nested_repository(dir: &Path) -> Option<PathBuf> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if entry.file_name() == ".git" && (kind.is_file() || occupied(&path)) {
                return Some(d);
            }
            if kind.is_dir() {
                if path.join("HEAD").is_file() && path.join("objects").is_dir() {
                    return Some(path);
                }
                stack.push(path);
            }
        }
    }
    None
}

// Create the template checkout, or move it to `start`
fn refresh_template(repo: &Path, template: &Path, start: &str) -> io::Result<()> {
    remove_any(&beside(template, "trash"));
    if template.join(".git").exists()
        && git(template, &["checkout", "--detach", "--force", start]).is_ok()
    {
        return Ok(());
    }
    discard(template);
    if clone_checkout(repo, template, start).is_ok() {
        return Ok(());
    }
    discard(template);
    git(
        repo,
        &[
            "worktree",
            "add",
            "-f",
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
            "-f",
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

// Whether `path`, or the nearest part of it that exists, resolves inside `root`
fn within(root: &Path, path: &Path) -> bool {
    path.ancestors()
        .skip(1)
        .find_map(|a| fs::canonicalize(a).ok())
        .is_some_and(|a| a.starts_with(root))
}

// `std::path::absolute` with `..` resolved by name, so `repo/..` is never a path git-cow deletes
fn absolute(path: &Path) -> io::Result<PathBuf> {
    let mut out = PathBuf::new();
    for c in std::path::absolute(path)?.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            c => out.push(c),
        }
    }
    Ok(out)
}

// Swap a clone of each ignored directory of `src` into the template. Loose ignored files stay
// out, so local secrets such as .env never reach a worktree
fn warm_template(src: &Path, template: &Path, share: &[String]) -> io::Result<()> {
    let staging = beside(template, "warm");
    let root = fs::canonicalize(template)?;
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
            // a tracked symlink on the way would lead the swap out of the template
            if !within(&root, &dst) {
                return Ok(());
            }
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
    let template = &absolute(template)?;
    if !template.join(".git").exists() {
        return Err(err(format!("no template at {}", template.display())));
    }
    if let Some(f) = foreign(template, made_by_git_cow(template)) {
        return Err(io::Error::other(f));
    }
    let _lock = lock_template(template)?;
    warm_template(src, template, &opts.share)?;
    stamp(template)
}

/// Remove `template`, its registration in `repo`, and the lock, stamp and staging files next to it.
/// A path that is not an extra checkout git-cow made fails with [`Foreign`] and stays. Call it
/// only when no [`add`] or [`warm`] on this template can run: one waiting for the lock would get a
/// deleted file, and the next caller a new one
pub fn remove_template(repo: &Path, template: &Path) -> io::Result<()> {
    let template = &absolute(template)?;
    if template.parent().is_none_or(|p| !p.exists()) {
        return Ok(());
    }
    if let Some(f) = foreign(template, made_by_git_cow(template)) {
        return Err(io::Error::other(f));
    }
    let _lock = lock_template(template)?;
    let registered = template.join(".git").exists();
    let aside = beside(template, "trash");
    remove_any(&aside);
    // aside before git deletes it in place, so a delete cut short never leaves files at the path
    let moved = fs::rename(template, &aside).is_ok();
    if registered
        && let Err(e) = git(
            repo,
            &["worktree", "remove", "--force", &template.to_string_lossy()],
        )
    {
        if moved {
            let _ = fs::rename(&aside, template);
        }
        return Err(e);
    }
    remove_any(&aside);
    remove_any(template);
    let _ = fs::remove_dir_all(beside(template, "warm"));
    for ext in ["warmed", "lock"] {
        match fs::remove_file(beside(template, ext)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

// Marks when the template was last warmed, so add_with can skip a warm-up it did recently
fn stamp_path(template: &Path) -> PathBuf {
    beside(template, "warmed")
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

/// The error inside the [`io::ErrorKind::AlreadyExists`] that [`add_with`] returns for a `wt` that
/// exists and is not empty. Test for it with [`is_occupied`]
#[derive(Debug)]
pub struct Occupied(pub PathBuf);

impl std::fmt::Display for Occupied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "'{}' already exists", self.0.display())
    }
}

impl std::error::Error for Occupied {}

/// Whether `e` is [`add_with`] refusing a `wt` that exists and is not empty, where a plain
/// `git worktree add` would fail too
pub fn is_occupied(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|inner| inner.is::<Occupied>())
}

/// The error inside the `io::Error` when git-cow cannot clone here at all: the volume has no
/// clone support or the repository has submodules. Test for it with [`is_unsupported`]
#[derive(Debug)]
pub struct Unsupported(pub &'static str);

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Unsupported {}

/// Whether `e` is [`Unsupported`], the one error after which a plain `git worktree add` runs.
/// Any other error is for the user to fix, so a plain worktree never hides it
pub fn is_unsupported(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|inner| inner.is::<Unsupported>())
}

// The branch git worktree add creates if none has its name yet, so a failed add may delete it:
// `-b`, or without a commit-ish one named after the path
fn new_branch_name(
    repo: &Path,
    wt: &Path,
    new_branch: Option<&str>,
    commitish: Option<&str>,
) -> Option<String> {
    let b = match (new_branch, commitish) {
        (Some(b), _) => b.to_string(),
        (None, None) => wt.file_name()?.to_string_lossy().into_owned(),
        _ => return None,
    };
    git(
        repo,
        &["rev-parse", "--verify", "-q", &format!("refs/heads/{b}")],
    )
    .is_err()
    .then_some(b)
}

// Something git worktree add refuses: anything but a missing path or an empty directory
fn occupied(wt: &Path) -> bool {
    match fs::symlink_metadata(wt) {
        Err(_) => false,
        Ok(m) if m.is_dir() => fs::read_dir(wt).map_or(true, |mut d| d.next().is_some()),
        Ok(_) => true,
    }
}

// Fill `wt`, already a worktree with no files, from the template
fn populate(template: &Path, wt: &Path) -> io::Result<()> {
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
/// `new_branch` and `commitish` are `git worktree add`'s. Like it, refuses a `wt` that exists and
/// is not empty, and on error removes only the worktree and branch this call created
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
    // git resolves a relative path against `repo` and the file calls against the current directory
    let wt = &absolute(wt)?;
    let template = &absolute(template)?;
    if occupied(wt) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            Occupied(wt.to_path_buf()),
        ));
    }
    let wt_parent = wt
        .parent()
        .ok_or_else(|| err("worktree has no parent".into()))?;
    let tpl_parent = template
        .parent()
        .ok_or_else(|| err("template has no parent".into()))?;
    fs::create_dir_all(wt_parent)?;
    fs::create_dir_all(tpl_parent)?;
    if !probe(tpl_parent, wt_parent)? {
        return Err(io::Error::other(Unsupported("volume cannot clone files")));
    }

    // checked before the lock, so a refused path gets no lock file beside it
    let replace = match foreign(template, made_by_git_cow(template)) {
        Some(Foreign::Files(_)) if opts.force > 0 => true,
        Some(f) => return Err(io::Error::other(f)),
        None => false,
    };
    let _lock = lock_template(template)?;
    if replace {
        discard(template);
    }
    refresh_template(repo, template, template_start)?;
    if template.join(".gitmodules").exists() {
        // A cloned submodule `.git` file would point into the template's gitdir
        return Err(io::Error::other(Unsupported(
            "submodules are not supported",
        )));
    }
    // Re-cloning big caches on every add costs seconds, and a template an hour old only means a
    // little more rebuilding. Bare repositories have no ignored files, so nothing changes for them
    let never = opts.warm_interval == std::time::Duration::MAX;
    if !never && (opts.warm_interval.is_zero() || !warmed_within(template, opts.warm_interval)) {
        let _ = warm_template(repo, template, &opts.share);
        let _ = stamp(template);
    }

    // Only a branch this call creates may be removed on failure, never one that was there before
    let ours = new_branch_name(repo, wt, new_branch, commitish);
    // --no-checkout creates the gitdir entry and the `.git` file, so git state stays per worktree.
    // Until it succeeds `wt` is not ours, so a failure here only drops the branch git made first
    let wt_s = wt.to_string_lossy();
    let mut args = vec!["worktree", "add", "--no-checkout"];
    args.extend(std::iter::repeat_n("--force", opts.force.into()));
    if let Some(b) = new_branch {
        args.extend(["-b", b]);
    }
    args.push(&wt_s);
    args.extend(commitish);
    let was_dir = wt.is_dir();
    if let Err(e) = git(repo, &args) {
        if let Some(b) = &ours {
            let _ = git(repo, &["branch", "-D", b.as_str()]);
        }
        return Err(e);
    }
    let res = populate(template, wt);
    if res.is_err() {
        let _ = git(
            repo,
            &["worktree", "remove", "--force", &wt.to_string_lossy()],
        );
        let _ = fs::remove_dir_all(wt);
        if was_dir {
            let _ = fs::create_dir(wt);
        }
        if let Some(b) = &ours {
            let _ = git(repo, &["branch", "-D", b.as_str()]);
        }
    }
    res
}

/// How [`add_or_plain`] made the worktree
#[derive(Debug)]
pub enum Added {
    /// Cloned from the template
    Cloned,
    /// With `git worktree add`, since cloning is [`Unsupported`] for this reason
    Plain(io::Error),
}

/// [`add_with`], or `git worktree add` with the same `wt`, `new_branch` and `commitish` when
/// cloning is [`Unsupported`]. Any other error is returned with no plain add
pub fn add_or_plain(
    repo: &Path,
    template: &Path,
    template_start: &str,
    wt: &Path,
    new_branch: Option<&str>,
    commitish: Option<&str>,
    opts: &Options,
) -> io::Result<Added> {
    let wt = &absolute(wt)?;
    let Err(why) = add_with(
        repo,
        template,
        template_start,
        wt,
        new_branch,
        commitish,
        opts,
    ) else {
        return Ok(Added::Cloned);
    };
    if !is_unsupported(&why) {
        return Err(why);
    }

    let wt_s = wt.to_string_lossy();
    let mut args = vec!["worktree", "add"];
    args.extend(std::iter::repeat_n("--force", opts.force.into()));
    if let Some(b) = new_branch {
        args.extend(["-b", b]);
    }
    args.push(&wt_s);
    args.extend(commitish);
    let ours = new_branch_name(repo, wt, new_branch, commitish);
    if let Err(e) = git(repo, &args) {
        // git creates the branch before it checks the path
        if let Some(b) = &ours {
            let _ = git(repo, &["branch", "-D", b.as_str()]);
        }
        return Err(err(format!("{e}, after cloning failed: {why}")));
    }
    Ok(Added::Plain(why))
}
