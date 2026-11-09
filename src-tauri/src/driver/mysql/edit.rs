//! Grid saves: every change of a save in one transaction on a pooled connection, all or nothing.
//!
//! The pool's connections count the rows an UPDATE matched (`CLIENT_FOUND_ROWS`), so saving a
//! cell's current value counts its row. A row locked elsewhere (an open transaction in a query tab)
//! fails the save after `innodb_lock_wait_timeout` (5 s, set for the save's session; the pool's
//! reset puts it back), and the whole save is stopped with `KILL QUERY` after `SAVE_LIMIT`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Params, Value};

use super::{discard, error_message, kill, query_error, text, MysqlDriver};
use crate::driver::postgres::{COMMIT_UNCERTAIN, SAVE_LOCKED, SAVE_TIMED_OUT};
use crate::driver::Driver;
use crate::edit::{plan_changes, row_changed_error, PlannedStatement};
use crate::error::{AppError, AppResult};
use crate::model::{ApplyResult, ChangeSet};

/// How long a whole save may take before it is stopped (`SAVE_TIMED_OUT` says 30 s).
const SAVE_LIMIT: Duration = Duration::from_secs(30);
const LOCK_WAIT_SQL: &str = "SET SESSION innodb_lock_wait_timeout = 5";
/// Adds `STRICT_ALL_TABLES` to the save's session `sql_mode`, so a value the server
/// would coerce with a warning (`abc` into an INT) fails the save instead. The pool's reset puts
/// the session's own mode back. An empty mode gets no leading comma.
const STRICT_SQL: &str =
    "SET SESSION sql_mode = IF(@@sql_mode = '', 'STRICT_ALL_TABLES', CONCAT(@@sql_mode, ',STRICT_ALL_TABLES'))";
/// The server's warning, after a ROLLBACK, that a table without transactions kept its changes.
const ER_WARNING_NOT_COMPLETE_ROLLBACK: u16 = 1196;
/// A failed save whose ROLLBACK couldn't undo changes to a table without transactions.
pub const SAVE_PARTLY_APPLIED: &str =
    "Some changes may have been saved: this table doesn't support transactions. Reload before saving again.";
/// The server's errno for a lock wait that passed `innodb_lock_wait_timeout`.
const ER_LOCK_WAIT_TIMEOUT: u16 = 1205;

/// Why a save stopped before its COMMIT, or how its COMMIT failed.
enum Failure {
    /// A statement of the save failed; `describe` names its change (`UPDATE id = 1`).
    Statement { describe: String, error: mysql_async::Error },
    /// An UPDATE or DELETE didn't hit exactly one row.
    RowChanged { key_label: String },
    /// The COMMIT failed.
    Commit(mysql_async::Error),
}

/// The copy for a save that hit its lock wait or its time limit, if it did.
fn save_limit_message(e: &mysql_async::Error) -> Option<&'static str> {
    match e {
        mysql_async::Error::Server(s) if s.code == ER_LOCK_WAIT_TIMEOUT => Some(SAVE_LOCKED),
        mysql_async::Error::Server(s) if s.code == kill::ER_QUERY_INTERRUPTED => Some(SAVE_TIMED_OUT),
        _ => None,
    }
}

/// A failed statement of a save: the limits get their own copy, anything else names the change
/// (`UPDATE id = 1: …`), as on Postgres.
fn statement_error(describe: &str, e: &mysql_async::Error) -> AppError {
    match save_limit_message(e) {
        Some(m) => AppError::Query(m.into()),
        None => AppError::Query(format!("{describe}: {}", error_message(e))),
    }
}

/// A COMMIT the server answered with an error didn't commit. Without an answer (the connection
/// broke), or stopped by the KILL at the time limit (errno 1317: the server may have committed
/// before the KILL landed), the save may or may not have been applied.
fn commit_error(e: &mysql_async::Error) -> AppError {
    match e {
        mysql_async::Error::Server(s) if s.code == kill::ER_QUERY_INTERRUPTED => {}
        mysql_async::Error::Server(_) => {
            return AppError::Query(save_limit_message(e).map_or_else(|| error_message(e), str::to_string));
        }
        _ => {}
    }
    eprintln!("apply_changes: commit failed: {}", error_message(e));
    AppError::Uncertain(COMMIT_UNCERTAIN.into())
}

/// One statement of the save, prepared, each value bound by its column (`text::to_column_value`).
async fn execute(conn: &mut Conn, st: &PlannedStatement) -> mysql_async::Result<()> {
    let values: Vec<Value> =
        st.params.iter().zip(&st.param_types).map(|(p, t)| text::to_column_value(p.as_deref(), t)).collect();
    conn.exec_drop(st.sql.as_str(), Params::Positional(values)).await
}

/// The save's transaction, up to and including its COMMIT. A failure of a change (including a
/// statement stopped by the KILL at the limit, errno 1317) is a `Failure`, with the transaction
/// still open unless it was the COMMIT that failed; only the session setup fails the future
/// itself. `committing` is set just before the COMMIT is sent.
async fn run_save(
    conn: &mut Conn,
    statements: Vec<PlannedStatement>,
    committing: Arc<AtomicBool>,
) -> mysql_async::Result<Result<usize, Failure>> {
    conn.query_drop(LOCK_WAIT_SQL).await?;
    conn.query_drop(STRICT_SQL).await?;
    conn.query_drop("START TRANSACTION").await?;
    for st in &statements {
        if let Err(error) = execute(conn, st).await {
            return Ok(Err(Failure::Statement { describe: st.describe.clone(), error }));
        }
        if st.expect_one_row && conn.affected_rows() != 1 {
            return Ok(Err(Failure::RowChanged { key_label: st.key_label.clone() }));
        }
    }
    committing.store(true, Ordering::SeqCst);
    if let Err(e) = conn.query_drop("COMMIT").await {
        return Ok(Err(Failure::Commit(e)));
    }
    Ok(Ok(statements.len()))
}

/// Rolls back a failed save, then lets the connection go back to the pool, and returns the
/// error the user sees: `error`, or `SAVE_PARTLY_APPLIED` when the server warns that a table
/// without transactions kept changes. A connection whose rollback failed is closed instead,
/// so it never returns to the pool with the save's transaction open.
async fn rollback(mut conn: Conn, error: AppError) -> AppError {
    if let Err(e) = conn.query_drop("ROLLBACK").await {
        eprintln!("apply_changes: rollback failed, closing that connection: {}", error_message(&e));
        discard(conn);
        return error;
    }
    if conn.get_warnings() == 0 {
        return error;
    }
    match super::warning_codes(&mut conn).await {
        Ok(codes) if codes.iter().any(|(code, _)| *code == ER_WARNING_NOT_COMPLETE_ROLLBACK) => {
            eprintln!("apply_changes: {error}; the rollback left a table without transactions changed");
            AppError::Uncertain(SAVE_PARTLY_APPLIED.into())
        }
        Ok(_) => error,
        Err(e) => {
            // Without the warnings it is unknown whether anything was kept.
            eprintln!("apply_changes: couldn't read the rollback's warnings: {}", error_message(&e));
            discard(conn);
            AppError::Uncertain(SAVE_PARTLY_APPLIED.into())
        }
    }
}

/// Plans the save, then runs it on a pooled connection within `SAVE_LIMIT`.
pub(super) async fn apply_changes(driver: &MysqlDriver, set: &ChangeSet) -> AppResult<ApplyResult> {
    let structure = driver.table_structure(&set.table).await?;
    let statements = plan_changes(&driver.dialect, &structure, set)?;
    let mut conn = driver.conn().await?;
    let committing = Arc::new(AtomicBool::new(false));
    let flag = committing.clone();
    let ran = kill::bounded(&driver.config, &mut conn, SAVE_LIMIT, move |c| Box::pin(run_save(c, statements, flag))).await;
    let committing = committing.load(Ordering::SeqCst);
    let error = match ran {
        Ok(Ok(applied)) => return Ok(ApplyResult { applied }),
        Ok(Err(Failure::Statement { describe, error })) => statement_error(&describe, &error),
        Ok(Err(Failure::RowChanged { key_label })) => row_changed_error(&key_label),
        Ok(Err(Failure::Commit(e))) => {
            let error = commit_error(&e);
            if matches!(error, AppError::Uncertain(_)) {
                discard(conn);
                return Err(error);
            }
            error
        }
        // A COMMIT left running may yet be applied.
        Err(kill::Bounded::Abandoned) if committing => {
            discard(conn);
            return Err(AppError::Uncertain(COMMIT_UNCERTAIN.into()));
        }
        Err(kill::Bounded::TimedOut) => AppError::Query(SAVE_TIMED_OUT.into()),
        // The statement may still be running: the connection is closed, and the server rolls
        // the transaction back.
        Err(kill::Bounded::Abandoned) => {
            discard(conn);
            return Err(AppError::Query(SAVE_TIMED_OUT.into()));
        }
        Err(kill::Bounded::Failed(e)) => query_error(e),
    };
    Err(rollback(conn, error).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mysql_async::{IoError, ServerError};

    fn server_error(code: u16, message: &str) -> mysql_async::Error {
        mysql_async::Error::Server(ServerError { code, message: message.into(), state: "HY000".into() })
    }

    fn io_error() -> mysql_async::Error {
        mysql_async::Error::Io(IoError::Io(std::io::Error::from(std::io::ErrorKind::ConnectionReset)))
    }

    #[test]
    fn mysql_save_errors_get_the_save_copy() {
        let q = |m: &str| AppError::Query(m.into());
        assert_eq!(statement_error("UPDATE id = 1", &server_error(1205, "Lock wait timeout exceeded")), q(SAVE_LOCKED));
        assert_eq!(statement_error("UPDATE id = 1", &server_error(1317, "Query execution was interrupted")), q(SAVE_TIMED_OUT));
        assert_eq!(
            statement_error("UPDATE id = 3", &server_error(1048, "Column 'name' cannot be null")),
            q("UPDATE id = 3: Column 'name' cannot be null")
        );
        assert!(matches!(statement_error("DELETE id = 2", &io_error()), AppError::Query(m) if m.starts_with("DELETE id = 2: ")));

        // A COMMIT the server refused didn't commit; one without an answer may have.
        assert_eq!(commit_error(&server_error(1180, "Got error during COMMIT")), q("Got error during COMMIT"));
        assert_eq!(commit_error(&server_error(1205, "Lock wait timeout exceeded")), q(SAVE_LOCKED));
        assert_eq!(commit_error(&io_error()), AppError::Uncertain(COMMIT_UNCERTAIN.into()));
    }

    /// A KILL at the time limit that lands on the COMMIT (errno 1317) leaves it unknown whether
    /// the server committed: the save is uncertain, not "timed out, nothing saved".
    #[test]
    fn mysql_a_commit_stopped_by_the_kill_is_uncertain() {
        assert_eq!(
            commit_error(&server_error(1317, "Query execution was interrupted")),
            AppError::Uncertain(COMMIT_UNCERTAIN.into())
        );
    }
}
