//! What buf reads for an input, found the way buf finds it.
//!
//! For a local directory, buf walks up to the filesystem root looking for
//! the workspace that controls it: a v2 `buf.yaml` whose modules contain or
//! lie within the directory, or a `buf.work.yaml` whose directories contain
//! it. Every module of that workspace is compiled, because modules import
//! each other. Without one, the nearest v1 `buf.yaml` within buf's bucket
//! marks the module, and otherwise the directory is the module.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

use crate::file_tracker::FileTracker;
use crate::provider::BoxError;
use crate::sha256::SHA256;

/// Module and workspace configuration names, current first, as buf reads them.
const BUF_YAML: [&str; 2] = ["buf.yaml", "buf.mod"];
const BUF_WORK_YAML: [&str; 2] = ["buf.work.yaml", "buf.work"];
const BUF_LOCK: &str = "buf.lock";

#[derive(Deserialize)]
struct BufYaml {
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    modules: Vec<BufYamlModule>,
}

#[derive(Deserialize)]
struct BufYamlModule {
    path: String,
}

#[derive(Deserialize)]
struct BufWorkYaml {
    #[serde(default)]
    directories: Vec<String>,
}

/// The `.proto` sources and configuration buf reads for a set of inputs.
#[derive(Debug, Default)]
pub(super) struct Sources {
    /// Directories whose `.proto` files buf compiles.
    proto_roots: Vec<PathBuf>,
    /// Files buf reads, hashed when they exist.
    files: Vec<PathBuf>,
    /// Config files named by `--config`, which buf reads as it would a
    /// `buf.yaml`.
    configs: Vec<PathBuf>,
}

impl Sources {
    /// Add a buf input, relative to `dir`. A directory or `.proto` file is
    /// followed to its workspace, and another local file, such as an image
    /// or archive, is read as is. Git, BSR, and remote inputs have no local
    /// sources.
    pub(super) fn add_input(&mut self, dir: Option<&str>, input: &str) -> Result<(), BoxError> {
        // `path#key=value,...` options select a format or subdirectory.
        let path = input.split_once('#').map_or(input, |(path, _)| path);
        if path.trim_end_matches('/').ends_with(".git") {
            return Ok(());
        }
        let cwd = absolute(&super::super::in_dir(dir, ""))?;
        let path = absolute(&super::super::in_dir(dir, path))?;
        if path.is_dir() {
            self.add_dir(&cwd, &path);
        } else if path.is_file() && path.extension().is_some_and(|ext| ext == "proto") {
            let parent = path.parent().expect("an absolute file path has a parent");
            self.add_dir(&cwd, parent);
        } else if path.is_file() {
            self.files.push(path);
        }
        Ok(())
    }

    pub(super) fn add_file(&mut self, path: PathBuf) {
        self.files.push(path);
    }

    /// Add a config file named by `--config`.
    pub(super) fn add_config(&mut self, path: PathBuf) {
        self.files.push(path.clone());
        self.configs.push(path);
    }

    /// The module and workspace `buf.yaml` files found, and any `--config`
    /// file.
    pub(super) fn buf_yamls(&self) -> impl Iterator<Item = &Path> {
        self.files
            .iter()
            .filter(|path| path.file_name().is_some_and(|name| name == BUF_YAML[0]))
            .chain(&self.configs)
            .map(PathBuf::as_path)
    }

    pub(super) fn hash(&self, tracker: &FileTracker) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let mut hashes = super::super::hash_protos(&self.proto_roots, tracker)?;
        let files: Vec<PathBuf> = self.files.iter().filter(|path| path.is_file()).cloned().collect();
        hashes.extend(tracker.hash_files(&files)?);
        Ok(hashes)
    }

    /// Add the module or workspace buf builds for the directory `input`,
    /// with buf run in `cwd`.
    fn add_dir(&mut self, cwd: &Path, input: &Path) {
        if input
            .ancestors()
            .any(|prefix| self.add_controlling_workspace(prefix, input))
        {
            return;
        }
        // Without a workspace, buf's bucket is `cwd` when the input lies
        // within it and the input itself otherwise, and the search for a v1
        // module stops at the bucket's root.
        let bucket = if input.starts_with(cwd) { cwd } else { input };
        let module = input
            .ancestors()
            .take_while(|prefix| prefix.starts_with(bucket))
            .find(|prefix| find(prefix, &BUF_YAML).is_some_and(|path| !is_v2(&path)));
        match module {
            Some(module) => self.add_module(module),
            None => self.proto_roots.push(input.to_path_buf()),
        }
    }

    /// Add the workspace at `prefix` if it controls `input`.
    fn add_controlling_workspace(&mut self, prefix: &Path, input: &Path) -> bool {
        let relative = input.strip_prefix(prefix).expect("prefix is an ancestor of input");
        if let Some(path) = find(prefix, &BUF_YAML)
            && let Some(config) = read::<BufYaml>(&path)
            && config.version.as_deref() == Some("v2")
        {
            let modules: Vec<PathBuf> = if config.modules.is_empty() {
                vec![PathBuf::new()]
            } else {
                config.modules.iter().map(|module| clean(&module.path)).collect()
            };
            // A v2 input may also be a directory holding several modules.
            if modules
                .iter()
                .any(|module| relative.starts_with(module) || module.starts_with(relative))
            {
                self.proto_roots
                    .extend(modules.iter().map(|module| prefix.join(module)));
                self.files.extend([path, prefix.join(BUF_LOCK)]);
                return true;
            }
        }
        if let Some(path) = find(prefix, &BUF_WORK_YAML)
            && let Some(config) = read::<BufWorkYaml>(&path)
        {
            let directories: Vec<PathBuf> = config.directories.iter().map(|dir| clean(dir)).collect();
            if relative.as_os_str().is_empty() || directories.iter().any(|dir| relative.starts_with(dir)) {
                self.files.push(path);
                for directory in directories {
                    self.add_module(&prefix.join(directory));
                }
                return true;
            }
        }
        false
    }

    /// Add a v1 module: its sources, `buf.yaml`, and `buf.lock`.
    fn add_module(&mut self, module: &Path) {
        self.proto_roots.push(module.to_path_buf());
        self.files.extend(BUF_YAML.iter().map(|name| module.join(name)));
        self.files.push(module.join(BUF_LOCK));
    }
}

/// The first of `names` that exists in `dir`.
fn find(dir: &Path, names: &[&str]) -> Option<PathBuf> {
    names.iter().map(|name| dir.join(name)).find(|path| path.is_file())
}

/// Parse a configuration file. One that cannot be read or parsed is treated
/// as absent here, and the buf command reports the error itself.
fn read<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    serde_yml::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn is_v2(path: &Path) -> bool {
    read::<BufYaml>(path).is_some_and(|config| config.version.as_deref() == Some("v2"))
}

/// A relative configuration path without `.` components, so that `.` is
/// empty and contains every path.
fn clean(path: &str) -> PathBuf {
    Path::new(path)
        .components()
        .filter(|c| *c != Component::CurDir)
        .collect()
}

/// `path` made absolute and lexically cleaned, as buf's `filepath.Abs` does,
/// so that walking up its ancestors walks up the directories buf sees.
fn absolute(path: &Path) -> Result<PathBuf, BoxError> {
    let path = if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    };
    let mut cleaned = PathBuf::new();
    for component in std::path::absolute(path)?.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                cleaned.pop();
            }
            component => cleaned.push(component),
        }
    }
    Ok(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, files: &[(&str, &str)]) {
        for (path, content) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
    }

    /// The files hashed for `input` run in `dir`, relative to `root`.
    fn hashed(root: &Path, dir: &str, input: &str) -> Vec<String> {
        let mut sources = Sources::default();
        let dir = root.join(dir).to_string_lossy().into_owned();
        sources.add_input(Some(&dir), input).unwrap();
        let root = absolute(root).unwrap();
        let mut names: Vec<String> = sources
            .hash(&FileTracker::default())
            .unwrap()
            .keys()
            .map(|path| {
                Path::new(path)
                    .strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn v2_workspace_above_the_input_controls_it() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            &[
                (
                    "buf.yaml",
                    "version: v2\nmodules:\n  - path: proto/a\n  - path: proto/b\n",
                ),
                ("buf.lock", ""),
                ("proto/a/a.proto", ""),
                ("proto/b/b.proto", ""),
                ("other/c.proto", ""),
            ],
        );
        let workspace = ["buf.lock", "buf.yaml", "proto/a/a.proto", "proto/b/b.proto"];
        assert_eq!(hashed(dir.path(), "proto/a", "."), workspace);
        assert_eq!(hashed(dir.path(), "", "proto"), workspace);
        assert_eq!(hashed(dir.path(), "", "proto/a/a.proto"), workspace);
    }

    #[test]
    fn v2_workspace_without_modules_is_one_module() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            &[
                ("buf.yaml", "version: v2\n"),
                ("proto/a.proto", ""),
                ("other/b.proto", ""),
            ],
        );
        assert_eq!(
            hashed(dir.path(), "", "proto"),
            ["buf.yaml", "other/b.proto", "proto/a.proto"]
        );
    }

    #[test]
    fn v2_workspace_that_does_not_contain_the_input_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            &[
                ("buf.yaml", "version: v2\nmodules:\n  - path: proto\n"),
                ("proto/a.proto", ""),
                ("other/b.proto", ""),
            ],
        );
        assert_eq!(hashed(dir.path(), "", "other"), ["other/b.proto"]);
    }

    #[test]
    fn v1_workspace_reads_each_module_config() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            &[
                ("buf.work.yaml", "version: v1\ndirectories:\n  - a\n  - b\n"),
                ("a/buf.yaml", "version: v1\n"),
                ("a/buf.lock", ""),
                ("a/a.proto", ""),
                ("b/b.proto", ""),
            ],
        );
        assert_eq!(
            hashed(dir.path(), "", "a"),
            ["a/a.proto", "a/buf.lock", "a/buf.yaml", "b/b.proto", "buf.work.yaml"]
        );
    }

    #[test]
    fn v1_module_is_found_within_the_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            &[("buf.yaml", "version: v1\n"), ("proto/a.proto", ""), ("b.proto", "")],
        );
        assert_eq!(
            hashed(dir.path(), "", "proto"),
            ["b.proto", "buf.yaml", "proto/a.proto"]
        );
        // Above the working directory, buf no longer sees the module.
        assert_eq!(hashed(dir.path(), "proto", "."), ["proto/a.proto"]);
    }

    #[test]
    fn local_files_are_hashed_and_remote_inputs_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), &[("image.binpb", ""), ("a.proto", "")]);
        assert_eq!(hashed(dir.path(), "", "image.binpb#format=binpb"), ["image.binpb"]);
        assert!(hashed(dir.path(), "", ".git#branch=main").is_empty());
        assert!(hashed(dir.path(), "", "buf.build/acme/api").is_empty());
    }
}
