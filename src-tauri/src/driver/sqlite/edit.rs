//! Grid saves: every change of a save in one transaction on one browse connection, all or
//! nothing.
//!
//! The whole save runs in one worker job, so nothing else interleaves on that connection: the
//! structure read and the checks (nothing has run if they refuse), `BEGIN IMMEDIATE` (the write
//! lock is taken before any change, so no change fails for want of it), the planned statements,
//! `COMMIT`. Any failure after BEGIN, including a COMMIT that SQLite refused, rolls back. A
//! ROLLBACK that fails and leaves the transaction open ends the worker (`After::End`): closing
//! the connection rolls back, and the browse driver opens a new one for the next request.
//!
//! The structure read, BEGIN and COMMIT may each wait for another connection's lock, each up to
//! the busy timeout; a step that gives up fails the save with `SAVE_LOCKED`. (Usually only one
//! of them waits: BEGIN for a writer, COMMIT for readers, the structure read for a connection
//! holding the file exclusively.)

use rusqlite::types::Value;
use rusqlite::{params_from_iter, Connection, ErrorCode};

use super::session::error_message;
use super::worker::After;
use super::{catalog, SqliteDialect};
use crate::edit::{plan_changes, row_changed_error, PlannedStatement};
use crate::error::{AppError, AppResult};
use crate::model::{ApplyResult, CellType, ChangeSet, RowChange, TableStructure};

/// A save that couldn't get the file: another connection holds a lock on it.
pub(super) const SAVE_LOCKED: &str = "The database is locked by another connection (an open transaction in a query tab?).";
/// A key that is a blob: the grid shows it as `\x…` text, which wouldn't find the row.
pub(super) const BLOB_KEY: &str = "Blob values can't be edited yet.";

/// Whether SQLite gave up waiting for another connection's lock.
fn is_locked(e: &rusqlite::Error) -> bool {
    matches!(e.sqlite_error_code(), Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked))
}

/// A failure outside a change (BEGIN, COMMIT): the locked copy, or SQLite's message.
fn save_error(e: rusqlite::Error) -> AppError {
    if is_locked(&e) {
        AppError::Query(SAVE_LOCKED.into())
    } else {
        AppError::Query(error_message(e))
    }
}

/// A failed change: the locked copy, or SQLite's message after the change, as on Postgres
/// (`UPDATE id = 1: …`).
fn statement_error(describe: &str, e: rusqlite::Error) -> AppError {
    if is_locked(&e) {
        AppError::Query(SAVE_LOCKED.into())
    } else {
        AppError::Query(format!("{describe}: {}", error_message(e)))
    }
}

/// Refuses a save naming a row by a blob key.
fn refuse_blob_keys(structure: &TableStructure, set: &ChangeSet) -> AppResult<()> {
    let blob_key = set.changes.iter().any(|change| match change {
        RowChange::Update { original_types, .. } | RowChange::Delete { original_types, .. } => {
            structure.primary_key.iter().any(|k| original_types.get(k) == Some(&CellType::Blob))
        }
        RowChange::Insert { .. } => false,
    });
    if blob_key {
        return Err(AppError::Query(BLOB_KEY.into()));
    }
    Ok(())
}

/// Runs the statements; every UPDATE or DELETE must hit exactly one row.
fn run_statements(c: &Connection, statements: &[PlannedStatement]) -> AppResult<()> {
    for st in statements {
        let bind = st.params.iter().map(|p| p.clone().map_or(Value::Null, Value::Text));
        let changed = c.execute(&st.sql, params_from_iter(bind)).map_err(|e| statement_error(&st.describe, e))?;
        if st.expect_one_row && changed != 1 {
            return Err(row_changed_error(&st.key_label));
        }
    }
    Ok(())
}

/// Rolls back a failed save, unless SQLite already has (an interrupted write, a full disk, …).
fn rollback(c: &Connection) -> rusqlite::Result<()> {
    if c.is_autocommit() {
        return Ok(());
    }
    c.execute_batch("ROLLBACK")
}

/// What a failed save reports, and whether its worker goes on. The save's own error is what the
/// user sees. A rollback that failed is logged; when it left the transaction open (and with it
/// the write lock), the worker ends, closing the connection, which rolls back, and the error
/// says so.
fn failed_save(error: AppError, rolled_back: rusqlite::Result<()>, still_in_transaction: bool) -> (AppError, After) {
    let Err(e) = rolled_back else {
        return (error, After::Continue);
    };
    eprintln!("apply_changes: rollback failed: {e}");
    if !still_in_transaction {
        return (error, After::Continue);
    }
    let reset = AppError::Query(format!("{} (the connection was reset)", error.message()));
    (reset, After::End)
}

/// The save, on the worker's thread, and whether the worker goes on.
pub(super) fn apply(c: &mut Connection, set: &ChangeSet) -> (AppResult<ApplyResult>, After) {
    apply_with(c, set, rollback)
}

/// `apply` with the rollback it uses on failure (tests make it fail).
pub(super) fn apply_with(
    c: &mut Connection,
    set: &ChangeSet,
    rollback: impl FnOnce(&Connection) -> rusqlite::Result<()>,
) -> (AppResult<ApplyResult>, After) {
    let statements = match plan(c, set) {
        Ok(statements) => statements,
        Err(e) => return (Err(e), After::Continue),
    };
    if let Err(e) = c.execute_batch("BEGIN IMMEDIATE") {
        return (Err(save_error(e)), After::Continue);
    }
    // A COMMIT that fails (a lock, a deferred foreign key) leaves the transaction open.
    let saved = run_statements(c, &statements).and_then(|()| c.execute_batch("COMMIT").map_err(save_error));
    match saved {
        Ok(()) => (Ok(ApplyResult { applied: statements.len() }), After::Continue),
        Err(error) => {
            let rolled_back = rollback(c);
            let (error, after) = failed_save(error, rolled_back, !c.is_autocommit());
            (Err(error), after)
        }
    }
}

/// The structure read and the checks; nothing has run if they refuse.
fn plan(c: &Connection, set: &ChangeSet) -> AppResult<Vec<PlannedStatement>> {
    // Reading the structure needs a read lock: a connection holding the file exclusively makes it
    // wait out the busy timeout, and that is a lock like any other here.
    let structure = catalog::read_table_structure(c, &set.table).map_err(|e| e.into_app(save_error))?;
    let statements = plan_changes(&SqliteDialect, &structure, set)?;
    refuse_blob_keys(&structure, set)?;
    Ok(statements)
}

#[cfg(test)]
pub(super) mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::model::{Editing, RelationKind, TableRef};

    fn structure(primary_key: &[&str]) -> TableStructure {
        TableStructure {
            columns: Vec::new(),
            primary_key: primary_key.iter().map(|k| k.to_string()).collect(),
            relation: RelationKind::Table,
            editing: Editing::Allowed,
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        }
    }

    fn types(pairs: &[(&str, CellType)]) -> BTreeMap<String, CellType> {
        pairs.iter().map(|(k, t)| (k.to_string(), *t)).collect()
    }

    fn set(changes: Vec<RowChange>) -> ChangeSet {
        ChangeSet { table: TableRef { schema: "main".into(), name: "t".into() }, changes }
    }

    #[test]
    fn only_a_blob_key_is_refused() {
        let s = structure(&["a", "b"]);
        let blob = Err(AppError::Query(BLOB_KEY.into()));
        let delete = |t| set(vec![RowChange::Delete { key: vec!["1".into(), "2".into()], original_types: t }]);
        assert_eq!(refuse_blob_keys(&s, &delete(types(&[("a", CellType::Integer), ("b", CellType::Blob)]))), blob);
        assert_eq!(refuse_blob_keys(&s, &delete(types(&[("a", CellType::Integer), ("b", CellType::Text)]))), Ok(()));
        assert_eq!(refuse_blob_keys(&s, &delete(BTreeMap::new())), Ok(()));
        let update = |t| {
            set(vec![
                RowChange::Insert { values: BTreeMap::new() },
                RowChange::Update { key: vec!["1".into(), "2".into()], values: BTreeMap::new(), original_types: t },
            ])
        };
        assert_eq!(refuse_blob_keys(&s, &update(types(&[("a", CellType::Blob)]))), blob);
        // An edited column that isn't part of the key.
        assert_eq!(refuse_blob_keys(&s, &update(types(&[("a", CellType::Integer), ("c", CellType::Blob)]))), Ok(()));
    }

    #[test]
    fn busy_and_locked_are_the_locked_copy() {
        use rusqlite::ffi;
        let failure = |code, m: &str| rusqlite::Error::SqliteFailure(ffi::Error::new(code), Some(m.into()));
        for code in [ffi::SQLITE_BUSY, ffi::SQLITE_LOCKED] {
            assert_eq!(save_error(failure(code, "database is locked")), AppError::Query(SAVE_LOCKED.into()));
            assert_eq!(statement_error("UPDATE id = 1", failure(code, "m")), AppError::Query(SAVE_LOCKED.into()));
        }
        assert_eq!(
            statement_error("UPDATE id = 3", failure(ffi::SQLITE_CONSTRAINT, "NOT NULL constraint failed: users.name")),
            AppError::Query("UPDATE id = 3: NOT NULL constraint failed: users.name".into())
        );
        assert_eq!(
            save_error(failure(ffi::SQLITE_READONLY, "attempt to write a readonly database")),
            AppError::Query("attempt to write a readonly database".into())
        );
    }

    fn failure(code: std::ffi::c_int, m: &str) -> rusqlite::Error {
        rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), Some(m.into()))
    }

    #[test]
    fn a_rollback_that_leaves_the_transaction_open_resets_the_connection() {
        let original = || AppError::Query("UPDATE id = 1: boom".into());
        let failed = || Err(failure(rusqlite::ffi::SQLITE_BUSY, "cannot rollback"));
        assert_eq!(
            failed_save(original(), failed(), true),
            (AppError::Query("UPDATE id = 1: boom (the connection was reset)".into()), After::End)
        );
        // SQLite rolled back by itself, or the rollback worked: the save's own error, and on.
        assert_eq!(failed_save(original(), failed(), false), (original(), After::Continue));
        assert_eq!(failed_save(original(), Ok(()), false), (original(), After::Continue));
        assert_eq!(failed_save(original(), Ok(()), true), (original(), After::Continue));
    }

    /// A file with `t (id INTEGER PRIMARY KEY, v TEXT)` holding `(1, 'one')`.
    pub(in crate::driver::sqlite) fn save_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("e.sqlite");
        Connection::open(&path)
            .unwrap()
            .execute_batch("create table t (id integer primary key, v text); insert into t values (1, 'one');")
            .unwrap();
        (dir, path)
    }

    /// Updates row 1, then deletes row 999, which isn't there: the save fails after BEGIN.
    pub(in crate::driver::sqlite) fn failing_save() -> ChangeSet {
        let id = || [("id".to_string(), CellType::Integer)].into();
        set(vec![
            RowChange::Update {
                key: vec!["1".into()],
                values: [("v".to_string(), Some("changed".to_string()))].into(),
                original_types: id(),
            },
            RowChange::Delete { key: vec!["999".into()], original_types: id() },
        ])
    }

    /// A rollback that fails and does nothing.
    pub(in crate::driver::sqlite) fn failing_rollback(_: &Connection) -> rusqlite::Result<()> {
        Err(failure(rusqlite::ffi::SQLITE_BUSY, "cannot rollback"))
    }

    #[test]
    fn a_save_whose_rollback_fails_asks_to_end_the_worker() {
        let (_dir, path) = save_fixture();
        let mut c = Connection::open(&path).unwrap();
        let (result, after) = apply_with(&mut c, &failing_save(), failing_rollback);
        assert_eq!(
            result.unwrap_err(),
            AppError::Query(
                "The row with id = 999 was changed or deleted by someone else. (the connection was reset)"
                    .into()
            )
        );
        assert_eq!(after, After::End);
        // Left in the save's transaction: only closing the connection can end it.
        assert!(!c.is_autocommit());
        drop(c);
        let check = Connection::open(&path).unwrap();
        assert_eq!(check.query_row("select v from t where id = 1", [], |r| r.get::<_, String>(0)).unwrap(), "one");

        // With the real rollback the same save fails with its own error, and the worker goes on.
        let mut c = Connection::open(&path).unwrap();
        let (result, after) = apply(&mut c, &failing_save());
        assert_eq!(result.unwrap_err(), AppError::Query("The row with id = 999 was changed or deleted by someone else.".into()));
        assert_eq!(after, After::Continue);
        assert!(c.is_autocommit());
    }
}
