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
        "out/\n.env\n.local/\nnode/\n.uv-cache/\n.venv/\n",
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
    std::fs::create_dir_all(p.join(".uv-cache/wheels")).unwrap();
    std::fs::write(p.join(".uv-cache/wheels/w.whl"), "wheel\n").unwrap();

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
        assert_eq!(
            std::fs::read_to_string(root.join(".uv-cache/wheels/w.whl")).unwrap(),
            "wheel\n",
            "a built-in shared cache must be cloned"
        );
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

    std::fs::remove_file(p.join("out/sub/dep.bin")).unwrap();
    std::fs::write(p.join("out/new.bin"), "new\n").unwrap();

    // Within the warm interval the template is not refreshed from the checkout
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
    assert!(wt2.join("out/sub/dep.bin").exists());
    assert!(!wt2.join("out/new.bin").exists());

    // With no interval it is, and the shared directory is replaced instead of merged into
    let now = git_cow::Options {
        warm_interval: std::time::Duration::ZERO,
        ..Default::default()
    };
    let wt3 = scratch.path().join("task-6");
    git_cow::add_with(
        p,
        &tpl,
        "origin/main",
        &wt3,
        Some("task-6"),
        Some("origin/main"),
        &now,
    )
    .unwrap();
    assert!(wt3.join("out/new.bin").exists());
    assert!(!wt3.join("out/sub/dep.bin").exists());
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

#[test]
fn cli_share_adds_a_hidden_directory_to_new_worktrees() {
    let (_r, project, scratch) = setup();
    if !supported(scratch.path()) {
        return;
    }
    let p = project.path();
    std::fs::create_dir_all(p.join(".venv/lib")).unwrap();
    std::fs::write(p.join(".venv/lib/x.py"), "x\n").unwrap();
    let bin = env!("CARGO_BIN_EXE_git-cow");
    let out = Command::new(bin)
        .args(["share", ".venv"])
        .current_dir(p)
        .output()
        .unwrap();
    assert!(out.status.success());
    let listed = String::from_utf8_lossy(&out.stdout);
    assert!(listed.lines().any(|l| l == ".venv") && listed.lines().any(|l| l == ".uv-cache"));
    let wt = scratch.path().join("wt-share");
    let st = Command::new(bin)
        .args(["add", "-b", "share-test"])
        .arg(&wt)
        .current_dir(p)
        .status()
        .unwrap();
    assert!(st.success());
    assert_eq!(
        std::fs::read_to_string(wt.join(".venv/lib/x.py")).unwrap(),
        "x\n"
    );
    let bad = Command::new(bin)
        .args(["share", "target"])
        .current_dir(p)
        .status()
        .unwrap();
    assert!(!bad.success(), "only hidden directory names are accepted");
}

#[test]
fn cli_list_hides_templates_and_remove_keeps_unmerged_branches() {
    let (_r, project, scratch) = setup();
    if !supported(scratch.path()) {
        return;
    }
    let p = project.path();
    let bin = env!("CARGO_BIN_EXE_git-cow");
    let wts = scratch.path().join("wts");
    let run = |args: &[&str]| {
        Command::new(bin)
            .args(args)
            .current_dir(p)
            .output()
            .unwrap()
    };
    assert!(
        run(&["add", "-b", "feat/a", wts.join("a").to_str().unwrap()])
            .status
            .success()
    );
    assert!(
        run(&["add", "-b", "feat/b", wts.join("b").to_str().unwrap()])
            .status
            .success()
    );

    let listed = String::from_utf8_lossy(&run(&["list"]).stdout).to_string();
    assert!(listed.contains("feat/a") && listed.contains("feat/b"));
    assert!(
        !listed.contains(".git-cow"),
        "list must hide the template: {listed}"
    );

    let tpl = wts.join(".git-cow").join(p.file_name().unwrap());
    assert!(
        !run(&["remove", tpl.to_str().unwrap()]).status.success(),
        "a template is never removed"
    );
    assert!(tpl.exists());

    // feat/a has no commits of its own, so it is merged and goes; feat/b gets one and stays
    sh(
        &wts.join("b"),
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@t.com",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "b",
        ],
    );
    assert!(
        run(&["remove", wts.join("a").to_str().unwrap()])
            .status
            .success()
    );
    assert!(
        run(&["remove", wts.join("b").to_str().unwrap()])
            .status
            .success()
    );
    assert!(!wts.join("a").exists() && !wts.join("b").exists());
    let branches = Command::new("git")
        .args(["branch", "--list"])
        .current_dir(p)
        .output()
        .unwrap();
    let branches = String::from_utf8_lossy(&branches.stdout);
    assert!(!branches.contains("feat/a"), "a merged branch is deleted");
    assert!(branches.contains("feat/b"), "an unmerged branch is kept");
}

#[test]
fn cli_expands_repo_in_root_and_template() {
    let (_r, project, scratch) = setup();
    let p = project.path();
    let name = p.file_name().unwrap().to_str().unwrap();
    let root = scratch.path().join("r");
    sh(
        p,
        &[
            "config",
            "cow.root",
            &format!("{}/{{repo}}-wts", root.display()),
        ],
    );
    sh(
        p,
        &[
            "config",
            "cow.template",
            &format!("{}/tpl/{{repo}}", scratch.path().display()),
        ],
    );
    let st = Command::new(env!("CARGO_BIN_EXE_git-cow"))
        .args(["add", "-b", "feat/x"])
        .current_dir(p)
        .status()
        .unwrap();
    assert!(st.success());
    assert!(
        root.join(format!("{name}-wts"))
            .join("x")
            .join("a.txt")
            .exists()
    );
    if supported(scratch.path()) {
        assert!(scratch.path().join("tpl").join(name).join(".git").exists());
    }
}
