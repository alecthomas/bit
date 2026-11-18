//! End-to-end tests of the `bit` binary, one per script in `tests/testdata`.
//!
//! Cargo runs these only when asked, with `cargo test --test integration`,
//! or `bit integration-test`. `BIT_TEST_SHARD=index/count` runs every
//! `count`th script from the `index`th, so CI can split them across jobs.
//!
//! A script is a txtar file, as Go's testscript uses: commands, one per
//! line, then the files to create, each after a `-- path --` line. Every
//! script runs in a fresh directory, `$WORK`, holding those files.
//!
//! ```text
//! # A comment.
//! bit --plan
//! stdout 'build ▶'
//!
//! -- BUILD.bit --
//! build = exec { command = "true" }
//! ```
//!
//! Commands:
//!
//! - `bit args…` runs the binary under test, and `exec program args…` any
//!   other program. Either must succeed, or with `!` must fail.
//! - `stdout regex` and `stderr regex` match the last command's output, and
//!   with `!` must not. Patterns are multi-line: `^` and `$` match at lines.
//! - `exists path…`, `grep regex file` and `cmp a b` check files, and with
//!   `!` the opposite. `stdout` and `stderr` name the last output in `cmp`
//!   and `cp`.
//! - `cp from to`, `rm path…`, `mkdir path…`, `cd dir` and `env NAME=value…`
//!   change the setup, and `skip reason` ends the test as ignored.
//!
//! A command may start with conditions, such as `[exec:go]` to run only when
//! `go` is on `PATH`, or `[!exec:go] skip`. `[docker]` holds when a Docker
//! daemon answers. Arguments split on spaces, and
//! single quotes keep one together, with `''` for a quote. `$NAME` and
//! `${NAME}` expand outside quotes: `$WORK`, `$HOME`, `$ROOT` for the
//! repository, `$BIT` for the binary under test, or any environment
//! variable.
//!
//! Each script gets its own `HOME` and build cache, so it never reads or
//! writes the user's.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use libtest_mimic::{Arguments, Completion, Failed, Trial};
use regex::Regex;

fn main() {
    let args = Arguments::from_args();
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/testdata");
    let mut scripts: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .map(|entry| entry.expect("read testdata entry").path())
        .filter(|path| path.extension().is_some_and(|e| e == "txt"))
        .collect();
    scripts.sort();
    let (shard, shards) = shard();
    let trials = scripts
        .into_iter()
        .enumerate()
        .filter(|(index, _)| index % shards == shard)
        .map(|(_, path)| {
            let name = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
            Trial::ignorable_test(name, move || run(&path))
        })
        .collect();
    libtest_mimic::run(&args, trials).exit();
}

/// The zero-based shard to run and the number of shards, from
/// `BIT_TEST_SHARD=index/count` with a one-based index, or every script when
/// it is unset.
fn shard() -> (usize, usize) {
    let Ok(spec) = std::env::var("BIT_TEST_SHARD") else {
        return (0, 1);
    };
    let parsed = spec
        .split_once('/')
        .and_then(|(index, count)| Some((index.parse::<usize>().ok()?, count.parse::<usize>().ok()?)));
    match parsed {
        Some((index, count)) if (1..=count).contains(&index) => (index - 1, count),
        _ => panic!("BIT_TEST_SHARD must be index/count with 1 <= index <= count, not {spec:?}"),
    }
}

/// Run the script at `path`, failing with its log.
fn run(path: &Path) -> Result<Completion, Failed> {
    let content = fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let (script, files) = parse_txtar(&content);
    let mut state = State::new(&files).map_err(|e| format!("setting up {}: {e}", path.display()))?;
    for (number, line) in script.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let _ = writeln!(state.log, "> {line}");
        // TEMPORARY: per-command timings for diagnosing slow Docker scripts on CI.
        let started = std::time::Instant::now();
        let result = state.execute(line);
        let secs = started.elapsed().as_secs_f64();
        let name = path.file_stem().unwrap_or_default().to_string_lossy();
        eprintln!("TIMING {name} {secs:>6.2}s {line}");
        if secs > 1.0 && line.trim_start_matches("! ").starts_with("bit") {
            for out in state.stdout.lines() {
                eprintln!("TIMING {name}         | {out}");
            }
        }
        match result {
            Ok(Flow::Continue) => {}
            Ok(Flow::Skip(reason)) => return Ok(Completion::ignored_with(reason)),
            Err(e) => {
                let _ = writeln!(state.log, "FAIL: {}:{}: {e}", path.display(), number + 1);
                return Err(state.log.into());
            }
        }
    }
    Ok(Completion::Completed)
}

/// Split a txtar file into its script and its files.
fn parse_txtar(content: &str) -> (String, Vec<(String, String)>) {
    let mut script = String::new();
    let mut files: Vec<(String, String)> = Vec::new();
    for line in content.split_inclusive('\n') {
        let header = line
            .trim_end_matches('\n')
            .strip_prefix("-- ")
            .and_then(|rest| rest.strip_suffix(" --"));
        match (header, files.last_mut()) {
            (Some(name), _) => files.push((name.trim().to_owned(), String::new())),
            (None, Some((_, file))) => file.push_str(line),
            (None, None) => script.push_str(line),
        }
    }
    (script, files)
}

enum Flow {
    Continue,
    Skip(String),
}

struct State {
    /// Holds the work directory, `HOME` and the build cache.
    _root: tempfile::TempDir,
    work: PathBuf,
    cwd: PathBuf,
    /// Variables set for every command, over the inherited environment.
    env: BTreeMap<String, String>,
    stdout: String,
    stderr: String,
    log: String,
}

impl State {
    fn new(files: &[(String, String)]) -> Result<Self, String> {
        let root = tempfile::tempdir().map_err(|e| e.to_string())?;
        // Canonical, as macOS reaches the temporary directory through a
        // symlink, and bit reports canonical paths.
        let base = root.path().canonicalize().map_err(|e| e.to_string())?;
        let work = base.join("work");
        let home = base.join("home");
        for dir in [&work, &home] {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        for (name, content) in files {
            let path = work.join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            fs::write(&path, content).map_err(|e| format!("{name}: {e}"))?;
        }

        let mut env = BTreeMap::from([
            ("HOME".to_owned(), home.display().to_string()),
            (
                "BIT_CACHE".to_owned(),
                format!("disk://{}", base.join("cache").display()),
            ),
            ("NO_COLOR".to_owned(), "1".to_owned()),
        ]);
        // Toolchains found through `HOME` stay where they are installed:
        // Hermit's stubs, rustup and cargo would otherwise install them
        // again into the empty `HOME`. Go's build and module caches are
        // content-addressed, so sharing them is safe, and saves compiling
        // the standard library in every script.
        let real_home = dirs::home_dir().unwrap_or_default();
        let cache = dirs::cache_dir().unwrap_or_default();
        let toolchain_homes = [
            ("HERMIT_STATE_DIR", cache.join("hermit")),
            ("RUSTUP_HOME", real_home.join(".rustup")),
            ("CARGO_HOME", real_home.join(".cargo")),
            ("GOCACHE", cache.join("go-build")),
            ("GOMODCACHE", real_home.join("go/pkg/mod")),
        ];
        for (name, default) in toolchain_homes {
            let value = std::env::var(name).unwrap_or_else(|_| default.display().to_string());
            env.insert(name.to_owned(), value);
        }

        Ok(Self {
            _root: root,
            cwd: work.clone(),
            work,
            env,
            stdout: String::new(),
            stderr: String::new(),
            log: String::new(),
        })
    }

    fn execute(&mut self, line: &str) -> Result<Flow, String> {
        let mut words = self.split(line)?;
        // Conditions come first, each in brackets.
        while let Some(condition) = words.first().and_then(|word| word.strip_prefix('[')?.strip_suffix(']')) {
            let (negated, condition) = match condition.strip_prefix('!') {
                Some(condition) => (true, condition),
                None => (false, condition),
            };
            let holds = match condition.split_once(':') {
                Some(("exec", program)) => bit::providers::find_executable(program).is_some(),
                None if condition == "docker" => docker_running(),
                _ => return Err(format!("unknown condition [{condition}]")),
            };
            if holds == negated {
                return Ok(Flow::Continue);
            }
            words.remove(0);
        }
        let negated = words.first().is_some_and(|word| word == "!");
        if negated {
            words.remove(0);
        }
        let Some((command, args)) = words.split_first() else {
            return Err("no command".into());
        };
        self.command(negated, command, args)
    }

    fn command(&mut self, negated: bool, command: &str, args: &[String]) -> Result<Flow, String> {
        match command {
            "bit" => self.exec(negated, Path::new(env!("CARGO_BIN_EXE_bit")), args)?,
            "exec" => {
                let (program, args) = args.split_first().ok_or("exec needs a program")?;
                self.exec(negated, Path::new(program), args)?;
            }
            "stdout" | "stderr" => {
                let [pattern] = args else {
                    return Err(format!("{command} needs one pattern"));
                };
                let output = if command == "stdout" {
                    &self.stdout
                } else {
                    &self.stderr
                };
                let found = regex(pattern)?.is_match(output);
                check(negated, found, || format!("{command} matches {pattern:?}"))?;
            }
            "exists" => {
                for path in args {
                    let exists = self.path(path).exists();
                    check(negated, exists, || format!("{path} exists"))?;
                }
            }
            "grep" => {
                let [pattern, file] = args else {
                    return Err("grep needs a pattern and a file".into());
                };
                let content = self.read(file)?;
                let found = regex(pattern)?.is_match(&content);
                check(negated, found, || format!("{file} matches {pattern:?}"))?;
            }
            "cmp" => {
                let [a, b] = args else {
                    return Err("cmp needs two files".into());
                };
                let (left, right) = (self.read(a)?, self.read(b)?);
                if (left == right) == negated {
                    let expectation = if negated { "differ" } else { "match" };
                    return Err(format!(
                        "{a} and {b} should {expectation}\n--- {a}\n{left}\n--- {b}\n{right}"
                    ));
                }
            }
            "cp" => {
                let [from, to] = args else {
                    return Err("cp needs a source and a destination".into());
                };
                let to = self.path(to);
                if let Some(parent) = to.parent() {
                    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                // Files are copied as bytes, as they may be jars. The copy
                // is modified now: `fs::copy` keeps the source's time on
                // macOS, and tools such as cargo would then take an edited
                // file for an unchanged one.
                let copied = match from.as_str() {
                    "stdout" | "stderr" => fs::write(&to, self.read(from)?),
                    _ => fs::copy(self.path(from), &to).and_then(|_| {
                        fs::File::options()
                            .write(true)
                            .open(&to)?
                            .set_modified(std::time::SystemTime::now())
                    }),
                };
                copied.map_err(|e| format!("cp {from} {}: {e}", to.display()))?;
            }
            "rm" => {
                for path in args {
                    let path = self.path(path);
                    let removed = if path.is_dir() {
                        fs::remove_dir_all(&path)
                    } else {
                        fs::remove_file(&path)
                    };
                    removed.map_err(|e| format!("{}: {e}", path.display()))?;
                }
            }
            "mkdir" => {
                for path in args {
                    fs::create_dir_all(self.path(path)).map_err(|e| format!("{path}: {e}"))?;
                }
            }
            "cd" => {
                let [dir] = args else {
                    return Err("cd needs a directory".into());
                };
                let dir = self.path(dir);
                self.cwd = dir.canonicalize().map_err(|e| format!("{}: {e}", dir.display()))?;
            }
            "env" => {
                for assignment in args {
                    let (name, value) = assignment
                        .split_once('=')
                        .ok_or_else(|| format!("env needs NAME=value, not {assignment:?}"))?;
                    self.env.insert(name.to_owned(), value.to_owned());
                }
            }
            "skip" => return Ok(Flow::Skip(args.join(" "))),
            _ => return Err(format!("unknown command {command:?}")),
        }
        Ok(Flow::Continue)
    }

    /// Run `program`, recording its output, which must succeed unless
    /// `negated`.
    fn exec(&mut self, negated: bool, program: &Path, args: &[String]) -> Result<(), String> {
        let output = Command::new(program)
            .args(args)
            .current_dir(&self.cwd)
            .envs(&self.env)
            .output()
            .map_err(|e| format!("cannot run {}: {e}", program.display()))?;
        self.stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        self.stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        for (name, text) in [("stdout", &self.stdout), ("stderr", &self.stderr)] {
            if !text.is_empty() {
                let _ = write!(self.log, "[{name}]\n{text}");
                if !text.ends_with('\n') {
                    self.log.push('\n');
                }
            }
        }
        match (output.status.success(), negated) {
            (true, true) => Err("succeeded unexpectedly".into()),
            (false, false) => Err(format!("failed: {}", output.status)),
            _ => Ok(()),
        }
    }

    /// The content of `name`: the last command's output for `stdout` and
    /// `stderr`, and otherwise a file.
    fn read(&self, name: &str) -> Result<String, String> {
        match name {
            "stdout" => Ok(self.stdout.clone()),
            "stderr" => Ok(self.stderr.clone()),
            _ => fs::read_to_string(self.path(name)).map_err(|e| format!("{name}: {e}")),
        }
    }

    fn path(&self, path: &str) -> PathBuf {
        self.cwd.join(path)
    }

    fn lookup(&self, name: &str) -> String {
        match name {
            "WORK" => self.work.display().to_string(),
            "ROOT" => env!("CARGO_MANIFEST_DIR").to_owned(),
            "BIT" => env!("CARGO_BIN_EXE_bit").to_owned(),
            _ => self
                .env
                .get(name)
                .cloned()
                .or_else(|| std::env::var(name).ok())
                .unwrap_or_default(),
        }
    }

    /// Split a command line into words: spaces separate them, single quotes
    /// keep text together with `''` for a quote, and variables expand
    /// outside quotes.
    fn split(&self, line: &str) -> Result<Vec<String>, String> {
        let mut words = Vec::new();
        let mut word = String::new();
        let mut in_word = false;
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                ' ' | '\t' => {
                    if in_word {
                        words.push(std::mem::take(&mut word));
                        in_word = false;
                    }
                }
                '\'' => {
                    in_word = true;
                    loop {
                        match chars.next() {
                            Some('\'') if chars.peek() == Some(&'\'') => {
                                chars.next();
                                word.push('\'');
                            }
                            Some('\'') => break,
                            Some(c) => word.push(c),
                            None => return Err("unterminated quote".into()),
                        }
                    }
                }
                '$' => {
                    in_word = true;
                    let name: String = if chars.peek() == Some(&'{') {
                        chars.next();
                        let name: String = chars.by_ref().take_while(|c| *c != '}').collect();
                        name
                    } else {
                        let mut name = String::new();
                        while let Some(c) = chars.peek().filter(|c| c.is_ascii_alphanumeric() || **c == '_') {
                            name.push(*c);
                            chars.next();
                        }
                        name
                    };
                    if name.is_empty() {
                        word.push('$');
                    } else {
                        word.push_str(&self.lookup(&name));
                    }
                }
                c => {
                    in_word = true;
                    word.push(c);
                }
            }
        }
        if in_word {
            words.push(word);
        }
        Ok(words)
    }
}

/// Whether a Docker daemon answers, asked once per run.
fn docker_running() -> bool {
    static RUNNING: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *RUNNING.get_or_init(|| {
        Command::new("docker")
            .args(["info", "--format", "{{.ServerVersion}}"])
            .output()
            .is_ok_and(|output| output.status.success())
    })
}

fn regex(pattern: &str) -> Result<Regex, String> {
    Regex::new(&format!("(?m){pattern}")).map_err(|e| format!("bad pattern {pattern:?}: {e}"))
}

/// Fail unless `holds` is what `negated` asks for, describing the
/// expectation with `what`.
fn check(negated: bool, holds: bool, what: impl FnOnce() -> String) -> Result<(), String> {
    match (holds, negated) {
        (true, true) => Err(format!("unexpectedly, {}", what())),
        (false, false) => Err(format!("expected: {}", what())),
        _ => Ok(()),
    }
}
