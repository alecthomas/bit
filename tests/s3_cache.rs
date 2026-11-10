//! `bit --cache s3://…` end to end, against the S3-compatible server that
//! the `AWS_*` variables and `BIT_S3_TEST_BUCKET` describe. `bit
//! s3-cache-test` starts one in Docker and sets them; without them these
//! tests do nothing.
//!
//! Each simulated machine has its own home directory, so its disk cache,
//! which lives in the platform cache directory, starts empty.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BUILD: &str = r#"
a = exec {
  command = "echo a >> ../runs.log && echo a > a.txt"
  outputs = ["a.txt"]
}

b = exec {
  command = "echo b >> ../runs.log && echo b > b.txt"
  outputs = ["b.txt"]
}
"#;

/// A machine with its own home directory and a copy of the project.
struct Machine {
    dir: PathBuf,
}

impl Machine {
    fn new(base: &Path, name: &str) -> Self {
        let dir = base.join(name);
        fs::create_dir_all(dir.join("home")).unwrap();
        fs::create_dir_all(dir.join("project")).unwrap();
        fs::write(dir.join("project/BUILD.bit"), BUILD).unwrap();
        Self { dir }
    }

    fn bit(&self, cache: &str, secret: Option<&str>) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bit"));
        command
            .arg("--cache")
            .arg(cache)
            .current_dir(self.dir.join("project"))
            .env("HOME", self.dir.join("home"))
            .env("XDG_CACHE_HOME", self.dir.join("home/.cache"))
            .env_remove("BIT_CACHE");
        if let Some(secret) = secret {
            command.env("AWS_SECRET_ACCESS_KEY", secret);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "bit failed:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    /// The targets whose commands ran on this machine.
    fn runs(&self) -> String {
        fs::read_to_string(self.dir.join("runs.log")).unwrap_or_default()
    }

    fn read(&self, file: &str) -> String {
        fs::read_to_string(self.dir.join("project").join(file)).unwrap()
    }
}

/// The bucket to test in, and a prefix in it no other test or run uses.
/// The test's name separates tests that start at the same instant.
fn bucket_and_prefix(test: &str) -> Option<(String, String)> {
    let bucket = std::env::var("BIT_S3_TEST_BUCKET").ok()?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    Some((bucket, format!("bit-cache-test/{nanos}/{test}")))
}

#[test]
fn a_result_published_by_one_machine_is_restored_on_another() {
    let Some((bucket, prefix)) = bucket_and_prefix("restored") else {
        return;
    };
    let base = tempfile::tempdir().unwrap();

    let ci = Machine::new(base.path(), "ci");
    ci.bit(&format!("s3+rw://{bucket}/{prefix}"), None);
    assert_eq!(ci.runs().lines().count(), 2);

    let dev = Machine::new(base.path(), "dev");
    let output = dev.bit(&format!("s3://{bucket}/{prefix}"), None);
    assert_eq!(dev.runs(), "", "nothing should run on a remote hit:\n{output:?}");
    assert_eq!(dev.read("a.txt"), "a\n");
    assert_eq!(dev.read("b.txt"), "b\n");
}

#[test]
fn a_read_only_remote_is_not_published_to() {
    let Some((bucket, prefix)) = bucket_and_prefix("read-only") else {
        return;
    };
    let base = tempfile::tempdir().unwrap();

    Machine::new(base.path(), "dev").bit(&format!("s3://{bucket}/{prefix}"), None);
    let other = Machine::new(base.path(), "other");
    other.bit(&format!("s3://{bucket}/{prefix}"), None);
    assert_eq!(other.runs().lines().count(), 2);
}

/// A remote that rejects every request costs its results, not the build,
/// and says so once rather than once per target.
#[test]
fn a_failing_remote_warns_once_and_the_build_succeeds() {
    let Some((bucket, prefix)) = bucket_and_prefix("failing") else {
        return;
    };
    let base = tempfile::tempdir().unwrap();

    let dev = Machine::new(base.path(), "dev");
    let output = dev.bit(&format!("s3://{bucket}/{prefix}"), Some("wrong-secret"));
    let shown = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(shown.matches("further remote cache failures").count(), 1, "{shown}");
    assert_eq!(dev.runs().lines().count(), 2);
}
