//! Query-tab sessions: one dedicated database connection per query tab.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::export::RowSink;
use crate::model::ParamValues;
use crate::sqltext::{SqlDialect, StatementKind};

pub mod bind;
pub mod mongo_runner;
pub mod runner;
pub mod sessions;

/// The language a driver's query tabs are written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryLanguage {
    Sql(SqlDialect),
    Mongo,
}

/// The code every session reports for a statement stopped by `cancel`; a driver maps its
/// own code for that to this one.
pub const CANCELED_CODE: &str = "57014";

/// The name of the savepoint a query export runs under.
pub const EXPORT_SAVEPOINT: &str = "ts_export";

/// A connection owned by one query tab. Browsing keeps using the driver's pool.
#[async_trait]
pub trait QuerySession: Send + Sync {
    /// Runs ONE statement that already uses `$n` placeholders. Never returns Err:
    /// failures are `RawOutcome::Error`.
    async fn run(&self, sql: &str, params: &[Option<String>], row_cap: usize) -> StatementRun;
    /// Runs ONE statement with the simple query protocol and returns its rows as the server
    /// sends them in text format (a bool is `t` or `f`); `columns` is left empty. Never
    /// returns Err: failures are `RawOutcome::Error`.
    ///
    /// The server sees one message, so outside a transaction block the statement's own
    /// implicit transaction starts with it. `run` uses the extended protocol, where Parse,
    /// Bind and Execute each restart `statement_timestamp()`.
    async fn run_simple(&self, sql: &str) -> RawOutcome;
    /// Runs ONE read statement that already uses `$n` placeholders and hands every row to
    /// `sink`, as the same text `run` returns, with no row cap. Server errors (a `cancel`
    /// too) keep the server's message; a sink error is returned as it is.
    async fn stream_rows(&self, _sql: &str, _params: &[Option<String>], _sink: &mut dyn RowSink) -> AppResult<()> {
        Err(AppError::Query("This connection can't export query results.".into()))
    }
    /// Whether the session is inside a transaction block, open or failed. Asks the session
    /// itself, so unlike `tx_status` it does not depend on `pg_stat_activity` (hidden from
    /// the pool after e.g. `SET ROLE` in the startup SQL). Outside a block the probe's own
    /// implicit transaction starts with it, so the two timestamps are equal; inside one,
    /// `transaction_timestamp()` is when the block began. A failed block rejects the probe
    /// with 25P02. Any other failure is returned as an error.
    async fn in_transaction(&self) -> AppResult<bool> {
        match self.run_simple("SELECT transaction_timestamp() <> statement_timestamp()").await {
            RawOutcome::Rows { rows, .. } => match rows.first().and_then(|r| r.first()).cloned().flatten().as_deref() {
                Some("t") => Ok(true),
                Some("f") => Ok(false),
                other => Err(AppError::Query(format!("Couldn't tell whether the tab is in a transaction (got {other:?})."))),
            },
            RawOutcome::Error { code: Some(code), .. } if code == "25P02" => Ok(true),
            RawOutcome::Error { message, .. } => Err(AppError::Query(message)),
            RawOutcome::Command { .. } => Err(AppError::Query("Couldn't tell whether the tab is in a transaction.".into())),
        }
    }
    /// True when the session's own transaction is aborted (SQLSTATE 25P02 on a probe).
    /// Unlike `tx_status` it does not depend on `pg_stat_activity`, which may be hidden
    /// from the pool.
    async fn in_failed_transaction(&self) -> bool {
        let run = self.run("SELECT 1", &[], 1).await;
        matches!(run.outcome, RawOutcome::Error { code: Some(ref c), .. } if c == "25P02")
    }
    /// Sets the savepoint a query export runs under, when the tab is inside a transaction.
    /// True when one was set and must be released or rolled back; false when there is
    /// nothing to protect.
    async fn export_savepoint(&self) -> AppResult<bool> {
        match self.run_simple(&format!("SAVEPOINT {EXPORT_SAVEPOINT}")).await {
            // 25P01: not in a transaction block, so there is nothing to protect.
            RawOutcome::Error { code: Some(code), .. } if code == "25P01" => Ok(false),
            RawOutcome::Error { code: Some(code), .. } if code == "25P02" => Err(AppError::Query(
                "The tab's transaction has failed, so the query can't run again. Roll back, then export.".into(),
            )),
            RawOutcome::Error { message, .. } => Err(AppError::Query(message)),
            _ => Ok(true),
        }
    }
    /// Runs ONE parsed MongoDB statement, keeping at most `row_cap` rows. Never returns Err:
    /// failures are `RawOutcome::Error`. Only a MongoDB session runs them.
    async fn run_mongo(&self, _call: &crate::mongoquery::MongoCall, _row_cap: usize) -> StatementRun {
        let message = "This connection doesn't run MongoDB statements.".to_string();
        StatementRun { outcome: RawOutcome::Error { message, code: None, position: None }, notices: vec![], elapsed_ms: 0 }
    }
    /// As `stream_rows`, for ONE parsed MongoDB read.
    async fn stream_mongo(&self, _call: &crate::mongoquery::MongoCall, _sink: &mut dyn RowSink) -> AppResult<()> {
        Err(AppError::Query("This connection can't export query results.".into()))
    }
    async fn cancel(&self) -> AppResult<()>;
    async fn tx_status(&self) -> TxStatus;
    /// True once the connection is gone, or a statement failed in a way that ends it.
    fn is_closed(&self) -> bool;
    /// Why the connection was lost, when known (e.g. the server's FATAL message).
    fn lost_reason(&self) -> Option<String> {
        None
    }
    /// `Some(schema)` points search_path at that schema; `None` restores the value the
    /// session had right after setup (captured at open, so startup SQL is respected).
    async fn set_search_path(&self, schema: Option<&str>) -> AppResult<()>;
    /// Whether a failed `set_search_path` before a run is that run's error (the statement
    /// doesn't run). False: it is logged and the run goes on, as on Postgres, where an aborted
    /// transaction rejects the SET and ROLLBACK must still go out. MySQL's `USE` says true.
    fn schema_switch_fails_run(&self) -> bool {
        false
    }
    async fn close(&self);
    /// How long a quick check on this session (the in-transaction probe before switching
    /// databases) may take before it counts as failed: the profile's connect timeout.
    fn probe_timeout(&self) -> std::time::Duration {
        DEFAULT_PROBE_TIMEOUT
    }
}

/// `probe_timeout` for sessions that don't know their profile's connect timeout.
pub const DEFAULT_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Postgres answers COMMIT in an aborted transaction with a silent ROLLBACK.
pub const COMMIT_IN_FAILED_TX: &str = "The transaction had failed, so COMMIT rolled it back. Nothing was committed.";

/// See `QuerySession::in_failed_transaction`.
pub async fn in_failed_transaction(session: &dyn QuerySession) -> bool {
    session.in_failed_transaction().await
}

/// The refusal when a tab inside a transaction is asked to switch databases.
pub const SWITCH_DATABASE_IN_TX: &str = "Commit or roll back this tab's transaction before switching databases.";

/// See `QuerySession::in_transaction`.
pub async fn in_transaction(session: &dyn QuerySession) -> AppResult<bool> {
    session.in_transaction().await
}

/// Ends the tab's transaction. A COMMIT of a failed transaction is rolled back and reported
/// as an error, because Postgres would answer it with a silent ROLLBACK.
pub async fn end_tab_transaction(session: &dyn QuerySession, commit: bool) -> AppResult<TxStatus> {
    if commit && in_failed_transaction(session).await {
        if let RawOutcome::Error { message, .. } = session.run("ROLLBACK", &[], 1).await.outcome {
            eprintln!("could not roll back a failed transaction: {message}");
        }
        return Err(AppError::Query(COMMIT_IN_FAILED_TX.into()));
    }
    let run = session.run(if commit { "COMMIT" } else { "ROLLBACK" }, &[], 1).await;
    if let RawOutcome::Error { message, .. } = run.outcome {
        return Err(AppError::Query(message));
    }
    Ok(session.tx_status().await)
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StatementRun {
    pub outcome: RawOutcome,
    pub notices: Vec<Notice>,
    pub elapsed_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum RawOutcome {
    Rows {
        columns: Vec<ResultColumn>,
        rows: Vec<Vec<Option<String>>>,
        truncated: bool,
        /// One `CellType` code per column for each row; `None` for Postgres.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cell_types: Option<Vec<String>>,
    },
    Command { rows_affected: u64 },
    /// `position`: 1-based CHAR index into the `sql` passed to `run`.
    Error { message: String, code: Option<String>, position: Option<usize> },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ResultColumn {
    pub name: String,
    /// Postgres type name, e.g. `int4`.
    pub data_type: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Notice {
    pub severity: String,
    pub message: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum TxStatus {
    Idle,
    Open,
    Failed,
    Unknown,
}

/// What to run. Offsets are 0-based UTF-16 code units into `RunRequest::sql`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum RunTarget {
    All,
    Range { from: usize, to: usize },
    Cursor { at: usize },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RunRequest {
    pub sql: String,
    pub target: RunTarget,
    pub params: ParamValues,
    pub schema: Option<String>,
    pub confirmed: bool,
    pub fetch_all: bool,
    pub saved_query_id: Option<String>,
    /// The database to run on; `None` is the profile's own.
    #[serde(default)]
    pub database: Option<String>,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Outcome {
    Rows {
        columns: Vec<ResultColumn>,
        rows: Vec<Vec<Option<String>>>,
        truncated: bool,
        /// As in `RawOutcome::Rows`.
        #[serde(skip_serializing_if = "Option::is_none")]
        cell_types: Option<Vec<String>>,
    },
    /// Verb INSERT/UPDATE/DELETE/MERGE/REPLACE, or a MongoDB insert, update, replace or delete.
    Affected { rows: u64 },
    /// Any other command.
    Done,
    /// `position`: 0-based UTF-16 offset in `RunRequest::sql`.
    Error { message: String, code: Option<String>, position: Option<usize> },
    /// SQLSTATE 57014: the user pressed Stop, or a server `statement_timeout` fired.
    Canceled,
    /// Not run: an earlier statement failed or was canceled.
    Skipped,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StatementResult {
    pub index: usize,
    /// UTF-16 span in `RunRequest::sql`.
    pub from: usize,
    pub to: usize,
    pub verb: String,
    pub kind: StatementKind,
    /// As written, with `:names`.
    pub sql: String,
    /// Only the names this statement uses.
    pub params: ParamValues,
    pub outcome: Outcome,
    pub elapsed_ms: u64,
    pub notices: Vec<Notice>,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum RunEvent {
    Started { index: usize, total: usize, from: usize, to: usize, verb: String },
    Finished { result: StatementResult },
}

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PendingWrite {
    pub verb: String,
    /// First 120 chars of the statement, whitespace collapsed.
    pub preview: String,
}

/// Results travel only in `Finished` events. `Completed` carries no rows, so a large
/// result is never sent twice.
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum RunOutcome {
    Completed { statement_count: usize, started_at: i64, tx_status: TxStatus },
    NeedsConfirmation { statements: Vec<PendingWrite> },
}

/// What `runner::run` returns to the command layer: the IPC outcome plus the results, for history.
#[derive(Clone, Debug, PartialEq)]
pub struct RunReport {
    pub outcome: RunOutcome,
    pub statements: Vec<StatementResult>,
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Answers `run_simple` from a script; everything else is unused.
    struct Probe(Mutex<Option<RawOutcome>>);

    #[async_trait]
    impl QuerySession for Probe {
        async fn run(&self, _: &str, _: &[Option<String>], _: usize) -> StatementRun {
            unreachable!()
        }
        async fn run_simple(&self, _: &str) -> RawOutcome {
            self.0.lock().unwrap().take().unwrap()
        }
        async fn cancel(&self) -> AppResult<()> {
            Ok(())
        }
        async fn tx_status(&self) -> TxStatus {
            TxStatus::Unknown
        }
        fn is_closed(&self) -> bool {
            false
        }
        async fn set_search_path(&self, _: Option<&str>) -> AppResult<()> {
            Ok(())
        }
        async fn close(&self) {}
    }

    fn text(v: &str) -> RawOutcome {
        RawOutcome::Rows { columns: vec![], rows: vec![vec![Some(v.into())]], truncated: false, cell_types: None }
    }

    #[tokio::test]
    async fn default_in_transaction_reads_the_probe() {
        let probe = |o| Probe(Mutex::new(Some(o)));
        assert!(probe(text("t")).in_transaction().await.unwrap());
        assert!(!probe(text("f")).in_transaction().await.unwrap());
        let failed = RawOutcome::Error { message: "aborted".into(), code: Some("25P02".into()), position: None };
        assert!(probe(failed.clone()).in_transaction().await.unwrap());
        assert!(in_transaction(&probe(failed)).await.unwrap());
        assert!(probe(text("x")).in_transaction().await.is_err());
    }
}
