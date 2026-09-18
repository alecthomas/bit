use std::fs;
use std::process::{Command, Output};

fn run_bit(project: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bit"))
        .args(args)
        .current_dir(project)
        .output()
        .expect("run bit")
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
