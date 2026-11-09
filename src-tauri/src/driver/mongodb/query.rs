//! A MongoDB query tab's session: runs the parsed statements of `mongoquery` on the driver's
//! client, in the tab's database.
//!
//! MongoDB tabs have no transactions of their own: the session is always idle, and it answers
//! only `COMMIT` and `ROLLBACK` (which `end_tab_transaction` sends) as SQL text.
//!
//! Stop: every run has its own `ClientSession`, and its operation carries the comment
//! `ts-run-<uuid v4>`. Stop drops the operation's future, then kills the session on the server
//! (`killSessions`), or, when that fails, the operations with the comment (`currentOp` and
//! `killOp`). Dropping alone stops a read but not a write, which runs to its end on the server.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::stream::{self, BoxStream};
use futures_util::{StreamExt, TryStreamExt};
use mongodb::action::Action;
use mongodb::bson::oid::ObjectId;
use mongodb::bson::{doc, Bson, Document};
use mongodb::error::{Error as MongoError, ErrorKind, InsertManyError, WriteFailure};
use mongodb::{Client, ClientSession, Collection, SessionCursor};
use tokio::sync::Notify;

use super::bulk::{bulk_batches, write_errors, BulkTally, BulkWriteError};
use super::edit::{DOCUMENT_VALIDATION_FAILURE, VALIDATION_FAILED};
use super::explain::{execution_failure, explain_command, explain_rows};
use super::{connection_error, lock, schema, server_error, values, UNAUTHORIZED};
use crate::error::{AppError, AppResult};
use crate::export::{ExportColumn, RowSink};
use crate::model::{row_types, CellType};
use crate::mongoquery::{ExplainVerbosity, FindAndAction, MongoCall, MongoQueryOp, UpdateSpec};
use crate::query::{Notice, QuerySession, RawOutcome, ResultColumn, StatementRun, TxStatus, CANCELED_CODE};
use crate::sqltext::StatementKind;

/// What `run` answers to anything but `COMMIT` and `ROLLBACK`.
const NO_SQL: &str = "A MongoDB tab runs MongoDB statements, not SQL.";
/// The code of a write on a read-only profile.
pub const READ_ONLY_CODE: &str = "MONGO_READ_ONLY";
/// The code of a statement stopped by the connection's statement time limit.
pub const TIMEOUT_CODE: &str = "MONGO_TIMEOUT";
/// `ResultColumn::data_type` of a column whose values have more than one type.
pub const MIXED_TYPE: &str = "mixed";
/// The notice of a stopped write: the server returns no counts for it.
const STOPPED_WRITE: &str = "Stopped. Some documents may already have been changed.";
/// The notice of a write the server may have applied without confirming it: a write-concern
/// error, or a connection that failed after the command may have been sent.
const WRITE_UNCONFIRMED: &str = "This write may or may not have been applied. Check before running it again.";
/// The server's codes for an operation it stopped: always a Stop (`killOp`, `killSessions`,
/// `killCursors`, a cursor gone after one).
const INTERRUPTED: i32 = 11601;
const CURSOR_KILLED: i32 = 237;
const CURSOR_NOT_FOUND: i32 = 43;
/// ClientDisconnect: a Stop only after one was asked for.
const CLIENT_DISCONNECT: i32 = 279;
/// NamespaceNotFound, which a count of a missing collection answers with 0 (as the driver's).
const NAMESPACE_NOT_FOUND: i32 = 26;
/// How much longer than its two bounded kills `close` waits for a stopped run to end.
const CLOSE_SLACK: Duration = Duration::from_secs(1);

pub struct MongoQuerySession {
    client: Client,
    /// The database statements run in: the tab's, from `set_search_path`.
    database: Mutex<String>,
    /// The database the session was opened on, which `set_search_path(None)` restores.
    opened_with: String,
    /// The profile's connect timeout (at least 1 s).
    timeout: Duration,
    /// Writes are refused, before anything is sent.
    read_only: bool,
    /// The profile's statement time limit: a statement still running after it is stopped as by
    /// Stop. `None` (the default) is no limit.
    statement_limit: Option<Duration>,
    /// The run in progress, which Stop signals.
    current: Mutex<Option<Arc<CurrentRun>>>,
    /// Set by `close`: nothing runs on the session after it (the rest of a run whose tab was
    /// closed, an export in progress).
    closed: AtomicBool,
}

/// A run in progress: what `cancel` and `close` signal, and what the server knows it by.
struct CurrentRun {
    /// Notified by Stop. `notify_one` keeps a permit, so a Stop that lands before the run waits
    /// on it still wins.
    stop: Notify,
    /// Stop was asked for: a server's ClientDisconnect (279) is a Stop then.
    stopped: AtomicBool,
    /// Notified when the run is over (with a kept permit), for `close` to wait on.
    ended: Notify,
    /// `ts-run-<uuid v4>`, set on the run's operation.
    comment: String,
}

impl CurrentRun {
    fn new() -> Self {
        Self {
            stop: Notify::new(),
            stopped: AtomicBool::new(false),
            ended: Notify::new(),
            comment: format!("ts-run-{}", uuid::Uuid::new_v4()),
        }
    }
}

/// The current run while it lasts: dropping it unregisters the run (unless another has taken
/// its place) and tells `close` it is over.
struct Registered<'a> {
    current: &'a Mutex<Option<Arc<CurrentRun>>>,
    run: Arc<CurrentRun>,
}

impl Drop for Registered<'_> {
    fn drop(&mut self) {
        {
            let mut current = lock(self.current);
            if current.as_ref().is_some_and(|r| Arc::ptr_eq(r, &self.run)) {
                *current = None;
            }
        }
        // A `close` already waiting is woken; one still to come gets the permit.
        self.run.ended.notify_waiters();
        self.run.ended.notify_one();
    }
}

impl MongoQuerySession {
    /// Public (hidden) for the Stop tests, which need the session's own type for its hooks.
    #[doc(hidden)]
    pub fn new(
        client: Client,
        database: String,
        timeout: Duration,
        read_only: bool,
        statement_limit: Option<Duration>,
    ) -> Self {
        Self {
            client,
            database: Mutex::new(database.clone()),
            opened_with: database,
            timeout,
            read_only,
            statement_limit,
            current: Mutex::new(None),
            closed: AtomicBool::new(false),
        }
    }

    /// The comment of the run in progress, once it has registered (it does before anything is
    /// sent). A test hook: to wait for the run, and to find its operation on the server.
    #[doc(hidden)]
    pub fn current_run_comment(&self) -> Option<String> {
        lock(&self.current).as_ref().map(|r| r.comment.clone())
    }

    fn database(&self) -> String {
        lock(&self.database).clone()
    }

    fn register(&self, run: Arc<CurrentRun>) -> Registered<'_> {
        *lock(&self.current) = Some(run.clone());
        Registered { current: &self.current, run }
    }

    /// Stops the run in progress, if any, and returns it.
    fn stop_current(&self) -> Option<Arc<CurrentRun>> {
        let run = lock(&self.current).clone()?;
        run.stopped.store(true, Ordering::SeqCst);
        run.stop.notify_one();
        Some(run)
    }

    /// `call` in a session of its own, until it ends, Stop, or the statement time limit. On Stop
    /// (or the limit) the operation's future is dropped (which stops a read, but not a write on
    /// the server), then the server kills what the session runs. The session lives until that
    /// kill has returned, so no other operation gets its id meanwhile. The limit's clock starts
    /// once the session is started; a result that is ready when the limit fires wins.
    async fn run_stoppable(&self, call: &MongoCall, row_cap: usize, run: &CurrentRun) -> (RawOutcome, Vec<Notice>) {
        let started = tokio::select! { biased;
            _ = run.stop.notified() => return stop_outcome(call, true, (canceled(), vec![])),
            s = self.client.start_session() => s,
        };
        let mut session = match started {
            Ok(s) => s,
            Err(e) => {
                let ns = format!("{}.{}", self.database(), call.collection);
                return (error_outcome(e, call.verb(), &ns), vec![]);
            }
        };
        let lsid = session.id().clone();
        let comment = Bson::String(run.comment.clone());
        let op = async {
            match call.kind() {
                StatementKind::Read => self.read(call, row_cap, &mut session, &comment).await,
                StatementKind::Write => self.write(call, row_cap, &mut session, &comment).await,
            }
        };
        // The limit, once it has passed; never without one.
        let deadline = async {
            match self.statement_limit {
                Some(limit) => {
                    tokio::time::sleep(limit).await;
                    limit
                }
                None => std::future::pending().await,
            }
        };
        let finished = tokio::select! { biased;
            _ = run.stop.notified() => Finished::Stopped,
            r = op => Finished::Ended(r),
            limit = deadline => Finished::OverLimit(limit),
        };
        let over_limit = match finished {
            Finished::Ended(result) => return stop_outcome(call, run.stopped.load(Ordering::SeqCst), result),
            Finished::Stopped => None,
            Finished::OverLimit(limit) => Some(limit),
        };
        let killed = kill_sessions(&self.client, &lsid, &run.comment, self.timeout).await;
        drop(session);
        stopped_outcome(call, over_limit, killed)
    }

    /// A read, in `session`, its operation carrying `comment`. Cursors are `SessionCursor`s,
    /// iterated in the session too.
    async fn read(
        &self,
        call: &MongoCall,
        row_cap: usize,
        session: &mut ClientSession,
        comment: &Bson,
    ) -> (RawOutcome, Vec<Notice>) {
        let database = self.database();
        let coll: Collection<Document> = self.client.database(&database).collection(&call.collection);
        let fail = |e: MongoError| (error_outcome(e, call.verb(), &format!("{database}.{}", call.collection)), vec![]);
        let docs: Vec<Document> = match &call.op {
            MongoQueryOp::Find { filter, projection, sort, skip, limit, one } => {
                let limit = find_limit(*one, *limit, row_cap);
                let found = coll
                    .find(filter.clone())
                    .optional(projection.clone(), |f, p| f.projection(p))
                    .optional(sort.clone(), |f, s| f.sort(s))
                    .optional(*skip, |f, n| f.skip(n))
                    .limit(limit)
                    .batch_size(u32::try_from(limit).unwrap_or(u32::MAX))
                    .comment(comment.clone())
                    .session(&mut *session)
                    .await;
                match collect(found, session).await {
                    Ok(docs) => docs,
                    Err(e) => return fail(e),
                }
            }
            MongoQueryOp::Aggregate { pipeline } => {
                let (pipeline, batch) = capped_pipeline(pipeline, row_cap);
                let found =
                    coll.aggregate(pipeline).batch_size(batch).comment(comment.clone()).session(&mut *session).await;
                match collect(found, session).await {
                    Ok(docs) => docs,
                    Err(e) => return fail(e),
                }
            }
            MongoQueryOp::CountDocuments { filter } => {
                match coll.count_documents(filter.clone()).comment(comment.clone()).session(session).await {
                    Ok(n) => vec![count_doc(n)],
                    Err(e) => return fail(e),
                }
            }
            // The driver's `estimated_document_count` takes no session: its `count` command,
            // run in the session.
            MongoQueryOp::EstimatedDocumentCount => {
                let command = doc! { "count": call.collection.as_str(), "comment": comment.clone() };
                match self.client.database(&database).run_command(command).session(session).await {
                    Ok(reply) => match reply_count(&reply) {
                        Some(n) => vec![count_doc(n)],
                        None => {
                            eprintln!("estimatedDocumentCount: a count reply without n: {reply}");
                            return (error(NO_COUNT), vec![]);
                        }
                    },
                    Err(e) if server_error(&e).is_some_and(|(code, _)| code == NAMESPACE_NOT_FOUND) => {
                        vec![count_doc(0)]
                    }
                    Err(e) => return fail(e),
                }
            }
            MongoQueryOp::Distinct { field, filter } => match coll
                .distinct(field, filter.clone())
                .comment(comment.clone())
                .session(session)
                .await
            {
                Ok(values) => {
                    let docs = values.into_iter().map(|v| doc! { field.as_str(): v }).collect();
                    return (rows_outcome(docs, row_cap, Some(field)), vec![]);
                }
                Err(e) => return fail(e),
            },
            // The first batch holds every index: a collection has at most 64. A missing
            // collection has none (as mongosh's `[]`).
            MongoQueryOp::GetIndexes => {
                let command = doc! { "listIndexes": call.collection.as_str(), "comment": comment.clone() };
                match self.client.database(&database).run_command(command).session(session).await {
                    Ok(reply) => match first_batch(&reply) {
                        Some(docs) => docs,
                        None => {
                            eprintln!("getIndexes: a listIndexes reply without a first batch: {reply}");
                            return (error(NO_INDEX_LIST), vec![]);
                        }
                    },
                    Err(e) if server_error(&e).is_some_and(|(code, _)| code == NAMESPACE_NOT_FOUND) => vec![],
                    Err(e) => return fail(e),
                }
            }
            // The winning plan as rows, with the stats or the "Planned only" notice.
            MongoQueryOp::Explain { of, verbosity } => {
                let Some(command) = explain_command(call, of, *verbosity, comment) else {
                    return (error(NOT_EXPLAINABLE), vec![]);
                };
                let reply = match self.client.database(&database).run_command(command).session(session).await {
                    Ok(reply) => reply,
                    Err(e) => return fail(e),
                };
                // A statement that failed while it ran is its error, as when it is run.
                if let Some((code, message)) = execution_failure(&reply) {
                    let outcome = match code {
                        Some(code) => coded_error(code, message, call.verb(), &format!("{database}.{}", call.collection)),
                        None => RawOutcome::Error { message, code: None, position: None },
                    };
                    return (outcome, vec![]);
                }
                let Some((docs, stats)) = explain_rows(&reply) else {
                    eprintln!("explain: a reply without a plan: {reply}");
                    return (error(NO_PLAN), vec![]);
                };
                let notice = match verbosity {
                    ExplainVerbosity::QueryPlanner => Some(PLANNED_ONLY.to_string()),
                    ExplainVerbosity::ExecutionStats | ExplainVerbosity::AllPlansExecution => {
                        if stats.is_none() {
                            eprintln!("explain: a {} reply without readable stats: {reply}", verbosity.as_str());
                        }
                        stats
                    }
                };
                return (rows_outcome(docs, row_cap, None), notice.into_iter().map(info).collect());
            }
            // `run_mongo` sends writes (and output pipelines) to `write`.
            MongoQueryOp::InsertOne { .. }
            | MongoQueryOp::InsertMany { .. }
            | MongoQueryOp::Update { .. }
            | MongoQueryOp::ReplaceOne { .. }
            | MongoQueryOp::Delete { .. }
            | MongoQueryOp::FindOneAnd { .. }
            | MongoQueryOp::BulkWrite { .. }
            | MongoQueryOp::Drop
            | MongoQueryOp::CreateCollection { .. }
            | MongoQueryOp::CreateIndex { .. }
            | MongoQueryOp::DropIndex { .. }
            | MongoQueryOp::RenameCollection { .. } => return (error(NOT_A_READ), vec![]),
        };
        (rows_outcome(docs, row_cap, None), vec![])
    }

    /// A write, in `session`, its operation carrying `comment`: its count and one notice, or the
    /// error (with its notices). A pipeline with an output stage is sent as written, with no
    /// `$limit` after its last stage. A findOneAnd* is the document it returns, as rows. A
    /// bulkWrite is one command per group of operations (`bulk.rs`), in order. The
    /// collection and index helpers are their commands, run as written; a reply that is ok but
    /// carries a `writeConcernError` adds `WRITE_UNCONFIRMED`.
    async fn write(
        &self,
        call: &MongoCall,
        row_cap: usize,
        session: &mut ClientSession,
        comment: &Bson,
    ) -> (RawOutcome, Vec<Notice>) {
        let database = self.database();
        let ns = format!("{database}.{}", call.collection);
        let coll: Collection<Document> = self.client.database(&database).collection(&call.collection);
        let done = |rows_affected: u64, notice: Option<String>| {
            (RawOutcome::Command { rows_affected }, notice.into_iter().map(info).collect())
        };
        match &call.op {
            MongoQueryOp::InsertOne { doc } => {
                match coll.insert_one(doc.clone()).comment(comment.clone()).session(session).await {
                    Ok(r) => done(1, Some(format!("Inserted 1 document (_id: {}).", values::shell(&r.inserted_id)))),
                    Err(e) => write_failure(e, call.verb(), &ns, 1, true),
                }
            }
            MongoQueryOp::InsertMany { docs, ordered } => {
                let n = docs.len();
                // Documents without an `_id` get an ObjectId here, first.
                let docs = docs.iter().cloned().map(with_id);
                match coll.insert_many(docs).ordered(*ordered).comment(comment.clone()).session(session).await {
                    Ok(_) => done(n as u64, Some(format!("Inserted {}.", plural_documents(n)))),
                    Err(e) => write_failure(e, call.verb(), &ns, n, *ordered),
                }
            }
            MongoQueryOp::Update { filter, update, upsert, many } => {
                let update = match (update, many) {
                    (UpdateSpec::Modifiers(m), false) => coll.update_one(filter.clone(), m.clone()),
                    (UpdateSpec::Modifiers(m), true) => coll.update_many(filter.clone(), m.clone()),
                    (UpdateSpec::Pipeline(p), false) => coll.update_one(filter.clone(), p.clone()),
                    (UpdateSpec::Pipeline(p), true) => coll.update_many(filter.clone(), p.clone()),
                };
                let result = update.upsert(*upsert).comment(comment.clone()).session(session).await;
                match result {
                    Ok(r) => update_done(r.matched_count, r.modified_count, r.upserted_id),
                    Err(e) => write_failure(e, call.verb(), &ns, 1, true),
                }
            }
            MongoQueryOp::ReplaceOne { filter, replacement, upsert } => {
                let replaced = coll
                    .replace_one(filter.clone(), replacement.clone())
                    .upsert(*upsert)
                    .comment(comment.clone())
                    .session(session)
                    .await;
                match replaced {
                    Ok(r) => update_done(r.matched_count, r.modified_count, r.upserted_id),
                    Err(e) => write_failure(e, call.verb(), &ns, 1, true),
                }
            }
            MongoQueryOp::Delete { filter, many } => {
                let result = if *many {
                    coll.delete_many(filter.clone()).comment(comment.clone()).session(session).await
                } else {
                    coll.delete_one(filter.clone()).comment(comment.clone()).session(session).await
                };
                match result {
                    Ok(r) => done(r.deleted_count, None),
                    Err(e) => write_failure(e, call.verb(), &ns, 1, true),
                }
            }
            MongoQueryOp::Aggregate { pipeline } => {
                // `$out` and `$merge` return an empty cursor, and have written by the time the
                // call returns.
                match coll.aggregate(pipeline.clone()).comment(comment.clone()).session(session).await {
                    Ok(_) => {
                        let target = call.output_stage().and_then(|s| output_target(s, &database));
                        let notice = match target {
                            Some(t) => format!("The pipeline wrote its output to {t}."),
                            None => "The pipeline wrote its output.".to_string(),
                        };
                        done(0, Some(notice))
                    }
                    Err(e) => write_failure(e, call.verb(), &ns, 1, true),
                }
            }
            // `projection` is `fields`; a delete is `remove`, which takes no `new` and no `upsert`.
            MongoQueryOp::FindOneAnd { filter, action, projection, sort, upsert, return_after } => {
                let mut command = doc! { "findAndModify": call.collection.as_str(), "query": filter.clone() };
                if let Some(sort) = sort {
                    command.insert("sort", sort.clone());
                }
                if let Some(projection) = projection {
                    command.insert("fields", projection.clone());
                }
                let update: Option<Bson> = match action {
                    FindAndAction::Update(UpdateSpec::Modifiers(m)) => Some(m.clone().into()),
                    FindAndAction::Update(UpdateSpec::Pipeline(p)) => Some(p.clone().into()),
                    FindAndAction::Replace(r) => Some(r.clone().into()),
                    FindAndAction::Delete => None,
                };
                match update {
                    Some(update) => {
                        command.insert("update", update);
                        command.insert("new", *return_after);
                        command.insert("upsert", *upsert);
                    }
                    None => {
                        command.insert("remove", true);
                    }
                }
                command.insert("comment", comment.clone());
                match self.client.database(&database).run_command(command).session(session).await {
                    Ok(reply) => find_and_modify_done(action, &reply, row_cap),
                    Err(e) => write_failure(e, call.verb(), &ns, 1, true),
                }
            }
            // A missing collection is no error: 6.0 fails with code 26, 8.0 replies `{ ok: 1 }`
            //. Every real drop replies with `ns`, a view's without `nIndexesWas`
            // (6.0 and 8.0, checked 2026-11-08).
            MongoQueryOp::Drop => {
                let command = doc! { "drop": call.collection.as_str(), "comment": comment.clone() };
                let missing = format!("There was no collection {ns} to drop.");
                match self.client.database(&database).run_command(command).session(session).await {
                    Ok(reply) if dropped(&reply) => {
                        helper_done(&reply, vec![format!("Dropped collection {ns}.")])
                    }
                    Ok(reply) => helper_done(&reply, vec![missing]),
                    Err(e) if server_error(&e).is_some_and(|(code, _)| code == NAMESPACE_NOT_FOUND) => {
                        (RawOutcome::Command { rows_affected: 0 }, vec![info(missing)])
                    }
                    Err(e) => write_failure(e, call.verb(), &ns, 1, true),
                }
            }
            MongoQueryOp::CreateCollection { options } => {
                let mut command = doc! { "create": call.collection.as_str() };
                command.extend(options.clone());
                command.insert("comment", comment.clone());
                match self.client.database(&database).run_command(command).session(session).await {
                    Ok(reply) => helper_done(&reply, vec![format!("Created collection {ns}.")]),
                    Err(e) => write_failure(e, call.verb(), &ns, 1, true),
                }
            }
            MongoQueryOp::CreateIndex { keys, name, options } => {
                let mut index = doc! { "key": keys.clone(), "name": name.as_str() };
                index.extend(options.clone());
                let command =
                    doc! { "createIndexes": call.collection.as_str(), "indexes": [index], "comment": comment.clone() };
                match self.client.database(&database).run_command(command).session(session).await {
                    Ok(reply) => helper_done(&reply, create_index_notices(&reply, name, &ns)),
                    Err(e) => write_failure(e, call.verb(), &ns, 1, true),
                }
            }
            MongoQueryOp::DropIndex { index } => {
                let command =
                    doc! { "dropIndexes": call.collection.as_str(), "index": index.clone(), "comment": comment.clone() };
                let name = match index {
                    Bson::String(name) => name.clone(),
                    keys => values::shell(keys),
                };
                match self.client.database(&database).run_command(command).session(session).await {
                    Ok(reply) => helper_done(&reply, vec![format!("Dropped index {name}.")]),
                    Err(e) => write_failure(e, call.verb(), &ns, 1, true),
                }
            }
            // On `admin`, with full names.
            MongoQueryOp::RenameCollection { to, drop_target } => {
                let target = format!("{database}.{to}");
                let command = doc! {
                    "renameCollection": ns.as_str(),
                    "to": target.as_str(),
                    "dropTarget": *drop_target,
                    "comment": comment.clone(),
                };
                match self.client.database("admin").run_command(command).session(session).await {
                    Ok(reply) => helper_done(&reply, vec![format!("Renamed {ns} to {target}.")]),
                    Err(e) => write_failure(e, call.verb(), &ns, 1, true),
                }
            }
            // One command per group of consecutive operations, each in the session; an
            // ordered bulk sends nothing after the first group with write errors.
            MongoQueryOp::BulkWrite { ops, ordered } => {
                let db = self.client.database(&database);
                let total = ops.len();
                let (mut tally, mut errors, mut unconfirmed) = (BulkTally::default(), Vec::new(), false);
                for (start, batch) in bulk_batches(ops) {
                    let command = batch.command(&call.collection, *ordered, comment);
                    match db.run_command(command).session(&mut *session).await {
                        Ok(reply) => {
                            // The write errors first: an unreadable reply adds nothing to the tally.
                            let read =
                                write_errors(&reply, start).and_then(|e| tally.add(batch.kind, &reply).map(|()| e));
                            let Ok(group_errors) = read else {
                                eprintln!("bulkWrite: a reply the app couldn't read: {reply}");
                                return bulk_unreadable(&tally);
                            };
                            unconfirmed |= reply.contains_key("writeConcernError");
                            errors.extend(group_errors);
                            if *ordered && !errors.is_empty() {
                                break;
                            }
                        }
                        Err(e) => {
                            let at = BulkStop { start, total, ordered: *ordered, failed: errors.len() };
                            return bulk_failed(e, &tally, &at, unconfirmed, call.verb(), &ns);
                        }
                    }
                }
                bulk_done(&tally, &errors, total, *ordered, unconfirmed, call.verb(), &ns)
            }
            // `run_mongo` sends reads to `read`.
            MongoQueryOp::Find { .. }
            | MongoQueryOp::CountDocuments { .. }
            | MongoQueryOp::EstimatedDocumentCount
            | MongoQueryOp::Distinct { .. }
            | MongoQueryOp::GetIndexes
            | MongoQueryOp::Explain { .. } => (error(NOT_A_WRITE), vec![]),
        }
    }
}

/// What `read` and `write` answer to a call of the other kind, which `run_mongo` never sends.
const NOT_A_READ: &str = "Internal error: a write was sent as a read.";
const NOT_A_WRITE: &str = "Internal error: a read was sent as a write.";

/// What an explain shows whose reply `explain_rows` finds no plan in.
const NO_PLAN: &str = "The server's explain reply had no plan in it.";
/// What an explain of anything but a find or an aggregate shows, which `parse` never gives.
const NOT_EXPLAINABLE: &str = "Internal error: only find and aggregate can be explained.";
/// The notice of an explain with the default verbosity, which doesn't run the statement.
const PLANNED_ONLY: &str = "Planned only. explain(\"executionStats\") also runs it and shows what it read.";

/// What a getIndexes whose reply `first_batch` can't read shows.
const NO_INDEX_LIST: &str = "The server's listIndexes reply had no indexes in it.";

/// The documents of a cursor reply's first batch; `None` when it has none.
fn first_batch(reply: &Document) -> Option<Vec<Document>> {
    let batch = reply.get_document("cursor").ok()?.get_array("firstBatch").ok()?;
    batch.iter().map(|d| d.as_document().cloned()).collect()
}

/// A findOneAnd*'s reply: its `value` (`null` when there is none) as at most one
/// row, and what `lastErrorObject` says happened. A reply without `lastErrorObject` (which the
/// server always sends) is logged, and `value` says it: a document is one changed.
fn find_and_modify_done(action: &FindAndAction, reply: &Document, row_cap: usize) -> (RawOutcome, Vec<Notice>) {
    let docs = match reply.get("value") {
        Some(Bson::Document(d)) => vec![d.clone()],
        _ => vec![],
    };
    let last = reply.get_document("lastErrorObject").ok();
    if last.is_none() {
        eprintln!("findAndModify: a reply without lastErrorObject: {reply}");
    }
    let notice = if let Some(id) = last.and_then(|l| l.get("upserted")) {
        format!("Upserted _id: {}.", values::shell(id))
    } else {
        let changed = match (last, action) {
            (None, _) => !docs.is_empty(),
            (Some(last), FindAndAction::Delete) => reply_count(last) == Some(1),
            (Some(last), _) => last.get_bool("updatedExisting").unwrap_or(false),
        };
        match (changed, action) {
            (false, _) => "No document matched.".to_string(),
            (true, FindAndAction::Update(_)) => "Updated 1 document.".to_string(),
            (true, FindAndAction::Replace(_)) => "Replaced 1 document.".to_string(),
            (true, FindAndAction::Delete) => "Deleted 1 document.".to_string(),
        }
    };
    let mut notices = vec![info(notice)];
    notices.extend(unconfirmed(reply));
    (rows_outcome(docs, row_cap, None), notices)
}

/// `WRITE_UNCONFIRMED` when an ok reply carries a `writeConcernError`: the command ran, but
/// wasn't confirmed as asked.
fn unconfirmed(reply: &Document) -> Option<Notice> {
    reply.contains_key("writeConcernError").then(|| warning(WRITE_UNCONFIRMED))
}

/// A bulkWrite whose groups were all answered (an ordered one: up to the first with write
/// errors): Done with the tally, or the first write error by operation order, with the tally
/// and what ran.
fn bulk_done(
    tally: &BulkTally,
    errors: &[BulkWriteError],
    total: usize,
    ordered: bool,
    unconfirmed: bool,
    verb: &str,
    ns: &str,
) -> (RawOutcome, Vec<Notice>) {
    let mut notices = vec![info(tally.notice())];
    let Some(first) = errors.iter().min_by_key(|e| e.index) else {
        notices.extend(unconfirmed.then(|| warning(WRITE_UNCONFIRMED)));
        return (RawOutcome::Command { rows_affected: tally.rows_affected() }, notices);
    };
    let outcome = coded_error(first.code, first.message.clone(), verb, ns);
    notices.push(info(if ordered {
        format!("Ran {} of {total} operations before the error.", first.index)
    } else {
        format!("{} of {total} operations failed; the others ran.", errors.len())
    }));
    if let (DOCUMENT_VALIDATION_FAILURE, Some(details)) = (first.code, &first.details) {
        notices.push(info(format!("Details: {}", values::ext_json(&Bson::Document(details.clone())))));
    }
    // A stopped write gets Stop's notice instead (`stop_outcome`).
    if unconfirmed && outcome != canceled() {
        notices.push(warning(WRITE_UNCONFIRMED));
    }
    (outcome, notices)
}

/// Where a bulkWrite's command failed: its group starts at operation `start` of `total`, and
/// `failed` operations of the groups before it had write errors (none, when ordered).
#[derive(Clone, Copy)]
struct BulkStop {
    start: usize,
    total: usize,
    ordered: bool,
    failed: usize,
}

/// A bulkWrite stopped by `e`, the failure of one group's command: the error, the tally of the
/// groups before it, what ran (ordered: the operations before the group; unordered: how many
/// of the earlier ones failed, and that the later groups weren't sent), and `WRITE_UNCONFIRMED`
/// when the failed group may have been applied (a connection failure) or an earlier reply was
/// unconfirmed.
fn bulk_failed(
    e: MongoError,
    tally: &BulkTally,
    at: &BulkStop,
    unconfirmed: bool,
    verb: &str,
    ns: &str,
) -> (RawOutcome, Vec<Notice>) {
    let (outcome, after) = write_failure(e, verb, ns, 1, true);
    let BulkStop { start, total, ordered, failed } = *at;
    let mut notices = vec![
        info(tally.notice()),
        info(if ordered {
            format!("Ran {start} of {total} operations before the error.")
        } else {
            format!("{failed} of {total} operations failed before the error; the rest weren't sent.")
        }),
    ];
    notices.extend(after);
    let warned = notices.iter().any(|n| n.message == WRITE_UNCONFIRMED);
    if unconfirmed && !warned && outcome != canceled() {
        notices.push(warning(WRITE_UNCONFIRMED));
    }
    (outcome, notices)
}

/// What a bulkWrite shows whose command reply couldn't be read (`UnreadableReply`).
const BULK_UNREADABLE: &str = "The server's bulkWrite reply couldn't be read.";

/// A bulkWrite stopped by a reply the app couldn't read: an error, the tally of the groups
/// before it, and `WRITE_UNCONFIRMED`, since what that group did is unknown.
fn bulk_unreadable(tally: &BulkTally) -> (RawOutcome, Vec<Notice>) {
    (error(BULK_UNREADABLE), vec![info(tally.notice()), warning(WRITE_UNCONFIRMED)])
}

/// Whether a `drop` reply that is ok dropped something: a collection's has `nIndexesWas` and
/// `ns`, a view's only `ns`; 8.0's for a missing name has neither.
fn dropped(reply: &Document) -> bool {
    reply.contains_key("ns") || reply.contains_key("nIndexesWas")
}

/// A collection or index helper that ran: Done, with `notices` and `unconfirmed`'s.
fn helper_done(reply: &Document, notices: Vec<String>) -> (RawOutcome, Vec<Notice>) {
    let mut notices: Vec<Notice> = notices.into_iter().map(info).collect();
    notices.extend(unconfirmed(reply));
    (RawOutcome::Command { rows_affected: 0 }, notices)
}

/// A createIndexes reply: the index created, or there already; and the collection,
/// when the index created it.
fn create_index_notices(reply: &Document, name: &str, ns: &str) -> Vec<String> {
    let mut notices = vec![if reply.contains_key("note") {
        format!("Index {name} already exists.")
    } else {
        format!("Created index {name} on {ns}.")
    }];
    if reply.get_bool("createdCollectionAutomatically").unwrap_or(false) {
        notices.push(format!("Created collection {ns} for it."));
    }
    notices
}

fn info(message: String) -> Notice {
    Notice { severity: "info".into(), message }
}

fn warning(message: &str) -> Notice {
    Notice { severity: "warning".into(), message: message.into() }
}

pub(super) fn plural_documents(n: usize) -> String {
    if n == 1 { "1 document".into() } else { format!("{n} documents") }
}

/// `doc` with an `ObjectId` `_id` first when it has none.
pub(super) fn with_id(doc: Document) -> Document {
    if doc.contains_key("_id") {
        return doc;
    }
    let mut with = doc! { "_id": ObjectId::new() };
    with.extend(doc);
    with
}

/// `{ matched, modified, upserted }` of an update or a replace.
fn update_done(matched: u64, modified: u64, upserted: Option<Bson>) -> (RawOutcome, Vec<Notice>) {
    let mut notice = format!("Matched {matched}, modified {modified}.");
    if let Some(id) = &upserted {
        notice.push_str(&format!(" Upserted _id: {}.", values::shell(id)));
    }
    let rows_affected = modified + u64::from(upserted.is_some());
    (RawOutcome::Command { rows_affected }, vec![info(notice)])
}

/// `<db>.<coll>` an output stage writes to: `$out: "c"` or `{ db, coll }`; `$merge: "c"` or
/// `{ into: "c" | { db, coll } }`; a missing `db` is the current one. `None` when the stage is shaped otherwise.
fn output_target(stage: &Document, current_db: &str) -> Option<String> {
    let named = |b: &Bson| match b {
        Bson::String(c) => Some(format!("{current_db}.{c}")),
        Bson::Document(d) => Some(format!("{}.{}", d.get_str("db").unwrap_or(current_db), d.get_str("coll").ok()?)),
        _ => None,
    };
    match (stage.get("$out"), stage.get("$merge")) {
        (Some(out), _) => named(out),
        (_, Some(Bson::Document(merge))) => named(merge.get("into")?),
        (_, Some(other)) => named(other),
        _ => None,
    }
}

/// A failed write, as `error_outcome`, with what the notices add: a validator's details (code
/// 121); for `insertMany` of `total` documents, how many went in before the error
/// (ordered: those before the first error; unordered: all but the failed ones); and
/// `WRITE_UNCONFIRMED` when the write may have been applied: a write-concern error (the write
/// ran, but wasn't confirmed as asked), or a connection that failed after the command may have
/// been sent (as the grid's save, `edit::command_error`).
fn write_failure(e: MongoError, verb: &str, ns: &str, total: usize, ordered: bool) -> (RawOutcome, Vec<Notice>) {
    let (code, message, details, inserted, unconfirmed) = match e.kind.as_ref() {
        // The first write error is the error; a write-concern error as well leaves the
        // documents that went in unconfirmed.
        ErrorKind::InsertMany(m) if m.write_errors.as_ref().is_some_and(|w| !w.is_empty()) => {
            let errors = m.write_errors.as_deref().unwrap_or_default();
            let first = &errors[0];
            let k = if ordered { first.index } else { total.saturating_sub(errors.len()) };
            (first.code, first.message.clone(), first.details.clone(), Some(k), m.write_concern_error.is_some())
        }
        // Every document went in, but the write concern wasn't met.
        ErrorKind::InsertMany(InsertManyError { write_concern_error: Some(w), .. }) => {
            (w.code, w.message.clone(), None, None, true)
        }
        ErrorKind::Write(WriteFailure::WriteError(w)) => (w.code, w.message.clone(), w.details.clone(), None, false),
        // The write ran on the primary, but wasn't confirmed as asked.
        ErrorKind::Write(WriteFailure::WriteConcernError(w)) => (w.code, w.message.clone(), None, None, true),
        ErrorKind::Io(_) | ErrorKind::ConnectionPoolCleared { .. } | ErrorKind::InvalidResponse { .. } => {
            eprintln!("query tab: {verb} on {ns} got no answer: {}", e.kind);
            return (error_outcome(e, verb, ns), vec![warning(WRITE_UNCONFIRMED)]);
        }
        _ => return (error_outcome(e, verb, ns), vec![]),
    };
    let outcome = coded_error(code, message, verb, ns);
    let mut notices = Vec::new();
    if let (DOCUMENT_VALIDATION_FAILURE, Some(details)) = (code, details) {
        notices.push(info(format!("Details: {}", values::ext_json(&Bson::Document(details)))));
    }
    if let Some(k) = inserted {
        notices.push(info(format!("Inserted {k} of {total} documents before the error.")));
    }
    // A stopped write gets Stop's notice instead (`stop_outcome`).
    if unconfirmed && outcome != canceled() {
        notices.push(warning(WRITE_UNCONFIRMED));
    }
    (outcome, notices)
}

/// The number of documents the user asked for: 1 for findOne; the limit, a negative one
/// counting as its absolute value (as mongosh's single batch); `None` for no limit or `0`.
pub(super) fn user_limit(one: bool, limit: Option<i64>) -> Option<u64> {
    if one {
        return Some(1);
    }
    limit.map(i64::unsigned_abs).filter(|n| *n > 0)
}

/// The limit a find sends, which is its batch size too: the user's (`user_limit`), at most the
/// cap plus one, which tells whether there were more; the cap plus one when there is none.
fn find_limit(one: bool, limit: Option<i64>, row_cap: usize) -> i64 {
    let fetch = i64::try_from(row_cap.saturating_add(1)).unwrap_or(i64::MAX);
    user_limit(one, limit).map_or(fetch, |n| i64::try_from(n).unwrap_or(i64::MAX).min(fetch))
}

/// The limit an export's find sends: the user's (`user_limit`), with no cap; `0` (none) when
/// there is none.
fn export_limit(one: bool, limit: Option<i64>) -> i64 {
    user_limit(one, limit).map_or(0, |n| i64::try_from(n).unwrap_or(i64::MAX))
}

/// A read pipeline with `{ $limit: cap+1 }` appended, and the batch size that returns it in one
/// round trip (cap+2).
fn capped_pipeline(pipeline: &[Document], row_cap: usize) -> (Vec<Document>, u32) {
    let fetch = row_cap.saturating_add(1);
    let mut capped = pipeline.to_vec();
    capped.push(doc! { "$limit": i64::try_from(fetch).unwrap_or(i64::MAX) });
    (capped, u32::try_from(fetch.saturating_add(1)).unwrap_or(u32::MAX))
}

/// The `comment` of an export's read, which a test finds in `$currentOp`.
pub const EXPORT_COMMENT: &str = "ts-query-export";
/// How many documents a CSV export reads before it fixes its columns (as the grid's sample).
const CSV_COLUMN_SAMPLE: usize = 1_000;

/// What an export of a tab that was closed meanwhile fails with.
const TAB_CLOSED: &str = "The query tab was closed.";

/// The documents of a `find`, an aggregation or a `distinct` (one `{ <field>: value }` per
/// value), streamed to `sink`: no cap. A find or an aggregation goes through a plain cursor;
/// dropping the future drops the cursor (`killCursors`), which is how an export is canceled.
/// A JSON sink takes each whole document; a CSV sink gets the columns of the first
/// `CSV_COLUMN_SAMPLE` documents (as in Results), and a field first seen later is left out; a
/// distinct's one column is its field, even with no values. Stops once `closed` is set.
async fn stream_documents(
    client: &Client,
    database: &str,
    call: &MongoCall,
    sink: &mut dyn RowSink,
    closed: &AtomicBool,
) -> AppResult<()> {
    if !call.is_exportable() {
        return Err(AppError::Query(crate::export::run::EXPORT_NOT_A_READ.into()));
    }
    let is_closed = || closed.load(Ordering::SeqCst).then(|| AppError::Query(TAB_CLOSED.into()));
    if let Some(e) = is_closed() {
        return Err(e);
    }
    let ns = format!("{database}.{}", call.collection);
    let coll: Collection<Document> = client.database(database).collection(&call.collection);
    let comment = Bson::String(EXPORT_COMMENT.into());
    let fail = |e: MongoError| export_error(e, call.verb(), &ns);
    let mut field = None;
    let mut docs: BoxStream<'_, mongodb::error::Result<Document>> = match &call.op {
        MongoQueryOp::Find { filter, projection, sort, skip, limit, one } => coll
            .find(filter.clone())
            .optional(projection.clone(), |f, p| f.projection(p))
            .optional(sort.clone(), |f, s| f.sort(s))
            .optional(*skip, |f, n| f.skip(n))
            .limit(export_limit(*one, *limit))
            .comment(comment)
            .await
            .map_err(fail)?
            .boxed(),
        // `is_exportable`: a read pipeline.
        MongoQueryOp::Aggregate { pipeline } => coll.aggregate(pipeline.clone()).comment(comment).await.map_err(fail)?.boxed(),
        MongoQueryOp::Distinct { field: name, filter } => {
            let values = coll.distinct(name, filter.clone()).comment(comment).await.map_err(fail)?;
            field = Some(name.as_str());
            stream::iter(values.into_iter().map(move |v| Ok(doc! { name.as_str(): v }))).boxed()
        }
        MongoQueryOp::CountDocuments { .. }
        | MongoQueryOp::EstimatedDocumentCount
        | MongoQueryOp::InsertOne { .. }
        | MongoQueryOp::InsertMany { .. }
        | MongoQueryOp::Update { .. }
        | MongoQueryOp::ReplaceOne { .. }
        | MongoQueryOp::Delete { .. }
        | MongoQueryOp::FindOneAnd { .. }
        | MongoQueryOp::BulkWrite { .. }
        | MongoQueryOp::Drop
        | MongoQueryOp::CreateCollection { .. }
        | MongoQueryOp::CreateIndex { .. }
        | MongoQueryOp::DropIndex { .. }
        | MongoQueryOp::GetIndexes
        | MongoQueryOp::RenameCollection { .. }
        | MongoQueryOp::Explain { .. } => return Err(AppError::Query(crate::export::run::EXPORT_NOT_A_READ.into())),
    };

    let documents = sink.wants_documents();
    let mut sample: Vec<Document> = Vec::new();
    let mut columns: Option<Vec<ResultColumn>> = None;
    if documents {
        sink.begin(&[])?;
    }
    let write = |sink: &mut dyn RowSink, columns: &[ResultColumn], d: Document| -> AppResult<()> {
        if documents {
            return sink.document(&values::ext_json(&Bson::Document(d)));
        }
        let (texts, codes): (Vec<Option<String>>, Vec<CellType>) =
            columns.iter().map(|c| values::cell(d.get(&c.name))).unzip();
        sink.typed_row(texts, Some(&row_types(&codes)))
    };
    let begin = |sink: &mut dyn RowSink, docs: &[Document]| -> AppResult<Vec<ResultColumn>> {
        let mut cols = result_columns(docs);
        if let (true, Some(name)) = (cols.is_empty(), field) {
            cols.push(ResultColumn { name: name.into(), data_type: schema::NO_TYPE.into() });
        }
        let export: Vec<ExportColumn> =
            cols.iter().map(|c| ExportColumn { name: c.name.clone(), data_type: c.data_type.clone() }).collect();
        sink.begin(&export)?;
        Ok(cols)
    };
    while let Some(d) = docs.try_next().await.map_err(fail)? {
        if let Some(e) = is_closed() {
            return Err(e);
        }
        if documents {
            write(sink, &[], d)?;
        } else if let Some(cols) = &columns {
            write(sink, cols, d)?;
        } else {
            sample.push(d);
            if sample.len() == CSV_COLUMN_SAMPLE {
                let cols = begin(sink, &sample)?;
                for d in sample.drain(..) {
                    write(sink, &cols, d)?;
                }
                columns = Some(cols);
            }
        }
    }
    if !documents && columns.is_none() {
        let cols = begin(sink, &sample)?;
        for d in sample {
            write(sink, &cols, d)?;
        }
    }
    Ok(())
}

/// An export's failure: a server error keeps its message (`server_message`), an operation the
/// server stopped included, since the export's own cancel drops the stream instead and never
/// gets here (so someone else stopped it: a `killOp`, a cursor the server dropped); anything
/// else is the connection's, of kind `Connection`. `AppError` has no room for the code.
fn export_error(e: MongoError, verb: &str, ns: &str) -> AppError {
    match server_error(&e) {
        Some((code, message)) => AppError::Query(server_message(code, message, verb, ns)),
        None => connection_error(e),
    }
}

/// Every document of `cursor`, read in the session that opened it.
async fn collect(
    cursor: mongodb::error::Result<SessionCursor<Document>>,
    session: &mut ClientSession,
) -> mongodb::error::Result<Vec<Document>> {
    cursor?.stream(session).try_collect().await
}

/// `n` of a `count` command's reply; `None` when it has none, or one that isn't a count.
fn reply_count(reply: &Document) -> Option<u64> {
    match reply.get("n") {
        Some(Bson::Int32(n)) => u64::try_from(*n).ok(),
        Some(Bson::Int64(n)) => u64::try_from(*n).ok(),
        Some(Bson::Double(n)) if *n >= 0.0 && n.fract() == 0.0 => Some(*n as u64),
        _ => None,
    }
}

/// What a count whose reply `reply_count` can't read shows.
const NO_COUNT: &str = "The server's count reply had no count in it.";

fn count_doc(n: u64) -> Document {
    doc! { "count": i64::try_from(n).unwrap_or(i64::MAX) }
}

/// At most `row_cap` of `docs` as rows, `truncated` when there were more, in the columns of
/// `result_columns`. `field` is the one column even when there is no document.
fn rows_outcome(mut docs: Vec<Document>, row_cap: usize, field: Option<&str>) -> RawOutcome {
    let truncated = docs.len() > row_cap;
    docs.truncate(row_cap);
    let mut columns = result_columns(&docs);
    if let (true, Some(name)) = (columns.is_empty(), field) {
        columns.push(ResultColumn { name: name.into(), data_type: schema::NO_TYPE.into() });
    }
    let (mut rows, mut types) = (Vec::with_capacity(docs.len()), Vec::with_capacity(docs.len()));
    for d in &docs {
        let (texts, codes): (Vec<Option<String>>, Vec<CellType>) =
            columns.iter().map(|c| values::cell(d.get(&c.name))).unzip();
        rows.push(texts);
        types.push(row_types(&codes));
    }
    RawOutcome::Rows { columns, rows, truncated, cell_types: Some(types) }
}

/// The union of the documents' top-level fields, in first-seen order. A column's type is its
/// values' `values::type_name` when they all agree, `MIXED_TYPE` when they don't, and
/// `schema::NO_TYPE` when every value is null or missing.
pub(crate) fn result_columns(docs: &[Document]) -> Vec<ResultColumn> {
    // Each field with its values' type so far: `None` until a non-null value.
    let mut fields: Vec<(&str, Option<&'static str>)> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    for d in docs {
        for (key, value) in d {
            let at = *index.entry(key.as_str()).or_insert_with(|| {
                fields.push((key, None));
                fields.len() - 1
            });
            if matches!(value, Bson::Null) {
                continue;
            }
            let seen = &mut fields[at].1;
            *seen = match *seen {
                None => Some(values::type_name(value)),
                Some(t) if t == values::type_name(value) => Some(t),
                Some(_) => Some(MIXED_TYPE),
            };
        }
    }
    fields
        .into_iter()
        .map(|(name, t)| ResultColumn { name: name.to_string(), data_type: t.unwrap_or(schema::NO_TYPE).to_string() })
        .collect()
}

/// A failed statement: a server error keeps its message and its code as `MONGO_<n>`, except
/// code 13, whose text repeats the command, and code 121 (`coded_error`); anything else is the
/// connection's failure.
fn error_outcome(e: MongoError, verb: &str, ns: &str) -> RawOutcome {
    match server_error(&e) {
        Some((code, message)) => coded_error(code, message, verb, ns),
        None => RawOutcome::Error { message: connection_error(e).message().to_string(), code: None, position: None },
    }
}

/// A server error: its message and `MONGO_<code>`, except code 13, whose text repeats the
/// command, code 121, which gets the app's copy, and an operation the server stopped, which is
/// `canceled`.
fn coded_error(code: i32, message: String, verb: &str, ns: &str) -> RawOutcome {
    if matches!(code, INTERRUPTED | CURSOR_KILLED | CURSOR_NOT_FOUND) {
        return canceled();
    }
    RawOutcome::Error { message: server_message(code, message, verb, ns), code: Some(format!("MONGO_{code}")), position: None }
}

/// A server error's text: its message, except code 13, whose text repeats the command, and
/// code 121, which gets the app's copy.
fn server_message(code: i32, message: String, verb: &str, ns: &str) -> String {
    match code {
        UNAUTHORIZED => format!("Not authorized to run {verb} on {ns}."),
        DOCUMENT_VALIDATION_FAILURE => VALIDATION_FAILED.to_string(),
        _ => message,
    }
}

/// How a run's operation ended in `run_stoppable`: by itself, by Stop, or by the statement
/// time limit (the limit's).
enum Finished {
    Ended((RawOutcome, Vec<Notice>)),
    Stopped,
    OverLimit(Duration),
}

/// The warning of a run stopped (by Stop or the limit) whose kill the server didn't confirm:
/// neither `killSessions` nor the comment fallback succeeded.
const NOT_KILLED: &str =
    "The server didn't confirm the stop, so the statement may still be running. Check before running it again.";

/// A run stopped by Stop (`over_limit` `None`) or by the statement time limit, once its kill
/// has returned: Canceled or the limit's error, a write's warning, and `NOT_KILLED` when the
/// kill wasn't confirmed (`killed` false).
fn stopped_outcome(call: &MongoCall, over_limit: Option<Duration>, killed: bool) -> (RawOutcome, Vec<Notice>) {
    let (outcome, mut notices) = match over_limit {
        Some(limit) => timed_out(call, limit),
        None => stop_outcome(call, true, (canceled(), vec![])),
    };
    if !killed {
        notices.push(warning(NOT_KILLED));
    }
    (outcome, notices)
}

/// A run stopped by the statement time limit `limit`: an error, not `canceled`, so the runner
/// skips the rest as after any error; a write says documents may have changed.
fn timed_out(call: &MongoCall, limit: Duration) -> (RawOutcome, Vec<Notice>) {
    let outcome = RawOutcome::Error {
        message: format!("Stopped after {} s, the connection's statement time limit.", limit.as_secs()),
        code: Some(TIMEOUT_CODE.into()),
        position: None,
    };
    let notices = match call.kind() {
        StatementKind::Write => vec![warning(STOPPED_WRITE)],
        StatementKind::Read => vec![],
    };
    (outcome, notices)
}

/// A stopped run.
fn canceled() -> RawOutcome {
    RawOutcome::Error { message: "Canceled".into(), code: Some(CANCELED_CODE.into()), position: None }
}

/// A run's result as Stop sees it: a canceled one (or, after a Stop, the server's
/// ClientDisconnect) is `canceled`, and a canceled write says documents may have changed.
fn stop_outcome(
    call: &MongoCall,
    stopped: bool,
    (outcome, mut notices): (RawOutcome, Vec<Notice>),
) -> (RawOutcome, Vec<Notice>) {
    let disconnected = format!("MONGO_{CLIENT_DISCONNECT}");
    let is_canceled = match &outcome {
        RawOutcome::Error { code: Some(code), .. } => code == CANCELED_CODE || (stopped && *code == disconnected),
        _ => false,
    };
    if !is_canceled {
        return (outcome, notices);
    }
    if matches!(call.kind(), StatementKind::Write) {
        notices.push(warning(STOPPED_WRITE));
    }
    (canceled(), notices)
}

/// Kills what the session `lsid` runs on the server: `killSessions`, or, when that fails or
/// takes longer than `timeout`, `kill_by_comment`. Whether the server confirmed a kill; a
/// failure is logged, and the caller tells the user (`NOT_KILLED`).
async fn kill_sessions(client: &Client, lsid: &Document, comment: &str, timeout: Duration) -> bool {
    let admin = client.database("admin");
    match tokio::time::timeout(timeout, admin.run_command(doc! { "killSessions": [lsid.clone()] })).await {
        Ok(Ok(_)) => return true,
        Ok(Err(e)) => eprintln!("Stop: killSessions failed, killing by comment instead: {e}"),
        Err(_) => eprintln!("Stop: killSessions took over {} s, killing by comment instead", timeout.as_secs()),
    }
    kill_by_comment(client, comment, timeout).await
}

/// The fallback of `kill_sessions` (killSessions isn't on every server): the user's own
/// operations with `comment` (`currentOp` with `$ownOps`), each killed with `killOp`, all
/// within `timeout`. The run's future is already dropped, so no later batch or getMore gets a
/// new opid. True when `currentOp` and every `killOp` succeeded (with no operation
/// left to kill too); failures are logged. Public (hidden) for its test.
#[doc(hidden)]
pub async fn kill_by_comment(client: &Client, comment: &str, timeout: Duration) -> bool {
    let admin = client.database("admin");
    let kill = async {
        let ops = admin.run_command(doc! { "currentOp": 1, "$ownOps": true, "command.comment": comment }).await?;
        let opids = ops.get_array("inprog").into_iter().flatten().filter_map(|op| op.as_document()?.get("opid"));
        let mut all = true;
        for opid in opids {
            if let Err(e) = admin.run_command(doc! { "killOp": 1, "op": opid.clone() }).await {
                eprintln!("Stop: killOp {opid} failed: {e}");
                all = false;
            }
        }
        Ok::<_, MongoError>(all)
    };
    match tokio::time::timeout(timeout, kill).await {
        Ok(Ok(all)) => all,
        Ok(Err(e)) => {
            eprintln!("Stop: currentOp by comment failed: {e}");
            false
        }
        Err(_) => {
            eprintln!("Stop: killing by comment took over {} s", timeout.as_secs());
            false
        }
    }
}

fn read_only_refusal(verb: &str) -> RawOutcome {
    RawOutcome::Error {
        message: format!("This connection is read-only. Turn read-only off in the connection settings to run {verb}."),
        code: Some(READ_ONLY_CODE.into()),
        position: None,
    }
}

fn error(message: &str) -> RawOutcome {
    RawOutcome::Error { message: message.into(), code: None, position: None }
}

#[async_trait]
impl QuerySession for MongoQuerySession {
    /// Only `COMMIT` and `ROLLBACK`, which end a transaction this session never has.
    async fn run(&self, sql: &str, _params: &[Option<String>], _row_cap: usize) -> StatementRun {
        let outcome = match sql {
            "COMMIT" | "ROLLBACK" => RawOutcome::Command { rows_affected: 0 },
            _ => error(NO_SQL),
        };
        StatementRun { outcome, notices: vec![], elapsed_ms: 0 }
    }

    async fn run_simple(&self, _sql: &str) -> RawOutcome {
        error(NO_SQL)
    }

    async fn run_mongo(&self, call: &MongoCall, row_cap: usize) -> StatementRun {
        let started = Instant::now();
        let (outcome, notices) = if self.closed.load(Ordering::SeqCst) {
            (canceled(), vec![])
        } else if matches!(call.kind(), StatementKind::Write) && self.read_only {
            (read_only_refusal(call.verb()), vec![])
        } else {
            // Registered before anything is awaited, so a Stop from now on reaches this run.
            let run = Arc::new(CurrentRun::new());
            let _registered = self.register(run.clone());
            self.run_stoppable(call, row_cap, &run).await
        };
        StatementRun { outcome, notices, elapsed_ms: started.elapsed().as_millis() as u64 }
    }

    async fn stream_mongo(&self, call: &MongoCall, sink: &mut dyn RowSink) -> AppResult<()> {
        stream_documents(&self.client, &self.database(), call, sink, &self.closed).await
    }

    async fn in_transaction(&self) -> AppResult<bool> {
        Ok(false)
    }

    async fn in_failed_transaction(&self) -> bool {
        false
    }

    async fn export_savepoint(&self) -> AppResult<bool> {
        Ok(false)
    }

    /// Stops the run in progress; with none, does nothing.
    async fn cancel(&self) -> AppResult<()> {
        self.stop_current();
        Ok(())
    }

    async fn tx_status(&self) -> TxStatus {
        TxStatus::Idle
    }

    /// Only after `close`: the driver's client reconnects by itself.
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// `Some(db)` is the database statements run in from now on; `None` the one it was opened on.
    async fn set_search_path(&self, schema: Option<&str>) -> AppResult<()> {
        *lock(&self.database) = schema.unwrap_or(&self.opened_with).to_string();
        Ok(())
    }

    /// Marks the session closed, so nothing runs on it any more, stops the run in progress, as
    /// `cancel`, and waits (bounded: the run's two kills, each within `timeout`) until it is
    /// over, so the server has been told before the driver's client may shut down. The client
    /// is the driver's, and closes with it.
    async fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let Some(run) = self.stop_current() else { return };
        let limit = 2 * self.timeout + CLOSE_SLACK;
        if tokio::time::timeout(limit, run.ended.notified()).await.is_err() {
            eprintln!("Stop: the stopped run didn't end within {} s of closing its tab", limit.as_secs());
        }
    }

    fn probe_timeout(&self) -> Duration {
        self.timeout
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mongodb::bson::oid::ObjectId;
    use mongodb::error::ErrorKind;

    use super::*;

    fn col(name: &str, data_type: &str) -> ResultColumn {
        ResultColumn { name: name.into(), data_type: data_type.into() }
    }

    #[test]
    fn result_columns_union_in_first_seen_order_and_mixed_types() {
        let docs = [
            doc! { "_id": ObjectId::new(), "b": 1, "a": "x", "n": null },
            doc! { "_id": ObjectId::new(), "a": "y", "c": [1], "b": 2_i64 },
            doc! { "z": { "k": 1 }, "b": null, "a": "z" },
        ];
        assert_eq!(
            result_columns(&docs),
            [
                col("_id", "objectId"),
                col("b", MIXED_TYPE),
                col("a", "string"),
                col("n", schema::NO_TYPE),
                col("c", "array"),
                col("z", "object"),
            ]
        );
        assert!(result_columns(&[]).is_empty());
    }

    /// The columns are the shown rows': a field only in the documents past the cap isn't one.
    #[test]
    fn rows_past_the_cap_add_no_columns() {
        let docs = vec![doc! { "a": 1 }, doc! { "a": 2 }, doc! { "b": 3 }];
        assert_eq!(
            rows_outcome(docs.clone(), 2, None),
            RawOutcome::Rows {
                columns: vec![col("a", "int32")],
                rows: vec![vec![Some("1".into())], vec![Some("2".into())]],
                truncated: true,
                cell_types: Some(vec!["i".into(), "i".into()]),
            }
        );
        let RawOutcome::Rows { columns, truncated, .. } = rows_outcome(docs, 3, None) else { panic!() };
        assert_eq!((columns, truncated), (vec![col("a", "int32"), col("b", "int32")], false));
        let RawOutcome::Rows { columns, rows, .. } = rows_outcome(vec![], 3, Some("x")) else { panic!() };
        assert_eq!((columns, rows.len()), (vec![col("x", schema::NO_TYPE)], 0));
    }

    /// The limit (and batch size) sent with a find: the server stops at the cap plus one.
    #[test]
    fn find_limit_caps_on_the_server() {
        let cap = 1000;
        for (one, user, want) in [
            (false, None, 1001),
            (false, Some(0), 1001),
            (false, Some(-10), 10),
            (false, Some(10), 10),
            (false, Some(5000), 1001),
            (false, Some(-5000), 1001),
            (false, Some(1000), 1000),
            (false, Some(1001), 1001),
            (false, Some(i64::MIN), 1001),
            (true, None, 1),
            (true, Some(50), 1),
        ] {
            assert_eq!(find_limit(one, user, cap), want, "one {one}, limit {user:?}");
        }
        assert_eq!(find_limit(false, None, 100_000), 100_001);
    }

    /// A read pipeline gets `{ $limit: cap+1 }` last, and a batch that holds it all.
    #[test]
    fn capped_pipeline_appends_the_limit() {
        let pipeline = [doc! { "$match": { "a": 1 } }, doc! { "$limit": 5 }];
        let (capped, batch) = capped_pipeline(&pipeline, 1000);
        assert_eq!(capped, [pipeline[0].clone(), pipeline[1].clone(), doc! { "$limit": 1001_i64 }]);
        assert_eq!(batch, 1002);
        let (capped, batch) = capped_pipeline(&[], 100_000);
        assert_eq!((capped, batch), (vec![doc! { "$limit": 100_001_i64 }], 100_002));
    }

    fn command_error(code: i32, message: &str) -> MongoError {
        let e: mongodb::error::CommandError =
            mongodb::bson::deserialize_from_document(doc! { "code": code, "codeName": "X", "errmsg": message }).unwrap();
        ErrorKind::Command(e).into()
    }

    #[test]
    fn output_target_names_the_database() {
        let t = |stage: Document| output_target(&stage, "fx");
        assert_eq!(t(doc! { "$out": "c" }).as_deref(), Some("fx.c"));
        assert_eq!(t(doc! { "$out": { "db": "o", "coll": "c" } }).as_deref(), Some("o.c"));
        assert_eq!(t(doc! { "$out": { "coll": "c" } }).as_deref(), Some("fx.c"));
        assert_eq!(t(doc! { "$merge": "m" }).as_deref(), Some("fx.m"));
        assert_eq!(t(doc! { "$merge": { "into": "m", "on": "_id" } }).as_deref(), Some("fx.m"));
        assert_eq!(t(doc! { "$merge": { "into": { "db": "o", "coll": "m" } } }).as_deref(), Some("o.m"));
        assert_eq!(t(doc! { "$merge": { "into": { "coll": "m" } } }).as_deref(), Some("fx.m"));
    }

    /// A write-concern failure of insertMany (no write errors) keeps the server's code, and
    /// says the documents may have gone in.
    #[test]
    fn insert_many_write_concern_error_keeps_its_code() {
        let wc = doc! { "code": 64, "codeName": "WriteConcernFailed", "errmsg": "waiting for replication timed out" };
        let e: InsertManyError = mongodb::bson::deserialize_from_document(doc! { "writeConcernError": wc }).unwrap();
        let (outcome, notices) = write_failure(ErrorKind::InsertMany(e).into(), "insertMany", "fx.users", 3, true);
        assert_eq!(
            outcome,
            RawOutcome::Error {
                message: "waiting for replication timed out".into(),
                code: Some("MONGO_64".into()),
                position: None
            }
        );
        assert_eq!(notices, [warning(WRITE_UNCONFIRMED)]);
    }

    fn write_concern_error() -> mongodb::error::WriteConcernError {
        let wc = doc! { "code": 64, "codeName": "WriteConcernFailed", "errmsg": "waiting for replication timed out" };
        mongodb::bson::deserialize_from_document(wc).unwrap()
    }

    /// A write that may have been applied keeps its error and says it may have been: a
    /// write-concern error of any write, or a connection that failed once the command may have
    /// been sent. A refused write, or a connection that failed before anything was sent, doesn't.
    #[test]
    fn unconfirmed_writes_say_they_may_have_been_applied() {
        let unconfirmed = vec![warning(WRITE_UNCONFIRMED)];
        let wc = || -> MongoError { ErrorKind::Write(WriteFailure::WriteConcernError(write_concern_error())).into() };
        for verb in ["updateMany", "replaceOne", "deleteOne", "aggregate"] {
            let (outcome, notices) = write_failure(wc(), verb, "fx.users", 1, true);
            let RawOutcome::Error { code, .. } = &outcome else { panic!("{outcome:?}") };
            assert_eq!((code.as_deref(), &notices), (Some("MONGO_64"), &unconfirmed), "{verb}");
        }
        // (The driver's pool-cleared and invalid-response errors can't be built outside it;
        // they take the same arm.)
        let io = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        let (outcome, notices) = write_failure(ErrorKind::Io(Arc::new(io)).into(), "updateMany", "fx.users", 1, true);
        assert!(matches!(outcome, RawOutcome::Error { code: None, .. }), "{outcome:?}");
        assert_eq!(notices, unconfirmed);
        // Write errors and a write-concern error: the first write error, how many went in, and
        // those unconfirmed.
        let both: InsertManyError = mongodb::bson::deserialize_from_document(doc! {
            "writeErrors": [{ "index": 1, "code": 11000, "codeName": "DuplicateKey", "errmsg": "E11000 dup" }],
            "writeConcernError": { "code": 64, "codeName": "WriteConcernFailed", "errmsg": "timed out" },
        })
        .unwrap();
        let (outcome, notices) = write_failure(ErrorKind::InsertMany(both).into(), "insertMany", "fx.users", 3, true);
        assert_eq!(outcome, RawOutcome::Error { message: "E11000 dup".into(), code: Some("MONGO_11000".into()), position: None });
        assert_eq!(notices, [info("Inserted 1 of 3 documents before the error.".into()), warning(WRITE_UNCONFIRMED)]);
        // Refused, or never sent: no such notice.
        assert_eq!(write_failure(command_error(2, "bad"), "updateOne", "fx.users", 1, true).1, []);
        assert_eq!(write_failure(ErrorKind::Shutdown.into(), "updateOne", "fx.users", 1, true).1, []);
        // A write the server stopped gets Stop's notice (`stop_outcome`), not this one.
        let mut stopped = write_concern_error();
        stopped.code = INTERRUPTED;
        let (outcome, notices) =
            write_failure(ErrorKind::Write(WriteFailure::WriteConcernError(stopped)).into(), "updateMany", "fx.users", 1, true);
        assert_eq!((outcome, notices), (canceled(), vec![]));
    }

    /// The limit of a find, run or exported: findOne is 1, a negative limit its absolute value
    /// (`i64::MIN` too), none or `0` no limit.
    #[test]
    fn user_limit_is_shared_by_run_and_export() {
        for (one, limit, user, export) in [
            (false, None, None, 0),
            (false, Some(0), None, 0),
            (false, Some(7), Some(7), 7),
            (false, Some(-7), Some(7), 7),
            (false, Some(i64::MAX), Some(i64::MAX as u64), i64::MAX),
            (false, Some(i64::MIN), Some(1 << 63), i64::MAX),
            (true, Some(50), Some(1), 1),
            (true, None, Some(1), 1),
        ] {
            assert_eq!(user_limit(one, limit), user, "one {one}, limit {limit:?}");
            assert_eq!(export_limit(one, limit), export, "one {one}, limit {limit:?}");
        }
    }

    /// An export's server error keeps its message (an operation stopped by someone else
    /// included: the export's own cancel never gets here); a connection failure keeps its kind.
    #[test]
    fn export_errors_keep_the_server_message_and_the_connection_kind() {
        let e = |code: i32, message: &str| export_error(command_error(code, message), "find", "fx.big");
        assert_eq!(e(INTERRUPTED, "operation was interrupted"), AppError::Query("operation was interrupted".into()));
        assert_eq!(e(CURSOR_NOT_FOUND, "cursor id 7 not found"), AppError::Query("cursor id 7 not found".into()));
        assert_eq!(e(2, "bad $foo"), AppError::Query("bad $foo".into()));
        assert_eq!(e(13, "not authorized: { pw: \"s3cret\" }"), AppError::Query("Not authorized to run find on fx.big.".into()));
        let io = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        let lost = export_error(ErrorKind::Io(Arc::new(io)).into(), "find", "fx.big");
        assert!(matches!(lost, AppError::Connection(_)), "{lost:?}");
    }

    /// The server's stopped-operation codes are a Stop whoever stopped it; ClientDisconnect only
    /// after one; a stopped write says documents may have changed.
    #[test]
    fn stopped_operations_are_canceled() {
        let call = |stmt: &str| crate::mongoquery::parse(stmt, &crate::model::ParamValues::new()).unwrap();
        let (find, update) = (call("db.big.find()"), call("db.big.updateMany({}, { $set: { s: 1 } })"));
        for code in [INTERRUPTED, CURSOR_KILLED, CURSOR_NOT_FOUND] {
            assert_eq!(error_outcome(command_error(code, "operation was interrupted"), "find", "fx.big"), canceled());
        }
        let disconnected =
            || (error_outcome(command_error(CLIENT_DISCONNECT, "client disconnected"), "find", "fx.big"), vec![]);
        assert_eq!(stop_outcome(&find, true, disconnected()), (canceled(), vec![]));
        assert_eq!(stop_outcome(&find, false, disconnected()), disconnected());
        let stopped_write = Notice { severity: "warning".into(), message: STOPPED_WRITE.into() };
        assert_eq!(stop_outcome(&update, false, (canceled(), vec![])), (canceled(), vec![stopped_write]));
        // Anything else passes through.
        let other = (error_outcome(command_error(2, "bad"), "updateMany", "fx.big"), vec![]);
        assert_eq!(stop_outcome(&update, true, other.clone()), other);
        let done = (RawOutcome::Command { rows_affected: 3 }, vec![info("Matched 3, modified 3.".into())]);
        assert_eq!(stop_outcome(&update, true, done.clone()), done);
    }

    /// A run stopped by Stop or by the limit: Canceled or the limit's error, the write's warning,
    /// and, when the server didn't confirm the kill, that it may still run.
    #[test]
    fn a_stop_the_server_didnt_confirm_says_so() {
        let call = |stmt: &str| crate::mongoquery::parse(stmt, &crate::model::ParamValues::new()).unwrap();
        let (find, update) = (call("db.big.find()"), call("db.big.updateMany({}, { $set: { s: 1 } })"));
        let limit = Some(Duration::from_secs(2));
        let (stopped_write, not_killed) = (warning(STOPPED_WRITE), warning(NOT_KILLED));
        let RawOutcome::Error { code, message, .. } = stopped_outcome(&find, limit, true).0 else { panic!() };
        assert_eq!(
            (code.as_deref(), message.as_str()),
            (Some(TIMEOUT_CODE), "Stopped after 2 s, the connection's statement time limit.")
        );
        for (c, over_limit, killed, notices) in [
            (&find, None, true, vec![]),
            (&find, None, false, vec![not_killed.clone()]),
            (&update, None, true, vec![stopped_write.clone()]),
            (&update, None, false, vec![stopped_write.clone(), not_killed.clone()]),
            (&find, limit, true, vec![]),
            (&find, limit, false, vec![not_killed.clone()]),
            (&update, limit, true, vec![stopped_write.clone()]),
            (&update, limit, false, vec![stopped_write.clone(), not_killed.clone()]),
        ] {
            let (outcome, got) = stopped_outcome(c, over_limit, killed);
            let want = if over_limit.is_some() { timed_out(c, Duration::from_secs(2)).0 } else { canceled() };
            assert_eq!((outcome, got), (want, notices), "{:?} {over_limit:?} {killed}", c.verb());
        }
        assert_eq!(
            NOT_KILLED,
            "The server didn't confirm the stop, so the statement may still be running. Check before running it again."
        );
    }

    /// The grid's save preview lines (`edit::preview`) run as they are in a query tab: each
    /// parses into the call that does what the save does, a collection name that needs quoting
    /// and values of every shape included.
    #[test]
    fn grid_preview_lines_parse_into_their_writes() {
        use super::super::edit::{preview, MongoOp};
        use mongodb::bson::{DateTime, Decimal128};
        let id = Bson::ObjectId(ObjectId::parse_str("65a1b2c3d4e5f60718293a4b").unwrap());
        let set = doc! {
            "age": 31,
            "big": 9_007_199_254_740_993_i64,
            "score": 7.5,
            "first name": "A \"q\" é😀",
            "when": DateTime::from_millis(1_704_164_645_678),
            "address": { "city": "Oslo", "tags": ["a", 1, null, { "z": 1, "a": 2 }] },
            "price": "1.10".parse::<Decimal128>().unwrap(),
            "ok": true,
        };
        let insert = doc! { "name": "Gus", "n": 5_i64, "nested": { "b": [1.5, "x"] } };
        let ops = [
            MongoOp::Update { id: id.clone(), set: set.clone(), key_label: String::new() },
            MongoOp::Delete { id: Bson::Int64(5), key_label: String::new() },
            MongoOp::Insert { doc: insert.clone() },
        ];
        let collection = "my \"coll\" x";
        let lines = preview(collection, &ops);
        let want = [
            MongoQueryOp::Update {
                filter: doc! { "_id": id },
                update: UpdateSpec::Modifiers(doc! { "$set": set }),
                upsert: false,
                many: false,
            },
            MongoQueryOp::Delete { filter: doc! { "_id": 5_i64 }, many: false },
            MongoQueryOp::InsertOne { doc: insert },
        ];
        assert_eq!(lines.len(), want.len());
        for (line, op) in lines.iter().zip(want) {
            let call = crate::mongoquery::parse(line, &crate::model::ParamValues::new())
                .unwrap_or_else(|e| panic!("{line}: {e:?}"));
            assert_eq!(call, MongoCall { collection: collection.into(), op }, "{line}");
        }
    }

    /// After `close` nothing runs on the session: the rest of a run whose tab was closed is
    /// canceled, an export refused, both before anything is sent (no server is listening here).
    #[tokio::test]
    async fn a_closed_session_runs_nothing() {
        let client = Client::with_uri_str("mongodb://127.0.0.1:9/?serverSelectionTimeoutMS=100").await.unwrap();
        let s = MongoQuerySession::new(client, "fx".into(), Duration::from_secs(1), false, None);
        assert!(!s.is_closed());
        s.close().await;
        assert!(s.is_closed());
        let call = |stmt: &str| crate::mongoquery::parse(stmt, &crate::model::ParamValues::new()).unwrap();
        for stmt in ["db.users.find()", "db.users.deleteMany({})"] {
            let run = s.run_mongo(&call(stmt), 1000).await;
            assert_eq!((run.outcome, run.notices), (canceled(), vec![]), "{stmt}");
        }
        assert_eq!(s.current_run_comment(), None);
        struct NoRows;
        impl RowSink for NoRows {
            fn begin(&mut self, _: &[ExportColumn]) -> AppResult<()> {
                panic!("nothing is exported")
            }
            fn row(&mut self, _: Vec<Option<String>>) -> AppResult<()> {
                panic!("nothing is exported")
            }
        }
        assert_eq!(s.stream_mongo(&call("db.users.find()"), &mut NoRows).await, Err(AppError::Query(TAB_CLOSED.into())));
    }

    /// A findOneAnd*'s notice comes from `lastErrorObject`; an ok reply with a
    /// `writeConcernError` adds `WRITE_UNCONFIRMED`, to it and to the helpers.
    #[test]
    fn find_and_modify_and_helper_replies() {
        let update = FindAndAction::Update(UpdateSpec::Modifiers(doc! { "$set": { "a": 1 } }));
        let replace = FindAndAction::Replace(doc! { "a": 1 });
        let notices = |action: &FindAndAction, reply: Document| -> Vec<String> {
            find_and_modify_done(action, &reply, 1000).1.into_iter().map(|n| n.message).collect()
        };
        let found = |last: Document| doc! { "lastErrorObject": last, "value": { "_id": 1 }, "ok": 1 };
        assert_eq!(notices(&update, found(doc! { "n": 1, "updatedExisting": true })), ["Updated 1 document."]);
        assert_eq!(notices(&replace, found(doc! { "n": 1, "updatedExisting": true })), ["Replaced 1 document."]);
        assert_eq!(notices(&FindAndAction::Delete, found(doc! { "n": 1 })), ["Deleted 1 document."]);
        let upserted = doc! { "lastErrorObject": { "n": 1, "updatedExisting": false, "upserted": 7 }, "value": null };
        assert_eq!(notices(&update, upserted), ["Upserted _id: 7."]);
        let none = || doc! { "lastErrorObject": { "n": 0, "updatedExisting": false }, "value": null, "ok": 1 };
        for action in [&update, &replace, &FindAndAction::Delete] {
            assert_eq!(notices(action, none()), ["No document matched."]);
        }
        let RawOutcome::Rows { rows, .. } = find_and_modify_done(&update, &none(), 1000).0 else { panic!() };
        assert!(rows.is_empty());
        let RawOutcome::Rows { rows, .. } =
            find_and_modify_done(&update, &found(doc! { "n": 1, "updatedExisting": true }), 1000).0
        else {
            panic!()
        };
        assert_eq!(rows, [vec![Some("1".to_string())]]);

        let wc = doc! { "code": 64, "errmsg": "waiting for replication timed out" };
        let mut unconfirmed_reply = found(doc! { "n": 1, "updatedExisting": true });
        unconfirmed_reply.insert("writeConcernError", wc.clone());
        assert_eq!(notices(&update, unconfirmed_reply), ["Updated 1 document.", WRITE_UNCONFIRMED]);
        assert_eq!(
            helper_done(&doc! { "ok": 1, "writeConcernError": wc }, vec!["Dropped index a_1.".into()]),
            (
                RawOutcome::Command { rows_affected: 0 },
                vec![info("Dropped index a_1.".into()), warning(WRITE_UNCONFIRMED)]
            )
        );
        assert_eq!(helper_done(&doc! { "ok": 1 }, vec!["x".into()]).1, [info("x".into())]);

        // drop: a collection's reply, a view's, and 8.0's for a missing name.
        assert!(dropped(&doc! { "nIndexesWas": 1, "ns": "fx.c", "ok": 1 }));
        assert!(dropped(&doc! { "ns": "fx.v", "ok": 1 }));
        assert!(!dropped(&doc! { "ok": 1 }));

        // createIndexes.
        let created = doc! { "numIndexesBefore": 1, "numIndexesAfter": 2, "createdCollectionAutomatically": true };
        assert_eq!(create_index_notices(&created, "a_1", "fx.c"), ["Created index a_1 on fx.c.", "Created collection fx.c for it."]);
        let there = doc! { "numIndexesBefore": 2, "numIndexesAfter": 2, "note": "all indexes already exist" };
        assert_eq!(create_index_notices(&there, "a_1", "fx.c"), ["Index a_1 already exists."]);

        // listIndexes' first batch.
        let reply = doc! { "cursor": { "id": 0_i64, "ns": "fx.c", "firstBatch": [{ "name": "_id_" }] }, "ok": 1 };
        assert_eq!(first_batch(&reply), Some(vec![doc! { "name": "_id_" }]));
        assert_eq!(first_batch(&doc! { "ok": 1 }), None);
        assert_eq!(first_batch(&doc! { "cursor": { "firstBatch": [1] } }), None);
    }

    /// A findOneAnd* reply without `lastErrorObject` (the server always sends one):
    /// `value` says what happened, a document that one was changed, null that none matched.
    #[test]
    fn find_and_modify_without_last_error_object_reads_the_value() {
        let update = FindAndAction::Update(UpdateSpec::Modifiers(doc! { "$set": { "a": 1 } }));
        let replace = FindAndAction::Replace(doc! { "a": 1 });
        let notices = |action: &FindAndAction, reply: Document| -> Vec<String> {
            find_and_modify_done(action, &reply, 1000).1.into_iter().map(|n| n.message).collect()
        };
        let returned = || doc! { "value": { "_id": 1 }, "ok": 1 };
        assert_eq!(notices(&update, returned()), ["Updated 1 document."]);
        assert_eq!(notices(&replace, returned()), ["Replaced 1 document."]);
        assert_eq!(notices(&FindAndAction::Delete, returned()), ["Deleted 1 document."]);
        for action in [&update, &replace, &FindAndAction::Delete] {
            assert_eq!(notices(action, doc! { "value": null, "ok": 1 }), ["No document matched."]);
            assert_eq!(notices(action, doc! { "ok": 1 }), ["No document matched."]);
        }
        let RawOutcome::Rows { rows, .. } = find_and_modify_done(&FindAndAction::Delete, &returned(), 1000).0 else {
            panic!()
        };
        assert_eq!(rows, [vec![Some("1".to_string())]]);
    }

    /// What a bulkWrite shows: the tally first; then what ran, or how many failed; then a
    /// validator's details and `WRITE_UNCONFIRMED`.
    #[test]
    fn bulk_outcomes() {
        let tally = BulkTally { inserted: 1, matched: 2, modified: 1, upserted: 1, deleted: 3 };
        let counts = "Inserted 1, matched 2, modified 1, upserted 1, deleted 3.";
        let msgs = |notices: &[Notice]| -> Vec<String> { notices.iter().map(|n| n.message.clone()).collect() };
        assert_eq!(
            bulk_done(&tally, &[], 9, true, false, "bulkWrite", "fx.c"),
            (RawOutcome::Command { rows_affected: 6 }, vec![info(counts.into())])
        );
        // A reply with a writeConcernError: done, but unconfirmed.
        assert_eq!(
            bulk_done(&tally, &[], 9, false, true, "bulkWrite", "fx.c"),
            (RawOutcome::Command { rows_affected: 6 }, vec![info(counts.into()), warning(WRITE_UNCONFIRMED)])
        );

        let err = |index: usize, code: i32, message: &str| BulkWriteError {
            index,
            code,
            message: message.into(),
            details: None,
        };
        let errors = [err(4, 11000, "E11000 a"), err(2, 66, "immutable")];
        let (outcome, notices) = bulk_done(&tally, &errors, 9, false, false, "bulkWrite", "fx.c");
        assert_eq!(outcome, RawOutcome::Error { message: "immutable".into(), code: Some("MONGO_66".into()), position: None });
        assert_eq!(msgs(&notices), [counts, "2 of 9 operations failed; the others ran."]);
        let (outcome, notices) = bulk_done(&tally, &errors[..1], 9, true, true, "bulkWrite", "fx.c");
        assert_eq!(outcome, RawOutcome::Error { message: "E11000 a".into(), code: Some("MONGO_11000".into()), position: None });
        assert_eq!(notices, [info(counts.into()), info("Ran 4 of 9 operations before the error.".into()), warning(WRITE_UNCONFIRMED)]);
        // A validator's failure: the app's copy, and its details.
        let mut invalid = err(0, 121, "Document failed validation");
        invalid.details = Some(doc! { "failingDocumentId": 1 });
        let (outcome, notices) = bulk_done(&BulkTally::default(), &[invalid], 1, true, false, "bulkWrite", "fx.c");
        assert_eq!(outcome, RawOutcome::Error { message: VALIDATION_FAILED.into(), code: Some("MONGO_121".into()), position: None });
        assert_eq!(
            msgs(&notices),
            [
                "Inserted 0, matched 0, modified 0, upserted 0, deleted 0.",
                "Ran 0 of 1 operations before the error.",
                "Details: {\"failingDocumentId\":1}",
            ]
        );
        // A write error the server stopped is a Stop (`stop_outcome` adds its notice).
        let (outcome, _) = bulk_done(&tally, &[err(1, INTERRUPTED, "interrupted")], 9, true, false, "bulkWrite", "fx.c");
        assert_eq!(outcome, canceled());

        // A connection that failed once a group was sent: the error, the finished groups'
        // tally, what ran, and unconfirmed.
        let ordered_at = BulkStop { start: 3, total: 9, ordered: true, failed: 0 };
        let ran = "Ran 3 of 9 operations before the error.";
        let io = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        let (outcome, notices) =
            bulk_failed(ErrorKind::Io(Arc::new(io)).into(), &tally, &ordered_at, false, "bulkWrite", "fx.c");
        assert!(matches!(outcome, RawOutcome::Error { code: None, .. }), "{outcome:?}");
        assert_eq!(notices, [info(counts.into()), info(ran.into()), warning(WRITE_UNCONFIRMED)]);
        // A command error: its code, short for code 13; unconfirmed once when an earlier reply was.
        let (outcome, notices) =
            bulk_failed(command_error(13, "not authorized { q: 1 }"), &tally, &ordered_at, true, "bulkWrite", "fx.c");
        assert_eq!(
            outcome,
            RawOutcome::Error {
                message: "Not authorized to run bulkWrite on fx.c.".into(),
                code: Some("MONGO_13".into()),
                position: None
            }
        );
        assert_eq!(notices, [info(counts.into()), info(ran.into()), warning(WRITE_UNCONFIRMED)]);
        let (_, notices) = bulk_failed(command_error(2, "bad"), &tally, &ordered_at, false, "bulkWrite", "fx.c");
        assert_eq!(msgs(&notices), [counts, ran]);
        // Unconfirmed is said once, when the failure says it too.
        let io = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        let (_, notices) =
            bulk_failed(ErrorKind::Io(Arc::new(io)).into(), &tally, &ordered_at, true, "bulkWrite", "fx.c");
        assert_eq!(notices, [info(counts.into()), info(ran.into()), warning(WRITE_UNCONFIRMED)]);
        // Unordered: the earlier groups' write errors are counted, and the later groups weren't
        // sent; with none, it says so too.
        let unordered_at = |failed: usize| BulkStop { start: 4, total: 9, ordered: false, failed };
        let (_, notices) = bulk_failed(command_error(2, "bad"), &tally, &unordered_at(2), false, "bulkWrite", "fx.c");
        assert_eq!(msgs(&notices), [counts, "2 of 9 operations failed before the error; the rest weren't sent."]);
        let io = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        let (_, notices) =
            bulk_failed(ErrorKind::Io(Arc::new(io)).into(), &tally, &unordered_at(0), false, "bulkWrite", "fx.c");
        assert_eq!(
            notices,
            [
                info(counts.into()),
                info("0 of 9 operations failed before the error; the rest weren't sent.".into()),
                warning(WRITE_UNCONFIRMED)
            ]
        );

        // A reply the app can't read: an error, not zeros, with the finished groups' tally;
        // what that group did is unknown.
        assert_eq!(
            bulk_unreadable(&tally),
            (error(BULK_UNREADABLE), vec![info(counts.into()), warning(WRITE_UNCONFIRMED)])
        );
        assert_eq!(BULK_UNREADABLE, "The server's bulkWrite reply couldn't be read.");
    }

    #[test]
    fn reply_count_reads_n() {
        assert_eq!(reply_count(&doc! { "n": 6, "ok": 1 }), Some(6));
        assert_eq!(reply_count(&doc! { "n": 7_i64 }), Some(7));
        assert_eq!(reply_count(&doc! { "n": 8.0 }), Some(8));
        // A reply it can't read is no count, not 0.
        for reply in [doc! { "ok": 1 }, doc! { "n": -1 }, doc! { "n": "6" }, doc! { "n": 1.5 }] {
            assert_eq!(reply_count(&reply), None, "{reply}");
        }
    }

    #[test]
    fn read_errors_keep_the_server_code() {
        let outcome = |message: &str, code: Option<&str>| RawOutcome::Error {
            message: message.into(),
            code: code.map(String::from),
            position: None,
        };
        assert_eq!(error_outcome(command_error(2, "bad $foo"), "find", "fx.users"), outcome("bad $foo", Some("MONGO_2")));
        // Code 13's text repeats the command, values included: never shown.
        let denied = command_error(13, r#"not authorized on fx to execute command { find: "users", filter: { pw: "s3cret" } }"#);
        assert_eq!(
            error_outcome(denied, "countDocuments", "fx.users"),
            outcome("Not authorized to run countDocuments on fx.users.", Some("MONGO_13"))
        );
        let io = || std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        let refused: MongoError = ErrorKind::Io(Arc::new(io())).into();
        assert_eq!(error_outcome(refused, "find", "fx.users"), outcome(&format!("I/O error: {}", io()), None));
    }
}
