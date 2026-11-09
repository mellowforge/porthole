//! Carries the previous version's data over on first launch.

use std::io;
use std::path::Path;

pub const LEGACY_IDENTIFIER: &str = "com.tableslider.app";

const SIDECARS: [&str; 2] = ["app.db-wal", "app.db-shm"];

/// Copies app.db (+ app.db-wal, app.db-shm when present) from `legacy_dir`
/// into `new_dir` when `new_dir/app.db` does not exist and `legacy_dir/app.db`
/// does. Never overwrites, never deletes or moves anything in `legacy_dir`.
/// Ok(true) = copied, Ok(false) = nothing to do. An error that leaves it unknown
/// whether there is anything to copy is returned, not taken for "nothing to do".
pub fn migrate_legacy_data(new_dir: &Path, legacy_dir: &Path) -> io::Result<bool> {
    let new_db = new_dir.join("app.db");
    let old_db = legacy_dir.join("app.db");
    if new_db.try_exists()? || !is_file(&old_db)? {
        return Ok(false);
    }
    // Assumes the old app is not running: its files are copied one by one, not as an atomic snapshot.
    // With no app.db here, any -wal/-shm or temp file in new_dir is a leftover of an earlier failed
    // attempt and must not end up beside a different database.
    remove_strays(new_dir);
    // The sidecars go first and app.db last: app.db appearing is what marks the copy as done,
    // so a crash in between leaves the next launch to redo it.
    let result = (|| {
        for name in SIDECARS {
            let from = legacy_dir.join(name);
            if is_file(&from)? {
                copy_atomically(&from, &new_dir.join(name))?;
            }
        }
        copy_atomically(&old_db, &new_db)
    })();
    if let Err(e) = result {
        remove_strays(new_dir);
        return Err(e);
    }
    Ok(true)
}

/// Whether `path` is a file. Only a path that cannot exist counts as absent: a file where
/// the legacy folder would be means there is no legacy folder.
fn is_file(path: &Path) -> io::Result<bool> {
    match std::fs::metadata(path) {
        Ok(meta) => Ok(meta.is_file()),
        Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Best effort: removes the sidecars and the temp files of a copy that did not finish.
fn remove_strays(new_dir: &Path) {
    for name in ["app.db-wal", "app.db-shm", "app.db-wal.migrating", "app.db-shm.migrating", "app.db.migrating"] {
        let _ = std::fs::remove_file(new_dir.join(name));
    }
}

/// Copies to a temporary name next to `to`, then renames into place.
fn copy_atomically(from: &Path, to: &Path) -> io::Result<()> {
    let mut tmp_name = to.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".migrating");
    let tmp = to.with_file_name(tmp_name);
    let result = std::fs::copy(from, &tmp).and_then(|_| std::fs::rename(&tmp, to));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appdb::AppDb;
    use crate::model::*;

    fn dirs() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let new_dir = root.path().join("new");
        let legacy = root.path().join(LEGACY_IDENTIFIER);
        std::fs::create_dir_all(&new_dir).unwrap();
        std::fs::create_dir_all(&legacy).unwrap();
        (root, new_dir, legacy)
    }

    #[test]
    fn copies_app_db_when_new_dir_has_none() {
        let (_r, new_dir, legacy) = dirs();
        std::fs::write(legacy.join("app.db"), b"old data").unwrap();
        assert!(migrate_legacy_data(&new_dir, &legacy).unwrap());
        assert_eq!(std::fs::read(new_dir.join("app.db")).unwrap(), b"old data");
        assert!(!new_dir.join("app.db.migrating").exists());
    }

    #[test]
    fn copies_wal_and_shm_alongside() {
        let (_r, new_dir, legacy) = dirs();
        std::fs::write(legacy.join("app.db"), b"db").unwrap();
        std::fs::write(legacy.join("app.db-wal"), b"wal").unwrap();
        std::fs::write(legacy.join("app.db-shm"), b"shm").unwrap();
        assert!(migrate_legacy_data(&new_dir, &legacy).unwrap());
        assert_eq!(std::fs::read(new_dir.join("app.db-wal")).unwrap(), b"wal");
        assert_eq!(std::fs::read(new_dir.join("app.db-shm")).unwrap(), b"shm");
    }

    #[test]
    fn never_overwrites_existing_new_db() {
        let (_r, new_dir, legacy) = dirs();
        std::fs::write(legacy.join("app.db"), b"old").unwrap();
        std::fs::write(new_dir.join("app.db"), b"new").unwrap();
        assert!(!migrate_legacy_data(&new_dir, &legacy).unwrap());
        assert_eq!(std::fs::read(new_dir.join("app.db")).unwrap(), b"new");
    }

    #[test]
    fn nothing_to_do_without_legacy_dir() {
        let root = tempfile::tempdir().unwrap();
        let new_dir = root.path().join("new");
        std::fs::create_dir_all(&new_dir).unwrap();
        let legacy = root.path().join(LEGACY_IDENTIFIER);
        assert!(!migrate_legacy_data(&new_dir, &legacy).unwrap());
        assert_eq!(std::fs::read_dir(&new_dir).unwrap().count(), 0);
        assert!(!legacy.exists());
    }

    #[test]
    fn leaves_the_legacy_files_in_place() {
        let (_r, new_dir, legacy) = dirs();
        std::fs::write(legacy.join("app.db"), b"db").unwrap();
        std::fs::write(legacy.join("app.db-wal"), b"wal").unwrap();
        migrate_legacy_data(&new_dir, &legacy).unwrap();
        assert_eq!(std::fs::read(legacy.join("app.db")).unwrap(), b"db");
        assert_eq!(std::fs::read(legacy.join("app.db-wal")).unwrap(), b"wal");
    }

    #[test]
    fn a_failed_copy_leaves_no_stray_files() {
        let (_r, new_dir, legacy) = dirs();
        std::fs::write(legacy.join("app.db"), b"db").unwrap();
        std::fs::write(legacy.join("app.db-wal"), b"wal").unwrap();
        std::fs::write(legacy.join("app.db-shm"), b"shm").unwrap();
        // A directory where the temp file goes makes the app.db step fail after the sidecars copied.
        std::fs::create_dir(new_dir.join("app.db.migrating")).unwrap();
        assert!(migrate_legacy_data(&new_dir, &legacy).is_err());
        assert!(!new_dir.join("app.db").exists());
        assert!(!new_dir.join("app.db-wal").exists());
        assert!(!new_dir.join("app.db-shm").exists());
        assert_eq!(std::fs::read(legacy.join("app.db")).unwrap(), b"db");
        assert_eq!(std::fs::read(legacy.join("app.db-wal")).unwrap(), b"wal");
        assert_eq!(std::fs::read(legacy.join("app.db-shm")).unwrap(), b"shm");
    }

    #[test]
    fn stale_sidecars_without_a_db_are_removed_first() {
        let (_r, new_dir, legacy) = dirs();
        std::fs::write(legacy.join("app.db"), b"db").unwrap();
        std::fs::write(new_dir.join("app.db-wal"), b"stale").unwrap();
        std::fs::write(new_dir.join("app.db.migrating"), b"stale").unwrap();
        assert!(migrate_legacy_data(&new_dir, &legacy).unwrap());
        assert!(!new_dir.join("app.db-wal").exists());
        assert!(!new_dir.join("app.db.migrating").exists());
    }

    #[test]
    fn a_file_where_the_legacy_folder_would_be_is_nothing_to_do() {
        let root = tempfile::tempdir().unwrap();
        let new_dir = root.path().join("new");
        std::fs::create_dir_all(&new_dir).unwrap();
        let legacy = root.path().join(LEGACY_IDENTIFIER);
        std::fs::write(&legacy, b"not a folder").unwrap();
        assert!(!migrate_legacy_data(&new_dir, &legacy).unwrap());
    }

    /// Mode 000 on a directory until dropped, so the temp dir can be removed even if an
    /// assertion fails. None when mode bits don't apply (running as root).
    #[cfg(unix)]
    struct Locked(std::path::PathBuf);

    #[cfg(unix)]
    impl Locked {
        fn new(dir: &Path) -> Option<Self> {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o000)).unwrap();
            let locked = Locked(dir.to_path_buf());
            std::fs::read_dir(dir).is_err().then_some(locked)
        }
    }

    #[cfg(unix)]
    impl Drop for Locked {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_legacy_folder_is_an_error_and_the_next_try_copies() {
        let (_r, new_dir, legacy) = dirs();
        std::fs::write(legacy.join("app.db"), b"old data").unwrap();
        let Some(locked) = Locked::new(&legacy) else { return };
        let err = migrate_legacy_data(&new_dir, &legacy).unwrap_err();
        drop(locked);
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(!new_dir.join("app.db").exists());
        assert!(migrate_legacy_data(&new_dir, &legacy).unwrap());
        assert_eq!(std::fs::read(new_dir.join("app.db")).unwrap(), b"old data");
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_new_folder_is_an_error() {
        let (_r, new_dir, legacy) = dirs();
        std::fs::write(legacy.join("app.db"), b"old data").unwrap();
        let Some(locked) = Locked::new(&new_dir) else { return };
        let err = migrate_legacy_data(&new_dir, &legacy).unwrap_err();
        drop(locked);
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }

    #[tokio::test]
    async fn copied_db_opens_with_its_rows() {
        let (_r, new_dir, legacy) = dirs();
        let old = AppDb::open(&legacy.join("app.db")).await.unwrap();
        old.upsert_connection(ConnectionProfile {
            id: String::new(),
            name: "kept".into(),
            kind: DbKind::Postgres,
            default_schema: None,
            environment: Environment::Local,
            connect_timeout_secs: 10,
            read_only: false,
            startup_sql: None,
            statement_timeout_secs: None,
            transport: Transport::Network(NetworkTarget {
                host: "localhost".into(),
                port: 5432,
                user: "u".into(),
                database: "d".into(),
                ssl_mode: SslMode::Prefer,
                ssl_root_cert: None,
                auth_source: None,
            }),
        })
        .await
        .unwrap();
        // The old app may still hold its WAL; the migration copies it alongside.
        drop(old);
        assert!(migrate_legacy_data(&new_dir, &legacy).unwrap());
        let db = AppDb::open(&new_dir.join("app.db")).await.unwrap();
        let names: Vec<_> = db.list_connections().await.unwrap().into_iter().map(|p| p.name).collect();
        assert_eq!(names, vec!["kept".to_string()]);
    }
}
