//! Reproducible jar files.
//!
//! bit writes jars itself rather than running the `jar` tool so that equal
//! inputs give byte-identical jars on every JDK: entries are sorted, carry a
//! fixed timestamp and permissions, and the manifest names nothing about
//! the machine. `jar --date` only appeared in JDK 19. Identical jars let the
//! shared cache match, and let a library whose rebuild changed nothing skip
//! the libraries that use it.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::provider::BoxError;

const MANIFEST: &str = "META-INF/MANIFEST.MF";

/// Longest manifest line, in bytes, before a continuation line, as the jar
/// specification requires.
const MANIFEST_LINE_BYTES: usize = 72;

/// A jar to write: files keyed by their entry name.
#[derive(Debug, Default)]
pub(super) struct Jar {
    entries: BTreeMap<String, PathBuf>,
}

impl Jar {
    /// Add every file beneath `dir`, named by its path relative to `dir`.
    /// Two files with one name is an error rather than one silently
    /// replacing the other.
    pub fn add_dir(&mut self, dir: &Path) -> Result<(), BoxError> {
        for file in crate::providers::files_under(dir)? {
            let relative = file.strip_prefix(dir)?;
            let name = relative
                .components()
                .map(|component| component.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            if name == MANIFEST {
                return Err(format!("{} would replace the manifest bit writes", file.display()).into());
            }
            if let Some(existing) = self.entries.insert(name.clone(), file.clone()) {
                return Err(format!(
                    "{} and {} are both `{name}` in the jar",
                    existing.display(),
                    file.display()
                )
                .into());
            }
        }
        Ok(())
    }

    /// Write the jar to `path`, replacing it only once complete.
    pub fn write(&self, path: &Path) -> Result<(), BoxError> {
        let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty());
        if let Some(parent) = parent {
            fs::create_dir_all(parent)?;
        }
        let staged = tempfile::NamedTempFile::new_in(parent.unwrap_or(Path::new(".")))?;
        let mut zip = ZipWriter::new(BufWriter::new(staged.as_file()));
        let file = SimpleFileOptions::DEFAULT
            .compression_method(CompressionMethod::Deflated)
            .unix_permissions(0o644);
        let directory = file.unix_permissions(0o755);

        // The manifest comes first, as `JarInputStream` requires.
        zip.add_directory("META-INF/", directory)?;
        zip.start_file(MANIFEST, file)?;
        zip.write_all(self.manifest().as_bytes())?;

        let mut written_dirs = vec!["META-INF/".to_owned()];
        for (name, source) in &self.entries {
            // Directory entries let class path scanners, such as Spring's,
            // find a package's resources with `getResources("com/example/")`.
            for dir in parent_dirs(name) {
                if !written_dirs.contains(&dir) {
                    zip.add_directory(dir.as_str(), directory)?;
                    written_dirs.push(dir);
                }
            }
            zip.start_file(name.as_str(), file)?;
            std::io::copy(&mut File::open(source)?, &mut zip)?;
        }
        zip.finish()?.flush()?;
        // Temporary files are created private to the user, but a jar is
        // copied into images and run by other users.
        fs::set_permissions(staged.path(), std::os::unix::fs::PermissionsExt::from_mode(0o644))?;
        staged.persist(path)?;
        Ok(())
    }

    fn manifest(&self) -> String {
        let mut manifest = String::new();
        manifest_attribute(&mut manifest, "Manifest-Version", "1.0");
        manifest_attribute(&mut manifest, "Created-By", "bit");
        manifest.push_str("\r\n");
        manifest
    }
}

/// The directories containing entry `name`, outermost first, each ending in
/// `/`.
fn parent_dirs(name: &str) -> Vec<String> {
    name.match_indices('/')
        .map(|(end, _)| name[..=end].to_owned())
        .collect()
}

/// Append `name: value`, split into lines of at most
/// [`MANIFEST_LINE_BYTES`], each continuation starting with a space.
fn manifest_attribute(manifest: &mut String, name: &str, value: &str) {
    let line = format!("{name}: {value}");
    let mut limit = MANIFEST_LINE_BYTES;
    let mut start = 0;
    let mut first = true;
    while start < line.len() || first {
        let mut end = (start + limit).min(line.len());
        // A multi-byte character stays whole on one line.
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        if !first {
            manifest.push(' ');
        }
        manifest.push_str(&line[start..end]);
        manifest.push_str("\r\n");
        start = end;
        first = false;
        limit = MANIFEST_LINE_BYTES - 1;
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    fn entry_names(path: &Path) -> Vec<String> {
        let mut archive = zip::ZipArchive::new(File::open(path).unwrap()).unwrap();
        (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().unwrap().into_owned())
            .collect()
    }

    fn read_entry(path: &Path, name: &str) -> String {
        let mut archive = zip::ZipArchive::new(File::open(path).unwrap()).unwrap();
        let mut content = String::new();
        archive.by_name(name).unwrap().read_to_string(&mut content).unwrap();
        content
    }

    fn tree(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in files {
            let path = dir.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
        dir
    }

    #[test]
    fn writes_the_manifest_first_and_directory_entries() {
        let classes = tree(&[
            ("com/example/App.class", "app"),
            ("com/example/util/Util.class", "util"),
        ]);
        let out = tempfile::tempdir().unwrap();
        let path = out.path().join("libs/app.jar");
        let mut jar = Jar::default();
        jar.add_dir(classes.path()).unwrap();
        jar.write(&path).unwrap();

        assert_eq!(
            entry_names(&path),
            [
                "META-INF/",
                "META-INF/MANIFEST.MF",
                "com/",
                "com/example/",
                "com/example/App.class",
                "com/example/util/",
                "com/example/util/Util.class",
            ]
        );
        assert_eq!(read_entry(&path, "com/example/App.class"), "app");
    }

    #[test]
    fn equal_inputs_give_identical_jars() {
        let classes = tree(&[("a/A.class", "a"), ("b/B.class", "b")]);
        let out = tempfile::tempdir().unwrap();
        let write = |name: &str| {
            let mut jar = Jar::default();
            jar.add_dir(classes.path()).unwrap();
            let path = out.path().join(name);
            jar.write(&path).unwrap();
            fs::read(path).unwrap()
        };
        let first = write("first.jar");
        // A later modification time must not reach the jar.
        fs::write(classes.path().join("a/A.class"), "a").unwrap();
        assert_eq!(first, write("second.jar"));
    }

    #[test]
    fn duplicate_entries_are_an_error() {
        let a = tree(&[("app.properties", "a")]);
        let b = tree(&[("app.properties", "b")]);
        let mut jar = Jar::default();
        jar.add_dir(a.path()).unwrap();
        let error = jar.add_dir(b.path()).unwrap_err().to_string();
        assert!(error.contains("are both `app.properties`"), "{error}");
    }

    #[test]
    fn a_resource_cannot_replace_the_manifest() {
        let resources = tree(&[("META-INF/MANIFEST.MF", "Manifest-Version: 1.0\n")]);
        let error = Jar::default().add_dir(resources.path()).unwrap_err().to_string();
        assert!(error.contains("would replace the manifest"), "{error}");
    }

    #[test]
    fn long_manifest_values_continue_on_lines_starting_with_a_space() {
        let value = "x".repeat(200);
        let mut manifest = String::new();
        manifest_attribute(&mut manifest, "Class-Path", &value);
        let lines: Vec<&str> = manifest.split("\r\n").filter(|line| !line.is_empty()).collect();
        assert!(lines.iter().all(|line| line.len() <= MANIFEST_LINE_BYTES), "{lines:?}");
        assert!(lines[1..].iter().all(|line| line.starts_with(' ')), "{lines:?}");
        let joined: String = lines
            .iter()
            .enumerate()
            .map(|(i, line)| if i == 0 { *line } else { &line[1..] })
            .collect();
        assert_eq!(joined, format!("Class-Path: {value}"));
    }
}
