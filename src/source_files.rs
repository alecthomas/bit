//! Listing the source files of directories: the files Git tracks or would
//! track, or every file outside a Git work tree.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::process::CommandExt;

/// The most pathspec bytes passed to one `git ls-files`, well below the
/// argument limits of the platforms bit runs on. `ls-files` cannot read
/// pathspecs from standard input, so a longer list is split across calls.
const PATHSPEC_BYTES: usize = 64 * 1024;

/// Directory listings shared by every target in a run.
///
/// Each `git ls-files` reads the whole index, so in a large repository the
/// cost is in the number of calls, not in how many directories each names.
/// Listing is therefore batched per work tree: the directories of one
/// request share a call, and requests from other targets that arrive while
/// a call runs share the next one.
#[derive(Default)]
pub(crate) struct SourceFiles {
    state: Mutex<State>,
    /// `git ls-files` calls made, so tests can check they are batched.
    #[cfg(test)]
    calls: std::sync::atomic::AtomicUsize,
}

#[derive(Default)]
struct State {
    /// Advanced by [`SourceFiles::invalidate`]. A listing started under an
    /// earlier generation may predate files a target wrote, so it is dropped.
    generation: u64,
    /// Sorted files of each listed directory, by the work tree the directory
    /// belongs to, `None` outside Git. A directory is answered from a listed
    /// ancestor only within the same work tree, since an outer repository's
    /// listing leaves out the files of a repository nested in it.
    listed: HashMap<Option<PathBuf>, BTreeMap<PathBuf, Arc<[PathBuf]>>>,
    /// Directories waiting to be listed, by work tree. Not cleared on
    /// invalidation: a queued directory is listed after it anyway.
    queues: HashMap<PathBuf, Queue>,
}

#[derive(Default)]
struct Queue {
    dirs: BTreeSet<PathBuf>,
    /// Held while the work tree is listed. Whoever holds it lists every
    /// queued directory, so requests that queue while a listing runs are
    /// served by one call instead of one each.
    listing: Arc<Mutex<()>>,
}

impl SourceFiles {
    /// The files under each of `dirs`, in the same order: those Git tracks
    /// or would track, or every file but those in `.git` directories outside
    /// a Git work tree, such as a path dependency elsewhere. A directory is
    /// in the work tree of the nearest of its ancestors holding `.git`.
    pub(crate) fn list(&self, dirs: &[PathBuf]) -> Vec<Vec<PathBuf>> {
        let dirs: Vec<(Option<PathBuf>, PathBuf)> = dirs
            .iter()
            .map(|dir| {
                let dir = std::path::absolute(dir).unwrap_or_else(|_| dir.clone());
                (work_tree(&dir), dir)
            })
            .collect();
        // A listing is dropped if the cache is invalidated while it runs,
        // so this repeats until every directory is answered at once.
        loop {
            let mut missing: BTreeMap<Option<&Path>, Vec<PathBuf>> = BTreeMap::new();
            {
                let state = self.lock();
                let answers: Vec<Option<Vec<PathBuf>>> = dirs
                    .iter()
                    .map(|(tree, dir)| state.find(tree.as_deref(), dir))
                    .collect();
                if answers.iter().all(Option::is_some) {
                    return answers.into_iter().flatten().collect();
                }
                for ((tree, dir), answer) in dirs.iter().zip(answers) {
                    if answer.is_none() {
                        missing.entry(tree.as_deref()).or_default().push(dir.clone());
                    }
                }
            }
            for (tree, dirs) in missing {
                match tree {
                    Some(tree) => self.list_in_work_tree(tree, dirs),
                    None => self.walk(dirs),
                }
            }
        }
    }

    /// Drop every listing, because a target may have added or removed files
    /// since they were made.
    pub(crate) fn invalidate(&self) {
        let mut state = self.lock();
        state.generation += 1;
        state.listed.clear();
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// List `dirs` in `tree` together with every directory other requests
    /// have queued for it, unless a listing that ran meanwhile covered them.
    fn list_in_work_tree(&self, tree: &Path, dirs: Vec<PathBuf>) {
        let listing = {
            let mut state = self.lock();
            let queue = state.queues.entry(tree.to_path_buf()).or_default();
            queue.dirs.extend(dirs.iter().cloned());
            Arc::clone(&queue.listing)
        };
        let _listing = listing.lock().unwrap_or_else(PoisonError::into_inner);
        let (generation, batch) = {
            let mut state = self.lock();
            if dirs.iter().all(|dir| state.find(Some(tree), dir).is_some()) {
                return;
            }
            let queued = state
                .queues
                .get_mut(tree)
                .map(|queue| std::mem::take(&mut queue.dirs))
                .unwrap_or_default();
            // Another request may have taken this one's directories from
            // the queue and then lost its listing to an invalidation.
            let batch: Vec<PathBuf> = queued
                .into_iter()
                .chain(dirs)
                .filter(|dir| state.find(Some(tree), dir).is_none())
                .collect();
            (state.generation, outermost(batch))
        };
        let files = self.git_ls_files(tree, &batch).unwrap_or_else(|| {
            let mut files = Vec::new();
            for dir in &batch {
                walk_files(dir, &mut files);
            }
            files.sort();
            files
        });
        self.store(generation, Some(tree), &batch, &files);
    }

    /// List `dirs`, which are outside any Git work tree, by walking them.
    fn walk(&self, dirs: Vec<PathBuf>) {
        let generation = self.lock().generation;
        let dirs = outermost(dirs);
        let mut files = Vec::new();
        for dir in &dirs {
            walk_files(dir, &mut files);
        }
        files.sort();
        self.store(generation, None, &dirs, &files);
    }

    /// Record the `files` of `dirs`, which do not nest, unless the cache was
    /// invalidated since `generation`. `files` is sorted.
    fn store(&self, generation: u64, tree: Option<&Path>, dirs: &[PathBuf], files: &[PathBuf]) {
        let mut state = self.lock();
        if state.generation != generation {
            return;
        }
        let listed = state.listed.entry(tree.map(Path::to_path_buf)).or_default();
        for dir in dirs {
            listed.insert(dir.clone(), files_under(files, dir).into());
        }
    }

    /// The sorted files under `dirs` that Git tracks or would track, from as
    /// few `git ls-files` calls as the argument limit allows, or `None` if
    /// Git fails, such as for a repository it does not trust.
    fn git_ls_files(&self, tree: &Path, dirs: &[PathBuf]) -> Option<Vec<PathBuf>> {
        let pathspecs: Vec<&OsStr> = dirs
            .iter()
            .map(|dir| match dir.strip_prefix(tree) {
                Ok(relative) if relative.as_os_str().is_empty() => OsStr::new("."),
                Ok(relative) => relative.as_os_str(),
                Err(_) => dir.as_os_str(),
            })
            .collect();
        let mut files = Vec::new();
        for chunk in chunks(&pathspecs) {
            #[cfg(test)]
            self.calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let output = crate::git::command()
                // Directory names are paths, not patterns: `a[b]` names itself.
                .arg("--literal-pathspecs")
                .args(["ls-files", "-z", "--cached", "--others", "--exclude-standard", "--"])
                .args(chunk)
                .current_dir(tree)
                .stderr(Stdio::null())
                .logged_output()
                .ok()?;
            if !output.status.success() {
                return None;
            }
            files.extend(
                output
                    .stdout
                    .split(|byte| *byte == 0)
                    .filter(|path| !path.is_empty())
                    .map(|path| tree.join(String::from_utf8_lossy(path).as_ref()))
                    // A tracked file deleted from the work tree is still listed.
                    .filter(|path| path.is_file()),
            );
        }
        files.sort();
        files.dedup();
        Some(files)
    }
}

impl State {
    /// The files under `dir`, from its own listing or a listed ancestor's
    /// in the same work tree.
    fn find(&self, tree: Option<&Path>, dir: &Path) -> Option<Vec<PathBuf>> {
        let listed = self.listed.get(&tree.map(Path::to_path_buf))?;
        let files = dir.ancestors().find_map(|ancestor| listed.get(ancestor))?;
        Some(files_under(files, dir).to_vec())
    }
}

/// The work tree `dir` is in: the nearest ancestor holding `.git`, a
/// directory or, for a worktree or submodule, a file.
fn work_tree(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .map(Path::to_path_buf)
}

/// `dirs` without duplicates or any directory inside another of them.
fn outermost(mut dirs: Vec<PathBuf>) -> Vec<PathBuf> {
    dirs.sort();
    dirs.dedup();
    let mut kept: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        // Paths order by component, so a directory's descendants follow it
        // directly.
        if !kept.last().is_some_and(|last| dir.starts_with(last)) {
            kept.push(dir);
        }
    }
    kept
}

/// The files of the sorted `files` that are under `dir`. Paths order by
/// component, so they are one contiguous run.
fn files_under<'a>(files: &'a [PathBuf], dir: &Path) -> &'a [PathBuf] {
    let start = files.partition_point(|file| file.as_path() < dir);
    let len = files[start..].iter().take_while(|file| file.starts_with(dir)).count();
    &files[start..start + len]
}

/// `pathspecs` split so no chunk exceeds [`PATHSPEC_BYTES`], though a
/// single longer pathspec gets a chunk of its own.
fn chunks<'a>(pathspecs: &'a [&'a OsStr]) -> Vec<&'a [&'a OsStr]> {
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut bytes = 0;
    for (index, pathspec) in pathspecs.iter().enumerate() {
        let len = pathspec.len() + 1;
        if index > start && bytes + len > PATHSPEC_BYTES {
            chunks.push(&pathspecs[start..index]);
            start = index;
            bytes = 0;
        }
        bytes += len;
    }
    if start < pathspecs.len() {
        chunks.push(&pathspecs[start..]);
    }
    chunks
}

fn walk_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(kind) if kind.is_dir() && entry.file_name() != ".git" => walk_files(&path, files),
            Ok(_) if path.is_file() => files.push(path),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::Ordering;

    use super::*;

    fn write(root: &Path, files: &[&str]) {
        for file in files {
            let path = root.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "").unwrap();
        }
    }

    fn git_init(dir: &Path) {
        let status = crate::git::command()
            .args(["init", "-q"])
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success());
    }

    /// The listed files of each directory, relative to `root`.
    fn relative(root: &Path, listed: Vec<Vec<PathBuf>>) -> Vec<Vec<String>> {
        listed
            .into_iter()
            .map(|files| {
                files
                    .iter()
                    .map(|file| file.strip_prefix(root).unwrap().to_string_lossy().into_owned())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn one_call_lists_every_directory_of_a_request() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git_init(root);
        write(
            root,
            &[
                ".gitignore",
                "a/one",
                "a/b/two",
                "a/ignored",
                "c[d]/three",
                "e/four",
                "f/five",
            ],
        );
        fs::write(root.join(".gitignore"), "ignored\n").unwrap();
        let sources = SourceFiles::default();

        let listed = sources.list(&[root.join("a"), root.join("a/b"), root.join("c[d]"), root.join("e")]);

        assert_eq!(
            relative(root, listed),
            [
                vec!["a/b/two", "a/one"],
                vec!["a/b/two"],
                vec!["c[d]/three"],
                vec!["e/four"],
            ]
        );
        assert_eq!(sources.calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn listings_are_reused_until_invalidated() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git_init(root);
        write(root, &["a/b/one"]);
        let sources = SourceFiles::default();
        sources.list(&[root.join("a")]);
        write(root, &["a/b/two"]);

        assert_eq!(relative(root, sources.list(&[root.join("a/b")])), [vec!["a/b/one"]]);
        assert_eq!(sources.calls.load(Ordering::Relaxed), 1);

        sources.invalidate();
        assert_eq!(
            relative(root, sources.list(&[root.join("a/b")])),
            [vec!["a/b/one", "a/b/two"]]
        );
    }

    #[test]
    fn requests_queued_during_a_listing_share_the_next() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        git_init(&root);
        let names = ["a", "b", "c", "d"];
        for name in names {
            write(&root, &[&format!("{name}/file")]);
        }
        let sources = SourceFiles::default();
        // Stand in for a listing in progress, so every request queues.
        let listing = Arc::clone(&sources.lock().queues.entry(root.clone()).or_default().listing);
        let running = listing.lock().unwrap();

        std::thread::scope(|scope| {
            let (sources, root) = (&sources, &root);
            let requests: Vec<_> = names
                .iter()
                .map(|name| scope.spawn(move || sources.list(&[root.join(name)])))
                .collect();
            while sources.lock().queues[root].dirs.len() < names.len() {
                std::thread::yield_now();
            }
            drop(running);
            for (request, name) in requests.into_iter().zip(names) {
                assert_eq!(relative(root, request.join().unwrap()), [vec![format!("{name}/file")]]);
            }
        });
        assert_eq!(sources.calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_nested_repository_is_listed_by_its_own_work_tree() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git_init(root);
        write(root, &["outer", "inner/.gitignore", "inner/file", "inner/ignored"]);
        fs::write(root.join("inner/.gitignore"), "ignored\n").unwrap();
        git_init(&root.join("inner"));

        let listed = SourceFiles::default().list(&[root.to_path_buf(), root.join("inner")]);

        assert_eq!(
            relative(root, listed),
            [vec!["outer"], vec!["inner/.gitignore", "inner/file"]]
        );
    }

    #[test]
    fn a_directory_outside_git_lists_every_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, &["a/one", "a/b/two"]);

        let listed = SourceFiles::default().list(&[root.join("a"), root.join("a/b")]);

        assert_eq!(relative(root, listed), [vec!["a/b/two", "a/one"], vec!["a/b/two"]]);
    }

    #[test]
    fn long_pathspec_lists_are_split() {
        let long = "x".repeat(PATHSPEC_BYTES / 2);
        let pathspecs = [OsStr::new(&long), OsStr::new(&long), OsStr::new("a")];
        let chunks = chunks(&pathspecs);
        assert_eq!(chunks.iter().map(|chunk| chunk.len()).collect::<Vec<_>>(), [1, 2]);
    }
}
