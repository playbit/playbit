#![allow(clippy::unwrap_used, reason = "Tests assert successful temporary Git and Cargo fixtures.")]

#[path = "../../scripts/build-version.rs"]
mod build_version;

use std::{fs, path::Path, process::Command};

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git").arg("-C").arg(root).args(args).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn repository(root: &Path) {
    git(root, &["init", "--quiet", "--initial-branch=main"]);
    git(root, &["config", "user.email", "version@example.invalid"]);
    git(root, &["config", "user.name", "Version test"]);
    git(root, &["-c", "commit.gpgsign=false", "commit", "--quiet", "--allow-empty", "-m", "First"]);
}

#[test]
fn source_archive_does_not_inherit_a_parent_repository_revision() {
    // An exported source directory inside a checkout must still report src.
    let directory = tempfile::tempdir().unwrap();
    repository(directory.path());
    let source = directory.path().join("export");
    fs::create_dir(&source).unwrap();
    assert_eq!(build_version::revision(&source).unwrap(), "src");
    assert!(build_version::git_inputs(&source).unwrap().is_empty());
}

#[test]
fn unchanged_source_archive_is_fresh_and_source_edits_rebuild() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    fs::create_dir(root.join("scripts")).unwrap();
    fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../scripts/build-version.rs"),
        root.join("scripts/build-version.rs"),
    )
    .unwrap();
    fs::write(root.join("Cargo.toml"), "[package]\nname = \"archive-fixture\"\nversion = \"1.2.3\"\nedition = \"2024\"\n[[bin]]\nname = \"archive-fixture\"\npath = \"main.rs\"\n").unwrap();
    fs::write(root.join("build.rs"), "#[path = \"scripts/build-version.rs\"] mod version;\nfn main() { version::emit(std::path::Path::new(&std::env::var(\"CARGO_MANIFEST_DIR\").unwrap())).unwrap(); }\n").unwrap();
    fs::write(
        root.join("main.rs"),
        "fn main() { println!(\"{}\", env!(\"PLAYBIT_BUILD_VERSION\")); }\n",
    )
    .unwrap();
    let build = || {
        let output = Command::new("cargo")
            .args(["build", "--offline", "--message-format=json"])
            .env_remove("CARGO_TARGET_DIR")
            .current_dir(root)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let artifacts: Vec<serde_json::Value> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .filter(|value: &serde_json::Value| value["reason"] == "compiler-artifact")
            .collect();
        assert!(!artifacts.is_empty());
        artifacts.iter().all(|value| value["fresh"] == true)
    };
    assert!(!build());
    assert!(build(), "an unchanged source archive must not rebuild");
    fs::write(
        root.join("main.rs"),
        "fn main() { println!(\"edited {}\", env!(\"PLAYBIT_BUILD_VERSION\")); }\n",
    )
    .unwrap();
    assert!(!build(), "source edits must rebuild");
    assert!(build());
    let output = Command::new(root.join("target/debug/archive-fixture")).output().unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "edited 1.2.3+src\n");
}

#[test]
fn revision_tracks_commits_and_worktrees_without_inspecting_uncommitted_edits() {
    // Check loose/packed refs and a worktree's own HEAD with dirty files present.
    let directory = tempfile::tempdir().unwrap();
    repository(directory.path());
    let first = git(directory.path(), &["rev-parse", "HEAD"]);
    assert_eq!(build_version::revision(directory.path()).unwrap(), &first[..8]);
    fs::write(directory.path().join("uncommitted"), "edited").unwrap();
    assert_eq!(build_version::revision(directory.path()).unwrap(), &first[..8]);
    git(directory.path(), &["pack-refs", "--all"]);
    let inputs = build_version::git_inputs(directory.path()).unwrap();
    assert!(inputs.iter().any(|path| path.ends_with("packed-refs")));
    let worktree = directory.path().join("other");
    git(
        directory.path(),
        &["worktree", "add", "--quiet", "-b", "other", worktree.to_str().unwrap()],
    );
    git(
        &worktree,
        &["-c", "commit.gpgsign=false", "commit", "--quiet", "--allow-empty", "-m", "Second"],
    );
    let second = git(&worktree, &["rev-parse", "HEAD"]);
    assert_ne!(first, second);
    assert_eq!(build_version::revision(&worktree).unwrap(), &second[..8]);
    assert_eq!(build_version::revision(directory.path()).unwrap(), &first[..8]);
    let inputs = build_version::git_inputs(&worktree).unwrap();
    assert!(inputs.iter().any(|path| path.contains("worktrees") && path.ends_with("HEAD")));
    assert!(inputs.iter().any(|path| path.ends_with(".git/refs")));
}

#[test]
fn cargo_builds_unowned_checkout_and_tracks_revision_changes() {
    // Build a tiny real Cargo client with the container checkout ownership mismatch,
    // then change only Git metadata and rebuild.
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    repository(root);
    fs::create_dir(root.join("scripts")).unwrap();
    fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../scripts/build-version.rs"),
        root.join("scripts/build-version.rs"),
    )
    .unwrap();
    fs::write(root.join("Cargo.toml"), "[package]\nname = \"version-fixture\"\nversion = \"1.2.3-beta.4\"\nedition = \"2024\"\n[[bin]]\nname = \"version-fixture\"\npath = \"main.rs\"\n").unwrap();
    fs::write(root.join("build.rs"), "#[path = \"scripts/build-version.rs\"] mod version;\nfn main() { version::emit(std::path::Path::new(&std::env::var(\"CARGO_MANIFEST_DIR\").unwrap())).unwrap(); }\n").unwrap();
    fs::write(
        root.join("main.rs"),
        "fn main() { println!(\"{}\", env!(\"PLAYBIT_BUILD_VERSION\")); }\n",
    )
    .unwrap();
    git(root, &["add", "Cargo.toml", "build.rs", "main.rs", "scripts/build-version.rs"]);
    git(root, &["-c", "commit.gpgsign=false", "commit", "--quiet", "-m", "Cargo fixture"]);
    let rejected = Command::new("git")
        .args(["-c", "safe.directory=", "-C"])
        .arg(root)
        .args(["rev-parse", "--verify", "HEAD"])
        .env("GIT_TEST_ASSUME_DIFFERENT_OWNER", "1")
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("dubious ownership"));
    let build = |source: &Path| {
        let output = Command::new("cargo")
            .args(["run", "--quiet", "--offline"])
            .env_remove("CARGO_TARGET_DIR")
            .env("GIT_TEST_ASSUME_DIFFERENT_OWNER", "1")
            // Clear inherited trust entries only for this child process.
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "safe.directory")
            .env("GIT_CONFIG_VALUE_0", "")
            .current_dir(source)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    let first = build(root);
    git(root, &["checkout", "--quiet", "-b", "next"]);
    git(root, &["-c", "commit.gpgsign=false", "commit", "--quiet", "--allow-empty", "-m", "Next"]);
    let second = build(root);
    assert_ne!(first, second);
    assert_eq!(second, format!("1.2.3-beta.4+{}", &git(root, &["rev-parse", "HEAD"])[..8]));
    git(root, &["checkout", "--quiet", "main"]);
    assert_eq!(build(root), first);
    git(root, &["pack-refs", "--all"]);
    assert_eq!(build(root), first, "packing refs preserves the compiled revision");
    let worktree = root.join("other");
    git(root, &["worktree", "add", "--quiet", "-b", "worktree", worktree.to_str().unwrap()]);
    assert_eq!(build(&worktree), first);
    git(
        &worktree,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "Worktree commit",
        ],
    );
    assert_eq!(
        build(&worktree),
        format!("1.2.3-beta.4+{}", &git(&worktree, &["rev-parse", "HEAD"])[..8])
    );
    assert_eq!(build(root), first, "a worktree commit must not change the main checkout version");
    fs::rename(root.join(".git"), root.join("saved-git")).unwrap();
    assert_eq!(build(root), "1.2.3-beta.4+src");
}

#[test]
fn client_release_version_is_the_root_workspace_version() {
    // Every client surface uses the compiled release plus exactly one revision.
    let (release, revision) = playbit::VERSION.split_once('+').unwrap();
    assert_eq!(release, env!("CARGO_PKG_VERSION"));
    assert!(
        revision == "src"
            || (revision.len() == 8 && revision.bytes().all(|byte| byte.is_ascii_hexdigit()))
    );
}
