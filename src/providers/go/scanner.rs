use crate::process::CommandExt;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use dashmap::DashMap;

use crate::provider::BoxError;

/// Cached per-directory scan result: direct files and local import directories.
///
/// What test files reference is kept apart from what sources reference, so a
/// scan that excludes tests does not track test-only dependencies either.
struct PackageScan {
    /// Non-test files in this directory that the go command compiles: .go
    /// sources and those in [`OTHER_SOURCE_EXTENSIONS`], and `default.pgo`.
    go_files: HashSet<PathBuf>,
    /// .go test files in this directory.
    test_files: HashSet<PathBuf>,
    /// Files embedded by //go:embed directives in source files.
    embed_files: HashSet<PathBuf>,
    /// Files embedded by //go:embed directives in test files.
    test_embed_files: HashSet<PathBuf>,
    /// Non-standard-library import paths of source files. They are kept
    /// unresolved because which are local depends on the workspace the scan
    /// is used in, and one module can belong to several.
    imports: HashSet<String>,
    /// Non-standard-library import paths of test files.
    test_imports: HashSet<String>,
}

/// Suffixes of the non-Go files the go command compiles into a package:
/// assembly, cgo C, C++, Objective-C and Fortran sources and headers, SWIG
/// definitions and system objects, as `go/build` classifies them.
const OTHER_SOURCE_EXTENSIONS: [&str; 19] = [
    ".s", ".S", ".sx", ".c", ".cc", ".cpp", ".cxx", ".m", ".h", ".hh", ".hpp", ".hxx", ".f", ".F", ".for", ".f90",
    ".swig", ".swigcxx", ".syso",
];

/// A module's identity and the scans of its package directories.
struct Module {
    root: PathBuf,
    path: String,
    /// Per-directory scan results, keyed by canonical path.
    packages: DashMap<PathBuf, Arc<PackageScan>>,
}

/// Scanned modules keyed by canonical root, shared across all go resource
/// resolve() calls. Blocks resolve concurrently and may belong to different
/// modules, so each module keeps its own cache rather than one cache being
/// swapped between them. The maps are sharded and locked only to copy an
/// entry in or out, never while a directory is read, so two blocks that
/// miss on the same directory may both scan it.
static MODULES: LazyLock<DashMap<PathBuf, Arc<Module>>> = LazyLock::new(DashMap::new);

/// Which test inputs a scan tracks for the packages a pattern names. Test
/// inputs are never tracked for the packages those import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tests {
    Excluded,
    /// `_test.go` files.
    Sources,
    /// `_test.go` files and each package's `testdata/` tree, which tests read
    /// at run time.
    SourcesAndTestdata,
}

/// The module containing `start_dir`, loaded on first use.
fn module(start_dir: &Path) -> Result<Arc<Module>, BoxError> {
    let root = find_module_root(start_dir)?;
    if let Some(module) = MODULES.get(&root) {
        return Ok(Arc::clone(&module));
    }
    let content = fs::read_to_string(root.join("go.mod"))?;
    let module = Arc::new(Module {
        root: root.clone(),
        path: parse_module_path(&content)?,
        packages: DashMap::new(),
    });
    Ok(MODULES.entry(root).or_insert(module).value().clone())
}

/// Scan a single package directory, whose canonical path is `canonical`,
/// returning the cached result if available.
fn scan_package_dir(module: &Module, pkg_dir: &Path, canonical: &Path) -> Result<Arc<PackageScan>, BoxError> {
    if let Some(scan) = module.packages.get(canonical) {
        return Ok(Arc::clone(&scan));
    }

    let entries = fs::read_dir(pkg_dir).map_err(|e| format!("reading {}: {e}", pkg_dir.display()))?;

    let mut scan = PackageScan {
        go_files: HashSet::new(),
        test_files: HashSet::new(),
        embed_files: HashSet::new(),
        test_embed_files: HashSet::new(),
        imports: HashSet::new(),
        test_imports: HashSet::new(),
    };

    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !name_str.ends_with(".go") {
            // `default.pgo` only applies to main packages, but which are main
            // is not parsed here, so it is tracked in every package.
            if name_str == "default.pgo"
                || OTHER_SOURCE_EXTENSIONS
                    .iter()
                    .any(|extension| name_str.ends_with(extension))
            {
                scan.go_files.insert(entry.path());
            }
            continue;
        }

        let path = entry.path();
        let source = fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;

        let (files, embed_files, imports) = if name_str.ends_with("_test.go") {
            (&mut scan.test_files, &mut scan.test_embed_files, &mut scan.test_imports)
        } else {
            (&mut scan.go_files, &mut scan.embed_files, &mut scan.imports)
        };
        files.insert(path);
        imports.extend(parse_imports(&source).into_iter().filter(|imp| !is_stdlib(imp)));

        let embeds = parse_embeds(&source);
        if !embeds.is_empty() {
            embed_files.extend(expand_embeds(pkg_dir, &embeds));
        }
    }

    let scan = Arc::new(scan);
    module.packages.insert(canonical.to_path_buf(), Arc::clone(&scan));
    Ok(scan)
}

/// Scan a package directory, returning its canonical path and cached scan.
fn package_scan(module: &Module, pkg_dir: &Path) -> Result<(PathBuf, Arc<PackageScan>), BoxError> {
    let canonical = pkg_dir.canonicalize().unwrap_or_else(|_| pkg_dir.to_path_buf());
    let scan = scan_package_dir(module, pkg_dir, &canonical)?;
    Ok((canonical, scan))
}

/// Collect a package's non-test files and those of its transitive local
/// imports. `visited` holds packages whose sources are already collected.
fn collect_sources(
    local: &LocalModules,
    module: &Module,
    pkg_dir: &Path,
    files: &mut HashSet<PathBuf>,
    visited: &mut HashSet<PathBuf>,
) -> Result<(), BoxError> {
    let (canonical, scan) = package_scan(module, pkg_dir)?;
    if !visited.insert(canonical) {
        return Ok(());
    }

    files.extend(scan.go_files.iter().cloned());
    files.extend(scan.embed_files.iter().cloned());
    for (dep_module, dep_dir) in scan.imports.iter().filter_map(|imp| local.resolve(imp)) {
        collect_sources(local, &dep_module, &dep_dir, files, visited)?;
    }

    Ok(())
}

/// Collect a pattern-named package's test inputs and test-only imports.
///
/// Skips the `visited` check for the package itself: it may have been visited
/// only as another package's import, which collects no test inputs.
fn collect_tests(
    local: &LocalModules,
    module: &Module,
    pkg_dir: &Path,
    tests: Tests,
    files: &mut HashSet<PathBuf>,
    visited: &mut HashSet<PathBuf>,
) -> Result<(), BoxError> {
    let (_, scan) = package_scan(module, pkg_dir)?;
    files.extend(scan.test_files.iter().cloned());
    files.extend(scan.test_embed_files.iter().cloned());
    if tests == Tests::SourcesAndTestdata {
        files.extend(files_under(&pkg_dir.join("testdata")));
    }
    for (dep_module, dep_dir) in scan.test_imports.iter().filter_map(|imp| local.resolve(imp)) {
        collect_sources(local, &dep_module, &dep_dir, files, visited)?;
    }
    Ok(())
}

/// Scan a Go package pattern and return all source files (local deps, embeds, go.mod/go.sum).
///
/// Results are cached per package directory so that multiple blocks sharing
/// transitive imports only scan each directory once.
///
/// `base_dir` is the directory from which module-root discovery begins (walking
/// upward looking for `go.mod`). Pass `Some(dir)` when the calling block runs
/// `go` from a non-project-root directory — e.g. a `go.exe` block with a `dir`
/// pointing at a separate-module subdirectory. `None` starts from bit's CWD.
pub fn scan(patterns: &[String], tests: Tests, base_dir: Option<&Path>) -> Result<HashSet<PathBuf>, BoxError> {
    let (local, pkg_dirs) = pattern_packages(patterns, base_dir)?;

    let mut all_files = local.files.clone();
    let mut visited = HashSet::new();

    for (module, dir) in &pkg_dirs {
        collect_sources(&local, module, dir, &mut all_files, &mut visited)?;
        if tests != Tests::Excluded {
            collect_tests(&local, module, dir, tests, &mut all_files, &mut visited)?;
        }
    }

    Ok(all_files)
}

/// A package a pattern names, with every file its tests depend on.
pub struct TestedPackage {
    pub import_path: String,
    /// Absolute package directory.
    pub dir: PathBuf,
    pub files: HashSet<PathBuf>,
}

/// List the packages the patterns name, each with the files a
/// [`Tests::SourcesAndTestdata`] scan of that package alone would return.
pub fn tested_packages(patterns: &[String], base_dir: Option<&Path>) -> Result<Vec<TestedPackage>, BoxError> {
    let (local, pkg_dirs) = pattern_packages(patterns, base_dir)?;
    pkg_dirs
        .iter()
        .map(|(module, dir)| {
            let mut files = local.files.clone();
            let mut visited = HashSet::new();
            collect_sources(&local, module, dir, &mut files, &mut visited)?;
            collect_tests(&local, module, dir, Tests::SourcesAndTestdata, &mut files, &mut visited)?;
            Ok(TestedPackage {
                import_path: import_path(module, dir),
                dir: dir.clone(),
                files,
            })
        })
        .collect()
}

/// Package directories, each with the module it belongs to.
type PackageDirs = Vec<(Arc<Module>, PathBuf)>;

/// The local modules the go command run in `base_dir` (or bit's CWD) builds
/// from, and the package directories `patterns` name, each once.
fn pattern_packages(
    patterns: &[String],
    base_dir: Option<&Path>,
) -> Result<(Arc<LocalModules>, PackageDirs), BoxError> {
    let base = base_module(base_dir)?;
    let cwd = match base_dir {
        Some(dir) => dir.to_path_buf(),
        None => std::env::current_dir()?,
    };
    // Canonical, like the module root, so package directories compare equal.
    let cwd = cwd.canonicalize().unwrap_or(cwd);
    let local = local_modules(&cwd, &base);
    let mut seen = HashSet::new();
    let mut pkg_dirs = Vec::new();
    for pattern in patterns {
        // An import path pattern may name another local module.
        let (path, module) = match pattern.starts_with('.') {
            true => (base.path.clone(), Arc::clone(&base)),
            false => local
                .module_for(pattern.trim_end_matches("/..."))
                .unwrap_or_else(|| (base.path.clone(), Arc::clone(&base))),
        };
        for dir in resolve_package_dirs(&module.root, &path, &cwd, pattern)? {
            if seen.insert(dir.clone()) {
                pkg_dirs.push((Arc::clone(&module), dir));
            }
        }
    }
    Ok((local, pkg_dirs))
}

/// The module `base_dir` (or bit's CWD) belongs to.
fn base_module(base_dir: Option<&Path>) -> Result<Arc<Module>, BoxError> {
    match base_dir {
        Some(dir) => module(dir),
        None => module(&std::env::current_dir()?),
    }
}

/// The import path of the module `base_dir` (or bit's CWD) belongs to.
pub fn module_path(base_dir: Option<&Path>) -> Result<String, BoxError> {
    Ok(base_module(base_dir)?.path.clone())
}

/// The import path of the package in `rel_dir`, a directory relative to
/// `base_dir` (or bit's CWD), as the go command resolves a `./` pattern.
pub fn package_import_path(base_dir: Option<&Path>, rel_dir: &str) -> Result<String, BoxError> {
    let base = match base_dir {
        Some(dir) => dir.to_path_buf(),
        None => std::env::current_dir()?,
    };
    let pkg_dir = base.join(rel_dir).canonicalize()?;
    Ok(import_path(&*module(&pkg_dir)?, &pkg_dir))
}

/// The modules a go command builds from local directories, and the files
/// that define them, which every package depends on.
struct LocalModules {
    /// Each module by the import path it is known by, longest first, so the
    /// deepest module owns an import.
    modules: Vec<(String, Arc<Module>)>,
    /// `go.work` and `go.work.sum`, and each module's `go.mod` and `go.sum`,
    /// where they exist.
    files: HashSet<PathBuf>,
}

/// Local module sets by canonical working directory, since `go.work`
/// discovery and `GOWORK` depend on where the go command runs.
static LOCAL_MODULES: LazyLock<DashMap<PathBuf, Arc<LocalModules>>> = LazyLock::new(DashMap::new);

impl LocalModules {
    /// The local module an import path belongs to, by the path it is known by.
    fn module_for(&self, import: &str) -> Option<(String, Arc<Module>)> {
        self.modules
            .iter()
            .find(|(path, _)| {
                import == path
                    || import
                        .strip_prefix(path.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
            })
            .map(|(path, module)| (path.clone(), Arc::clone(module)))
    }

    /// The module and package directory of a local import, if it is one.
    fn resolve(&self, import: &str) -> Option<(Arc<Module>, PathBuf)> {
        let (path, module) = self.module_for(import)?;
        let rel = import[path.len()..].trim_start_matches('/');
        let dir = if rel.is_empty() {
            module.root.clone()
        } else {
            module.root.join(rel)
        };
        dir.is_dir().then_some((module, dir))
    }
}

/// The local modules the go command run in `cwd` builds from: the main
/// modules, which are the workspace's `use` modules or else `base`, and the
/// targets of `replace` directives naming local directories.
///
/// The go command reports the main modules and the workspace file, and
/// parses the `replace` directives, so this follows its rules rather than
/// reimplementing them. Without a working go command, only `base` is local.
fn local_modules(cwd: &Path, base: &Arc<Module>) -> Arc<LocalModules> {
    if let Some(local) = LOCAL_MODULES.get(cwd) {
        return Arc::clone(&local);
    }
    let mut modules = vec![(base.path.clone(), Arc::clone(base))];
    if let Some(main) = go_json(cwd, &["list", "-m", "-json"]) {
        for entry in &main {
            if let (Some(path), Some(dir)) = (entry["Path"].as_str(), entry["Dir"].as_str())
                && let Ok(module) = module(Path::new(dir))
            {
                modules.push((path.to_owned(), module));
            }
        }
    }

    // `replace` directives apply from `go.work` and from each main module's
    // `go.mod`, which `go work edit` and `go mod edit` parse.
    let mut files = HashSet::new();
    let mut replace_sources: Vec<(PathBuf, &str)> = vec![];
    let gowork = go_output(cwd, &["env", "GOWORK"]).filter(|path| !path.is_empty() && path != "off");
    if let Some(gowork) = gowork {
        files.insert(PathBuf::from(format!("{gowork}.sum")));
        replace_sources.push((PathBuf::from(gowork), "work"));
    }

    // The go command builds dependencies from `vendor/` when it holds a
    // `modules.txt`: the workspace's, beside `go.work`, or else the main
    // module's. Whether `-mod` or the `go` version disables it is not
    // checked, since tracking an unused tree only costs a rerun.
    let vendor_roots: Vec<PathBuf> = match replace_sources.first() {
        Some((gowork, _)) => gowork.parent().into_iter().map(Path::to_path_buf).collect(),
        None => modules.iter().map(|(_, module)| module.root.clone()).collect(),
    };
    for vendor in vendor_roots.iter().map(|root| root.join("vendor")) {
        if vendor.join("modules.txt").is_file() {
            files.extend(files_under(&vendor));
        }
    }
    for (_, module) in &modules {
        let go_mod = module.root.join("go.mod");
        if !replace_sources.iter().any(|(source, _)| *source == go_mod) {
            replace_sources.push((go_mod, "mod"));
        }
    }

    for (source, kind) in &replace_sources {
        files.insert(source.clone());
        let source_arg = source.to_string_lossy();
        let Some(edit) = go_json(cwd, &[kind, "edit", "-json", &source_arg]).and_then(|mut edits| edits.pop()) else {
            continue;
        };
        let source_dir = source.parent().unwrap_or(Path::new(""));
        for replace in edit["Replace"].as_array().map(Vec::as_slice).unwrap_or_default() {
            let (Some(path), Some(target)) = (replace["Old"]["Path"].as_str(), replace["New"]["Path"].as_str()) else {
                continue;
            };
            // The go command treats a replacement as a directory when it is
            // a filesystem path, and as a module otherwise.
            if !(target.starts_with("./") || target.starts_with("../") || Path::new(target).is_absolute()) {
                continue;
            }
            if let Ok(module) = module(&source_dir.join(target)) {
                modules.push((path.to_owned(), module));
            }
        }
    }

    for (_, module) in &modules {
        files.extend(["go.mod", "go.sum"].map(|name| module.root.join(name)));
    }
    files.retain(|path| path.exists());
    // Longest first, and equal paths together so the first, a main module
    // ahead of any replacement, is kept.
    modules.sort_by(|(a, _), (b, _)| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    modules.dedup_by(|(a, _), (b, _)| a == b);

    let local = Arc::new(LocalModules { modules, files });
    LOCAL_MODULES.entry(cwd.to_path_buf()).or_insert(local).value().clone()
}

/// The trimmed stdout of `go <args>` run in `dir`, or `None` if it failed.
fn go_output(dir: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("go")
        .args(args)
        .current_dir(dir)
        .stderr(std::process::Stdio::null())
        .logged_output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// The JSON values `go <args>` prints, one after another, in `dir`.
fn go_json(dir: &Path, args: &[&str]) -> Option<Vec<serde_json::Value>> {
    let output = go_output(dir, args)?;
    serde_json::Deserializer::from_str(&output)
        .into_iter::<serde_json::Value>()
        .collect::<Result<_, _>>()
        .ok()
}

fn import_path(module: &Module, pkg_dir: &Path) -> String {
    let rel = pkg_dir.strip_prefix(&module.root).unwrap_or(pkg_dir);
    if rel.as_os_str().is_empty() {
        return module.path.clone();
    }
    let rel: Vec<_> = rel.components().map(|c| c.as_os_str().to_string_lossy()).collect();
    format!("{}/{}", module.path, rel.join("/"))
}

/// Find the module root by walking up from `start` looking for `go.mod`.
fn find_module_root(start: &Path) -> Result<PathBuf, BoxError> {
    let mut dir = if start.is_absolute() {
        start.to_path_buf()
    } else {
        std::env::current_dir()?.join(start)
    };
    // Canonicalize to resolve symlinks (e.g. /var -> /private/var on macOS).
    dir = dir.canonicalize().unwrap_or(dir);
    loop {
        if dir.join("go.mod").exists() {
            return Ok(dir);
        }
        if !dir.pop() {
            return Err("could not find go.mod in any parent directory".into());
        }
    }
}

/// Parse the `module` directive from go.mod content.
fn parse_module_path(content: &str) -> Result<String, BoxError> {
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("module") {
            let rest = rest.trim();
            let path = rest.split_whitespace().next().unwrap_or("");
            if !path.is_empty() {
                return Ok(path.to_owned());
            }
        }
    }
    Err("go.mod missing module directive".into())
}

/// Returns true if an import path refers to the Go standard library.
///
/// Stdlib packages have no dot in the first path component
/// (e.g. "fmt", "net/http"), while external packages start with a
/// domain (e.g. "github.com/foo/bar").
fn is_stdlib(import_path: &str) -> bool {
    let first = import_path.split('/').next().unwrap_or(import_path);
    !first.contains('.')
}

/// Parse import paths from a single `.go` file's content.
fn parse_imports(source: &str) -> Vec<String> {
    let mut imports = Vec::new();
    let mut in_import_block = false;

    for line in source.lines() {
        let trimmed = line.trim();

        if in_import_block {
            if trimmed.starts_with(')') {
                in_import_block = false;
                continue;
            }
            if let Some(path) = extract_import_path(trimmed) {
                imports.push(path);
            }
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("import") {
            let rest = rest.trim();
            if rest.starts_with('(') {
                in_import_block = true;
            } else if let Some(path) = extract_import_path(rest) {
                imports.push(path);
            }
        }
    }

    imports
}

/// Extract a quoted import path from a line, handling optional aliases.
///
/// Handles: `"path"`, `alias "path"`, `. "path"`, `_ "path"`.
fn extract_import_path(line: &str) -> Option<String> {
    let trimmed = line.split("//").next()?.trim();
    let start = trimmed.find('"')? + 1;
    let end = start + trimmed[start..].find('"')?;
    Some(trimmed[start..end].to_owned())
}

/// Parse `//go:embed` directives from a `.go` file, returning glob patterns
/// in the `glob` crate's syntax.
fn parse_embeds(source: &str) -> Vec<String> {
    let mut patterns = Vec::new();
    for line in source.lines() {
        let Some(rest) = line.trim().strip_prefix("//go:embed") else {
            continue;
        };
        if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
            continue;
        }
        for pat in embed_args(rest) {
            let pat = pat.strip_prefix("all:").unwrap_or(&pat);
            // `path.Match` negates a class with `^`, the `glob` crate with `!`.
            patterns.push(pat.replace("[^", "[!"));
        }
    }
    patterns
}

/// The space-separated arguments of a `//go:embed` directive, each bare or
/// quoted as a Go interpreted ("…") or raw (`…`) string literal.
fn embed_args(rest: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut chars = rest.trim().chars().peekable();
    while let Some(&first) = chars.peek() {
        if first.is_whitespace() {
            chars.next();
            continue;
        }
        let mut arg = String::new();
        match first {
            '"' => {
                chars.next();
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => arg.extend(chars.next()),
                        _ => arg.push(c),
                    }
                }
            }
            '`' => {
                chars.next();
                arg.extend(chars.by_ref().take_while(|&c| c != '`'));
            }
            _ => {
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() {
                        break;
                    }
                    arg.push(c);
                    chars.next();
                }
            }
        }
        args.push(arg);
    }
    args
}

/// Expand embed patterns relative to a package directory into file paths.
/// As with `go:embed`, a pattern matching a directory embeds every file
/// beneath it.
fn expand_embeds(pkg_dir: &Path, patterns: &[String]) -> Vec<PathBuf> {
    let pkg_dir = pkg_dir.to_string_lossy();
    let mut files = Vec::new();
    for pat in patterns {
        let Ok(entries) = glob::glob(&crate::provider::glob_in_dir(Some(&pkg_dir), pat)) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.is_dir() {
                files.extend(files_under(&entry));
            } else if entry.is_file() {
                files.push(entry);
            }
        }
    }
    files
}

/// All files beneath `dir`, recursively; empty if `dir` does not exist.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = glob::glob(&format!("{}/**/*", dir.display())) else {
        return Vec::new();
    };
    entries.flatten().filter(|entry| entry.is_file()).collect()
}

/// Resolve a package pattern to a list of package directories. A relative
/// pattern is relative to `cwd`, where the go command runs, as the go
/// command resolves it.
fn resolve_package_dirs(
    module_root: &Path,
    module_path: &str,
    cwd: &Path,
    pattern: &str,
) -> Result<Vec<PathBuf>, BoxError> {
    if let Some(base) = pattern.strip_suffix("/...") {
        let base_dir = if base == "." {
            cwd.to_path_buf()
        } else if let Some(rel) = base.strip_prefix("./") {
            cwd.join(rel)
        } else if let Some(rel) = base.strip_prefix(module_path) {
            module_root.join(rel.trim_start_matches('/'))
        } else {
            module_root.join(base)
        };
        return find_go_package_dirs(&base_dir);
    }

    if pattern.starts_with("./") || pattern == "." {
        let rel = pattern.strip_prefix("./").unwrap_or("");
        let dir = if rel.is_empty() {
            cwd.to_path_buf()
        } else {
            cwd.join(rel)
        };
        if dir.is_dir() {
            return Ok(vec![dir]);
        }
        return Err(format!("package directory not found: {}", dir.display()).into());
    }

    if let Some(rel) = pattern.strip_prefix(module_path) {
        let rel = rel.strip_prefix('/').unwrap_or(rel);
        let dir = if rel.is_empty() {
            module_root.to_path_buf()
        } else {
            module_root.join(rel)
        };
        if dir.is_dir() {
            return Ok(vec![dir]);
        }
        return Err(format!("package directory not found: {}", dir.display()).into());
    }

    Err(format!("cannot resolve package pattern: {pattern}").into())
}

/// Recursively find all directories containing `.go` files.
fn find_go_package_dirs(base: &Path) -> Result<Vec<PathBuf>, BoxError> {
    let mut dirs = Vec::new();
    visit_go_dirs(base, &mut dirs)?;
    Ok(dirs)
}

fn visit_go_dirs(dir: &Path, result: &mut Vec<PathBuf>) -> Result<(), BoxError> {
    let entries = fs::read_dir(dir).map_err(|e| format!("reading {}: {e}", dir.display()))?;
    let mut has_go = false;
    let mut subdirs = Vec::new();

    for entry in entries {
        let entry = entry?;
        let ft = entry.file_type()?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if ft.is_dir() {
            let path = entry.path();
            // As with `go`, a nested module's packages are not part of this one.
            if !name_str.starts_with('.')
                && !name_str.starts_with('_')
                && name_str != "vendor"
                && name_str != "testdata"
                && !path.join("go.mod").is_file()
            {
                subdirs.push(path);
            }
            continue;
        }

        if ft.is_file() && name_str.ends_with(".go") {
            has_go = true;
        }
    }

    if has_go {
        result.push(dir.to_path_buf());
    }

    for sub in subdirs {
        visit_go_dirs(&sub, result)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_module_path_basic() {
        assert_eq!(
            parse_module_path("module github.com/foo/bar\n\ngo 1.22\n").unwrap(),
            "github.com/foo/bar"
        );
    }

    #[test]
    fn parse_module_path_with_comment() {
        assert_eq!(
            parse_module_path("module example.com/pkg // my module\n").unwrap(),
            "example.com/pkg"
        );
    }

    #[test]
    fn parse_module_path_missing() {
        assert!(parse_module_path("go 1.22\n").is_err());
    }

    #[test]
    fn stdlib_detection() {
        assert!(is_stdlib("fmt"));
        assert!(is_stdlib("net/http"));
        assert!(is_stdlib("encoding/json"));
        assert!(is_stdlib("crypto/sha256"));
        assert!(!is_stdlib("github.com/foo/bar"));
        assert!(!is_stdlib("golang.org/x/tools"));
        assert!(!is_stdlib("gopkg.in/yaml.v2"));
    }

    #[test]
    fn parse_single_import() {
        let source = r#"package main

import "fmt"
"#;
        assert_eq!(parse_imports(source), vec!["fmt"]);
    }

    #[test]
    fn parse_grouped_imports() {
        let source = r#"package main

import (
	"fmt"
	"os"

	"github.com/foo/bar"
	_ "github.com/lib/pq"
)
"#;
        assert_eq!(
            parse_imports(source),
            vec!["fmt", "os", "github.com/foo/bar", "github.com/lib/pq"]
        );
    }

    #[test]
    fn parse_aliased_import() {
        let source = r#"package main

import myalias "github.com/foo/bar"
"#;
        assert_eq!(parse_imports(source), vec!["github.com/foo/bar"]);
    }

    #[test]
    fn parse_dot_import() {
        let source = r#"package main

import . "github.com/foo/bar"
"#;
        assert_eq!(parse_imports(source), vec!["github.com/foo/bar"]);
    }

    #[test]
    fn parse_import_with_comment() {
        let source = r#"package main

import (
	"fmt" // standard
	"github.com/foo/bar" // external
)
"#;
        assert_eq!(parse_imports(source), vec!["fmt", "github.com/foo/bar"]);
    }

    #[test]
    fn parse_embed_directives() {
        let source = r#"package main

import "embed"

//go:embed data.txt
var data string

//go:embed templates/* static/index.html
var content embed.FS
"#;
        assert_eq!(
            parse_embeds(source),
            vec!["data.txt", "templates/*", "static/index.html"]
        );
    }

    #[test]
    fn parse_embed_all_prefix() {
        let source = r#"package main

//go:embed all:templates
var content embed.FS
"#;
        assert_eq!(parse_embeds(source), vec!["templates"]);
    }

    #[test]
    fn parse_embed_quoted_patterns_and_negated_classes() {
        let source = "package main\n\n//go:embed \"my file.txt\" `raw dir` \"esc\\\"aped\" data/[^._]*\n//go:embedded nope\nvar content embed.FS\n";
        assert_eq!(
            parse_embeds(source),
            vec!["my file.txt", "raw dir", "esc\"aped", "data/[!._]*"]
        );
    }

    #[test]
    fn embed_globs_matching_directories_embed_their_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        write_files(
            &root,
            &[
                ("go.mod", "module example.com/test\n"),
                (
                    "main.go",
                    "package main\n\nimport \"embed\"\n\n//go:embed static/* \"my docs\"\nvar content embed.FS\n",
                ),
                ("static/css/site/main.css", ""),
                ("static/index.html", ""),
                ("my docs/guide.md", ""),
            ],
        );

        let files = scan(&[".".into()], Tests::Excluded, Some(&root)).unwrap();
        for expected in ["static/css/site/main.css", "static/index.html", "my docs/guide.md"] {
            assert!(files.contains(&root.join(expected)), "missing {expected}: {files:?}");
        }
    }

    #[test]
    fn extract_import_path_variants() {
        assert_eq!(extract_import_path(r#""fmt""#), Some("fmt".into()));
        assert_eq!(
            extract_import_path(r#"alias "github.com/foo""#),
            Some("github.com/foo".into())
        );
        assert_eq!(
            extract_import_path(r#". "github.com/foo""#),
            Some("github.com/foo".into())
        );
        assert_eq!(
            extract_import_path(r#"_ "github.com/foo""#),
            Some("github.com/foo".into())
        );
        assert_eq!(extract_import_path("// just a comment"), None);
        assert_eq!(extract_import_path(""), None);
    }

    #[test]
    fn resolve_relative_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let cmd_dir = dir.path().join("cmd/app");
        fs::create_dir_all(&cmd_dir).unwrap();
        fs::write(cmd_dir.join("main.go"), "package main").unwrap();
        fs::write(dir.path().join("go.mod"), "module example.com/test\n").unwrap();

        let dirs = resolve_package_dirs(dir.path(), "example.com/test", dir.path(), "./cmd/app").unwrap();
        assert_eq!(dirs, vec![cmd_dir]);
    }

    fn write_files(root: &Path, files: &[(&str, &str)]) {
        for (path, content) in files {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
    }

    #[test]
    fn scan_follows_imports_into_workspace_modules() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        write_files(
            &root,
            &[
                ("go.work", "go 1.22\n\nuse (\n\t./app\n\t./lib\n)\n"),
                ("app/go.mod", "module example.com/app\n\ngo 1.22\n"),
                (
                    "app/main.go",
                    "package main\n\nimport _ \"example.com/lib/util\"\n\nfunc main() {}\n",
                ),
                ("lib/go.mod", "module example.com/lib\n\ngo 1.22\n"),
                ("lib/util/util.go", "package util\n"),
                ("lib/other/other.go", "package other\n"),
            ],
        );

        let files = scan(&[".".into()], Tests::Excluded, Some(&root.join("app"))).unwrap();
        for expected in ["lib/util/util.go", "lib/go.mod", "go.work", "app/go.mod"] {
            assert!(files.contains(&root.join(expected)), "missing {expected}: {files:?}");
        }
        assert!(!files.contains(&root.join("lib/other/other.go")), "{files:?}");

        // A pattern may name another workspace module by its import path.
        let files = scan(
            &["example.com/lib/...".into()],
            Tests::Excluded,
            Some(&root.join("app")),
        )
        .unwrap();
        assert!(files.contains(&root.join("lib/other/other.go")), "{files:?}");
    }

    #[test]
    fn scan_follows_imports_into_local_replacements() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        write_files(
            &root,
            &[
                (
                    "app/go.mod",
                    "module example.com/app\n\ngo 1.22\n\nrequire example.com/dep v0.0.0\n\nreplace example.com/dep => ../dep\n",
                ),
                (
                    "app/main.go",
                    "package main\n\nimport _ \"example.com/dep\"\n\nfunc main() {}\n",
                ),
                ("dep/go.mod", "module example.com/dep\n\ngo 1.22\n"),
                ("dep/dep.go", "package dep\n"),
            ],
        );

        let files = scan(&[".".into()], Tests::Excluded, Some(&root.join("app"))).unwrap();
        for expected in ["dep/dep.go", "dep/go.mod", "app/go.mod"] {
            assert!(files.contains(&root.join(expected)), "missing {expected}: {files:?}");
        }
    }

    #[test]
    fn scan_tracks_vendor_only_when_go_uses_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        write_files(
            &root,
            &[
                ("go.mod", "module example.com/app\n\ngo 1.22\n"),
                ("main.go", "package main\n\nimport _ \"example.com/dep\"\n"),
                ("vendor/example.com/dep/dep.go", "package dep\n"),
            ],
        );
        let files = scan(&[".".into()], Tests::Excluded, Some(&root)).unwrap();
        assert!(
            !files.contains(&root.join("vendor/example.com/dep/dep.go")),
            "{files:?}"
        );

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        write_files(
            &root,
            &[
                ("go.mod", "module example.com/app\n\ngo 1.22\n"),
                ("main.go", "package main\n\nimport _ \"example.com/dep\"\n"),
                (
                    "vendor/modules.txt",
                    "# example.com/dep v1.0.0\n## explicit\nexample.com/dep\n",
                ),
                ("vendor/example.com/dep/dep.go", "package dep\n"),
            ],
        );
        let files = scan(&[".".into()], Tests::Excluded, Some(&root)).unwrap();
        for expected in ["vendor/modules.txt", "vendor/example.com/dep/dep.go"] {
            assert!(files.contains(&root.join(expected)), "missing {expected}: {files:?}");
        }
    }

    #[test]
    fn relative_patterns_resolve_against_the_directory_go_runs_in() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::write(root.join("go.mod"), "module example.com/test\n").unwrap();
        fs::create_dir_all(root.join("svc/cmd")).unwrap();
        fs::write(root.join("svc/main.go"), "package main").unwrap();
        fs::write(root.join("svc/cmd/cmd.go"), "package cmd").unwrap();
        fs::write(root.join("root.go"), "package root").unwrap();

        let files = scan(&[".".into()], Tests::Excluded, Some(&root.join("svc"))).unwrap();
        assert!(files.contains(&root.join("svc/main.go")), "{files:?}");
        assert!(!files.contains(&root.join("root.go")), "{files:?}");
        let mut dirs = resolve_package_dirs(&root, "example.com/test", &root.join("svc"), "./...").unwrap();
        dirs.sort();
        assert_eq!(dirs, [root.join("svc"), root.join("svc/cmd")]);
    }

    #[test]
    fn resolve_absolute_import() {
        let dir = tempfile::tempdir().unwrap();
        let pkg_dir = dir.path().join("internal/cache");
        fs::create_dir_all(&pkg_dir).unwrap();
        fs::write(pkg_dir.join("cache.go"), "package cache").unwrap();
        fs::write(dir.path().join("go.mod"), "module example.com/test\n").unwrap();

        let dirs = resolve_package_dirs(
            dir.path(),
            "example.com/test",
            dir.path(),
            "example.com/test/internal/cache",
        )
        .unwrap();
        assert_eq!(dirs, vec![pkg_dir]);
    }

    #[test]
    fn resolve_recursive_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join("go.mod"), "module example.com/test\n").unwrap();
        fs::create_dir_all(root.join("cmd/app")).unwrap();
        fs::write(root.join("cmd/app/main.go"), "package main").unwrap();
        fs::create_dir_all(root.join("internal/lib")).unwrap();
        fs::write(root.join("internal/lib/lib.go"), "package lib").unwrap();
        fs::create_dir_all(root.join("docs")).unwrap();

        let mut dirs = resolve_package_dirs(root, "example.com/test", root, "./...").unwrap();
        dirs.sort();
        assert_eq!(dirs.len(), 2);
        assert!(dirs.contains(&root.join("cmd/app")));
        assert!(dirs.contains(&root.join("internal/lib")));
    }

    // Each test uses its own temporary module, and caches are keyed by
    // module root, so tests running in parallel share no cache entries.
    fn scan_from(root: &Path, pattern: &str, tests: Tests) -> Result<HashSet<PathBuf>, BoxError> {
        scan(&[pattern.to_owned()], tests, Some(root))
    }

    /// How many package directories the module rooted at `root` has cached.
    fn cached_packages(root: &Path) -> usize {
        MODULES.get(&root.canonicalize().unwrap()).unwrap().packages.len()
    }

    #[test]
    fn scan_follows_local_imports() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join("go.mod"), "module example.com/test\n").unwrap();
        fs::write(root.join("go.sum"), "").unwrap();

        fs::create_dir_all(root.join("cmd/app")).unwrap();
        fs::write(
            root.join("cmd/app/main.go"),
            r#"package main

import "example.com/test/internal/lib"

func main() { lib.Hello() }
"#,
        )
        .unwrap();

        fs::create_dir_all(root.join("internal/lib")).unwrap();
        fs::write(
            root.join("internal/lib/lib.go"),
            r#"package lib

import "fmt"

func Hello() { fmt.Println("hello") }
"#,
        )
        .unwrap();

        let files = scan_from(root, "./cmd/app", Tests::Excluded).unwrap();
        let canon_root = root.canonicalize().unwrap();
        let rel: HashSet<_> = files
            .iter()
            .map(|p| p.strip_prefix(&canon_root).unwrap().to_path_buf())
            .collect();

        assert!(rel.contains(Path::new("cmd/app/main.go")));
        assert!(rel.contains(Path::new("internal/lib/lib.go")));
        assert!(rel.contains(Path::new("go.mod")));
        assert!(rel.contains(Path::new("go.sum")));
        assert_eq!(rel.len(), 4);
    }

    #[test]
    fn scan_tracks_non_go_files_the_go_command_compiles() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        write_files(
            &root,
            &[
                ("go.mod", "module example.com/test\n"),
                ("main.go", "package main\n\nimport _ \"example.com/test/lib\"\n"),
                ("default.pgo", ""),
                ("lib/lib.go", "package lib\n"),
                ("lib/add_amd64.s", ""),
                ("lib/add_arm64.S", ""),
                ("lib/native.c", ""),
                ("lib/native.h", ""),
                ("lib/native.cpp", ""),
                ("lib/bridge.m", ""),
                ("lib/legacy.f90", ""),
                ("lib/wrap.swig", ""),
                ("lib/rsrc_windows.syso", ""),
                ("lib/README.md", ""),
            ],
        );

        let files = scan(&[".".into()], Tests::Excluded, Some(&root)).unwrap();
        for expected in [
            "default.pgo",
            "lib/add_amd64.s",
            "lib/add_arm64.S",
            "lib/native.c",
            "lib/native.h",
            "lib/native.cpp",
            "lib/bridge.m",
            "lib/legacy.f90",
            "lib/wrap.swig",
            "lib/rsrc_windows.syso",
        ] {
            assert!(files.contains(&root.join(expected)), "missing {expected}: {files:?}");
        }
        assert!(!files.contains(&root.join("lib/README.md")), "{files:?}");
    }

    #[test]
    fn scan_excludes_test_files_when_not_requested() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join("go.mod"), "module example.com/test\n").unwrap();
        fs::create_dir_all(root.join("pkg")).unwrap();
        fs::write(root.join("pkg/lib.go"), "package pkg\n").unwrap();
        fs::write(root.join("pkg/lib_test.go"), "package pkg\n").unwrap();

        let without_tests = scan_from(root, "./pkg", Tests::Excluded).unwrap();
        let with_tests = scan_from(root, "./pkg", Tests::Sources).unwrap();

        let canon_root = root.canonicalize().unwrap();
        let rel_without: HashSet<_> = without_tests
            .iter()
            .map(|p| p.strip_prefix(&canon_root).unwrap().to_path_buf())
            .collect();
        let rel_with: HashSet<_> = with_tests
            .iter()
            .map(|p| p.strip_prefix(&canon_root).unwrap().to_path_buf())
            .collect();

        assert!(!rel_without.contains(Path::new("pkg/lib_test.go")));
        assert!(rel_with.contains(Path::new("pkg/lib_test.go")));
    }

    #[test]
    fn scan_includes_testdata_of_tested_packages_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join("go.mod"), "module example.com/test\n").unwrap();
        fs::create_dir_all(root.join("pkg/testdata/nested")).unwrap();
        fs::write(
            root.join("pkg/lib.go"),
            "package pkg\nimport \"example.com/test/dep\"\n",
        )
        .unwrap();
        fs::write(root.join("pkg/testdata/nested/golden.txt"), "golden\n").unwrap();
        fs::create_dir_all(root.join("dep/testdata")).unwrap();
        fs::write(root.join("dep/dep.go"), "package dep\n").unwrap();
        fs::write(root.join("dep/testdata/fixture.txt"), "fixture\n").unwrap();

        let canon_root = root.canonicalize().unwrap();
        let relative = |files: HashSet<PathBuf>| -> HashSet<PathBuf> {
            files
                .iter()
                .map(|p| p.strip_prefix(&canon_root).unwrap().to_path_buf())
                .collect()
        };
        let sources = relative(scan_from(root, "./pkg", Tests::Sources).unwrap());
        let testdata = relative(scan_from(root, "./pkg", Tests::SourcesAndTestdata).unwrap());

        assert!(!sources.contains(Path::new("pkg/testdata/nested/golden.txt")));
        assert!(testdata.contains(Path::new("pkg/testdata/nested/golden.txt")));
        assert!(testdata.contains(Path::new("dep/dep.go")));
        assert!(!testdata.contains(Path::new("dep/testdata/fixture.txt")));
    }

    #[test]
    fn scan_tracks_tests_of_named_package_imported_by_another() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join("go.mod"), "module example.com/test\n").unwrap();
        fs::write(
            root.join("main.go"),
            "package main\nimport \"example.com/test/lib\"\nfunc main() { lib.F() }\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("lib/testdata")).unwrap();
        fs::write(root.join("lib/lib.go"), "package lib\nfunc F() {}\n").unwrap();
        fs::write(root.join("lib/lib_test.go"), "package lib\n").unwrap();
        fs::write(root.join("lib/testdata/golden.txt"), "golden\n").unwrap();

        let canon_root = root.canonicalize().unwrap();
        let files: HashSet<_> = scan_from(root, "./...", Tests::SourcesAndTestdata)
            .unwrap()
            .iter()
            .map(|p| p.strip_prefix(&canon_root).unwrap().to_path_buf())
            .collect();

        assert!(files.contains(Path::new("lib/lib_test.go")), "{files:?}");
        assert!(files.contains(Path::new("lib/testdata/golden.txt")), "{files:?}");
    }

    #[test]
    fn scan_tracks_test_only_references_only_with_tests() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join("go.mod"), "module example.com/test\n").unwrap();
        fs::create_dir_all(root.join("pkg")).unwrap();
        fs::write(root.join("pkg/lib.go"), "package pkg\n").unwrap();
        fs::write(
            root.join("pkg/lib_test.go"),
            "package pkg\nimport \"example.com/test/testutil\"\n//go:embed fixture.txt\nvar fixture string\n",
        )
        .unwrap();
        fs::write(root.join("pkg/fixture.txt"), "fixture\n").unwrap();
        fs::create_dir_all(root.join("testutil")).unwrap();
        fs::write(root.join("testutil/util.go"), "package testutil\n").unwrap();

        let canon_root = root.canonicalize().unwrap();
        let relative = |files: HashSet<PathBuf>| -> HashSet<PathBuf> {
            files
                .iter()
                .map(|p| p.strip_prefix(&canon_root).unwrap().to_path_buf())
                .collect()
        };
        let excluded = relative(scan_from(root, "./pkg", Tests::Excluded).unwrap());
        let sources = relative(scan_from(root, "./pkg", Tests::Sources).unwrap());

        for test_only in ["testutil/util.go", "pkg/fixture.txt"] {
            assert!(!excluded.contains(Path::new(test_only)), "{excluded:?}");
            assert!(sources.contains(Path::new(test_only)), "{sources:?}");
        }
    }

    #[test]
    fn scan_skips_dirs_go_ignores() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join("go.mod"), "module example.com/test\n").unwrap();
        fs::write(root.join("main.go"), "package main\n").unwrap();

        fs::create_dir_all(root.join("nested")).unwrap();
        fs::write(root.join("nested/go.mod"), "module example.com/nested\n").unwrap();
        fs::write(root.join("nested/nested.go"), "package nested\n").unwrap();

        fs::create_dir_all(root.join("_tools")).unwrap();
        fs::write(root.join("_tools/tools.go"), "package tools\n").unwrap();

        fs::create_dir_all(root.join("vendor/github.com/foo")).unwrap();
        fs::write(root.join("vendor/github.com/foo/bar.go"), "package foo\n").unwrap();

        fs::create_dir_all(root.join("testdata")).unwrap();
        fs::write(root.join("testdata/fixture.go"), "package testdata\n").unwrap();

        let dirs = resolve_package_dirs(root, "example.com/test", root, "./...").unwrap();
        let dir_strs: Vec<_> = dirs.iter().map(|d| d.to_string_lossy().to_string()).collect();

        assert_eq!(dirs, [root.to_path_buf()], "{dir_strs:?}");
    }

    #[test]
    fn cache_reuses_shared_deps() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join("go.mod"), "module example.com/test\n").unwrap();
        fs::write(root.join("go.sum"), "").unwrap();

        // Two commands that share a common dependency.
        fs::create_dir_all(root.join("cmd/a")).unwrap();
        fs::write(
            root.join("cmd/a/main.go"),
            "package main\nimport \"example.com/test/internal/shared\"\n",
        )
        .unwrap();

        fs::create_dir_all(root.join("cmd/b")).unwrap();
        fs::write(
            root.join("cmd/b/main.go"),
            "package main\nimport \"example.com/test/internal/shared\"\n",
        )
        .unwrap();

        fs::create_dir_all(root.join("internal/shared")).unwrap();
        fs::write(root.join("internal/shared/lib.go"), "package shared\n").unwrap();

        let files_a = scan_from(root, "./cmd/a", Tests::Excluded).unwrap();
        let files_b = scan_from(root, "./cmd/b", Tests::Excluded).unwrap();
        let cached_count = cached_packages(root);

        let canon_root = root.canonicalize().unwrap();
        let rel_a: HashSet<_> = files_a
            .iter()
            .map(|p| p.strip_prefix(&canon_root).unwrap().to_path_buf())
            .collect();
        let rel_b: HashSet<_> = files_b
            .iter()
            .map(|p| p.strip_prefix(&canon_root).unwrap().to_path_buf())
            .collect();

        // Both should include the shared dep.
        assert!(rel_a.contains(Path::new("internal/shared/lib.go")));
        assert!(rel_b.contains(Path::new("internal/shared/lib.go")));

        // Verify cache was populated: 3 package dirs scanned total.
        assert_eq!(cached_count, 3);
    }

    #[test]
    fn scan_base_dir_selects_correct_module() {
        // Regression: when a block sets `dir = "subpath"` pointing at a
        // separate-module subdirectory, scan() must locate that submodule's
        // `go.mod` rather than the outer module's. Without `base_dir` it
        // walked up from bit's CWD and grabbed the wrong module.
        let dir = tempfile::tempdir().unwrap();
        let outer_root = dir.path();
        fs::write(outer_root.join("go.mod"), "module example.com/outer\n").unwrap();
        fs::write(outer_root.join("outer.go"), "package main\n").unwrap();

        let inner_root = outer_root.join("sub");
        fs::create_dir_all(&inner_root).unwrap();
        fs::write(inner_root.join("go.mod"), "module example.com/inner\n").unwrap();
        fs::write(inner_root.join("inner.go"), "package main\n").unwrap();

        // base_dir = inner_root → scanner finds the inner module's go.mod
        // and returns only inner.go (plus go.mod).
        let inner_files = scan_from(&inner_root, ".", Tests::Excluded).unwrap();
        assert!(
            inner_files.iter().any(|p| p.ends_with("inner.go")),
            "expected inner.go in {inner_files:?}"
        );
        assert!(
            !inner_files.iter().any(|p| p.ends_with("outer.go")),
            "did not expect outer.go in {inner_files:?}"
        );

        // base_dir = outer_root → scanner finds the outer module and skips
        // inner.go (different module).
        let outer_files = scan_from(outer_root, ".", Tests::Excluded).unwrap();
        assert!(
            outer_files.iter().any(|p| p.ends_with("outer.go")),
            "expected outer.go in {outer_files:?}"
        );
        assert!(
            !outer_files.iter().any(|p| p.ends_with("inner.go")),
            "did not expect inner.go in {outer_files:?}"
        );

        // Scanning the outer module did not discard the inner module's cache.
        assert_eq!(cached_packages(&inner_root), 1);
    }

    #[test]
    fn concurrent_scans_agree() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("go.mod"), "module example.com/test\n").unwrap();
        for name in ["a", "b", "c", "d"] {
            fs::create_dir_all(root.join(name)).unwrap();
            fs::write(
                root.join(name).join("lib.go"),
                "package lib\nimport \"example.com/test/shared\"\n",
            )
            .unwrap();
        }
        fs::create_dir_all(root.join("shared")).unwrap();
        fs::write(root.join("shared/shared.go"), "package shared\n").unwrap();

        let results: Vec<_> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| scan_from(root, "./...", Tests::Excluded).unwrap()))
                .collect();
            handles.into_iter().map(|handle| handle.join().unwrap()).collect()
        });
        assert_eq!(results[0].len(), 6, "five sources and go.mod: {:?}", results[0]);
        assert!(results.iter().all(|files| *files == results[0]));
        assert_eq!(cached_packages(root), 5);
    }

    #[test]
    fn patterns_name_each_package_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("go.mod"), "module example.com/test\n").unwrap();
        for name in ["a", "b", "c"] {
            fs::create_dir_all(root.join(name)).unwrap();
            fs::write(root.join(name).join("lib.go"), "package lib\n").unwrap();
        }

        let patterns = ["./a".to_owned(), "./b".to_owned(), "./a".to_owned()];
        let mut packages: Vec<_> = tested_packages(&patterns, Some(root))
            .unwrap()
            .into_iter()
            .map(|p| p.import_path)
            .collect();
        packages.sort();
        assert_eq!(packages, ["example.com/test/a", "example.com/test/b"]);

        let files = scan(&patterns, Tests::Excluded, Some(root)).unwrap();
        assert!(files.iter().any(|p| p.ends_with("b/lib.go")), "{files:?}");
        assert!(!files.iter().any(|p| p.ends_with("c/lib.go")), "{files:?}");
    }
}
