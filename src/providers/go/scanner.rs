use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::provider::BoxError;

/// Cached per-directory scan result: direct files and local import directories.
struct PackageScan {
    /// .go source files in this directory (non-test).
    go_files: HashSet<PathBuf>,
    /// .go test files in this directory.
    test_files: HashSet<PathBuf>,
    /// Embedded files referenced by //go:embed directives.
    embed_files: HashSet<PathBuf>,
    /// Directories of local (same-module) imports.
    local_import_dirs: HashSet<PathBuf>,
}

/// Module-level cache shared across all go resource resolve() calls.
struct ModuleCache {
    module_root: PathBuf,
    module_path: String,
    /// Per-directory scan results, keyed by canonical path.
    packages: HashMap<PathBuf, PackageScan>,
}

static CACHE: Mutex<Option<ModuleCache>> = Mutex::new(None);

/// Get or initialize the module cache, starting the search from `start_dir`.
fn with_cache<F, R>(start_dir: &Path, f: F) -> Result<R, BoxError>
where
    F: FnOnce(&mut ModuleCache) -> Result<R, BoxError>,
{
    let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let module_root = find_module_root(start_dir)?;

    // Invalidate cache if module root changed (different project).
    let needs_init = match &*guard {
        Some(cache) => cache.module_root != module_root,
        None => true,
    };

    if needs_init {
        let content = fs::read_to_string(module_root.join("go.mod"))?;
        let module_path = parse_module_path(&content)?;
        *guard = Some(ModuleCache {
            module_root,
            module_path,
            packages: HashMap::new(),
        });
    }

    f(guard.as_mut().expect("cache initialized above"))
}

/// Scan a single package directory, returning cached results if available.
fn scan_package_dir(cache: &mut ModuleCache, pkg_dir: &Path) -> Result<(), BoxError> {
    let canonical = pkg_dir.canonicalize().unwrap_or_else(|_| pkg_dir.to_path_buf());
    if cache.packages.contains_key(&canonical) {
        return Ok(());
    }

    let entries = fs::read_dir(pkg_dir).map_err(|e| format!("reading {}: {e}", pkg_dir.display()))?;

    let mut go_files = HashSet::new();
    let mut test_files = HashSet::new();
    let mut embed_files = HashSet::new();
    let mut local_import_dirs = HashSet::new();

    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !name_str.ends_with(".go") {
            continue;
        }

        let path = entry.path();
        let source =
            fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;

        if name_str.ends_with("_test.go") {
            test_files.insert(path);
        } else {
            go_files.insert(path);
        }

        for imp in parse_imports(&source) {
            if is_stdlib(&imp) {
                continue;
            }
            if let Some(rel) = imp.strip_prefix(&cache.module_path) {
                let rel = rel.strip_prefix('/').unwrap_or(rel);
                let dep_dir = if rel.is_empty() {
                    cache.module_root.clone()
                } else {
                    cache.module_root.join(rel)
                };
                if dep_dir.is_dir() {
                    local_import_dirs.insert(dep_dir);
                }
            }
        }

        let embeds = parse_embeds(&source);
        if !embeds.is_empty() {
            for f in expand_embeds(pkg_dir, &embeds) {
                embed_files.insert(f);
            }
        }
    }

    cache.packages.insert(canonical, PackageScan {
        go_files,
        test_files,
        embed_files,
        local_import_dirs,
    });

    Ok(())
}

/// Collect all files transitively from a package directory using the cache.
fn collect_transitive(
    cache: &mut ModuleCache,
    pkg_dir: &Path,
    include_tests: bool,
    files: &mut HashSet<PathBuf>,
    visited: &mut HashSet<PathBuf>,
) -> Result<(), BoxError> {
    let canonical = pkg_dir.canonicalize().unwrap_or_else(|_| pkg_dir.to_path_buf());
    if !visited.insert(canonical.clone()) {
        return Ok(());
    }

    scan_package_dir(cache, pkg_dir)?;

    // Collect files and import dirs from the cached scan.
    let (local_files, dep_dirs): (HashSet<PathBuf>, Vec<PathBuf>) = {
        let scan = cache.packages.get(&canonical).expect("just scanned");
        let mut f = scan.go_files.clone();
        f.extend(scan.embed_files.iter().cloned());
        if include_tests {
            f.extend(scan.test_files.iter().cloned());
        }
        (f, scan.local_import_dirs.iter().cloned().collect())
    };

    files.extend(local_files);

    // Recurse into deps (never with tests).
    for dep_dir in dep_dirs {
        collect_transitive(cache, &dep_dir, false, files, visited)?;
    }

    Ok(())
}

/// Scan a Go package pattern and return all source files (local deps, embeds, go.mod/go.sum).
///
/// Results are cached per package directory so that multiple blocks sharing
/// transitive imports only scan each directory once.
pub fn scan(pattern: &str, include_tests: bool) -> Result<HashSet<PathBuf>, BoxError> {
    let cwd = std::env::current_dir()?;
    with_cache(&cwd, |cache| {
        let pkg_dirs = resolve_package_dirs(&cache.module_root, &cache.module_path, pattern)?;

        let mut all_files = HashSet::new();
        let mut visited = HashSet::new();

        for dir in &pkg_dirs {
            collect_transitive(cache, dir, include_tests, &mut all_files, &mut visited)?;
        }

        let go_mod = cache.module_root.join("go.mod");
        let go_sum = cache.module_root.join("go.sum");
        if go_mod.exists() {
            all_files.insert(go_mod);
        }
        if go_sum.exists() {
            all_files.insert(go_sum);
        }

        Ok(all_files)
    })
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

/// Parse `//go:embed` directives from a `.go` file, returning glob patterns.
fn parse_embeds(source: &str) -> Vec<String> {
    let mut patterns = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("//go:embed") {
            for pat in rest.split_whitespace() {
                let pat = pat.strip_prefix("all:").unwrap_or(pat);
                patterns.push(pat.to_owned());
            }
        }
    }
    patterns
}

/// Expand embed patterns relative to a package directory into file paths.
fn expand_embeds(pkg_dir: &Path, patterns: &[String]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for pat in patterns {
        let full = pkg_dir.join(pat);
        if full.is_dir() {
            if let Ok(entries) = glob::glob(&format!("{}/**/*", full.display())) {
                for entry in entries.flatten() {
                    if entry.is_file() {
                        files.push(entry);
                    }
                }
            }
        } else if let Ok(entries) = glob::glob(&full.to_string_lossy()) {
            for entry in entries.flatten() {
                if entry.is_file() {
                    files.push(entry);
                }
            }
        }
    }
    files
}

/// Resolve a package pattern to a list of package directories.
fn resolve_package_dirs(
    module_root: &Path,
    module_path: &str,
    pattern: &str,
) -> Result<Vec<PathBuf>, BoxError> {
    if let Some(base) = pattern.strip_suffix("/...") {
        let base_dir = if base == "." {
            module_root.to_path_buf()
        } else {
            module_root.join(base.strip_prefix("./").unwrap_or(base))
        };
        return find_go_package_dirs(&base_dir);
    }

    if pattern.starts_with("./") || pattern == "." {
        let rel = pattern.strip_prefix("./").unwrap_or("");
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
            if !name_str.starts_with('.') && name_str != "vendor" && name_str != "testdata" {
                subdirs.push(entry.path());
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
        assert_eq!(
            parse_imports(source),
            vec!["fmt", "github.com/foo/bar"]
        );
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

        let dirs = resolve_package_dirs(dir.path(), "example.com/test", "./cmd/app").unwrap();
        assert_eq!(dirs, vec![cmd_dir]);
    }

    #[test]
    fn resolve_absolute_import() {
        let dir = tempfile::tempdir().unwrap();
        let pkg_dir = dir.path().join("internal/cache");
        fs::create_dir_all(&pkg_dir).unwrap();
        fs::write(pkg_dir.join("cache.go"), "package cache").unwrap();
        fs::write(dir.path().join("go.mod"), "module example.com/test\n").unwrap();

        let dirs =
            resolve_package_dirs(dir.path(), "example.com/test", "example.com/test/internal/cache")
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

        let mut dirs = resolve_package_dirs(root, "example.com/test", "./...").unwrap();
        dirs.sort();
        assert_eq!(dirs.len(), 2);
        assert!(dirs.contains(&root.join("cmd/app")));
        assert!(dirs.contains(&root.join("internal/lib")));
    }

    /// Clear the global cache between tests to avoid cross-test contamination.
    fn clear_cache() {
        *CACHE.lock().unwrap() = None;
    }

    fn scan_from(root: &Path, pattern: &str, include_tests: bool) -> Result<HashSet<PathBuf>, BoxError> {
        with_cache(root, |cache| {
            let pkg_dirs = resolve_package_dirs(&cache.module_root, &cache.module_path, pattern)?;
            let mut all_files = HashSet::new();
            let mut visited = HashSet::new();
            for dir in &pkg_dirs {
                collect_transitive(cache, dir, include_tests, &mut all_files, &mut visited)?;
            }
            let go_mod = cache.module_root.join("go.mod");
            let go_sum = cache.module_root.join("go.sum");
            if go_mod.exists() {
                all_files.insert(go_mod);
            }
            if go_sum.exists() {
                all_files.insert(go_sum);
            }
            Ok(all_files)
        })
    }

    fn scan_from_cached_count(root: &Path, pattern: &str, include_tests: bool) -> Result<(HashSet<PathBuf>, usize), BoxError> {
        with_cache(root, |cache| {
            let pkg_dirs = resolve_package_dirs(&cache.module_root, &cache.module_path, pattern)?;
            let mut all_files = HashSet::new();
            let mut visited = HashSet::new();
            for dir in &pkg_dirs {
                collect_transitive(cache, dir, include_tests, &mut all_files, &mut visited)?;
            }
            let go_mod = cache.module_root.join("go.mod");
            let go_sum = cache.module_root.join("go.sum");
            if go_mod.exists() {
                all_files.insert(go_mod);
            }
            if go_sum.exists() {
                all_files.insert(go_sum);
            }
            let cached = cache.packages.len();
            Ok((all_files, cached))
        })
    }

    #[test]
    fn scan_follows_local_imports() {
        clear_cache();
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

        let files = scan_from(root, "./cmd/app", false).unwrap();
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
    fn scan_excludes_test_files_when_not_requested() {
        clear_cache();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join("go.mod"), "module example.com/test\n").unwrap();
        fs::create_dir_all(root.join("pkg")).unwrap();
        fs::write(root.join("pkg/lib.go"), "package pkg\n").unwrap();
        fs::write(root.join("pkg/lib_test.go"), "package pkg\n").unwrap();

        let without_tests = scan_from(root, "./pkg", false).unwrap();
        clear_cache();
        let with_tests = scan_from(root, "./pkg", true).unwrap();

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
    fn scan_skips_vendor_and_testdata() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join("go.mod"), "module example.com/test\n").unwrap();
        fs::write(root.join("main.go"), "package main\n").unwrap();

        fs::create_dir_all(root.join("vendor/github.com/foo")).unwrap();
        fs::write(
            root.join("vendor/github.com/foo/bar.go"),
            "package foo\n",
        )
        .unwrap();

        fs::create_dir_all(root.join("testdata")).unwrap();
        fs::write(root.join("testdata/fixture.go"), "package testdata\n").unwrap();

        let dirs = resolve_package_dirs(root, "example.com/test", "./...").unwrap();
        let dir_strs: Vec<_> = dirs.iter().map(|d| d.to_string_lossy().to_string()).collect();

        assert_eq!(dirs.len(), 1);
        assert!(!dir_strs.iter().any(|d| d.contains("vendor")));
        assert!(!dir_strs.iter().any(|d| d.contains("testdata")));
    }

    #[test]
    fn cache_reuses_shared_deps() {
        clear_cache();
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

        let files_a = scan_from(root, "./cmd/a", false).unwrap();
        let (files_b, cached_count) = scan_from_cached_count(root, "./cmd/b", false).unwrap();

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
}
