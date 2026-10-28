pub mod build;
pub mod exe;
pub mod fmt;
pub mod generate;
pub mod lint;
pub mod scanner;
pub mod test;

use crate::process::CommandExt;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::provider::{Ambient, BoxError, Changes, DynResource, FuncSignature, Provider, VersionCommand};
use crate::sha256::SHA256;
use crate::value::Value;
use scanner::Tests;

/// First-class Go environment variables shared across all go resources.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, bit_derive::Schema)]
pub struct GoEnv {
    /// Target OS
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goos: Option<String>,
    /// Target architecture
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goarch: Option<String>,
    /// Enable cgo
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgo: Option<bool>,
}

impl GoEnv {
    pub fn apply_to(&self, cmd: &mut Command) {
        cmd.envs(self.vars());
    }

    /// The environment variables these overrides set.
    fn vars(&self) -> Vec<(&'static str, String)> {
        let mut vars = Vec::new();
        if let Some(v) = &self.goos {
            vars.push(("GOOS", v.clone()));
        }
        if let Some(v) = &self.goarch {
            vars.push(("GOARCH", v.clone()));
        }
        if let Some(cgo) = self.cgo {
            vars.push(("CGO_ENABLED", if cgo { "1" } else { "0" }.to_owned()));
        }
        vars
    }
}

/// Deserialize a block's `package` field: one Go package pattern or a list of
/// them. The list must not be empty, so a deserialized field always has a
/// first pattern.
fn package_patterns<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let packages = crate::providers::string_or_vec(deserializer)?;
    // The go command treats no patterns as the current directory's package,
    // so an empty list would silently build something the block never named.
    if packages.is_empty() {
        return Err(serde::de::Error::custom("package list must not be empty"));
    }
    Ok(packages)
}

/// Scan Go source files for package patterns and return hashed inputs,
/// together with the files the go command `flags` name and the files
/// matching the block's extra `inputs` globs.
///
/// `dir` is the block's `dir`. The go command runs there, or where a `-C`
/// flag moves it, and the scanner walks up from there to locate `go.mod`.
/// `None` starts from bit's CWD. The `inputs` globs are relative to `dir`,
/// as in every other block.
pub fn resolve_go_inputs(
    patterns: &[String],
    dir: Option<&str>,
    flags: &[String],
    tests: Tests,
    inputs: &[String],
    tracker: &FileTracker,
) -> Result<BTreeMap<String, SHA256>, BoxError> {
    let go_dir = run_dir(dir, flags);
    let mut files: Vec<_> = scanner::scan(patterns, tests, go_dir.as_deref().map(Path::new))?
        .into_iter()
        .collect();
    files.extend(flag_inputs(go_dir.as_deref(), flags));
    let mut hashes = tracker.hash_files(&files)?;
    for pattern in inputs {
        hashes.extend(tracker.hash_glob(&crate::provider::glob_in_dir(dir, pattern))?);
    }
    Ok(hashes)
}

/// A go command flag that takes a value, as a block's `flags` give it.
struct FlagValue<'a> {
    name: &'a str,
    value: &'a str,
    /// The indices of the arguments it spans: the flag, and its value when
    /// that is a separate argument.
    args: std::ops::Range<usize>,
}

/// The flags among `flags` named in `names`, each given as `-name value`
/// or `-name=value`, with one or two dashes. Every name must be a flag that
/// takes a value, so the argument after it is its value.
fn flag_values<'a>(flags: &'a [String], names: &[&str]) -> Vec<FlagValue<'a>> {
    let mut found = Vec::new();
    let mut next = 0;
    while next < flags.len() {
        let start = next;
        let arg = &flags[next];
        next += 1;
        let Some(flag) = arg.strip_prefix("--").or_else(|| arg.strip_prefix('-')) else {
            continue;
        };
        let (name, inline) = match flag.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (flag, None),
        };
        if !names.contains(&name) {
            continue;
        }
        let value = match inline {
            Some(value) => value,
            None => match flags.get(next) {
                Some(value) => {
                    next += 1;
                    value.as_str()
                }
                None => break,
            },
        };
        found.push(FlagValue {
            name,
            value,
            args: start..next,
        });
    }
    found
}

/// The directory the go command runs in: the block's `dir`, changed by a
/// `-C` flag. bit applies `-C` itself, by running the go command there,
/// because the go command accepts `-C` only as its first flag and bit
/// places its own flags ahead of the block's.
fn run_dir(dir: Option<&str>, flags: &[String]) -> Option<String> {
    match flag_values(flags, &["C"]).last() {
        Some(chdir) => Some(crate::provider::in_dir(dir, chdir.value)),
        None => dir.map(str::to_owned),
    }
}

/// `flags` without the `-C` flags [`run_dir`] applies.
fn run_flags(flags: &[String]) -> Vec<String> {
    let chdirs = flag_values(flags, &["C"]);
    flags
        .iter()
        .enumerate()
        .filter(|(index, _)| !chdirs.iter().any(|chdir| chdir.args.contains(index)))
        .map(|(_, flag)| flag.clone())
        .collect()
}

/// The files the go command reads because of `flags`, as bit's process
/// sees them when the go command runs in `go_dir`: the `-modfile` and the
/// `.sum` file beside it, the `-overlay` config and the files it
/// substitutes, and the profile `-pgo` names.
fn flag_inputs(go_dir: Option<&str>, flags: &[String]) -> Vec<PathBuf> {
    let path = |value: &str| PathBuf::from(crate::provider::in_dir(go_dir, value));
    let mut files = Vec::new();
    for flag in flag_values(flags, &["modfile", "overlay", "pgo"]) {
        match flag.name {
            "modfile" => {
                files.push(path(flag.value));
                let stem = flag.value.strip_suffix(".mod").unwrap_or(flag.value);
                files.push(path(&format!("{stem}.sum")));
            }
            "overlay" => {
                files.push(path(flag.value));
                let replacements = std::fs::read_to_string(path(flag.value))
                    .ok()
                    .and_then(|config| serde_json::from_str::<serde_json::Value>(&config).ok());
                let replacements = replacements.as_ref().and_then(|config| config["Replace"].as_object());
                // An empty replacement deletes the file rather than naming one.
                files.extend(
                    replacements
                        .into_iter()
                        .flat_map(|replace| replace.values())
                        .filter_map(|target| target.as_str().filter(|target| !target.is_empty()))
                        .map(path),
                );
            }
            _ if flag.value != "auto" && flag.value != "off" => files.push(path(flag.value)),
            _ => {}
        }
    }
    files
}

/// The files and directories `go build` writes because of `-o` in `flags`,
/// as bit's process sees them when the go command runs in `go_dir`. Writing
/// to the null device discards the binary, so it names no output.
fn build_outputs(go_dir: Option<&str>, flags: &[String]) -> Vec<String> {
    flag_values(flags, &["o"])
        .into_iter()
        .filter(|flag| flag.value != "/dev/null" && !flag.value.eq_ignore_ascii_case("NUL"))
        .map(|flag| crate::provider::in_dir(go_dir, flag.value))
        .collect()
}

/// `paths` shortened for display in a start line: project-relative, as the
/// engine shows input paths. bit's working directory is the project root;
/// it is canonicalized to match the scanner's canonical paths.
fn display_paths<'a>(paths: impl IntoIterator<Item = &'a Path>) -> Vec<String> {
    let root = std::env::current_dir().and_then(|dir| dir.canonicalize()).ok();
    paths
        .into_iter()
        .map(|path| {
            let path = path.to_string_lossy();
            match &root {
                Some(root) => crate::cache::project_relative(root, &path).into_owned(),
                None => path.into_owned(),
            }
        })
        .collect()
}

/// `package` relative to `module`, as `./sub/pkg` or `.` for the module's
/// root package, so displayed lines are not dominated by the module path.
/// Anything outside the module, including flags, is returned unchanged.
fn display_package<'a>(package: &'a str, module: Option<&str>) -> std::borrow::Cow<'a, str> {
    let Some(module) = module else {
        return package.into();
    };
    if package == module {
        return ".".into();
    }
    match package.strip_prefix(module).and_then(|rest| rest.strip_prefix('/')) {
        Some(rest) => format!("./{rest}").into(),
        None => package.into(),
    }
}

/// `program` and `args` for display, with import paths inside the module
/// that `dir` (or bit's CWD) belongs to shortened by [`display_package`].
/// Only the displayed line changes; the command still receives `args`.
fn display_command(program: &str, args: &[String], dir: Option<&str>) -> String {
    let module = scanner::module_path(dir.map(Path::new)).ok();
    let shown: Vec<_> = args.iter().map(|arg| display_package(arg, module.as_deref())).collect();
    crate::providers::display_command(program, &shown)
}

/// The effective Go toolchain and target, as `go env` reports them once the
/// block's `GoEnv` overrides and the ambient environment apply. Run in
/// `dir`, the go command honours the module's `toolchain` line, so a
/// toolchain switch behind it is seen.
pub fn toolchain(env: &GoEnv, dir: Option<&str>) -> Ambient {
    let mut command = VersionCommand::new(
        "go",
        &[
            "env",
            "GOVERSION",
            "GOOS",
            "GOARCH",
            "CGO_ENABLED",
            "GOFLAGS",
            "GOEXPERIMENT",
            "GOFIPS140",
            // Each architecture's feature level.
            "GOARM",
            "GOARM64",
            "GOAMD64",
            "GO386",
            "GOPPC64",
            "GOMIPS",
            "GOMIPS64",
            "GORISCV64",
            "GOWASM",
            // The C toolchain cgo builds with.
            "CC",
            "CXX",
            "FC",
            "CGO_CFLAGS",
            "CGO_CPPFLAGS",
            "CGO_CXXFLAGS",
            "CGO_FFLAGS",
            "CGO_LDFLAGS",
            "PKG_CONFIG",
            // The workspace file in use, from `GOWORK` or found upward.
            "GOWORK",
        ],
    )
    .dir(dir);
    for (name, value) in env.vars() {
        command = command.env(name, &value);
    }
    command.ambient()
}

/// List Go packages matching a package pattern.
#[bit_derive::provider_function]
fn packages(pattern: String, dir: Option<String>) -> Result<Vec<String>, BoxError> {
    let mut command = Command::new("go");
    command.args(["list", &pattern]);
    if let Some(dir) = &dir {
        command.current_dir(dir);
    }
    let output = command
        .logged_output()
        .map_err(|error| format!("failed to execute `go list {pattern}`: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "`go list {pattern}` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }

    Ok(package_list(&output.stdout))
}

/// List Go modules at or below a directory.
///
/// Returns each `go.mod` directory relative to bit's working directory, skipping
/// `vendor`, `testdata`, and directories starting with `.` or `_`, as `./...` does.
#[bit_derive::provider_function]
fn modules(dir: Option<String>) -> Result<Vec<String>, BoxError> {
    let base = dir.unwrap_or_else(|| ".".to_owned());
    let mut found = Vec::new();
    visit_module_dirs(Path::new(&base), PathBuf::new(), &mut found)?;
    let mut modules: Vec<_> = found
        .into_iter()
        .map(|relative| {
            if relative.as_os_str().is_empty() {
                base.clone()
            } else if base == "." {
                relative.to_string_lossy().into_owned()
            } else {
                Path::new(&base).join(relative).to_string_lossy().into_owned()
            }
        })
        .collect();
    modules.sort();
    Ok(modules)
}

fn visit_module_dirs(base: &Path, relative: PathBuf, found: &mut Vec<PathBuf>) -> Result<(), BoxError> {
    let dir = base.join(&relative);
    if dir.join("go.mod").is_file() {
        found.push(relative.clone());
    }
    let entries = std::fs::read_dir(&dir).map_err(|error| format!("reading {}: {error}", dir.display()))?;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !entry.file_type()?.is_dir()
            || name.starts_with('.')
            || name.starts_with('_')
            || name == "vendor"
            || name == "testdata"
        {
            continue;
        }
        visit_module_dirs(base, relative.join(name.as_ref()), found)?;
    }
    Ok(())
}

pub(super) fn package_list(output: &[u8]) -> Vec<String> {
    let mut packages: Vec<_> = String::from_utf8_lossy(output)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    packages.sort();
    packages.dedup();
    packages
}

/// The packages `go list -e` reports for `patterns` in `dir`, with extra
/// `args` such as build tags.
fn list_packages(
    patterns: &[String],
    args: &[String],
    dir: Option<&str>,
    env: &GoEnv,
) -> Result<Vec<String>, BoxError> {
    let mut cmd = Command::new("go");
    cmd.args(["list", "-e"]).args(args).args(patterns);
    if let Some(dir) = dir {
        cmd.current_dir(dir);
    }
    env.apply_to(&mut cmd);
    let patterns = patterns.join(" ");
    let output = cmd
        .logged_output()
        .map_err(|e| format!("failed to execute `go list {patterns}`: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "`go list {patterns}` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(package_list(&output.stdout))
}

/// The packages of a pattern that a change affects.
struct Affected {
    /// Packages `go` lists whose files include a changed path.
    packages: Vec<scanner::TestedPackage>,
    /// Whether a changed path also affects a package that build constraints
    /// exclude from the listing.
    excluded: bool,
    /// How many packages `go` lists.
    total: usize,
}

/// The packages `patterns` name whose files include a changed path, or `None`
/// to run the whole patterns. `tests` must match what the block resolves,
/// so each tracked path belongs to the packages that depend on it. `list`
/// reports the packages `go` matches for the patterns, so `args` and `env`
/// must match those of the narrowed command.
fn affected_packages(
    patterns: &[String],
    tests: Tests,
    dir: Option<&str>,
    args: &[String],
    env: &GoEnv,
    changes: &Changes,
) -> Result<Option<Affected>, BoxError> {
    let Changes::Paths(changed) = changes else {
        return Ok(None);
    };
    let scanned = scanner::tested_packages(patterns, tests, dir.map(Path::new))?;
    // A path no package depends on is an extra input, a config file or a
    // deleted file, whose dependents cannot be found.
    if changed
        .iter()
        .any(|path| !scanned.iter().any(|p| p.files.contains(path)))
    {
        return Ok(None);
    }
    // The scanner ignores build constraints, and go fails on a named package
    // they exclude, so only packages `go` lists may be named.
    let listed = list_packages(patterns, args, dir, env)?;
    if listed
        .iter()
        .any(|path| !scanned.iter().any(|p| &p.import_path == path))
    {
        return Ok(None);
    }
    let (packages, excluded): (Vec<_>, Vec<_>) = scanned
        .into_iter()
        .filter(|p| changed.iter().any(|path| p.files.contains(path)))
        .partition(|p| listed.contains(&p.import_path));
    Ok(Some(Affected {
        packages,
        excluded: !excluded.is_empty(),
        total: listed.len(),
    }))
}

/// Go provider with `exe`, `build`, and `test` resources.
pub struct GoProvider {
    tracker: Arc<FileTracker>,
}

impl GoProvider {
    pub fn new(tracker: Arc<FileTracker>) -> Self {
        Self { tracker }
    }
}

impl Provider for GoProvider {
    fn name(&self) -> &str {
        "go"
    }

    fn resources(&self) -> Vec<Box<dyn DynResource>> {
        vec![
            Box::new(exe::GoExeResource::new(self.tracker.clone())),
            Box::new(build::GoBuildResource::new(self.tracker.clone())),
            Box::new(generate::GoGenerateResource::new(self.tracker.clone())),
            Box::new(test::GoTestResource::new(self.tracker.clone())),
            Box::new(lint::GoLintResource::new(self.tracker.clone())),
            Box::new(fmt::GoFmtResource::new(self.tracker.clone())),
            Box::new(fmt::GoFmtCheckResource::new(self.tracker.clone())),
        ]
    }

    fn functions(&self) -> Vec<FuncSignature> {
        vec![__bit_signature_packages(), __bit_signature_modules()]
    }

    fn call_function(&self, name: &str, args: &[Value]) -> Result<Value, BoxError> {
        match name {
            "packages" => __bit_call_packages(args),
            "modules" => __bit_call_modules(args),
            _ => Err(format!("go provider has no function '{name}'").into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_registration() {
        let provider = GoProvider::new(Arc::new(FileTracker::default()));
        assert_eq!(provider.name(), "go");
        let resources = provider.resources();
        assert_eq!(resources.len(), 7);
        assert_eq!(resources[0].name(), "exe");
        assert_eq!(resources[1].name(), "build");
        assert_eq!(resources[2].name(), "generate");
        assert_eq!(resources[3].name(), "test");
        assert_eq!(resources[4].name(), "lint");
        assert_eq!(resources[5].name(), "fmt");
        assert_eq!(resources[6].name(), "fmt-l");
    }

    #[test]
    fn modules_lists_nested_modules_and_skips_ignored_dirs() {
        let root = tempfile::tempdir().unwrap();
        for dir in [
            "",
            "svc",
            "lib/inner",
            "svc/vendor/dep",
            "lib/testdata/fixture",
            "_tools",
            ".hidden",
        ] {
            let dir = root.path().join(dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("go.mod"), "module example\n").unwrap();
        }
        std::fs::create_dir_all(root.path().join("plain")).unwrap();

        let base = root.path().to_string_lossy().into_owned();
        assert_eq!(
            modules(Some(base.clone())).unwrap(),
            [base.clone(), format!("{base}/lib/inner"), format!("{base}/svc")]
        );
    }

    #[test]
    fn display_package_elides_the_module_path() {
        let module = Some("github.com/gohugoio/hugo");
        assert_eq!(
            display_package("github.com/gohugoio/hugo/tpl/safe", module),
            "./tpl/safe"
        );
        assert_eq!(display_package("github.com/gohugoio/hugo", module), ".");
        assert_eq!(
            display_package("github.com/gohugoio/hugoext/x", module),
            "github.com/gohugoio/hugoext/x"
        );
        assert_eq!(display_package("-json", module), "-json");
        assert_eq!(
            display_package("github.com/gohugoio/hugo/tpl", None),
            "github.com/gohugoio/hugo/tpl"
        );
    }

    #[test]
    fn display_command_shortens_import_paths_in_the_module() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("go.mod"), "module example.com/m\n").unwrap();
        let args = ["test", "-json", "example.com/m/lib", "example.com/other"].map(String::from);
        let line = display_command("go", &args, dir.path().to_str());
        assert_eq!(line, "go test -json ./lib example.com/other");
    }

    #[test]
    fn toolchain_reports_cgo_and_architecture_settings() {
        let Ambient::Version(command) = toolchain(&GoEnv::default(), None) else {
            panic!("the toolchain is a version command");
        };
        for name in [
            "CC",
            "CXX",
            "CGO_CFLAGS",
            "CGO_LDFLAGS",
            "PKG_CONFIG",
            "GOARM64",
            "GOWASM",
            "GOWORK",
        ] {
            assert!(command.args.iter().any(|arg| arg == name), "missing {name}");
        }
    }

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn flag_values_read_every_spelling() {
        let flags = strings(&[
            "-race",
            "-C",
            "svc",
            "--modfile=alt.mod",
            "-pgo",
            "off",
            "-ldflags=-o x",
        ]);
        let values: Vec<_> = flag_values(&flags, &["C", "modfile", "pgo", "o"])
            .into_iter()
            .map(|flag| (flag.name, flag.value, flag.args))
            .collect();
        assert_eq!(
            values,
            [("C", "svc", 1..3), ("modfile", "alt.mod", 3..4), ("pgo", "off", 4..6)]
        );
    }

    #[test]
    fn chdir_moves_the_go_command_and_leaves_the_flags() {
        let flags = strings(&["-C", "svc", "-trimpath", "-C=cmd"]);
        assert_eq!(run_dir(Some("app"), &flags).as_deref(), Some("app/cmd"));
        assert_eq!(run_dir(None, &strings(&["-C", "svc"])).as_deref(), Some("svc"));
        assert_eq!(run_dir(Some("app"), &[]).as_deref(), Some("app"));
        assert_eq!(run_flags(&flags), ["-trimpath"]);
    }

    #[test]
    fn flag_inputs_follow_the_files_flags_name() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_string_lossy().into_owned();
        std::fs::write(
            dir.path().join("overlay.json"),
            r#"{"Replace": {"a.go": "gen/a.go", "b.go": ""}}"#,
        )
        .unwrap();
        let flags = strings(&["-modfile=tools.mod", "-overlay", "overlay.json", "-pgo=cpu.pprof"]);
        let files = flag_inputs(Some(&root), &flags);
        let expected: Vec<PathBuf> = ["tools.mod", "tools.sum", "overlay.json", "gen/a.go", "cpu.pprof"]
            .iter()
            .map(|name| dir.path().join(name))
            .collect();
        assert_eq!(files, expected);
        assert!(flag_inputs(None, &strings(&["-pgo=auto", "-pgo", "off"])).is_empty());
    }

    #[test]
    fn build_outputs_are_relative_to_the_go_command() {
        let flags = strings(&["-o", "bin/", "-o=/dev/null"]);
        assert_eq!(build_outputs(Some("svc"), &flags), ["svc/bin/"]);
        assert!(build_outputs(None, &[]).is_empty());
    }

    #[test]
    fn package_list_is_sorted_and_deduplicated() {
        assert_eq!(
            package_list(b"example/z\nexample/a\nexample/z\n"),
            vec!["example/a", "example/z"]
        );
    }
}
