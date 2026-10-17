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

fn clean(dir: &Path) -> bool {
    let out = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(dir)
        .output()
        .unwrap();
    out.stdout.is_empty()
}

// CI on a clone-capable volume sets this, so a failed probe fails the test instead of skipping
fn expect_clone() -> bool {
    std::env::var("GIT_COW_EXPECT_CLONE").is_ok_and(|v| v == "1")
}

// A remote, a clone of it with two committed files, and a scratch dir for templates and worktrees
fn setup() -> (tempfile::TempDir, tempfile::TempDir, tempfile::TempDir) {
    let remote = tempfile::tempdir().unwrap();
    sh(remote.path(), &["init", "-q", "--bare", "-b", "main"]);
    let project = tempfile::tempdir().unwrap();
    sh(
        project.path(),
        &["clone", "-q", remote.path().to_str().unwrap(), "."],
    );
    sh(project.path(), &["config", "user.email", "t@t.com"]);
    sh(project.path(), &["config", "user.name", "T"]);
    // the user's global config may sign commits through an agent that is not there in a test
    sh(project.path(), &["config", "commit.gpgsign", "false"]);
    std::fs::write(project.path().join("a.txt"), "a\n").unwrap();
    std::fs::write(project.path().join("d.txt"), "d\n").unwrap();
    std::fs::write(
        project.path().join(".gitignore"),
        "out/\n.env\n.local/\nnode/\n",
    )
    .unwrap();
    sh(project.path(), &["add", "."]);
    sh(project.path(), &["commit", "-q", "-m", "init"]);
    sh(project.path(), &["push", "-q", "origin", "HEAD:main"]);
    sh(project.path(), &["fetch", "-q", "origin"]);
    let scratch = tempfile::tempdir().unwrap();
    (remote, project, scratch)
}

fn supported(scratch: &Path) -> bool {
    let ok = git_cow::can_clone(scratch, scratch);
    if expect_clone() {
        assert!(
            ok,
            "GIT_COW_EXPECT_CLONE is set but the volume cannot clone"
        );
    }
    ok
}

#[test]
fn clone_gives_clean_worktree_or_leaves_nothing() {
    let (_r, project, scratch) = setup();
    let tpl = scratch.path().join(".git-cow/p");
    let wt = scratch.path().join("task-1");
    let res = git_cow::add(
        project.path(),
        &tpl,
        "origin/main",
        &wt,
        Some("task-1"),
        Some("origin/main"),
    );
    if supported(scratch.path()) {
        res.expect("clone path should succeed");
        assert_eq!(std::fs::read_to_string(wt.join("a.txt")).unwrap(), "a\n");
        assert!(clean(&wt), "clone must have a clean index");
        assert!(wt.join(".git").is_file());
    } else {
        assert!(res.is_err());
        assert!(!wt.exists(), "failed clone must leave no directory");
        let out = Command::new("git")
            .args(["branch", "--list", "task-1"])
            .current_dir(project.path())
            .output()
            .unwrap();
        assert!(
            out.stdout.is_empty(),
            "failed clone must not leave its new branch"
        );
    }
}

#[test]
fn branch_differing_from_template_gets_its_own_files() {
    let (_r, project, scratch) = setup();
    if !supported(scratch.path()) {
        return;
    }
    sh(
        project.path(),
        &["checkout", "-q", "-b", "task-3", "origin/main"],
    );
    std::fs::write(project.path().join("a.txt"), "changed\n").unwrap();
    std::fs::write(project.path().join("b.txt"), "b\n").unwrap();
    std::fs::remove_file(project.path().join("d.txt")).unwrap();
    sh(project.path(), &["add", "-A"]);
    sh(project.path(), &["commit", "-q", "-m", "task"]);
    sh(project.path(), &["checkout", "-q", "--detach"]);

    let wt = scratch.path().join("task-3");
    git_cow::add(
        project.path(),
        &scratch.path().join(".git-cow/p"),
        "origin/main",
        &wt,
        None,
        Some("task-3"),
    )
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(wt.join("a.txt")).unwrap(),
        "changed\n"
    );
    assert_eq!(std::fs::read_to_string(wt.join("b.txt")).unwrap(), "b\n");
    assert!(!wt.join("d.txt").exists());
    assert!(clean(&wt));
}

#[test]
fn ignored_dirs_are_shared_but_secrets_and_hidden_dirs_are_not() {
    let (_r, project, scratch) = setup();
    if !supported(scratch.path()) {
        return;
    }
    let p = project.path();
    std::fs::create_dir_all(p.join("out/sub")).unwrap();
    std::fs::write(p.join("out/sub/dep.bin"), "dep\n").unwrap();
    std::fs::create_dir_all(p.join(".local")).unwrap();
    std::fs::write(p.join(".local/settings"), "x\n").unwrap();
    std::fs::create_dir_all(p.join("pkg/a/node")).unwrap();
    std::fs::write(p.join("pkg/a/node/dep.bin"), "nested\n").unwrap();
    std::fs::create_dir_all(p.join("pkg/a/.hidden/node")).unwrap();
    std::fs::write(p.join("pkg/a/.hidden/node/x"), "x\n").unwrap();
    // an untracked file keeps git from reporting pkg/ as one fully ignored directory
    std::fs::write(p.join("pkg/a/keep.txt"), "k\n").unwrap();
    std::fs::write(p.join(".env"), "secret\n").unwrap();

    let tpl = scratch.path().join(".git-cow/p");
    let wt = scratch.path().join("task-4");
    git_cow::add(
        p,
        &tpl,
        "origin/main",
        &wt,
        Some("task-4"),
        Some("origin/main"),
    )
    .unwrap();
    for root in [&tpl, &wt] {
        assert_eq!(
            std::fs::read_to_string(root.join("out/sub/dep.bin")).unwrap(),
            "dep\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("pkg/a/node/dep.bin")).unwrap(),
            "nested\n"
        );
        assert!(!root.join(".env").exists(), "loose ignored file was copied");
        assert!(!root.join(".local").exists(), "hidden dir was copied");
        assert!(
            !root.join("pkg/a/.hidden").exists(),
            "nested hidden dir was copied"
        );
    }
    assert_ne!(
        std::fs::read_to_string(wt.join(".git")).unwrap(),
        std::fs::read_to_string(tpl.join(".git")).unwrap(),
        "the worktree must keep its own gitdir"
    );

    // A second add replaces the shared directory instead of merging into it
    std::fs::remove_file(p.join("out/sub/dep.bin")).unwrap();
    std::fs::write(p.join("out/new.bin"), "new\n").unwrap();
    let wt2 = scratch.path().join("task-5");
    git_cow::add(
        p,
        &tpl,
        "origin/main",
        &wt2,
        Some("task-5"),
        Some("origin/main"),
    )
    .unwrap();
    assert!(wt2.join("out/new.bin").exists());
    assert!(!wt2.join("out/sub/dep.bin").exists());
}

#[test]
fn warm_takes_a_merged_worktrees_build_output() {
    let (_r, project, scratch) = setup();
    if !supported(scratch.path()) {
        return;
    }
    let tpl = scratch.path().join(".git-cow/p");
    let wt = scratch.path().join("task-6");
    git_cow::add(
        project.path(),
        &tpl,
        "origin/main",
        &wt,
        Some("task-6"),
        Some("origin/main"),
    )
    .unwrap();
    assert!(!tpl.join("out").exists());
    std::fs::create_dir_all(wt.join("out")).unwrap();
    std::fs::write(wt.join("out/dep.bin"), "dep\n").unwrap();
    std::fs::write(wt.join("loose.log"), "x\n").unwrap();

    git_cow::warm(&wt, &tpl).unwrap();
    assert_eq!(
        std::fs::read_to_string(tpl.join("out/dep.bin")).unwrap(),
        "dep\n"
    );
    assert!(!tpl.join("loose.log").exists());
}

#[test]
fn failed_add_never_deletes_a_branch_that_existed() {
    let (_r, project, scratch) = setup();
    sh(project.path(), &["branch", "keep", "origin/main"]);
    let res = git_cow::add(
        project.path(),
        &scratch.path().join(".git-cow/p"),
        "origin/main",
        &scratch.path().join("x"),
        Some("keep"),
        None,
    );
    assert!(res.is_err(), "-b with an existing branch must fail");
    sh(
        project.path(),
        &["rev-parse", "--verify", "-q", "refs/heads/keep"],
    );
}

#[test]
fn cli_puts_a_pathless_worktree_under_cow_root() {
    let (_r, project, scratch) = setup();
    let root = scratch.path().join("wts");
    sh(
        project.path(),
        &["config", "cow.root", root.to_str().unwrap()],
    );
    let st = Command::new(env!("CARGO_BIN_EXE_git-cow"))
        .args(["add", "-b", "feat/login"])
        .current_dir(project.path())
        .status()
        .unwrap();
    assert!(st.success());
    let name = project.path().file_name().unwrap();
    let wt = root.join(name).join("login");
    assert_eq!(std::fs::read_to_string(wt.join("a.txt")).unwrap(), "a\n");
    assert!(clean(&wt));
}
