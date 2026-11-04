//! Age-based trimming of the disk cache, modelled on Go's build cache.
//!
//! Every receipt and blob a run uses has its mtime refreshed, and a trim
//! removes whatever has gone unused for [`TRIM_LIMIT`]. Nothing is locked
//! against deletion: a reader that loses a file to a trim sees a miss and
//! runs the action, and anything in use has an mtime far inside the limit.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long an entry may go unused before a trim removes it. Go found that
/// almost all reuse happens within five days of the previous use
/// (golang/go#22990).
const TRIM_LIMIT: Duration = Duration::from_secs(5 * 24 * 60 * 60);

/// How often a trim runs. Each one walks the whole cache.
const TRIM_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// A use refreshes an mtime only when it is older than this, to avoid an
/// inode write on every hit. Trims allow for the staleness this causes.
const MTIME_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Record that the file at `path` was used, so a trim keeps it.
pub(super) fn mark_used(path: &Path) {
    let Ok(mtime) = fs::metadata(path).and_then(|meta| meta.modified()) else {
        return;
    };
    let now = SystemTime::now();
    // An mtime in the future is left alone, as Go does.
    if now.duration_since(mtime).map_or(true, |age| age < MTIME_INTERVAL) {
        return;
    }
    // Blobs are read-only, but their owner may still set their times.
    if let Ok(file) = File::open(path) {
        let _ = file.set_modified(now);
    }
}

/// Remove everything under `dirs` unused for [`TRIM_LIMIT`], at most once
/// per [`TRIM_INTERVAL`] as recorded in the `stamp` file. Best-effort:
/// failures leave entries for the next trim.
pub(super) fn trim(stamp: &Path, dirs: &[&Path]) {
    let now = SystemTime::now();
    // Checked without the lock first so that almost every run costs one
    // read; it is checked again under the lock before anything is removed.
    let due = fs::read_to_string(stamp).map_or(true, |text| is_due(&text, now));
    if !due || !claim(stamp, now) {
        return;
    }
    let cutoff = now - TRIM_LIMIT - MTIME_INTERVAL;
    for dir in dirs {
        sweep(dir, cutoff);
    }
}

/// Whether a trim is due given the `stamp` contents. An unreadable stamp,
/// or one implausibly far in the future, is due rather than trusted.
fn is_due(stamp: &str, now: SystemTime) -> bool {
    let Ok(secs) = stamp.trim().parse::<u64>() else {
        return true;
    };
    let last = UNIX_EPOCH + Duration::from_secs(secs);
    match now.duration_since(last) {
        Ok(elapsed) => elapsed >= TRIM_INTERVAL,
        Err(ahead) => ahead.duration() >= MTIME_INTERVAL,
    }
}

/// Take the trim for this process: under an exclusive lock, confirm one is
/// still due and record `now` as the last trim. The stamp is written before
/// anything is removed, so processes finishing meanwhile skip the trim
/// rather than repeat it. A process that finds the lock held skips it too,
/// so no run ever waits for another's trim.
fn claim(stamp: &Path, now: SystemTime) -> bool {
    let Ok(mut file) = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(stamp)
    else {
        return false;
    };
    if file.try_lock().is_err() {
        return false;
    }
    let mut text = String::new();
    if file.read_to_string(&mut text).is_err() || !is_due(&text, now) {
        return false;
    }
    let secs = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    file.set_len(0).is_ok() && file.rewind().is_ok() && file.write_all(secs.to_string().as_bytes()).is_ok()
}

/// Remove every file beneath `dir` last modified before `cutoff`.
/// Directories are kept, because a writer may be about to publish into one.
fn sweep(dir: &Path, cutoff: SystemTime) {
    // Listed in full before removing anything, so removal cannot disturb
    // the directory scan.
    let Ok(entries) = fs::read_dir(dir).map(|entries| entries.flatten().collect::<Vec<_>>()) else {
        return;
    };
    for entry in entries {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_dir() {
            sweep(&path, cutoff);
        } else if entry
            .metadata()
            .and_then(|meta| meta.modified())
            .is_ok_and(|mtime| mtime < cutoff)
        {
            let _ = fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// Make `path` look unused for `age`.
    pub(in crate::cache) fn backdate(path: &Path, age: Duration) {
        File::open(path).unwrap().set_modified(SystemTime::now() - age).unwrap();
    }

    pub(in crate::cache) fn mtime(path: &Path) -> SystemTime {
        fs::metadata(path).unwrap().modified().unwrap()
    }

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    #[test]
    fn removes_only_files_unused_past_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let shard = dir.path().join("cache/ab");
        fs::create_dir_all(&shard).unwrap();
        let (old, fresh) = (shard.join("old"), shard.join("fresh"));
        fs::write(&old, b"x").unwrap();
        fs::write(&fresh, b"x").unwrap();
        backdate(&old, 6 * DAY);
        backdate(&fresh, 4 * DAY);

        trim(&dir.path().join("trim.txt"), &[&dir.path().join("cache")]);

        assert!(!old.exists());
        assert!(fresh.exists());
        assert!(shard.is_dir(), "directories are kept");
    }

    #[test]
    fn runs_at_most_once_per_interval() {
        let dir = tempfile::tempdir().unwrap();
        let stamp = dir.path().join("trim.txt");
        let cache = dir.path().join("cache");
        fs::create_dir_all(&cache).unwrap();
        trim(&stamp, &[&cache]);

        let old = cache.join("old");
        fs::write(&old, b"x").unwrap();
        backdate(&old, 6 * DAY);
        trim(&stamp, &[&cache]);
        assert!(old.exists(), "a trim already ran today");

        fs::write(&stamp, b"0").unwrap();
        trim(&stamp, &[&cache]);
        assert!(!old.exists());
    }

    #[test]
    fn an_unreadable_or_future_stamp_is_due() {
        let now = SystemTime::now();
        let secs = |t: SystemTime| t.duration_since(UNIX_EPOCH).unwrap().as_secs().to_string();
        assert!(is_due("garbage", now));
        assert!(is_due(&secs(now + 2 * MTIME_INTERVAL), now));
        assert!(!is_due(&secs(now - DAY / 2), now));
        assert!(is_due(&secs(now - DAY), now));
    }

    #[test]
    fn use_refreshes_a_stale_mtime_only() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("blob");
        fs::write(&file, b"x").unwrap();
        fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o444)).unwrap();

        backdate(&file, MTIME_INTERVAL / 2);
        let recent = mtime(&file);
        mark_used(&file);
        assert_eq!(mtime(&file), recent, "a recent mtime is not rewritten");

        backdate(&file, 6 * DAY);
        mark_used(&file);
        assert!(SystemTime::now().duration_since(mtime(&file)).unwrap() < MTIME_INTERVAL);
    }
}
