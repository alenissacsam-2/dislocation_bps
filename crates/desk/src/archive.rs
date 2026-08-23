//! Moving a finished run out of the way so the next one starts clean.
//!
//! HANDOVER §7: rows measured under different parameters aggregate by different rules,
//! and nothing downstream shows the mixture. So a parameter change ends the run rather
//! than continuing into it. This is the mechanism.

use serde::Serialize;
use std::path::{Path, PathBuf};

/// The three files SQLite keeps in WAL mode. Moving only the first silently leaves the
/// most recent writes behind — the hazard `scripts/archive-ledger.sh` exists to avoid.
const SUFFIXES: [&str; 3] = ["", "-wal", "-shm"];

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchivedRun {
    pub name: String,
    pub path: PathBuf,
    pub bytes: u64,
}

/// Move `ledger` and its `-wal`/`-shm` into `archive_dir`, stamped.
///
/// `Ok(None)` means there was no ledger to move, which is the normal first-run case
/// rather than a fault.
///
/// # Errors
/// If the archive directory cannot be created, or a file cannot be moved or copied.
pub fn archive_ledger(
    ledger: &Path,
    archive_dir: &Path,
    stamp: &str,
) -> anyhow::Result<Option<PathBuf>> {
    if !ledger.exists() {
        return Ok(None);
    }
    std::fs::create_dir_all(archive_dir)?;
    let stem = ledger.file_stem().and_then(|s| s.to_str()).unwrap_or("cryptobot");
    let target = archive_dir.join(format!("{stem}-{stamp}.db"));
    for suffix in SUFFIXES {
        let from = PathBuf::from(format!("{}{suffix}", ledger.display()));
        if !from.exists() {
            continue;
        }
        let to = PathBuf::from(format!("{}{suffix}", target.display()));
        // Rename first: same volume, atomic, and cheap on a 138 MB file. Fall back to
        // copy-then-remove only when the archive sits on another volume.
        if std::fs::rename(&from, &to).is_err() {
            std::fs::copy(&from, &to)?;
            std::fs::remove_file(&from)?;
        }
    }
    Ok(Some(target))
}

/// Archived runs, newest first. The stamp sorts lexicographically because it is
/// `YYYYmmdd-HHMMSS`, so no date parsing is needed to order them.
#[must_use]
pub fn list_archives(archive_dir: &Path) -> Vec<ArchivedRun> {
    let Ok(entries) = std::fs::read_dir(archive_dir) else {
        return Vec::new();
    };
    let mut out: Vec<ArchivedRun> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "db"))
        .map(|e| ArchivedRun {
            name: e.file_name().to_string_lossy().into_owned(),
            path: e.path(),
            bytes: e.metadata().map(|m| m.len()).unwrap_or(0),
        })
        .collect();
    out.sort_by(|a, b| b.name.cmp(&a.name));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!("cbdesk-arch-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A SQLite database in WAL mode is three files. Moving only the `.db` silently
    /// leaves the most recent writes behind.
    #[test]
    fn archiving_takes_the_wal_and_shm_with_the_database() {
        let d = scratch("wal");
        let db = d.join("cryptobot.db");
        std::fs::write(&db, b"main").unwrap();
        std::fs::write(d.join("cryptobot.db-wal"), b"wal").unwrap();
        std::fs::write(d.join("cryptobot.db-shm"), b"shm").unwrap();
        let arch = d.join("archive");
        let moved = archive_ledger(&db, &arch, "20260823-101500").unwrap().unwrap();
        assert!(moved.exists());
        assert!(arch.join("cryptobot-20260823-101500.db-wal").exists());
        assert!(arch.join("cryptobot-20260823-101500.db-shm").exists());
        assert!(!db.exists(), "the live ledger must be moved, not copied");
    }

    #[test]
    fn archiving_a_ledger_that_does_not_exist_is_not_an_error() {
        let d = scratch("none");
        let got = archive_ledger(&d.join("cryptobot.db"), &d.join("archive"), "x").unwrap();
        assert!(got.is_none(), "a first run has nothing to archive");
    }

    #[test]
    fn archived_runs_are_listed_newest_first() {
        let d = scratch("list");
        let arch = d.join("archive");
        std::fs::create_dir_all(&arch).unwrap();
        std::fs::write(arch.join("cryptobot-20260101-000000.db"), b"a").unwrap();
        std::fs::write(arch.join("cryptobot-20260823-000000.db"), b"bb").unwrap();
        let got = list_archives(&arch);
        assert_eq!(got.len(), 2);
        assert!(got[0].name.contains("20260823"), "newest first");
    }

    #[test]
    fn the_wal_and_shm_are_not_listed_as_separate_runs() {
        let d = scratch("nowal");
        let arch = d.join("archive");
        std::fs::create_dir_all(&arch).unwrap();
        std::fs::write(arch.join("cryptobot-20260101-000000.db"), b"a").unwrap();
        std::fs::write(arch.join("cryptobot-20260101-000000.db-wal"), b"a").unwrap();
        assert_eq!(list_archives(&arch).len(), 1);
    }

    #[test]
    fn listing_a_directory_that_does_not_exist_is_empty_not_a_panic() {
        assert!(list_archives(Path::new("nowhere-at-all")).is_empty());
    }
}
