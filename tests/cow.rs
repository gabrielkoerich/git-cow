//! The library and the `git cow` command against real repositories and volumes
use std::os::unix::fs::PermissionsExt;
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
            "cow.base",
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

#[test]
fn extra_checkout_never_carries_the_users_edits_or_untracked_files() {
    let (_r, project, scratch) = setup();
    if !supported(scratch.path()) {
        return;
    }
    let p = project.path();
    // a tracked folder, so untracked files can hide inside what gets cloned
    std::fs::create_dir_all(p.join("src")).unwrap();
    std::fs::write(p.join("src/lib.txt"), "lib\n").unwrap();
    sh(p, &["add", "src"]);
    sh(p, &["commit", "-q", "-m", "src"]);
    sh(p, &["push", "-q", "origin", "HEAD:main"]);
    sh(p, &["fetch", "-q", "origin"]);
    std::fs::write(p.join("src/lib.txt"), "edited, not committed\n").unwrap();
    std::fs::write(p.join("src/untracked.txt"), "u\n").unwrap();
    std::fs::write(p.join("src/.env"), "secret\n").unwrap();
    std::fs::create_dir_all(p.join("src/.local")).unwrap();
    std::fs::write(p.join("src/.local/settings"), "x\n").unwrap();
    std::fs::create_dir_all(p.join("src/out")).unwrap();
    std::fs::write(p.join("src/out/dep.bin"), "dep\n").unwrap();

    let tpl = scratch.path().join(".git-cow/p");
    let wt = scratch.path().join("task-7");
    git_cow::add(
        p,
        &tpl,
        "origin/main",
        &wt,
        Some("task-7"),
        Some("origin/main"),
    )
    .unwrap();
    for root in [&tpl, &wt] {
        assert_eq!(
            std::fs::read_to_string(root.join("src/lib.txt")).unwrap(),
            "lib\n",
            "an uncommitted edit leaked"
        );
        assert!(
            !root.join("src/untracked.txt").exists(),
            "an untracked file leaked"
        );
        assert!(
            !root.join("src/.env").exists() && !root.join("src/.local").exists(),
            "a secret or tool folder leaked"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("src/out/dep.bin")).unwrap(),
            "dep\n",
            "build output must still be shared"
        );
        assert!(clean(root), "{} must have a clean status", root.display());
    }
    // the user's own checkout is untouched
    assert_eq!(
        std::fs::read_to_string(p.join("src/lib.txt")).unwrap(),
        "edited, not committed\n"
    );
    assert!(p.join("src/untracked.txt").exists() && p.join("src/.env").exists());
}

#[test]
fn cli_warm_interval_setting_decides_when_add_warms() {
    let (_r, project, scratch) = setup();
    if !supported(scratch.path()) {
        return;
    }
    let p = project.path();
    let bin = env!("CARGO_BIN_EXE_git-cow");
    let wts = scratch.path().join("wts");
    let add = |name: &str| {
        let st = Command::new(bin)
            .args(["add", "-b", name])
            .arg(wts.join(name))
            .current_dir(p)
            .status()
            .unwrap();
        assert!(st.success());
    };
    std::fs::create_dir_all(p.join("out")).unwrap();
    std::fs::write(p.join("out/one.bin"), "1\n").unwrap();
    add("w1");
    std::fs::write(p.join("out/two.bin"), "2\n").unwrap();
    add("w2");
    assert!(
        !wts.join("w2/out/two.bin").exists(),
        "the default hour keeps the warm-up"
    );
    sh(p, &["config", "cow.warmInterval", "0"]);
    add("w3");
    assert!(
        wts.join("w3/out/two.bin").exists(),
        "an interval of 0 warms every add"
    );
}

#[test]
fn add_recreates_a_template_whose_folder_was_deleted() {
    let (_r, project, scratch) = setup();
    if !supported(scratch.path()) {
        return;
    }
    let p = project.path();
    let tpl = scratch.path().join(".git-cow/p");
    let add = |name: &str| {
        git_cow::add(
            p,
            &tpl,
            "origin/main",
            &scratch.path().join(name),
            Some(name),
            Some("origin/main"),
        )
    };
    add("t1").unwrap();
    std::fs::remove_dir_all(&tpl).unwrap();
    add("t2").expect("a template git still lists but is gone must be made again");
    assert!(tpl.join(".git").is_file());
    assert!(clean(&scratch.path().join("t2")));
}

#[test]
fn options_from_repo_reads_share_and_warm_interval() {
    let (_r, project, _scratch) = setup();
    let p = project.path();
    let opts = git_cow::Options::from_repo(p);
    assert_eq!(opts.share, git_cow::Options::default().share);
    assert_eq!(opts.warm_interval, std::time::Duration::from_secs(3600));
    sh(p, &["config", "--add", "cow.share", ".venv"]);
    sh(p, &["config", "--add", "cow.share", ".uv-cache"]);
    sh(p, &["config", "cow.warmInterval", "30m"]);
    let opts = git_cow::Options::from_repo(p);
    assert!(opts.share.iter().any(|s| s == ".venv"));
    assert_eq!(
        opts.share.iter().filter(|s| *s == ".uv-cache").count(),
        1,
        "a built-in entry is not listed twice"
    );
    assert_eq!(opts.warm_interval, std::time::Duration::from_secs(1800));
}

#[test]
fn probes_from_many_threads_agree() {
    let scratch = tempfile::tempdir().unwrap();
    let expected = supported(scratch.path());
    for round in 0..30 {
        let src = scratch.path().join(format!("src-{round}"));
        std::fs::create_dir_all(&src).unwrap();
        let results: Vec<bool> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|i| {
                    let dst = scratch.path().join(format!("dst-{round}-{i}"));
                    std::fs::create_dir_all(&dst).unwrap();
                    let src = &src;
                    s.spawn(move || git_cow::can_clone(src, &dst))
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(
            results.iter().all(|r| *r == expected),
            "round {round}: {results:?}, expected all {expected}"
        );
    }
}

#[test]
fn add_never_prunes_other_worktrees() {
    let (_r, project, scratch) = setup();
    if !supported(scratch.path()) {
        return;
    }
    let p = project.path();
    // a worktree on a drive that is not mounted right now
    let away = scratch.path().join("away");
    sh(
        p,
        &["worktree", "add", "-q", "--detach", away.to_str().unwrap()],
    );
    std::fs::rename(&away, scratch.path().join("unmounted")).unwrap();
    git_cow::add(
        p,
        &scratch.path().join(".git-cow/p"),
        "origin/main",
        &scratch.path().join("t1"),
        Some("t1"),
        Some("origin/main"),
    )
    .unwrap();
    let out = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(p)
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("/away\n"),
        "git-cow removed another worktree's registration"
    );
}

#[test]
fn probe_leaves_no_files_and_keeps_its_answer() {
    let scratch = tempfile::tempdir().unwrap();
    let (src, dst) = (scratch.path().join("src"), scratch.path().join("dst"));
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&dst).unwrap();
    let first = git_cow::can_clone(&src, &dst);
    assert_eq!(first, supported(scratch.path()));
    assert_eq!(git_cow::can_clone(&src, &dst), first);
    for d in [&src, &dst] {
        assert_eq!(
            std::fs::read_dir(d).unwrap().count(),
            0,
            "probe left a file"
        );
    }
}

#[test]
fn add_failing_after_git_registered_the_worktree_leaves_nothing() {
    let (_r, project, scratch) = setup();
    if !supported(scratch.path()) {
        return;
    }
    let p = project.path();
    // a required smudge filter that fails, so the reset in the new worktree fails
    sh(p, &["config", "filter.boom.smudge", "false"]);
    sh(p, &["config", "filter.boom.clean", "cat"]);
    sh(p, &["config", "filter.boom.required", "true"]);
    sh(p, &["checkout", "-q", "-b", "clash", "origin/main"]);
    std::fs::write(p.join(".gitattributes"), "*.dat filter=boom\n").unwrap();
    std::fs::write(p.join("x.dat"), "x\n").unwrap();
    sh(p, &["add", "-A"]);
    sh(p, &["commit", "-q", "-m", "clash"]);
    sh(p, &["checkout", "-q", "--detach", "origin/main"]);

    let wt = scratch.path().join("t1");
    let res = git_cow::add(
        p,
        &scratch.path().join(".git-cow/p"),
        "origin/main",
        &wt,
        Some("t1"),
        Some("clash"),
    );
    assert!(res.is_err(), "the clash must fail populate");
    assert!(!wt.exists());
    let out = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(p)
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("/t1\n"),
        "a failed add left its registration"
    );
    let out = Command::new("git")
        .args(["branch", "--list", "t1"])
        .current_dir(p)
        .output()
        .unwrap();
    assert!(out.stdout.is_empty(), "a failed add left its branch");
}

#[test]
fn add_warms_again_once_the_interval_passed() {
    let (_r, project, scratch) = setup();
    if !supported(scratch.path()) {
        return;
    }
    let p = project.path();
    let tpl = scratch.path().join(".git-cow/p");
    let opts = git_cow::Options {
        warm_interval: std::time::Duration::from_secs(1),
        ..Default::default()
    };
    let add = |name: &str| {
        git_cow::add_with(
            p,
            &tpl,
            "origin/main",
            &scratch.path().join(name),
            Some(name),
            Some("origin/main"),
            &opts,
        )
        .unwrap();
    };
    std::fs::create_dir_all(p.join("out")).unwrap();
    std::fs::write(p.join("out/one.bin"), "1\n").unwrap();
    add("w1");
    assert!(scratch.path().join("w1/out/one.bin").exists());
    // not yet passed is covered by the default hour, a short one is too tight for a busy machine
    std::fs::write(p.join("out/two.bin"), "2\n").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    add("w3");
    assert!(scratch.path().join("w3/out/two.bin").exists());
}

#[test]
fn max_interval_never_warms_from_the_checkout() {
    let (_r, project, scratch) = setup();
    if !supported(scratch.path()) {
        return;
    }
    let p = project.path();
    std::fs::create_dir_all(p.join("out")).unwrap();
    std::fs::write(p.join("out/host.bin"), "host\n").unwrap();
    let never = git_cow::Options {
        warm_interval: std::time::Duration::MAX,
        ..Default::default()
    };
    let tpl = scratch.path().join(".git-cow/p");
    let wt = scratch.path().join("w1");
    git_cow::add_with(
        p,
        &tpl,
        "origin/main",
        &wt,
        Some("w1"),
        Some("origin/main"),
        &never,
    )
    .unwrap();
    assert!(!tpl.join("out").exists() && !wt.join("out").exists());
    // an explicit warm still warms
    git_cow::warm_with(p, &tpl, &never).unwrap();
    assert!(tpl.join("out/host.bin").exists());
}

#[test]
fn warm_interval_never_in_git_config() {
    let (_r, project, _scratch) = setup();
    sh(project.path(), &["config", "cow.warmInterval", "never"]);
    assert_eq!(
        git_cow::Options::from_repo(project.path()).warm_interval,
        std::time::Duration::MAX
    );
}

#[test]
fn remove_template_leaves_nothing_and_add_makes_it_again() {
    let (_r, project, scratch) = setup();
    if !supported(scratch.path()) {
        return;
    }
    let p = project.path();
    let dir = scratch.path().join(".git-cow");
    let tpl = dir.join("p");
    let add = |name: &str| {
        git_cow::add(
            p,
            &tpl,
            "origin/main",
            &scratch.path().join(name),
            Some(name),
            Some("origin/main"),
        )
    };
    add("w1").unwrap();
    git_cow::remove_template(p, &tpl).unwrap();
    let left: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(left.is_empty(), "left behind: {left:?}");
    let out = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(p)
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&out.stdout).contains("/.git-cow/p\n"));
    assert!(
        clean(&scratch.path().join("w1")),
        "a worktree made from it stays"
    );
    git_cow::remove_template(p, &tpl).expect("removing it twice is fine");
    add("w2").unwrap();
    assert!(clean(&scratch.path().join("w2")));
}

// git-cow refuses submodules on every volume, so this fails the clone wherever the test runs
fn with_gitmodules(p: &Path) {
    std::fs::write(p.join(".gitmodules"), "").unwrap();
    sh(p, &["add", ".gitmodules"]);
    sh(p, &["commit", "-q", "-m", "gitmodules"]);
    sh(p, &["push", "-q", "origin", "HEAD:main"]);
    sh(p, &["fetch", "-q", "origin"]);
}

#[test]
fn plain_worktree_add_works_after_a_failed_add() {
    let (_r, project, scratch) = setup();
    let p = project.path();
    with_gitmodules(p);
    let wt = scratch.path().join("w1");
    let res = git_cow::add(
        p,
        &scratch.path().join(".git-cow/p"),
        "origin/main",
        &wt,
        Some("w1"),
        Some("origin/main"),
    );
    assert!(res.is_err());
    sh(
        p,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "w1",
            wt.to_str().unwrap(),
            "origin/main",
        ],
    );
    assert!(clean(&wt));
}

#[test]
fn add_or_plain_clones_or_falls_back() {
    let (_r, project, scratch) = setup();
    let p = project.path();
    let tpl = scratch.path().join(".git-cow/p");
    let opts = git_cow::Options::default();
    let add = |name: &str| {
        git_cow::add_or_plain(
            p,
            &tpl,
            "origin/main",
            &scratch.path().join(name),
            Some(name),
            Some("origin/main"),
            &opts,
        )
    };
    let first = add("w1").unwrap();
    if supported(scratch.path()) {
        assert!(matches!(first, git_cow::Added::Cloned), "{first:?}");
    } else {
        assert!(matches!(first, git_cow::Added::Plain(_)), "{first:?}");
    }
    assert!(clean(&scratch.path().join("w1")));

    with_gitmodules(p);
    match add("w2").unwrap() {
        git_cow::Added::Plain(why) => assert!(
            why.to_string().contains("submodules") || !supported(scratch.path()),
            "{why}"
        ),
        git_cow::Added::Cloned => panic!("expected the plain fallback, got a clone"),
    }
    assert!(clean(&scratch.path().join("w2")));
    sh(p, &["rev-parse", "--verify", "-q", "refs/heads/w2"]);

    // -b with a branch that exists fails both ways, and nothing is left
    sh(p, &["branch", "w2-again", "origin/main"]);
    assert!(add("w2-again").is_err());
    assert!(!scratch.path().join("w2-again").exists());
    sh(p, &["rev-parse", "--verify", "-q", "refs/heads/w2-again"]);
}

// Every case must fail and leave the path as it was, like git worktree add does
#[test]
fn add_never_deletes_an_existing_path() {
    let (_r, project, scratch) = setup();
    let tpl = scratch.path().join(".git-cow/p");
    let taken = scratch.path().join("taken");
    sh(
        project.path(),
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "first",
            taken.to_str().unwrap(),
        ],
    );
    std::fs::write(taken.join("uncommitted.txt"), "work\n").unwrap();
    let dir = project.path().join("notes");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("untracked.md"), "notes\n").unwrap();
    std::fs::write(project.path().join("untracked.txt"), "work\n").unwrap();

    for (wt, branch) in [
        (&taken, Some("second")),
        (&dir, None),
        (&project.path().to_path_buf(), Some("x")),
    ] {
        let res = git_cow::add(project.path(), &tpl, "origin/main", wt, branch, None);
        assert!(res.is_err(), "add into {} must fail", wt.display());
        let plain = git_cow::add_or_plain(
            project.path(),
            &tpl,
            "origin/main",
            wt,
            branch,
            None,
            &git_cow::Options::default(),
        );
        assert!(
            plain.is_err(),
            "add_or_plain into {} must fail",
            wt.display()
        );
    }
    assert_eq!(
        std::fs::read_to_string(taken.join("uncommitted.txt")).unwrap(),
        "work\n"
    );
    sh(&taken, &["rev-parse", "--verify", "-q", "refs/heads/first"]);
    assert_eq!(
        std::fs::read_to_string(dir.join("untracked.md")).unwrap(),
        "notes\n"
    );
    assert_eq!(
        std::fs::read_to_string(project.path().join("untracked.txt")).unwrap(),
        "work\n"
    );
    assert!(project.path().join(".git").is_dir());
    for b in ["second", "x"] {
        let out = Command::new("git")
            .args(["branch", "--list", b])
            .current_dir(project.path())
            .output()
            .unwrap();
        assert!(
            out.stdout.is_empty(),
            "failed add must not leave branch {b}"
        );
    }
}

#[test]
fn cli_fails_when_the_default_path_is_another_worktree() {
    let (_r, project, scratch) = setup();
    let root = scratch.path().join("wts");
    sh(
        project.path(),
        &["config", "cow.root", root.to_str().unwrap()],
    );
    let bin = env!("CARGO_BIN_EXE_git-cow");
    let st = Command::new(bin)
        .args(["add", "-b", "feat/login"])
        .current_dir(project.path())
        .status()
        .unwrap();
    assert!(st.success());
    let wt = root.join(project.path().file_name().unwrap()).join("login");
    std::fs::write(wt.join("uncommitted.txt"), "work\n").unwrap();
    let st = Command::new(bin)
        .args(["add", "-b", "fix/login"])
        .current_dir(project.path())
        .status()
        .unwrap();
    assert!(!st.success(), "a second add into the same path must fail");
    assert_eq!(
        std::fs::read_to_string(wt.join("uncommitted.txt")).unwrap(),
        "work\n"
    );
    assert!(!has_branch(project.path(), "fix/login"));
    sh(
        &wt,
        &["rev-parse", "--verify", "-q", "refs/heads/feat/login"],
    );
}

#[test]
fn add_into_an_empty_directory_works_like_git() {
    let (_r, project, scratch) = setup();
    let wt = scratch.path().join("empty");
    std::fs::create_dir(&wt).unwrap();
    let res = git_cow::add_or_plain(
        project.path(),
        &scratch.path().join(".git-cow/p"),
        "origin/main",
        &wt,
        Some("empty"),
        None,
        &git_cow::Options::default(),
    );
    res.expect("an empty directory is a valid target");
    assert_eq!(std::fs::read_to_string(wt.join("a.txt")).unwrap(), "a\n");
    assert!(clean(&wt));
}

fn has_branch(repo: &Path, b: &str) -> bool {
    let out = Command::new("git")
        .args(["branch", "--list", b])
        .current_dir(repo)
        .output()
        .unwrap();
    !out.stdout.is_empty()
}

// git worktree add creates the -b branch before it refuses a registered path whose directory is gone
#[test]
fn failed_add_into_a_missing_worktree_leaves_no_branch() {
    let (_r, project, scratch) = setup();
    let tpl = scratch.path().join(".git-cow/p");
    let wt = scratch.path().join("moved");
    sh(
        project.path(),
        &["worktree", "add", "-q", "-b", "first", wt.to_str().unwrap()],
    );
    std::fs::rename(&wt, scratch.path().join("elsewhere")).unwrap();

    let res = git_cow::add(
        project.path(),
        &tpl,
        "origin/main",
        &wt,
        Some("second"),
        None,
    );
    assert!(res.is_err());
    assert!(!has_branch(project.path(), "second"), "add left its branch");
    let res = git_cow::add_or_plain(
        project.path(),
        &tpl,
        "origin/main",
        &wt,
        Some("third"),
        None,
        &git_cow::Options::default(),
    );
    assert!(res.is_err());
    assert!(
        !has_branch(project.path(), "third"),
        "add_or_plain left its branch"
    );
    assert!(has_branch(project.path(), "first"));
}

#[test]
fn cli_failed_add_into_a_missing_worktree_leaves_no_branch() {
    let (_r, project, scratch) = setup();
    let root = scratch.path().join("wts");
    sh(
        project.path(),
        &["config", "cow.root", root.to_str().unwrap()],
    );
    let bin = env!("CARGO_BIN_EXE_git-cow");
    let st = Command::new(bin)
        .args(["add", "-b", "feat/login"])
        .current_dir(project.path())
        .status()
        .unwrap();
    assert!(st.success());
    let wt = root.join(project.path().file_name().unwrap()).join("login");
    std::fs::rename(&wt, scratch.path().join("elsewhere")).unwrap();
    let st = Command::new(bin)
        .args(["add", "-b", "fix/login"])
        .current_dir(project.path())
        .status()
        .unwrap();
    assert!(!st.success());
    assert!(!has_branch(project.path(), "fix/login"));
    assert!(has_branch(project.path(), "feat/login"));
}

// A clone error falls back to a plain worktree and says why, only a refused path stops the add
#[test]
fn add_or_plain_falls_back_when_the_template_parent_is_a_file() {
    let (_r, project, scratch) = setup();
    let file = scratch.path().join("not-a-dir");
    std::fs::write(&file, "x").unwrap();
    let wt = scratch.path().join("wt");
    let res = git_cow::add_or_plain(
        project.path(),
        &file.join("p"),
        "origin/main",
        &wt,
        Some("plain"),
        None,
        &git_cow::Options::default(),
    );
    match res {
        Ok(git_cow::Added::Plain(why)) => assert!(!git_cow::is_unsupported(&why), "{why}"),
        other => panic!("expected a plain add, got {other:?}"),
    }
    assert_eq!(std::fs::read_to_string(wt.join("a.txt")).unwrap(), "a\n");
}

#[test]
fn cli_warns_and_falls_back_when_cloning_errors() {
    let (_r, project, scratch) = setup();
    let file = scratch.path().join("not-a-dir");
    std::fs::write(&file, "x").unwrap();
    sh(
        project.path(),
        &["config", "cow.base", file.join("p").to_str().unwrap()],
    );
    let wt = scratch.path().join("wt");
    let out = Command::new(env!("CARGO_BIN_EXE_git-cow"))
        .args(["add", "-b", "plain"])
        .arg(&wt)
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("warning"), "no warning in: {stderr}");
    assert!(stderr.contains("not a copy-on-write clone"), "{stderr}");
    assert_eq!(std::fs::read_to_string(wt.join("a.txt")).unwrap(), "a\n");
}

// A template path that git-cow did not make must never be reset, cleaned or deleted
#[test]
fn add_never_takes_over_a_template_path_it_did_not_make() {
    let (_r, project, scratch) = setup();
    if !supported(scratch.path()) {
        return;
    }
    let p = project.path();
    std::fs::write(p.join("a.txt"), "edited\n").unwrap();
    let user_wt = scratch.path().join("mine");
    sh(
        p,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "mine",
            user_wt.to_str().unwrap(),
        ],
    );
    std::fs::write(user_wt.join("a.txt"), "mine\n").unwrap();
    let dir = scratch.path().join("notes");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("n.md"), "notes\n").unwrap();

    for (i, tpl) in [p.to_path_buf(), user_wt.clone(), dir.clone()]
        .iter()
        .enumerate()
    {
        let wt = scratch.path().join(format!("wt-{i}"));
        let b = format!("b-{i}");
        let res = git_cow::add(p, tpl, "origin/main", &wt, Some(&b), None);
        let e = res.expect_err("not a template");
        assert!(no_cli_advice(&e), "{e}");
        assert!(!wt.exists());
        assert!(
            git_cow::warm_with(p, tpl, &git_cow::Options::default()).is_err(),
            "warm into {}",
            tpl.display()
        );
        assert!(
            git_cow::remove_template(p, tpl).is_err(),
            "remove {}",
            tpl.display()
        );
    }
    assert_eq!(
        std::fs::read_to_string(p.join("a.txt")).unwrap(),
        "edited\n"
    );
    assert_eq!(
        std::fs::read_to_string(user_wt.join("a.txt")).unwrap(),
        "mine\n"
    );
    sh(&user_wt, &["symbolic-ref", "-q", "HEAD"]);
    assert_eq!(
        std::fs::read_to_string(dir.join("n.md")).unwrap(),
        "notes\n"
    );
}

// A template made outside git-cow, a detached worktree of the repo, is used as before
#[test]
fn add_uses_a_detached_worktree_as_the_template() {
    let (_r, project, scratch) = setup();
    if !supported(scratch.path()) {
        return;
    }
    let tpl = scratch.path().join(".git-cow/p");
    sh(
        project.path(),
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            tpl.to_str().unwrap(),
            "origin/main",
        ],
    );
    std::fs::write(tpl.join("a.txt"), "user edit\n").unwrap();
    let wt = scratch.path().join("wt");
    let res = git_cow::add(project.path(), &tpl, "origin/main", &wt, Some("wt"), None);
    assert!(
        res.is_err(),
        "a detached worktree without git-cow's lock is the user's"
    );
    assert_eq!(
        std::fs::read_to_string(tpl.join("a.txt")).unwrap(),
        "user edit\n"
    );
    // every git-cow version takes its lock beside the template, so with it the path is git-cow's
    std::fs::write(scratch.path().join(".git-cow/.p.lock"), "").unwrap();
    git_cow::add(project.path(), &tpl, "origin/main", &wt, Some("wt"), None)
        .expect("a detached worktree with the lock is a template");
    assert!(clean(&wt));
}

// An old template keeps the old behavior: an edit in it is reset, never a reason to refuse it
#[test]
fn add_still_resets_an_edited_template() {
    let (_r, project, scratch) = setup();
    if !supported(scratch.path()) {
        return;
    }
    let tpl = scratch.path().join(".git-cow/p");
    sh(
        project.path(),
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            tpl.to_str().unwrap(),
            "origin/main",
        ],
    );
    std::fs::write(tpl.join("a.txt"), "stray\n").unwrap();
    std::fs::write(scratch.path().join(".git-cow/.p.lock"), "").unwrap();
    let wt = scratch.path().join("wt");
    git_cow::add(project.path(), &tpl, "origin/main", &wt, Some("wt"), None)
        .expect("an edited template is reset");
    assert_eq!(std::fs::read_to_string(wt.join("a.txt")).unwrap(), "a\n");
    assert!(clean(&wt));
}

#[test]
fn force_replaces_a_template_path_git_cow_did_not_make() {
    let (_r, project, scratch) = setup();
    if !supported(scratch.path()) {
        return;
    }
    let tpl = scratch.path().join("notes");
    std::fs::create_dir(&tpl).unwrap();
    std::fs::write(tpl.join("n.md"), "notes\n").unwrap();
    let wt = scratch.path().join("wt");
    let force = git_cow::Options {
        force: 1,
        ..Default::default()
    };
    git_cow::add_with(
        project.path(),
        &tpl,
        "origin/main",
        &wt,
        Some("wt"),
        None,
        &force,
    )
    .expect("--force takes the path over");
    assert!(!tpl.join("n.md").exists());
    assert!(tpl.join(".git").is_file());
    assert!(clean(&wt));
}

#[test]
fn cli_refusal_names_force_and_force_takes_the_base_over() {
    let (_r, project, scratch) = setup();
    if !supported(scratch.path()) {
        return;
    }
    let base = scratch.path().join("notes");
    std::fs::create_dir(&base).unwrap();
    std::fs::write(base.join("n.md"), "notes\n").unwrap();
    sh(
        project.path(),
        &["config", "cow.base", base.to_str().unwrap()],
    );
    let bin = env!("CARGO_BIN_EXE_git-cow");
    let out = Command::new(bin)
        .args(["add", "-b", "one"])
        .arg(scratch.path().join("one"))
        .current_dir(project.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--force"), "no hint in: {stderr}");
    assert!(
        !scratch.path().join(".notes.lock").exists(),
        "a refused add left a lock file"
    );
    assert!(
        !out.status.success(),
        "a refused base must not fall back to git worktree add"
    );
    assert!(!scratch.path().join("one").exists());
    assert!(base.join("n.md").exists());
    let plain = git_cow::add_or_plain(
        project.path(),
        &base,
        "origin/main",
        &scratch.path().join("three"),
        Some("three"),
        None,
        &git_cow::Options::default(),
    );
    assert!(plain.is_err(), "add_or_plain fell back: {plain:?}");
    assert!(!scratch.path().join("three").exists());

    let st = Command::new(bin)
        .args(["add", "--force", "-b", "two"])
        .arg(scratch.path().join("two"))
        .current_dir(project.path())
        .status()
        .unwrap();
    assert!(st.success());
    assert!(!base.join("n.md").exists());
    assert!(
        base.join(".git").is_file(),
        "--force did not make the extra checkout"
    );
}

// --force keeps git's meaning: a branch checked out elsewhere is allowed
#[test]
fn cli_force_passes_through_to_git() {
    let (_r, project, scratch) = setup();
    let st = Command::new(env!("CARGO_BIN_EXE_git-cow"))
        .args(["add", "--force"])
        .arg(scratch.path().join("again"))
        .arg("main")
        .current_dir(project.path())
        .status()
        .unwrap();
    assert!(st.success());
    assert_eq!(
        std::fs::read_to_string(scratch.path().join("again/a.txt")).unwrap(),
        "a\n"
    );
    // old -f skipped cloning, so the extra checkout shows that git-cow cloned
    if supported(scratch.path()) {
        let name = project.path().file_name().unwrap();
        assert!(
            scratch
                .path()
                .join(".git-cow")
                .join(name)
                .join(".git")
                .is_file()
        );
    }
}

// --force only replaces a directory of files, a checkout git-cow would reset or delete is never taken
#[test]
fn force_never_takes_a_checkout_or_a_branch_worktree() {
    let (_r, project, scratch) = setup();
    if !supported(scratch.path()) {
        return;
    }
    let p = project.path();
    sh(p, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(p.join("a.txt"), "edited\n").unwrap();
    let user_wt = scratch.path().join("mine");
    sh(
        p,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "mine",
            user_wt.to_str().unwrap(),
        ],
    );
    std::fs::write(user_wt.join("a.txt"), "mine\n").unwrap();
    let force = git_cow::Options {
        force: 1,
        ..Default::default()
    };
    for (i, base) in [p.to_path_buf(), user_wt.clone()].iter().enumerate() {
        let wt = scratch.path().join(format!("wt-{i}"));
        let b = format!("b-{i}");
        let res = git_cow::add_with(p, base, "origin/main", &wt, Some(&b), None, &force);
        let e = res.expect_err("--force must not take a checkout");
        assert!(refused_for_good(&e), "{e}");
        assert!(no_cli_advice(&e), "{e}");
        assert!(!wt.exists());
    }
    sh(p, &["config", "cow.base", p.to_str().unwrap()]);
    let out = Command::new(env!("CARGO_BIN_EXE_git-cow"))
        .args(["add", "--force", "-b", "z"])
        .arg(scratch.path().join("z"))
        .current_dir(p)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cow.base") && !stderr.contains("--force"),
        "{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(p.join("a.txt")).unwrap(),
        "edited\n"
    );
    assert_eq!(
        std::fs::read_to_string(user_wt.join("a.txt")).unwrap(),
        "mine\n"
    );
    let head = Command::new("git")
        .args(["symbolic-ref", "HEAD"])
        .current_dir(p)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&head.stdout).trim(),
        "refs/heads/feature"
    );
    sh(&user_wt, &["symbolic-ref", "-q", "HEAD"]);
}

// Clones with one directory name share the default extra checkout path, as they did before
#[test]
fn two_repositories_share_one_template_path() {
    let (_r1, one, scratch) = setup();
    let (_r2, two, _s2) = setup();
    if !supported(scratch.path()) {
        return;
    }
    let tpl = scratch.path().join(".git-cow/api");
    for (i, repo) in [one.path(), two.path()].iter().enumerate() {
        let wt = scratch.path().join(format!("wt-{i}"));
        git_cow::add(
            repo,
            &tpl,
            "origin/main",
            &wt,
            Some(&format!("b-{i}")),
            None,
        )
        .expect("a template of another repository is rebuilt");
        assert!(clean(&wt));
        let common = Command::new("git")
            .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .current_dir(&wt)
            .output()
            .unwrap();
        let common = std::fs::canonicalize(String::from_utf8_lossy(&common.stdout).trim()).unwrap();
        assert_eq!(common, std::fs::canonicalize(repo.join(".git")).unwrap());
    }
}

// A probe that cannot write is a clone error, not a volume without cloning, and falls back
#[test]
fn unwritable_template_parent_falls_back_with_the_reason() {
    let uid = Command::new("id").arg("-u").output().unwrap();
    // root writes through mode 555
    if String::from_utf8_lossy(&uid.stdout).trim() == "0" {
        return;
    }
    let (_r, project, scratch) = setup();
    let parent = scratch.path().join("ro");
    std::fs::create_dir(&parent).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o555)).unwrap();
    let wt = scratch.path().join("wt");
    let res = git_cow::add_or_plain(
        project.path(),
        &parent.join("p"),
        "origin/main",
        &wt,
        Some("ro"),
        None,
        &git_cow::Options::default(),
    );
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
    match res {
        Ok(git_cow::Added::Plain(why)) => assert!(!git_cow::is_unsupported(&why), "{why}"),
        other => panic!("expected a plain add, got {other:?}"),
    }
    assert!(wt.join("a.txt").exists());
}

// git-cow moves a template aside before deleting it, so a delete cut short never leaves files at the path
#[test]
fn a_template_set_aside_for_deletion_never_blocks_an_add() {
    let (_r, project, scratch) = setup();
    if !supported(scratch.path()) {
        return;
    }
    let tpl = scratch.path().join(".git-cow/p");
    let aside = scratch.path().join(".git-cow/.p.trash");
    std::fs::create_dir_all(aside.join("src")).unwrap();
    std::fs::write(aside.join("src/half.txt"), "x").unwrap();
    let wt = scratch.path().join("wt");
    git_cow::add(project.path(), &tpl, "origin/main", &wt, Some("wt"), None).unwrap();
    assert!(!aside.exists(), "the leftover was not cleaned");
    git_cow::remove_template(project.path(), &tpl).unwrap();
    assert!(!tpl.exists() && !aside.exists());
}

// A refusal --force does not lift
fn refused_for_good(e: &std::io::Error) -> bool {
    e.get_ref()
        .and_then(|i| i.downcast_ref::<git_cow::Foreign>())
        .is_some_and(|f| !matches!(f, git_cow::Foreign::Files(_)))
}

// The library leaves the advice to its caller, the CLI or another tool
fn no_cli_advice(e: &std::io::Error) -> bool {
    let m = e.to_string();
    !m.contains("--force") && !m.contains("cow.base")
}

fn forced() -> git_cow::Options {
    git_cow::Options {
        force: 1,
        ..Default::default()
    }
}

// --force deletes the template path before any checkout runs, and never one with tracked files
#[test]
fn force_never_touches_a_checkout_around_the_template() {
    let (_r, project, scratch) = setup();
    if !supported(scratch.path()) {
        return;
    }
    let p = project.path();
    std::fs::create_dir_all(p.join("src")).unwrap();
    std::fs::write(p.join("src/lib.txt"), "lib\n").unwrap();
    sh(p, &["add", "src"]);
    sh(p, &["commit", "-q", "-m", "src"]);
    std::fs::write(p.join("a.txt"), "edited\n").unwrap();
    std::fs::create_dir_all(p.join("sub/.git")).unwrap();

    let res = git_cow::add_with(
        p,
        &p.join("sub"),
        "origin/main",
        &scratch.path().join("w1"),
        Some("w1"),
        None,
        &forced(),
    );
    assert!(
        res.is_ok(),
        "an empty junk .git is a directory of files: {res:?}"
    );
    assert_eq!(
        std::fs::read_to_string(p.join("a.txt")).unwrap(),
        "edited\n"
    );
    sh(p, &["symbolic-ref", "-q", "HEAD"]);

    let res = git_cow::add_with(
        p,
        &p.join("src"),
        "origin/main",
        &scratch.path().join("w2"),
        Some("w2"),
        None,
        &forced(),
    );
    let e = res.expect_err("tracked files are never replaced");
    assert!(refused_for_good(&e), "{e}");
    assert_eq!(
        std::fs::read_to_string(p.join("src/lib.txt")).unwrap(),
        "lib\n"
    );
}

#[test]
fn force_never_replaces_a_bare_repository() {
    let (remote, project, scratch) = setup();
    if !supported(scratch.path()) {
        return;
    }
    let res = git_cow::add_with(
        project.path(),
        remote.path(),
        "origin/main",
        &scratch.path().join("w"),
        Some("w"),
        None,
        &forced(),
    );
    let e = res.expect_err("a bare repository is never replaced");
    assert!(refused_for_good(&e), "{e}");
    assert!(remote.path().join("HEAD").is_file());
    sh(
        remote.path(),
        &["rev-parse", "--verify", "-q", "refs/heads/main"],
    );
}

// The lock file next to a path shows git-cow made a template there, so what is left of it is rebuilt
#[test]
fn a_folder_with_a_stale_lock_needs_force() {
    let (_r, project, scratch) = setup();
    if !supported(scratch.path()) {
        return;
    }
    let tpl = scratch.path().join(".git-cow/p");
    std::fs::create_dir_all(tpl.join("src")).unwrap();
    std::fs::write(tpl.join("src/half.txt"), "x").unwrap();
    std::fs::write(scratch.path().join(".git-cow/.p.lock"), "").unwrap();
    // the lock outlives the template, so beside a folder with no `.git` it proves nothing
    let wt = scratch.path().join("wt");
    let res = git_cow::add(project.path(), &tpl, "origin/main", &wt, Some("wt"), None);
    assert!(
        res.is_err(),
        "a stale lock let git-cow take a folder of files"
    );
    assert!(git_cow::remove_template(project.path(), &tpl).is_err());
    assert!(tpl.join("src/half.txt").exists());
    git_cow::add_with(
        project.path(),
        &tpl,
        "origin/main",
        &wt,
        Some("wt"),
        None,
        &forced(),
    )
    .expect("--force replaces it");
    assert!(!tpl.join("src/half.txt").exists());
    assert!(clean(&wt));
}

// A checkout git cannot read is never treated as a broken template and deleted
#[test]
fn a_checkout_git_cannot_read_is_left_alone() {
    let (_r, project, scratch) = setup();
    if !supported(scratch.path()) {
        return;
    }
    let odd = scratch.path().join("odd");
    std::fs::create_dir(&odd).unwrap();
    sh(&odd, &["init", "-q"]);
    sh(&odd, &["config", "core.repositoryformatversion", "1"]);
    sh(&odd, &["config", "extensions.zzzunknown", "true"]);
    std::fs::write(odd.join("u.txt"), "u\n").unwrap();

    let other = scratch.path().join("other");
    std::fs::create_dir(&other).unwrap();
    sh(&other, &["init", "-q", "-b", "main"]);
    sh(
        &other,
        &[
            "-c",
            "user.email=t@t.com",
            "-c",
            "user.name=T",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "i",
        ],
    );
    let mine = scratch.path().join("mine");
    sh(
        &other,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "mine",
            mine.to_str().unwrap(),
        ],
    );
    std::fs::write(mine.join("mine.txt"), "mine\n").unwrap();
    std::fs::rename(&other, scratch.path().join("moved")).unwrap();

    for (i, base) in [&odd, &mine].iter().enumerate() {
        let res = git_cow::add_with(
            project.path(),
            base,
            "origin/main",
            &scratch.path().join(format!("w{i}")),
            Some(&format!("w{i}")),
            None,
            &forced(),
        );
        assert!(res.is_err(), "{} was taken", base.display());
    }
    assert_eq!(std::fs::read_to_string(odd.join("u.txt")).unwrap(), "u\n");
    assert_eq!(
        std::fs::read_to_string(mine.join("mine.txt")).unwrap(),
        "mine\n"
    );
}

#[test]
fn force_replaces_a_file_at_the_template_path_and_leaves_nothing_aside() {
    let (_r, project, scratch) = setup();
    if !supported(scratch.path()) {
        return;
    }
    let tpl = scratch.path().join("tfile");
    std::fs::write(&tpl, "x").unwrap();
    git_cow::add_with(
        project.path(),
        &tpl,
        "origin/main",
        &scratch.path().join("w"),
        Some("w"),
        None,
        &forced(),
    )
    .unwrap();
    assert!(tpl.join(".git").is_file());
    assert!(!scratch.path().join(".tfile.trash").exists());
}

// -f -f is git's way to add over a locked missing worktree, and git-cow passes it through
#[test]
fn cli_double_force_reaches_git() {
    let (_r, project, scratch) = setup();
    let lk = scratch.path().join("lk");
    sh(
        project.path(),
        &[
            "worktree",
            "add",
            "-q",
            "--lock",
            "-b",
            "lk",
            lk.to_str().unwrap(),
        ],
    );
    std::fs::rename(&lk, scratch.path().join("lk-gone")).unwrap();
    let st = Command::new(env!("CARGO_BIN_EXE_git-cow"))
        .args(["add", "-f", "-f", "-b", "lk2"])
        .arg(&lk)
        .current_dir(project.path())
        .status()
        .unwrap();
    assert!(st.success());
    assert_eq!(std::fs::read_to_string(lk.join("a.txt")).unwrap(), "a\n");
}

// Without -b, git makes a branch named after the path, and a failed add deletes it too
#[test]
fn failed_add_without_b_leaves_no_branch() {
    let (_r, project, scratch) = setup();
    let wt = scratch.path().join("gone");
    sh(
        project.path(),
        &["worktree", "add", "-q", "-b", "first", wt.to_str().unwrap()],
    );
    std::fs::rename(&wt, scratch.path().join("elsewhere")).unwrap();
    let res = git_cow::add_or_plain(
        project.path(),
        &scratch.path().join(".git-cow/p"),
        "origin/main",
        &wt,
        None,
        None,
        &git_cow::Options::default(),
    );
    assert!(res.is_err());
    assert!(
        !has_branch(project.path(), "gone"),
        "a failed add left the branch git named after the path"
    );
}

// A folder of files is only replaced when no checkout or repository sits anywhere inside it.
// Every path here is inside `scratch`, so a regression can only delete test folders
#[test]
fn force_never_deletes_checkouts_below_the_template_path() {
    let (_r, project, scratch) = setup();
    if !supported(scratch.path()) {
        return;
    }
    let outer = scratch.path().join("outer");
    let repo = outer.join("repo");
    std::fs::create_dir_all(&outer).unwrap();
    sh(
        &outer,
        &["clone", "-q", project.path().to_str().unwrap(), "repo"],
    );
    std::fs::write(outer.join("marker.txt"), "m\n").unwrap();
    let folder = scratch.path().join("wts");
    let inner = folder.join("feat");
    std::fs::create_dir_all(&folder).unwrap();
    sh(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feat",
            inner.to_str().unwrap(),
        ],
    );
    std::fs::write(inner.join("a.txt"), "edit\n").unwrap();
    let projects = scratch.path().join("projects");
    std::fs::create_dir_all(projects.join("other")).unwrap();
    sh(&projects.join("other"), &["init", "-q"]);
    std::fs::write(projects.join("other/o.txt"), "o\n").unwrap();
    // `..` names `outer`, which holds the repository
    let dotdot = repo.join("..");

    for (i, base) in [&folder, &projects, &dotdot].iter().enumerate() {
        let wt = scratch.path().join(format!("w{i}"));
        let res = git_cow::add_with(
            &repo,
            base,
            "origin/main",
            &wt,
            Some(&format!("w{i}")),
            None,
            &forced(),
        );
        let e = res.expect_err("a folder holding checkouts is never replaced");
        assert!(refused_for_good(&e), "{e}");
    }
    assert_eq!(
        std::fs::read_to_string(inner.join("a.txt")).unwrap(),
        "edit\n"
    );
    assert_eq!(
        std::fs::read_to_string(projects.join("other/o.txt")).unwrap(),
        "o\n"
    );
    assert_eq!(
        std::fs::read_to_string(outer.join("marker.txt")).unwrap(),
        "m\n"
    );
    assert!(repo.join(".git").is_dir() && repo.join("a.txt").exists());
}

// A worktree mid-rebase is detached, and still the user's even with a lock beside it
#[test]
fn a_worktree_in_a_rebase_is_never_a_template() {
    let (_r, project, scratch) = setup();
    if !supported(scratch.path()) {
        return;
    }
    let tpl = scratch.path().join(".git-cow/p");
    sh(
        project.path(),
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            tpl.to_str().unwrap(),
            "origin/main",
        ],
    );
    std::fs::write(tpl.join("a.txt"), "resolving\n").unwrap();
    let gitdir = Command::new("git")
        .args(["rev-parse", "--absolute-git-dir"])
        .current_dir(&tpl)
        .output()
        .unwrap();
    let gitdir = std::path::PathBuf::from(String::from_utf8_lossy(&gitdir.stdout).trim());
    std::fs::write(scratch.path().join(".git-cow/.p.lock"), "").unwrap();
    for marker in [
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "MERGE_HEAD",
        "BISECT_LOG",
    ] {
        std::fs::write(gitdir.join(marker), "").unwrap();
        let res = git_cow::add(
            project.path(),
            &tpl,
            "origin/main",
            &scratch.path().join("wt"),
            Some("wt"),
            None,
        );
        assert!(res.is_err(), "{marker} in progress");
        std::fs::remove_file(gitdir.join(marker)).unwrap();
    }
    for marker in ["sequencer", "rebase-apply"] {
        std::fs::create_dir(gitdir.join(marker)).unwrap();
        let res = git_cow::add(
            project.path(),
            &tpl,
            "origin/main",
            &scratch.path().join("wt"),
            Some("wt"),
            None,
        );
        assert!(res.is_err(), "{marker} in progress");
        std::fs::remove_dir(gitdir.join(marker)).unwrap();
    }
    std::fs::create_dir(gitdir.join("rebase-merge")).unwrap();
    let res = git_cow::add(
        project.path(),
        &tpl,
        "origin/main",
        &scratch.path().join("wt"),
        Some("wt"),
        None,
    );
    assert!(res.is_err());
    assert_eq!(
        std::fs::read_to_string(tpl.join("a.txt")).unwrap(),
        "resolving\n"
    );
}

// Any `.git` directory with something in it is a repository, even without HEAD
#[test]
fn force_never_deletes_a_checkout_missing_head() {
    let (_r, project, scratch) = setup();
    if !supported(scratch.path()) {
        return;
    }
    let other = scratch.path().join("other");
    sh(
        scratch.path(),
        &[
            "clone",
            "-q",
            project.path().to_str().unwrap(),
            other.to_str().unwrap(),
        ],
    );
    std::fs::rename(other.join(".git/HEAD"), scratch.path().join("HEAD-aside")).unwrap();
    let res = git_cow::add_with(
        project.path(),
        &other,
        "origin/main",
        &scratch.path().join("w"),
        Some("w"),
        None,
        &forced(),
    );
    assert!(res.is_err());
    assert!(other.join(".git/objects").is_dir() && other.join("a.txt").exists());
}

// The warm-up writes only inside the template, never through a tracked symlink out of it
#[test]
fn warm_never_writes_through_a_symlink_out_of_the_template() {
    let (_r, project, scratch) = setup();
    if !supported(scratch.path()) {
        return;
    }
    let p = project.path();
    let victim = scratch.path().join("victim");
    std::fs::create_dir_all(victim.join("sub")).unwrap();
    std::fs::write(victim.join("sub/keep"), "keep\n").unwrap();
    std::os::unix::fs::symlink(&victim, p.join("out")).unwrap();
    std::fs::write(p.join(".gitignore"), "sub/\n").unwrap();
    sh(p, &["add", "-A"]);
    sh(p, &["commit", "-q", "-m", "out link"]);
    sh(p, &["push", "-q", "origin", "HEAD:main"]);
    sh(p, &["fetch", "-q", "origin"]);
    sh(p, &["checkout", "-q", "-b", "real"]);
    std::fs::remove_file(p.join("out")).unwrap();
    std::fs::create_dir_all(p.join("out/sub")).unwrap();
    std::fs::write(p.join("out/sub/build"), "b\n").unwrap();
    std::fs::write(p.join("out/x.txt"), "x\n").unwrap();
    sh(p, &["add", "-A"]);
    sh(p, &["commit", "-q", "-m", "real out"]);
    let opts = git_cow::Options {
        warm_interval: std::time::Duration::ZERO,
        ..Default::default()
    };
    let _ = git_cow::add_with(
        p,
        &scratch.path().join(".git-cow/p"),
        "origin/main",
        &scratch.path().join("w"),
        Some("w"),
        None,
        &opts,
    );
    assert_eq!(
        std::fs::read_to_string(victim.join("sub/keep")).unwrap(),
        "keep\n"
    );
}

// git-cow's git calls run on its own paths, never on a repository an inherited GIT_DIR or index names
#[test]
fn cli_ignores_an_inherited_git_index() {
    let (_r, project, scratch) = setup();
    let p = project.path();
    std::fs::write(p.join("staged.txt"), "s\n").unwrap();
    sh(p, &["add", "staged.txt"]);
    let index = p.join(".git/index");
    let st = Command::new(env!("CARGO_BIN_EXE_git-cow"))
        .args(["add", "-b", "env"])
        .arg(scratch.path().join("env"))
        .env("GIT_INDEX_FILE", &index)
        .current_dir(p)
        .status()
        .unwrap();
    assert!(st.success());
    let staged = Command::new("git")
        .args(["diff", "--cached", "--name-only"])
        .current_dir(p)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&staged.stdout).trim(),
        "staged.txt",
        "the user's index changed"
    );
}

// Worktrees and branches in every state cleanup tells apart, against origin/main
struct Sweep {
    _r: tempfile::TempDir,
    project: tempfile::TempDir,
    scratch: tempfile::TempDir,
}

impl Sweep {
    fn wt(&self, name: &str) -> std::path::PathBuf {
        self.scratch.path().join(name)
    }
}

fn commit_on(dir: &Path, file: &str) {
    std::fs::write(dir.join(file), format!("{file}\n")).unwrap();
    sh(dir, &["add", file]);
    sh(dir, &["commit", "-q", "-m", file]);
}

fn sweep_setup() -> Sweep {
    let (r, project, scratch) = setup();
    let p = project.path();
    let s = scratch.path();
    let add_wt = |name: &str| {
        let wt = s.join(name);
        sh(
            p,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                name,
                wt.to_str().unwrap(),
                "main",
            ],
        );
        commit_on(&wt, &format!("{name}.txt"));
        wt
    };
    let merged = add_wt("feat-merged");
    let squashed = add_wt("feat-squashed");
    add_wt("feat-active");
    let dirty = add_wt("feat-dirty");
    let gone = add_wt("feat-gone");
    let locked = add_wt("locked-merged");
    for b in ["only-merged", "only-squashed", "only-active"] {
        sh(p, &["branch", b, "main"]);
        sh(p, &["checkout", "-q", b]);
        commit_on(p, &format!("{b}.txt"));
        sh(p, &["checkout", "-q", "main"]);
    }
    sh(p, &["merge", "-q", "--ff-only", "feat-merged"]);
    for b in ["feat-dirty", "locked-merged", "only-merged"] {
        sh(p, &["merge", "-q", "--no-edit", b]);
    }
    for b in ["feat-squashed", "only-squashed"] {
        sh(p, &["merge", "-q", "--squash", b]);
        sh(p, &["commit", "-q", "-m", &format!("squash {b}")]);
    }
    sh(p, &["push", "-q", "origin", "main"]);
    sh(&gone, &["push", "-q", "-u", "origin", "feat-gone"]);
    sh(p, &["push", "-q", "origin", "--delete", "feat-gone"]);
    sh(p, &["fetch", "-q", "--prune", "origin"]);
    std::fs::write(dirty.join("notes.txt"), "keep\n").unwrap();
    sh(p, &["worktree", "lock", locked.to_str().unwrap()]);
    sh(
        p,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            s.join("det").to_str().unwrap(),
            "origin/main",
        ],
    );
    let _ = (merged, squashed);
    Sweep {
        _r: r,
        project,
        scratch,
    }
}

fn cow(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_git-cow"))
        .args(args)
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn cleanup_dry_run_and_no_tty_change_nothing() {
    let w = sweep_setup();
    let p = w.project.path();
    for args in [&["cleanup", "-n"][..], &["cleanup"][..]] {
        let out = cow(p, args);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let plan = String::from_utf8_lossy(&out.stdout);
        for (name, state) in [
            ("feat-merged", "merged"),
            ("feat-squashed", "squashed"),
            ("feat-active", "active"),
            ("feat-gone", "gone"),
            ("only-squashed", "squashed"),
        ] {
            assert!(
                plan.lines().any(|l| l.contains(name) && l.contains(state)),
                "{name} is not {state} in:\n{plan}"
            );
        }
        assert!(w.wt("feat-merged").exists() && w.wt("feat-squashed").exists());
        assert!(has_branch(p, "only-merged"));
    }
    let out = cow(p, &["cleanup"]);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("--yes"),
        "no hint without a terminal"
    );
}

#[test]
fn cleanup_removes_what_landed_and_keeps_the_rest() {
    let w = sweep_setup();
    let p = w.project.path();
    let out = cow(p, &["cleanup", "--yes"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    for gone in ["feat-merged", "feat-squashed", "det"] {
        assert!(!w.wt(gone).exists(), "{gone} worktree is still there");
    }
    for b in [
        "feat-merged",
        "feat-squashed",
        "only-merged",
        "only-squashed",
    ] {
        assert!(!has_branch(p, b), "{b} branch is still there");
    }
    for kept in ["feat-active", "feat-dirty", "feat-gone", "locked-merged"] {
        assert!(w.wt(kept).exists(), "{kept} worktree was removed");
        assert!(has_branch(p, kept), "{kept} branch was deleted");
    }
    assert!(has_branch(p, "only-active") && has_branch(p, "main"));
    assert_eq!(
        std::fs::read_to_string(w.wt("feat-dirty").join("notes.txt")).unwrap(),
        "keep\n"
    );
    assert!(p.join(".git").is_dir());

    let out = cow(p, &["cleanup", "--yes", "--force"]);
    assert!(out.status.success());
    assert!(
        !w.wt("feat-dirty").exists(),
        "--force removes a merged worktree with changes"
    );
    assert!(
        w.wt("locked-merged").exists(),
        "a locked worktree stays even with --force"
    );
    assert!(w.wt("feat-active").exists());
}

#[test]
fn tree_lists_the_worktrees_of_every_repository_below() {
    let w = sweep_setup();
    let out = cow(
        w.scratch.path(),
        &["tree", w.project.path().to_str().unwrap()],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains(&w.project.path().display().to_string()),
        "{text}"
    );
    assert!(
        text.lines()
            .any(|l| l.contains("feat-squashed") && l.contains("squashed")),
        "{text}"
    );
    assert!(w.wt("feat-squashed").exists(), "tree changed something");
}
