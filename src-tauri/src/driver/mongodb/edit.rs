//! Grid saves on MongoDB.
//!
//! A save is planned into `MongoOp`s with the SQL planner's checks (`edit::check_rows`, the
//! columns' `read_only`), every value a `Bson` value parsed by the cell's own code or, for a cell
//! that was null and for an inserted document, by the column's dominant type.
//!
//! - **Transactions** (a replica set or mongos): one session, one transaction, all or nothing.
//!   A write conflict fails at once (the server's transaction lock wait is 5 ms) with
//!   `MONGO_SAVE_LOCKED`.
//! - **No transactions** (a standalone server): one raw `update` / `delete` / `insert` command
//!   at a time, each with `maxTimeMS` (a plain write to a document held by a transaction
//!   would otherwise block for up to 60 s); running out of it (code 50) is `MONGO_SAVE_LOCKED`
//!   too. A failure after the first change reports exactly how many changes stayed saved
//!   (`AppError::PartlySaved`).
//!
//! The whole save is limited to `SAVE_LIMIT` either way.

use std::future::{Future, IntoFuture};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use mongodb::bson::{doc, Bson, Document};
use mongodb::error::{Error as MongoError, ErrorKind, TRANSIENT_TRANSACTION_ERROR, UNKNOWN_TRANSACTION_COMMIT_RESULT};
use mongodb::{ClientSession, Collection, Database};

use super::values::{parse_cell, parse_new, shell};
use super::{connection_error, server_error};
use crate::driver::postgres::{COMMIT_UNCERTAIN, SAVE_TIMED_OUT};
use crate::edit::{check_rows, editable_values, key_columns, row_changed_error};
use crate::error::{AppError, AppResult};
use crate::model::{CellType, CellValues, ChangeSet, ColumnInfo, RowChange, TableStructure};

/// How long a whole save may take (`SAVE_TIMED_OUT` says 30 s).
const SAVE_LIMIT: Duration = Duration::from_secs(30);
/// The `maxTimeMS` of each write on a server without transactions; code 50 when it passes.
const WRITE_TIME_LIMIT_MS: u64 = 5000;
/// How long past a write's `maxTimeMS` its answer is waited for, so the server's own refusal
/// (code 50) arrives before the write is given up as unanswered.
const ANSWER_GRACE: Duration = Duration::from_secs(1);
/// How long the abort after a failed write in a transaction may take before the write's own
/// error is shown anyway (the dropped session aborts it then).
const ABORT_WAIT: Duration = Duration::from_secs(2);

pub const READ_ONLY_REFUSAL: &str =
    "This connection is read-only. Turn read-only off in the connection settings to edit.";
/// An edited cell of a type the grid shows but can't write back (`x`, `b`).
pub const VALUE_NOT_EDITABLE: &str = "Values of this type can't be edited yet.";
pub const VALIDATION_FAILED: &str = "The document doesn't match the collection's validation rules.";
/// A write on a server without transactions that got no answer, when it was the first.
pub const WRITE_UNCERTAIN: &str =
    "The connection failed while saving, so the change may or may not have been applied. Reload before saving again.";
/// A document held by another session's transaction: a write conflict on a replica set, or
/// a write that ran out of its `maxTimeMS` on a standalone server. A MongoDB query tab never
/// holds a transaction, so the SQL drivers' copy ("an open transaction in a query tab?") can't
/// be right here.
pub const MONGO_SAVE_LOCKED: &str = "Another session's transaction is holding this document. Try again in a moment.";
/// A key that came without its cell's type: its text alone can't tell an ObjectId from a string.
pub const KEY_WITHOUT_TYPE: &str = "The row's key came without its type, so it can't name one document.";

const DUPLICATE_KEY: i32 = 11000;
/// The server's code for a document the collection's validator refused.
pub(super) const DOCUMENT_VALIDATION_FAILURE: i32 = 121;
const UNAUTHORIZED: i32 = 13;
const MAX_TIME_EXPIRED: i32 = 50;
const WRITE_CONFLICT: i32 = 112;
const NO_SUCH_TRANSACTION: i32 = 251;

/// One write of a save.
#[derive(Clone, Debug, PartialEq)]
pub enum MongoOp {
    /// `$set` of `set` on the document whose `_id` is `id`; it must match exactly one.
    Update { id: Bson, set: Document, key_label: String },
    /// The document whose `_id` is `id`; it must be there.
    Delete { id: Bson, key_label: String },
    /// A new document; the server (or the driver) gives it an `_id`.
    Insert { doc: Document },
}

/// The type `parse_new` reads a column's new values as: its dominant type, when that is one
/// `parse_new` knows. An all-null column (`"null"`) or any other name has none.
fn column_type(c: &ColumnInfo) -> Option<&str> {
    const PARSEABLE: [&str; 10] =
        ["int32", "int64", "double", "decimal", "string", "bool", "objectId", "date", "object", "array"];
    PARSEABLE.contains(&c.data_type.as_str()).then_some(c.data_type.as_str())
}

/// An edited value: NULL stays null; a cell that was null (or whose type the page didn't send)
/// takes the column's type; any other cell keeps its own. `x` and `b` cells can't be written.
fn edited_value(c: &ColumnInfo, text: Option<&str>, code: Option<CellType>) -> AppResult<Bson> {
    let Some(text) = text else { return Ok(Bson::Null) };
    match code {
        None | Some(CellType::Null) => Ok(parse_new(text, column_type(c))),
        Some(CellType::Other | CellType::Blob) => Err(AppError::Query(VALUE_NOT_EDITABLE.into())),
        Some(code) => parse_cell(text, code, &c.name).map_err(AppError::Query),
    }
}

/// A new document's value: by the column's type.
fn new_value(c: &ColumnInfo, text: Option<&str>) -> Bson {
    text.map_or(Bson::Null, |t| parse_new(t, column_type(c)))
}

/// The changed columns of `values`, unknown and read-only ones refused, in column order (the
/// order of `$set` and of a new document's fields).
fn columns_in_order<'a>(
    structure: &'a TableStructure,
    values: &'a CellValues,
) -> AppResult<Vec<(&'a ColumnInfo, Option<&'a str>)>> {
    let mut cols = editable_values(structure, values)?;
    let at = |c: &ColumnInfo| structure.columns.iter().position(|x| x.name == c.name);
    cols.sort_by_key(|(c, _)| at(c));
    Ok(cols)
}

/// The `_id` a change names: its text, parsed by its cell's code (an ObjectId, the string with
/// the same hex, an int64, a UUID or a document each find their own document).
fn key_id(
    structure: &TableStructure,
    key: &[String],
    types: &std::collections::BTreeMap<String, CellType>,
) -> AppResult<(Bson, String)> {
    let key = key_columns(structure, key)?;
    let [(c, text)] = key.as_slice() else {
        return Err(AppError::Query("Row key doesn't match the primary key".into()));
    };
    let code = types.get(&c.name).copied().ok_or_else(|| AppError::Query(KEY_WITHOUT_TYPE.into()))?;
    let id = parse_cell(text, code, &c.name).map_err(AppError::Query)?;
    let label = format!("{} {}", c.name, shell(&id));
    Ok((id, label))
}

/// The save's writes, each with the index of its change in `set` (an update with nothing to set
/// is skipped). Validates the whole set first: any problem fails it and plans nothing.
pub(crate) fn plan_indexed(structure: &TableStructure, set: &ChangeSet) -> AppResult<Vec<(usize, MongoOp)>> {
    check_rows(structure, set)?;
    let mut out = Vec::new();
    for (i, change) in set.changes.iter().enumerate() {
        let op = match change {
            RowChange::Update { key, values, original_types } => {
                let (id, key_label) = key_id(structure, key, original_types)?;
                let cols = columns_in_order(structure, values)?;
                if cols.is_empty() {
                    continue;
                }
                let mut set = Document::new();
                for (c, v) in cols {
                    set.insert(c.name.clone(), edited_value(c, v, original_types.get(&c.name).copied())?);
                }
                MongoOp::Update { id, set, key_label }
            }
            RowChange::Delete { key, original_types } => {
                let (id, key_label) = key_id(structure, key, original_types)?;
                MongoOp::Delete { id, key_label }
            }
            RowChange::Insert { values } => {
                let doc = columns_in_order(structure, values)?.into_iter().map(|(c, v)| (c.name.clone(), new_value(c, v))).collect();
                MongoOp::Insert { doc }
            }
        };
        out.push((i, op));
    }
    Ok(out)
}

/// The save's writes, in order.
pub fn plan(structure: &TableStructure, set: &ChangeSet) -> AppResult<Vec<MongoOp>> {
    Ok(plan_indexed(structure, set)?.into_iter().map(|(_, op)| op).collect())
}

/// One preview line per write, in mongosh syntax. Display only.
pub fn preview(collection: &str, ops: &[MongoOp]) -> Vec<String> {
    let coll = format!("db.getCollection({})", serde_json::Value::String(collection.into()));
    ops.iter()
        .map(|op| match op {
            MongoOp::Update { id, set, .. } => format!(
                "{coll}.updateOne({}, {{ $set: {} }})",
                shell(&Bson::Document(doc! { "_id": id.clone() })),
                shell(&Bson::Document(set.clone()))
            ),
            MongoOp::Delete { id, .. } => {
                format!("{coll}.deleteOne({})", shell(&Bson::Document(doc! { "_id": id.clone() })))
            }
            MongoOp::Insert { doc } => format!("{coll}.insertOne({})", shell(&Bson::Document(doc.clone()))),
        })
        .collect()
}

/// A write the server refused, by its code: a duplicate key is the server's own message; a
/// failed validator and a missing privilege get the app's copy (code 13's text repeats the
/// command with its values, so it is never shown); a write that waited past `maxTimeMS` (on a
/// document held by a transaction) or lost a write conflict is the lock copy.
fn write_message(code: i32, message: String, ns: &str) -> AppError {
    AppError::Query(match code {
        DUPLICATE_KEY => message,
        DOCUMENT_VALIDATION_FAILURE => VALIDATION_FAILED.into(),
        UNAUTHORIZED => format!("Not authorized to write to {ns}."),
        MAX_TIME_EXPIRED | WRITE_CONFLICT => MONGO_SAVE_LOCKED.into(),
        _ => message,
    })
}

/// A failed write or transaction step on a replica set.
fn transaction_error(e: MongoError, ns: &str) -> AppError {
    let transient = e.contains_label(TRANSIENT_TRANSACTION_ERROR);
    transaction_failure(e, transient, ns)
}

/// A server error that is a write conflict (112), the abort it causes (251), or any other the
/// server labels a transient transaction error (`transient`) is the lock copy. Without a server
/// error (a dropped connection, no server to select) it is the connection's message: the driver
/// labels those transient too inside a transaction, but nothing holds the document.
fn transaction_failure(e: MongoError, transient: bool, ns: &str) -> AppError {
    match server_error(&e) {
        Some((WRITE_CONFLICT | NO_SUCH_TRANSACTION, _)) => AppError::Query(MONGO_SAVE_LOCKED.into()),
        Some(_) if transient => AppError::Query(MONGO_SAVE_LOCKED.into()),
        Some((code, message)) => write_message(code, message, ns),
        None => connection_error(e),
    }
}

/// A failed commit: without the server's answer (`UnknownTransactionCommitResult`, or no server
/// error at all) the save may or may not have been applied.
fn commit_error(e: MongoError, ns: &str) -> AppError {
    if e.contains_label(UNKNOWN_TRANSACTION_COMMIT_RESULT) || (server_error(&e).is_none() && !e.contains_label(TRANSIENT_TRANSACTION_ERROR)) {
        eprintln!("apply_changes: commit failed: {}", e.kind);
        return AppError::Uncertain(COMMIT_UNCERTAIN.into());
    }
    transaction_error(e, ns)
}

/// One write inside the save's transaction.
async fn write_in_transaction(coll: &Collection<Document>, op: &MongoOp, s: &mut ClientSession, ns: &str) -> AppResult<()> {
    match op {
        MongoOp::Update { id, set, key_label } => {
            let r = coll
                .update_one(doc! { "_id": id.clone() }, doc! { "$set": set.clone() })
                .session(&mut *s)
                .await
                .map_err(|e| transaction_error(e, ns))?;
            if r.matched_count != 1 {
                return Err(row_changed_error(key_label));
            }
        }
        MongoOp::Delete { id, key_label } => {
            let r = coll.delete_one(doc! { "_id": id.clone() }).session(&mut *s).await.map_err(|e| transaction_error(e, ns))?;
            if r.deleted_count != 1 {
                return Err(row_changed_error(key_label));
            }
        }
        MongoOp::Insert { doc } => {
            coll.insert_one(doc.clone()).session(&mut *s).await.map_err(|e| transaction_error(e, ns))?;
        }
    }
    Ok(())
}

/// Every write in one transaction, then the commit. A failed write aborts it, for at most
/// `ABORT_WAIT` (the abort's own error is dropped: the server has usually aborted already, and
/// the write's error is the one to show). `committing` is set just before the commit is sent.
async fn save_in_transaction(
    coll: &Collection<Document>,
    ops: &[(usize, MongoOp)],
    ns: &str,
    committing: &AtomicBool,
) -> AppResult<usize> {
    let mut s = coll.client().start_session().await.map_err(connection_error)?;
    s.start_transaction().await.map_err(|e| transaction_error(e, ns))?;
    for (_, op) in ops {
        if let Err(e) = write_in_transaction(coll, op, &mut s, ns).await {
            let _ = tokio::time::timeout(ABORT_WAIT, s.abort_transaction()).await;
            return Err(e);
        }
    }
    committing.store(true, Ordering::SeqCst);
    s.commit_transaction().await.map_err(|e| commit_error(e, ns))?;
    Ok(ops.len())
}

/// The raw command of one write, with its `maxTimeMS`.
fn command(collection: &str, op: &MongoOp, max_time_ms: u64) -> Document {
    let max_time_ms = i64::try_from(max_time_ms).unwrap_or(i64::MAX);
    match op {
        MongoOp::Update { id, set, .. } => doc! {
            "update": collection,
            "updates": [{ "q": { "_id": id.clone() }, "u": { "$set": set.clone() } }],
            "ordered": true,
            "maxTimeMS": max_time_ms,
        },
        MongoOp::Delete { id, .. } => doc! {
            "delete": collection,
            "deletes": [{ "q": { "_id": id.clone() }, "limit": 1 }],
            "ordered": true,
            "maxTimeMS": max_time_ms,
        },
        MongoOp::Insert { doc } => doc! {
            "insert": collection,
            "documents": [doc.clone()],
            "ordered": true,
            "maxTimeMS": max_time_ms,
        },
    }
}

/// A reply's number, whatever its integer type.
fn count(reply: &Document, field: &str) -> Option<i64> {
    match reply.get(field)? {
        Bson::Int32(n) => Some(i64::from(*n)),
        Bson::Int64(n) => Some(*n),
        Bson::Double(n) => Some(*n as i64),
        _ => None,
    }
}

/// The code and message of a reply's write error or write-concern error.
fn code_and_message(e: &Document) -> (i32, String) {
    let code = count(e, "code").and_then(|c| i32::try_from(c).ok()).unwrap_or_default();
    (code, e.get_str("errmsg").unwrap_or_default().to_string())
}

/// What one write's reply means (the server answers `ok: 1` and lists a refused write in it):
/// - its first write error: the write was refused;
/// - a write-concern error with `n` ≥ 1: the write was applied but not confirmed as asked, so its
///   outcome is unknown (`Uncertain`);
/// - a write-concern error otherwise: the server's message;
/// - a changed row when an update or delete didn't find its document (`n` is the matched count,
///   so an unchanged value still counts).
fn reply_outcome(reply: &Document, op: &MongoOp, ns: &str) -> AppResult<()> {
    let write_error = reply.get_array("writeErrors").ok().and_then(|errors| errors.iter().find_map(Bson::as_document));
    if let Some(e) = write_error {
        let (code, message) = code_and_message(e);
        return Err(write_message(code, message, ns));
    }
    if let Ok(e) = reply.get_document("writeConcernError") {
        let (code, message) = code_and_message(e);
        if count(reply, "n").unwrap_or(0) >= 1 {
            eprintln!("apply_changes: a write was applied, but its write concern failed: {code} {message}");
            return Err(AppError::Uncertain(WRITE_UNCERTAIN.into()));
        }
        return Err(write_message(code, message, ns));
    }
    match op {
        MongoOp::Update { key_label, .. } | MongoOp::Delete { key_label, .. } if count(reply, "n") != Some(1) => {
            Err(row_changed_error(key_label))
        }
        _ => Ok(()),
    }
}

/// A failed write command: the server's refusal by its code; a failure before anything was sent
/// (no server, a refused login) is the connection's; one after the command may have been sent
/// (the connection broke) leaves it unknown whether that write was applied.
fn command_error(e: MongoError, ns: &str) -> AppError {
    if let Some((code, message)) = server_error(&e) {
        return write_message(code, message, ns);
    }
    match e.kind.as_ref() {
        ErrorKind::Io(_) | ErrorKind::ConnectionPoolCleared { .. } | ErrorKind::InvalidResponse { .. } => {
            eprintln!("apply_changes: a write got no answer: {}", e.kind);
            AppError::Uncertain(WRITE_UNCERTAIN.into())
        }
        _ => connection_error(e),
    }
}

/// The error of a save without transactions that stopped at the change with index `change` of
/// `total`: the error itself when nothing was saved before it, otherwise the partial-save copy
/// with the number of changes before it, which stay saved. A write with an unknown outcome
/// (`Uncertain`) after saved changes is a partial save too, so the UI drops the saved ones (and
/// doesn't apply them twice), with copy saying the next one may or may not have been saved, and
/// `uncertain` set so the UI asks before saving again.
pub(crate) fn stopped_at(change: usize, total: usize, error: AppError) -> AppError {
    if change == 0 {
        return error;
    }
    if matches!(error, AppError::Uncertain(_)) {
        return AppError::PartlySaved {
            message: format!(
                "Saved {change} of {total} changes, then the connection failed, so the next one may or may not have been saved. Reload before saving again."
            ),
            applied: change,
            uncertain: true,
        };
    }
    AppError::PartlySaved {
        message: format!(
            "Saved {change} of {total} changes, then: {} This server has no transactions, so the saved changes stay saved; the rest are still pending.",
            error.message()
        ),
        applied: change,
        uncertain: false,
    }
}

/// The timing of a save without transactions.
struct Limits {
    /// The whole save (`SAVE_LIMIT`).
    save: Duration,
    /// Each write's `maxTimeMS` (`WRITE_TIME_LIMIT_MS`).
    write_ms: u64,
    /// The client's server-selection timeout: how long the driver looks for the server before it
    /// sends anything.
    selection: Duration,
    /// How long past those an answer is still waited for (`ANSWER_GRACE`).
    grace: Duration,
}

/// How long one write's answer is waited for: its `maxTimeMS` or the server-selection timeout,
/// whichever is longer, plus the grace, so the server's own refusal (code 50) or the driver's
/// "no server" arrives before the write is given up as unanswered. Within the save's limit: at
/// most the time `left`, or `max_time` plus the grace when that is longer.
fn answer_wait(max_time: Duration, limits: &Limits, left: Duration) -> Duration {
    (max_time.max(limits.selection) + limits.grace).min(left.max(max_time + limits.grace))
}

/// The writes one at a time, in order, each sent by `write` (the raw command with the given
/// `maxTimeMS`), within the save's limit. A write near the limit gets only the time left, and
/// running out of it is the time-out copy, not the lock copy.
async fn one_by_one<W, F>(ops: &[(usize, MongoOp)], total: usize, ns: &str, limits: &Limits, mut write: W) -> AppResult<usize>
where
    W: FnMut(&MongoOp, u64) -> F,
    F: Future<Output = mongodb::error::Result<Document>>,
{
    let deadline = Instant::now() + limits.save;
    for (change, op) in ops {
        let left = deadline.saturating_duration_since(Instant::now());
        let max_time_ms = limits.write_ms.min(left.as_millis() as u64);
        if max_time_ms == 0 {
            return Err(stopped_at(*change, total, AppError::Query(SAVE_TIMED_OUT.into())));
        }
        let wait = answer_wait(Duration::from_millis(max_time_ms), limits, left);
        let outcome = match tokio::time::timeout(wait, write(op, max_time_ms)).await {
            // No answer, even past its limit: the write may have been applied.
            Err(_) => {
                eprintln!("apply_changes: a write got no answer within {wait:?}");
                Err(AppError::Uncertain(WRITE_UNCERTAIN.into()))
            }
            Ok(Err(e)) => Err(command_error(e, ns)),
            Ok(Ok(reply)) => reply_outcome(&reply, op, ns),
        };
        let outcome = match outcome {
            Err(AppError::Query(m)) if m == MONGO_SAVE_LOCKED && max_time_ms < limits.write_ms => {
                Err(AppError::Query(SAVE_TIMED_OUT.into()))
            }
            other => other,
        };
        outcome.map_err(|e| stopped_at(*change, total, e))?;
    }
    Ok(ops.len())
}

/// `one_by_one` with raw commands on `db.collection`.
async fn save_one_by_one(
    db: &Database,
    collection: &str,
    ops: &[(usize, MongoOp)],
    total: usize,
    ns: &str,
    selection: Duration,
) -> AppResult<usize> {
    let limits = Limits { save: SAVE_LIMIT, write_ms: WRITE_TIME_LIMIT_MS, selection, grace: ANSWER_GRACE };
    one_by_one(ops, total, ns, &limits, |op, max_time_ms| db.run_command(command(collection, op, max_time_ms)).into_future())
        .await
}

/// Runs the planned writes of a save of `total` changes on `db.collection`: in one transaction
/// when the server has them, otherwise one by one (`selection` is the client's server-selection
/// timeout).
pub(crate) async fn apply(
    db: &Database,
    collection: &str,
    ops: &[(usize, MongoOp)],
    total: usize,
    transactional: bool,
    selection: Duration,
) -> AppResult<usize> {
    let ns = format!("{}.{collection}", db.name());
    if !transactional {
        return save_one_by_one(db, collection, ops, total, &ns, selection).await;
    }
    let coll = db.collection::<Document>(collection);
    let committing = AtomicBool::new(false);
    match tokio::time::timeout(SAVE_LIMIT, save_in_transaction(&coll, ops, &ns, &committing)).await {
        Ok(result) => result,
        // A commit left running may yet be applied. Otherwise the dropped session aborts the
        // transaction.
        Err(_) if committing.load(Ordering::SeqCst) => Err(AppError::Uncertain(COMMIT_UNCERTAIN.into())),
        Err(_) => Err(AppError::Query(SAVE_TIMED_OUT.into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Editing, RelationKind, TableRef};
    use mongodb::bson::oid::ObjectId;
    use std::collections::BTreeMap;

    fn col(name: &str, data_type: &str) -> ColumnInfo {
        let read_only = name == "_id" || name.contains('.') || name.starts_with('$');
        ColumnInfo {
            name: name.into(),
            data_type: data_type.into(),
            full_type: data_type.into(),
            cast_type: data_type.into(),
            nullable: true,
            default: None,
            is_primary_key: name == "_id",
            read_only,
        }
    }

    fn users() -> TableStructure {
        TableStructure {
            columns: vec![
                col("_id", "objectId"),
                col("name", "string"),
                col("age", "int32"),
                col("gone", "null"),
                col("blob", "binary"),
                col("a.b", "int32"),
            ],
            primary_key: vec!["_id".into()],
            relation: RelationKind::Table,
            editing: Editing::Allowed,
            indexes: vec![],
            foreign_keys: vec![],
        }
    }

    const ANN: &str = "650000000000000000000001";

    fn types(pairs: &[(&str, CellType)]) -> BTreeMap<String, CellType> {
        pairs.iter().map(|(k, t)| (k.to_string(), *t)).collect()
    }

    fn update(key: &str, vals: &[(&str, Option<&str>, CellType)]) -> RowChange {
        let mut original = types(&[("_id", CellType::ObjectId)]);
        original.extend(vals.iter().map(|(k, _, t)| (k.to_string(), *t)));
        RowChange::Update {
            key: vec![key.into()],
            values: vals.iter().map(|(k, v, _)| (k.to_string(), v.map(String::from))).collect(),
            original_types: original,
        }
    }

    fn insert(vals: &[(&str, Option<&str>)]) -> RowChange {
        RowChange::Insert { values: vals.iter().map(|(k, v)| (k.to_string(), v.map(String::from))).collect() }
    }

    fn set(changes: Vec<RowChange>) -> ChangeSet {
        ChangeSet { table: TableRef { schema: "fx".into(), name: "users".into() }, changes }
    }

    fn plan_err(changes: Vec<RowChange>) -> String {
        match plan(&users(), &set(changes)) {
            Err(AppError::Query(m)) => m,
            other => panic!("expected a query error, got {other:?}"),
        }
    }

    fn ann() -> Bson {
        Bson::ObjectId(ObjectId::parse_str(ANN).unwrap())
    }

    #[test]
    fn mongo_plan_parses_by_code_and_by_column_type() {
        let ops = plan(
            &users(),
            &set(vec![
                // A cell with a code keeps it; a null cell takes the column's type; NULL is null.
                update(ANN, &[("age", Some("31"), CellType::Long), ("name", None, CellType::Text)]),
                update(ANN.replace('1', "2").as_str(), &[("age", Some("7"), CellType::Null)]),
                // An all-null column ("null") has no type: integer text is an int32.
                insert(&[("name", Some("Gus")), ("age", Some("52")), ("gone", Some("5")), ("blob", Some("6"))]),
            ]),
        )
        .unwrap();
        assert_eq!(
            ops[0],
            MongoOp::Update {
                id: ann(),
                set: doc! { "name": Bson::Null, "age": 31i64 },
                key_label: format!(r#"_id ObjectId("{ANN}")"#),
            }
        );
        let MongoOp::Update { set: second, .. } = &ops[1] else { panic!("{:?}", ops[1]) };
        assert_eq!(second, &doc! { "age": 7i32 });
        let MongoOp::Insert { doc: new } = &ops[2] else { panic!("{:?}", ops[2]) };
        assert_eq!(new, &doc! { "name": "Gus", "age": 52i32, "gone": 5i32, "blob": 6i32 });
        let keys: Vec<&str> = new.keys().map(String::as_str).collect();
        assert_eq!(keys, ["name", "age", "gone", "blob"], "column order");
    }

    #[test]
    fn mongo_plan_keys_by_their_code() {
        let key = |text: &str, code: CellType| {
            let change = RowChange::Delete { key: vec![text.into()], original_types: types(&[("_id", code)]) };
            match plan(&users(), &set(vec![change])).unwrap().remove(0) {
                MongoOp::Delete { id, key_label } => (id, key_label),
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(key(ANN, CellType::ObjectId).0, ann());
        assert_eq!(key(ANN, CellType::Text), (Bson::String(ANN.into()), format!(r#"_id "{ANN}""#)));
        assert_eq!(key("9007199254740993", CellType::Long).0, Bson::Int64(9007199254740993));
        assert_eq!(key(r#"{"k":1}"#, CellType::Json).0, Bson::Document(doc! { "k": 1 }));
        // A UUID key is an `x` cell: keys parse by any code, values don't.
        let uuid = key(r#"{"$binary":{"base64":"ESIzRFVmd4iZqrvM3e7/AA==","subType":"04"}}"#, CellType::Other).0;
        assert!(matches!(uuid, Bson::Binary(_)), "{uuid:?}");
    }

    #[test]
    fn mongo_plan_refuses_what_sql_refuses() {
        // Not editable.
        let mut view = users();
        view.editing = Editing::View;
        assert!(plan(&view, &set(vec![insert(&[])])).is_err());
        // An empty key, the same row twice, a key without its type.
        let null_key = RowChange::Delete { key: vec!["".into()], original_types: types(&[("_id", CellType::Null)]) };
        assert_eq!(plan_err(vec![null_key]), crate::edit::EMPTY_KEY);
        let twice = || update(ANN, &[("name", Some("x"), CellType::Text)]);
        assert_eq!(plan_err(vec![twice(), twice()]), "The same row appears more than once in one save.");
        let untyped = RowChange::Delete { key: vec![ANN.into()], original_types: BTreeMap::new() };
        assert_eq!(plan_err(vec![untyped]), KEY_WITHOUT_TYPE);
        // The same hex as a string and as an ObjectId are two rows.
        let as_text = RowChange::Delete { key: vec![ANN.into()], original_types: types(&[("_id", CellType::Text)]) };
        let as_oid = RowChange::Delete { key: vec![ANN.into()], original_types: types(&[("_id", CellType::ObjectId)]) };
        assert_eq!(plan(&users(), &set(vec![as_text, as_oid])).unwrap().len(), 2);
        // Unknown and read-only columns, on an update or an insert.
        assert_eq!(plan_err(vec![update(ANN, &[("nope", Some("1"), CellType::Integer)])]), "Unknown column nope");
        assert_eq!(plan_err(vec![update(ANN, &[("a.b", Some("1"), CellType::Integer)])]), "a.b can't be edited");
        assert_eq!(plan_err(vec![insert(&[("_id", Some("1"))])]), "_id can't be edited");
        // `x` and `b` cells.
        for code in [CellType::Other, CellType::Blob] {
            assert_eq!(plan_err(vec![update(ANN, &[("blob", Some("AQ=="), code)])]), VALUE_NOT_EDITABLE);
        }
        // A value that doesn't parse as its cell's type.
        assert_eq!(
            plan_err(vec![update(ANN, &[("age", Some("abc"), CellType::Integer)])]),
            r#"age: expected a whole number (int32), got "abc"."#
        );
    }

    #[test]
    fn mongo_plan_skips_empty_updates_and_keeps_change_indexes() {
        let empty = RowChange::Update { key: vec![ANN.into()], values: CellValues::new(), original_types: types(&[("_id", CellType::ObjectId)]) };
        let planned = plan_indexed(&users(), &set(vec![empty, insert(&[])])).unwrap();
        assert_eq!(planned, [(1, MongoOp::Insert { doc: Document::new() })]);
    }

    #[test]
    fn mongo_preview_is_mongosh() {
        let ops = [
            MongoOp::Update { id: ann(), set: doc! { "age": 31, "first name": "A" }, key_label: String::new() },
            MongoOp::Delete { id: Bson::Int64(5), key_label: String::new() },
            MongoOp::Insert { doc: doc! { "name": "Gus" } },
        ];
        assert_eq!(
            preview("my \"coll\"", &ops),
            [
                format!(r#"db.getCollection("my \"coll\"").updateOne({{ _id: ObjectId("{ANN}") }}, {{ $set: {{ age: 31, "first name": "A" }} }})"#),
                r#"db.getCollection("my \"coll\"").deleteOne({ _id: NumberLong("5") })"#.to_string(),
                r#"db.getCollection("my \"coll\"").insertOne({ name: "Gus" })"#.to_string(),
            ]
        );
    }

    #[test]
    fn mongo_write_errors_get_the_plan_copy() {
        let q = |m: &str| AppError::Query(m.into());
        let dup = r#"E11000 duplicate key error collection: fx.users index: email_1 dup key: { email: "bob@example.com" }"#;
        assert_eq!(write_message(11000, dup.into(), "fx.users"), q(dup));
        assert_eq!(write_message(121, "Document failed validation".into(), "fx.users"), q(VALIDATION_FAILED));
        let echo = r#"not authorized on fx to execute command { update: "users", updates: [ { u: { $set: { pw: "s3cret" } } } ] }"#;
        assert_eq!(write_message(13, echo.into(), "fx.users"), q("Not authorized to write to fx.users."));
        assert_eq!(write_message(50, "operation exceeded time limit".into(), "fx.users"), q(MONGO_SAVE_LOCKED));
        assert_eq!(write_message(112, "Write conflict".into(), "fx.users"), q(MONGO_SAVE_LOCKED));
        assert_eq!(write_message(28, "Cannot create field".into(), "fx.users"), q("Cannot create field"));
    }

    #[test]
    fn mongo_replies_are_read_for_counts_and_write_errors() {
        let label = r#"_id ObjectId("x")"#;
        let up = MongoOp::Update { id: Bson::Int32(1), set: doc! { "a": 1 }, key_label: label.into() };
        let ins = MongoOp::Insert { doc: doc! {} };
        // Matched 1, modified 0: saved.
        assert_eq!(reply_outcome(&doc! { "n": 1, "nModified": 0, "ok": 1.0 }, &up, "fx.t"), Ok(()));
        assert_eq!(reply_outcome(&doc! { "n": 0i64, "nModified": 0, "ok": 1.0 }, &up, "fx.t"), Err(row_changed_error(label)));
        assert_eq!(reply_outcome(&doc! { "n": 1, "ok": 1.0 }, &ins, "fx.t"), Ok(()));
        let refused = doc! { "n": 0, "ok": 1.0, "writeErrors": [{ "index": 0, "code": 11000, "errmsg": "E11000 dup" }] };
        assert_eq!(reply_outcome(&refused, &ins, "fx.t"), Err(AppError::Query("E11000 dup".into())));
        // A write-concern error after the write was applied (n ≥ 1): its outcome is unknown, even
        // with an empty `writeErrors` beside it.
        let concern = doc! { "n": 1, "ok": 1.0, "writeConcernError": { "code": 64, "errmsg": "waiting for replication timed out" } };
        assert_eq!(reply_outcome(&concern, &ins, "fx.t"), Err(AppError::Uncertain(WRITE_UNCERTAIN.into())));
        let mut empty_errors = concern.clone();
        empty_errors.insert("writeErrors", Bson::Array(vec![]));
        assert_eq!(reply_outcome(&empty_errors, &up, "fx.t"), Err(AppError::Uncertain(WRITE_UNCERTAIN.into())));
        // Nothing applied (n 0): the server's message.
        let unapplied = doc! { "n": 0, "ok": 1.0, "writeConcernError": { "code": 64, "errmsg": "waiting for replication timed out" } };
        assert_eq!(reply_outcome(&unapplied, &up, "fx.t"), Err(AppError::Query("waiting for replication timed out".into())));
        // A write error wins over a write-concern error.
        let mut both = refused.clone();
        both.insert("writeConcernError", doc! { "code": 64, "errmsg": "waiting for replication timed out" });
        assert_eq!(reply_outcome(&both, &ins, "fx.t"), Err(AppError::Query("E11000 dup".into())));
    }

    fn server(code: i32, message: &str) -> MongoError {
        let e: mongodb::error::CommandError =
            mongodb::bson::deserialize_from_document(doc! { "code": code, "codeName": "X", "errmsg": message }).unwrap();
        ErrorKind::Command(e).into()
    }

    fn io(kind: std::io::ErrorKind) -> MongoError {
        ErrorKind::Io(std::sync::Arc::new(std::io::Error::from(kind))).into()
    }

    /// Inside a transaction the driver labels a dropped connection transient too: that is the
    /// connection's message, not the lock copy.
    #[test]
    fn mongo_transaction_errors_are_locks_only_from_the_server() {
        let locked = AppError::Query(MONGO_SAVE_LOCKED.into());
        for transient in [false, true] {
            assert_eq!(transaction_failure(server(112, "Write conflict"), transient, "fx.t"), locked);
            assert_eq!(transaction_failure(server(251, "Transaction aborted"), transient, "fx.t"), locked);
        }
        // Another server error the server labels transient.
        assert_eq!(transaction_failure(server(91, "shutting down"), true, "fx.t"), locked);
        assert_eq!(transaction_failure(server(11000, "E11000 dup"), false, "fx.t"), AppError::Query("E11000 dup".into()));
        assert_eq!(transaction_failure(server(13, "echo"), false, "fx.t"), AppError::Query("Not authorized to write to fx.t.".into()));
        // No server error: the connection's, labelled or not.
        for transient in [false, true] {
            let e = transaction_failure(io(std::io::ErrorKind::ConnectionReset), transient, "fx.t");
            assert!(matches!(e, AppError::Connection(_)), "{e:?}");
            let e = transaction_failure(ErrorKind::Shutdown.into(), transient, "fx.t");
            assert!(matches!(e, AppError::Connection(_)), "{e:?}");
        }
    }

    type Reply = mongodb::error::Result<Document>;

    enum Step {
        Answer(Reply),
        /// No answer at all.
        Hang,
    }

    fn ok() -> Step {
        Step::Answer(Ok(doc! { "n": 1, "nModified": 1, "ok": 1.0 }))
    }

    /// A write step that answers each write with the next step.
    fn scripted(steps: Vec<Step>) -> impl FnMut(&MongoOp, u64) -> futures_util::future::BoxFuture<'static, Reply> {
        let mut steps = steps.into_iter();
        move |_, _| match steps.next().expect("a write past the script") {
            Step::Answer(r) => Box::pin(std::future::ready(r)),
            Step::Hang => Box::pin(std::future::pending()),
        }
    }

    fn quick() -> Limits {
        Limits { save: Duration::from_secs(10), write_ms: 20, selection: Duration::ZERO, grace: Duration::from_millis(20) }
    }

    /// Updates for the changes with these indexes in a save of `total`.
    fn updates(indexes: &[usize]) -> Vec<(usize, MongoOp)> {
        indexes
            .iter()
            .map(|i| (*i, MongoOp::Update { id: Bson::Int32(*i as i32), set: doc! { "a": 1 }, key_label: format!("_id {i}") }))
            .collect()
    }

    async fn run(indexes: &[usize], total: usize, steps: Vec<Step>) -> AppResult<usize> {
        one_by_one(&updates(indexes), total, "fx.t", &quick(), scripted(steps)).await
    }

    fn unknown_after(applied: usize, total: usize) -> AppError {
        AppError::PartlySaved {
            message: format!(
                "Saved {applied} of {total} changes, then the connection failed, so the next one may or may not have been saved. Reload before saving again."
            ),
            applied,
            uncertain: true,
        }
    }

    /// Through the loop itself: a write with an unknown outcome (no answer in time, a broken
    /// connection, an unconfirmed write concern) is `Uncertain` when it was the first, otherwise
    /// a partial save of the changes before it, marked uncertain.
    #[tokio::test]
    async fn mongo_one_by_one_reports_unknown_outcomes() {
        let uncertain = AppError::Uncertain(WRITE_UNCERTAIN.into());
        assert_eq!(run(&[0, 1, 2], 3, vec![ok(), ok(), ok()]).await, Ok(3));
        // No answer within the wait (the timeout arm).
        assert_eq!(run(&[0, 1, 2], 3, vec![Step::Hang]).await, Err(uncertain.clone()));
        assert_eq!(run(&[0, 1, 2], 3, vec![ok(), Step::Hang]).await, Err(unknown_after(1, 3)));
        // A connection that broke after the write was sent.
        let reset = || Step::Answer(Err(io(std::io::ErrorKind::ConnectionReset)));
        assert_eq!(run(&[0, 1, 2], 3, vec![reset()]).await, Err(uncertain.clone()));
        assert_eq!(run(&[0, 1, 2], 3, vec![ok(), ok(), reset()]).await, Err(unknown_after(2, 3)));
        // Applied, but its write concern failed.
        let concern = || Step::Answer(Ok(doc! { "n": 1, "ok": 1.0, "writeConcernError": { "code": 64, "errmsg": "w" } }));
        assert_eq!(run(&[0, 1], 2, vec![concern()]).await, Err(uncertain.clone()));
        assert_eq!(run(&[0, 1], 2, vec![ok(), concern()]).await, Err(unknown_after(1, 2)));
        // `applied` is the failing change's index in the set (skipped empty updates count).
        assert_eq!(run(&[1, 3], 4, vec![ok(), Step::Hang]).await, Err(unknown_after(3, 4)));
        // A known refusal after a saved change is a plain partial save.
        let dup = Step::Answer(Ok(doc! { "n": 0, "ok": 1.0, "writeErrors": [{ "index": 0, "code": 11000, "errmsg": "E11000 dup" }] }));
        assert!(matches!(
            run(&[0, 1], 2, vec![ok(), dup]).await,
            Err(AppError::PartlySaved { applied: 1, uncertain: false, .. })
        ));
    }

    #[test]
    fn mongo_answer_waits_cover_server_selection() {
        let ms = Duration::from_millis;
        let limits = |selection: u64| Limits { save: SAVE_LIMIT, write_ms: WRITE_TIME_LIMIT_MS, selection: ms(selection), grace: ANSWER_GRACE };
        let left = ms(30_000);
        // maxTimeMS or the server-selection timeout, whichever is longer, plus the grace.
        assert_eq!(answer_wait(ms(5000), &limits(2000), left), ms(6000));
        assert_eq!(answer_wait(ms(5000), &limits(10_000), left), ms(11_000));
        // Within the save's limit: the time left, or maxTimeMS plus the grace when longer.
        assert_eq!(answer_wait(ms(5000), &limits(10_000), ms(7000)), ms(7000));
        assert_eq!(answer_wait(ms(3000), &limits(10_000), ms(3000)), ms(4000));
    }

    /// A server that can't be reached (a closed port) is the connection's message: the answer is
    /// waited for past the driver's server-selection timeout, not just past `maxTimeMS`.
    #[tokio::test]
    async fn mongo_an_unreachable_server_is_a_connection_error() {
        let mut o = mongodb::options::ClientOptions::default();
        o.hosts = vec![mongodb::options::ServerAddress::Tcp { host: "127.0.0.1".into(), port: Some(1) }];
        o.direct_connection = Some(true);
        o.server_selection_timeout = Some(Duration::from_millis(300));
        let client = mongodb::Client::with_options(o).unwrap();
        let db = client.database("fx");
        let limits = Limits { selection: Duration::from_millis(300), ..quick() };
        let started = Instant::now();
        let r = one_by_one(&updates(&[0]), 1, "fx.t", &limits, |op, ms| db.run_command(command("t", op, ms)).into_future()).await;
        assert!(matches!(r, Err(AppError::Connection(_))), "{r:?}");
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
        // Waiting only past maxTimeMS gives the write up as unanswered first.
        let r = one_by_one(&updates(&[0]), 1, "fx.t", &quick(), |op, ms| db.run_command(command("t", op, ms)).into_future()).await;
        assert_eq!(r, Err(AppError::Uncertain(WRITE_UNCERTAIN.into())));
    }

    #[test]
    fn mongo_partial_save_copy() {
        let error = AppError::Query("E11000 dup".into());
        assert_eq!(stopped_at(0, 3, error.clone()), error);
        assert_eq!(
            stopped_at(1, 3, error),
            AppError::PartlySaved {
                message: "Saved 1 of 3 changes, then: E11000 dup This server has no transactions, so the saved changes stay saved; the rest are still pending.".into(),
                applied: 1,
                uncertain: false,
            }
        );
        // A write with an unknown outcome: uncertain when it was the first, otherwise the saves
        // before it are kept.
        let unknown = AppError::Uncertain(WRITE_UNCERTAIN.into());
        assert_eq!(
            stopped_at(0, 3, unknown.clone()),
            AppError::Uncertain(
                "The connection failed while saving, so the change may or may not have been applied. Reload before saving again.".into()
            )
        );
        assert_eq!(
            stopped_at(2, 3, unknown),
            AppError::PartlySaved {
                message: "Saved 2 of 3 changes, then the connection failed, so the next one may or may not have been saved. Reload before saving again.".into(),
                applied: 2,
                uncertain: true,
            }
        );
    }

    #[test]
    fn mongo_lost_connections_leave_the_write_unknown() {
        let io: MongoError = ErrorKind::Io(std::sync::Arc::new(std::io::Error::from(std::io::ErrorKind::ConnectionReset))).into();
        let unknown = command_error(io, "fx.t");
        assert_eq!(unknown, AppError::Uncertain(WRITE_UNCERTAIN.into()));
        assert_eq!(stopped_at(0, 2, unknown.clone()), AppError::Uncertain(WRITE_UNCERTAIN.into()));
        assert!(
            matches!(stopped_at(1, 2, unknown), AppError::PartlySaved { applied: 1, uncertain: true, ref message } if message.starts_with("Saved 1 of 2 changes, then the connection failed"))
        );
        let unsent: MongoError = ErrorKind::Shutdown.into();
        assert!(matches!(command_error(unsent, "fx.t"), AppError::Connection(_)));
    }
}
