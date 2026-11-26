//! Relative paths, alone in their own binary since the test changes the current directory
use std::path::Path;
use std::process::Command;

fn sh(dir: &Path, args: &[&str]) {
    let st = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(st.success(), "git {args:?}");
}

fn repo(dir: &Path) {
    sh(dir, &["init", "-q", "-b", "main"]);
    sh(dir, &["config", "user.email", "t@t.com"]);
    sh(dir, &["config", "user.name", "T"]);
    sh(dir, &["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.join("a.txt"), "a\n").unwrap();
    sh(dir, &["add", "."]);
    sh(dir, &["commit", "-q", "-m", "init"]);
}

// git runs in `repo` and the file calls in the current directory, so both must see one path
#[test]
fn relative_paths_resolve_against_the_current_directory() {
    let project = tempfile::tempdir().unwrap();
    repo(project.path());
    let cwd = tempfile::tempdir().unwrap();
    repo(cwd.path());
    std::fs::write(cwd.path().join("a.txt"), "uncommitted\n").unwrap();
    std::fs::create_dir(cwd.path().join("w")).unwrap();
    std::env::set_current_dir(cwd.path()).unwrap();

    let res = git_cow::add_or_plain(
        project.path(),
        Path::new(".git-cow/p"),
        "HEAD",
        Path::new("w"),
        Some("rel"),
        None,
        &git_cow::Options::default(),
    );
    res.expect("add with relative paths");
    assert_eq!(
        std::fs::read_to_string(cwd.path().join("a.txt")).unwrap(),
        "uncommitted\n",
        "the repo in the current directory was reset"
    );
    assert!(
        cwd.path().join("w/a.txt").exists(),
        "worktree is not under the current directory"
    );
    assert!(
        !project.path().join("w").exists(),
        "worktree went under the repo"
    );
}
