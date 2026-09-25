use std::fs;
use std::process::{Command, Output};

fn run_bit(project: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bit"))
        .args(args)
        .current_dir(project)
        .output()
        .expect("run bit")
}

fn run_bit_with_cache(project: &std::path::Path, cache: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bit"))
        .args(args)
        .current_dir(project)
        .env("BIT_CACHE_DIR", cache)
        .output()
        .expect("run bit")
}

fn run_git(project: &std::path::Path, args: &[&str]) -> Output {
    bit::git::command()
        .args(args)
        .current_dir(project)
        .output()
        .expect("run git")
}

#[test]
fn list_shows_targets_and_repeated_list_shows_blocks() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("BUILD.bit"),
        r#"
alpha = exec {
  command = "true"
}

beta = exec {
  command = "true"
}

# Build the project
target build = [alpha, beta]
target deploy = [beta]
"#,
    )
    .unwrap();

    let targets = run_bit(project.path(), &["-l"]);
    assert!(targets.status.success(), "{}", String::from_utf8_lossy(&targets.stderr));
    let stdout = String::from_utf8(targets.stdout).unwrap();
    assert!(stdout.contains("build"));
    assert!(stdout.contains("Build the project"));
    assert!(stdout.contains("deploy"));
    assert!(!stdout.contains("exec.exec"));

    let blocks = run_bit(project.path(), &["-ll"]);
    assert!(blocks.status.success(), "{}", String::from_utf8_lossy(&blocks.stderr));
    let stdout = String::from_utf8(blocks.stdout).unwrap();
    assert!(stdout.contains("alpha"));
    assert!(stdout.contains("beta"));
    assert!(stdout.contains("exec.exec"));
    assert!(!stdout.contains("Build the project"));
}

#[test]
fn force_rebuilds_implicit_and_specified_blocks_without_cache() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("BUILD.bit"),
        r#"
implicit = exec {
  command = "printf x >> implicit.txt"
  output = "implicit.txt"
  inputs = []
}

explicit specified = exec {
  command = "printf x >> specified.txt"
  output = "specified.txt"
  inputs = []
}
"#,
    )
    .unwrap();

    let run = |args: &[&str]| {
        let output = run_bit_with_cache(project.path(), cache.path(), args);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    };

    run(&[]);
    run(&[]);
    assert_eq!(fs::read_to_string(project.path().join("implicit.txt")).unwrap(), "x");
    assert!(!project.path().join("specified.txt").exists());

    run(&["--force"]);
    assert_eq!(fs::read_to_string(project.path().join("implicit.txt")).unwrap(), "xx");

    run(&["specified"]);
    run(&["specified"]);
    assert_eq!(fs::read_to_string(project.path().join("specified.txt")).unwrap(), "x");

    run(&["--force", "specified"]);
    assert_eq!(fs::read_to_string(project.path().join("specified.txt")).unwrap(), "xx");
}

#[test]
fn since_lists_changed_blocks_and_content_dependents() {
    let project = tempfile::tempdir().unwrap();
    fs::write(project.path().join("a.txt"), "a\n").unwrap();
    fs::write(project.path().join("b.txt"), "b\n").unwrap();
    fs::write(project.path().join("c.txt"), "c\n").unwrap();
    fs::write(project.path().join("e.txt"), "e\n").unwrap();
    fs::write(
        project.path().join("BUILD.bit"),
        r#"
a = exec {
  command = "true"
  inputs = ["a.txt"]
}

b = exec {
  command = "true"
  inputs = ["b.txt"]
}

c = exec {
  command = "true"
  inputs = ["c.txt"]
  depends_on = [a]
}

d = exec {
  command = "true"
  inputs = ["d.txt"]
}

e = exec {
  command = "true"
  inputs = ["e.txt"]
}
"#,
    )
    .unwrap();

    assert!(run_git(project.path(), &["init", "-q"]).status.success());
    assert!(run_git(project.path(), &["add", "."]).status.success());
    assert!(
        run_git(
            project.path(),
            &[
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=Test",
                "commit",
                "-qm",
                "base",
            ],
        )
        .status
        .success()
    );
    let base = run_git(project.path(), &["rev-parse", "HEAD"]);
    assert!(base.status.success());
    let base = String::from_utf8(base.stdout).unwrap();
    let base = base.trim();

    fs::write(project.path().join("a.txt"), "changed\n").unwrap();
    assert!(run_git(project.path(), &["add", "a.txt"]).status.success());
    fs::write(project.path().join("e.txt"), "changed\n").unwrap();
    fs::write(project.path().join("d.txt"), "untracked\n").unwrap();

    let target_list = run_bit(project.path(), &["--list", "--since", base]);
    assert!(!target_list.status.success());
    assert!(String::from_utf8_lossy(&target_list.stderr).contains("--since requires -ll"));

    let output = run_bit(project.path(), &["-ll", "--since", base]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!stdout.starts_with('\n') && !stdout.starts_with('\r'), "{stdout:?}");
    assert!(stdout.contains("a ("), "{stdout:?}");
    assert!(stdout.contains("c ("), "{stdout:?}");
    assert!(stdout.contains("d ("), "{stdout:?}");
    assert!(stdout.contains("e ("), "{stdout:?}");
    assert!(!stdout.contains("b ("), "{stdout:?}");
}
