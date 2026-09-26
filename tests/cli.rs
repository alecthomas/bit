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
fn fmt_formats_a_named_file_without_a_project_and_preserves_comments() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("example.bit");
    fs::write(
        &path,
        "# Header\n\n# Command\njob=exec{\ncommand='echo # hello' # inline\n}\n",
    )
    .unwrap();

    let output = run_bit(directory.path(), &["--fmt", "example.bit"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(output.stdout.is_empty());
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "# Header\n\n# Command\njob = exec {\n  command = 'echo # hello'  # inline\n}\n"
    );

    let second = run_bit(directory.path(), &["--fmt", "example.bit"]);
    assert!(second.status.success(), "{}", String::from_utf8_lossy(&second.stderr));
    assert!(second.stdout.is_empty());
}

#[test]
fn fmt_without_a_path_formats_the_project_build_file() {
    let project = tempfile::tempdir().unwrap();
    let nested = project.path().join("nested");
    fs::create_dir(&nested).unwrap();
    let path = project.path().join("BUILD.bit");
    fs::write(&path, "# Comment\njob=exec{command='true'}\n").unwrap();
    fs::write(project.path().join("other.bit"), "let x=1\n").unwrap();

    let output = run_bit(&nested, &["--fmt"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(output.stdout.is_empty());
    assert_eq!(
        fs::read_to_string(path).unwrap(),
        "# Comment\njob = exec { command = 'true' }\n"
    );
    assert_eq!(
        fs::read_to_string(project.path().join("other.bit")).unwrap(),
        "let x=1\n"
    );
}

#[test]
fn fmt_rejects_invalid_input_without_rewriting_it() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("example.bit");
    let source = "job = exec {\n";
    fs::write(&path, source).unwrap();
    let output = run_bit(directory.path(), &["--fmt", "example.bit"]);
    assert!(!output.status.success());
    assert_eq!(fs::read_to_string(&path).unwrap(), source);
    assert!(String::from_utf8_lossy(&output.stderr).contains("example.bit"));

    let output = run_bit(directory.path(), &["--fmt", "example.bit", "another.bit"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("accepts zero or one .bit file"));
}

#[test]
fn quiet_suppresses_success_output_across_modes() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("BUILD.bit"),
        "task = exec { command = \"printf build-output\" }\ntarget build = [task]\n",
    )
    .unwrap();

    for args in [
        vec!["-q"],
        vec!["--quiet", "--debug", "--long"],
        vec!["-q", "--plan"],
        vec!["-q", "--list"],
        vec!["-q", "-ll"],
        vec!["-q", "--graph"],
        vec!["-q", "--dump"],
        vec!["-q", "--info"],
        vec!["-q", "--schema", "go"],
        vec!["-q", "--schema", "--json"],
        vec!["-q", "--update", "missing"],
        vec!["-q", "--cache"],
    ] {
        let output = run_bit_with_cache(project.path(), cache.path(), &args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stdout.is_empty(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            output.stderr.is_empty(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn quiet_preserves_errors() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("BUILD.bit"),
        "task = exec { command = \"false\" }\n",
    )
    .unwrap();

    let output = run_bit(project.path(), &["-q"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("error:"));

    let output = run_bit(project.path(), &["--quiet", "--schema", "missing"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown provider/resource"));
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
