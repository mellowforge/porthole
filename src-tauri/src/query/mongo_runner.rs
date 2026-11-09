//! Runs the statements of a `RunRequest` written in the MongoDB subset (`mongoquery`), one by
//! one on a tab session, as `runner` does for SQL.

use super::runner::{now_ms, pick_targets, production_gate, DEFAULT_CAP, FETCH_ALL_CAP};
use super::{
    Outcome, QuerySession, RawOutcome, RunEvent, RunOutcome, RunReport, RunRequest, StatementResult, TxStatus,
    CANCELED_CODE,
};
use crate::error::{AppError, AppResult};
use crate::model::{Environment, ParamValues};
use crate::mongoquery::{self, MongoCall, MongoSpan, ParseError};
use crate::sqltext::{StatementKind, Utf16Index};

/// The code of a statement that doesn't parse.
pub const PARSE_ERROR_CODE: &str = "MONGO_PARSE";

/// The values of `stmt`'s parameters, from `values`. A parameter with no value, or an empty (or
/// blank) one, is `No value for :<name>`: in a MongoDB filter an empty string still matches
/// something (`$gt: ""` matches every string), so a field left empty would give plausible wrong
/// rows. `""` passes an empty string; NULL stays `null`.
pub fn bind_params(stmt: &str, values: &ParamValues) -> AppResult<ParamValues> {
    let mut params = ParamValues::new();
    for name in mongoquery::find_params(stmt) {
        let value = match values.get(&name) {
            Some(Some(text)) if text.trim().is_empty() => None,
            Some(value) => Some(value.clone()),
            None => None,
        };
        let value = value.ok_or_else(|| AppError::Query(format!("No value for :{name}")))?;
        params.insert(name, value);
    }
    Ok(params)
}

/// A target statement with everything needed to run it.
struct Prepared {
    /// Char span in `req.sql`.
    start: usize,
    end: usize,
    text: String,
    params: ParamValues,
    call: Result<MongoCall, ParseError>,
    verb: String,
    kind: StatementKind,
}

/// As `runner::run_in` for SQL: every target statement's parameters must have a value before
/// anything runs; on Production a write waits for confirmation; the statements then run in
/// order, and an error or a cancel skips the rest. A statement that doesn't parse is an error
/// at its position, and nothing is sent for it. The kind and the verb come from the parsed call
/// (a parameter can hold a `$out` pipeline), or from the tokens when it doesn't parse.
pub async fn run(
    session: &dyn QuerySession,
    env: &Environment,
    req: &RunRequest,
    on_event: &(dyn Fn(RunEvent) + Send + Sync),
) -> AppResult<RunReport> {
    let idx = Utf16Index::new(&req.sql);
    let chars: Vec<char> = req.sql.chars().collect();
    let split = |text: &str| -> Vec<(usize, usize, MongoSpan)> {
        mongoquery::split(text).into_iter().map(|s| (s.start, s.end, s)).collect()
    };

    let mut prepared = Vec::new();
    for (start, end, span) in pick_targets(req, &idx, split) {
        let text: String = chars[start..end].iter().collect();
        let params = bind_params(&text, &req.params)?;
        let call = mongoquery::parse(&text, &params);
        let (kind, verb) = match &call {
            Ok(c) => (c.kind(), c.verb().to_string()),
            Err(_) => (span.kind, span.verb),
        };
        prepared.push(Prepared { start, end, text, params, call, verb, kind });
    }

    let statements = prepared.iter().map(|p| (p.kind, p.verb.as_str(), p.text.as_str()));
    if let Some(held) = production_gate(env, req, statements) {
        return Ok(held);
    }

    let started_at = now_ms();
    let cap = if req.fetch_all { FETCH_ALL_CAP } else { DEFAULT_CAP };
    let total = prepared.len();
    let mut results: Vec<StatementResult> = Vec::with_capacity(total);
    let mut stopped = false;
    for (index, p) in prepared.into_iter().enumerate() {
        let from = idx.to_utf16(p.start);
        let to = idx.to_utf16(p.end);
        let mut result = StatementResult {
            index,
            from,
            to,
            verb: p.verb.clone(),
            kind: p.kind,
            sql: p.text,
            params: p.params,
            outcome: Outcome::Skipped,
            elapsed_ms: 0,
            notices: vec![],
        };
        if !stopped {
            on_event(RunEvent::Started { index, total, from, to, verb: p.verb.clone() });
            result.outcome = match &p.call {
                Err(e) => Outcome::Error {
                    message: e.message.clone(),
                    code: Some(PARSE_ERROR_CODE.into()),
                    position: Some(idx.to_utf16(p.start + e.at)),
                },
                Ok(call) => {
                    let run = session.run_mongo(call, cap).await;
                    result.elapsed_ms = run.elapsed_ms;
                    result.notices = run.notices;
                    match run.outcome {
                        RawOutcome::Rows { columns, rows, truncated, cell_types } => {
                            Outcome::Rows { columns, rows, truncated, cell_types }
                        }
                        RawOutcome::Command { rows_affected } if call.reports_affected() => {
                            Outcome::Affected { rows: rows_affected }
                        }
                        RawOutcome::Command { .. } => Outcome::Done,
                        RawOutcome::Error { code, .. } if code.as_deref() == Some(CANCELED_CODE) => Outcome::Canceled,
                        // `position`: 1-based char in the statement.
                        RawOutcome::Error { message, code, position } => Outcome::Error {
                            message,
                            code,
                            position: position.map(|pos| idx.to_utf16(p.start + pos.saturating_sub(1))),
                        },
                    }
                }
            };
            stopped = matches!(result.outcome, Outcome::Error { .. } | Outcome::Canceled);
        }
        on_event(RunEvent::Finished { result: result.clone() });
        results.push(result);
    }

    // MongoDB tabs have no transactions of their own.
    Ok(RunReport {
        outcome: RunOutcome::Completed { statement_count: total, started_at, tx_status: TxStatus::Idle },
        statements: results,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;
    use crate::mongoquery::MongoCall;
    use crate::model::ParamValues;
    use crate::query::{
        Outcome, PendingWrite, RawOutcome, ResultColumn, RunOutcome, RunTarget, StatementRun, TxStatus, CANCELED_CODE,
    };
    use crate::sqltext::StatementKind;

    /// Records the calls `run_mongo` gets and answers them from a script (then `Rows` of one
    /// row).
    #[derive(Default)]
    struct FakeMongo {
        calls: Mutex<Vec<(MongoCall, usize)>>,
        script: Mutex<VecDeque<RawOutcome>>,
    }

    impl FakeMongo {
        fn scripted(outcomes: Vec<RawOutcome>) -> Self {
            Self { script: Mutex::new(outcomes.into()), ..Self::default() }
        }
        fn collections(&self) -> Vec<String> {
            self.calls.lock().unwrap().iter().map(|c| c.0.collection.clone()).collect()
        }
    }

    #[async_trait]
    impl QuerySession for FakeMongo {
        async fn run(&self, _: &str, _: &[Option<String>], _: usize) -> StatementRun {
            unreachable!("a MongoDB tab never runs SQL")
        }
        async fn run_simple(&self, _: &str) -> RawOutcome {
            unreachable!("a MongoDB tab never runs SQL")
        }
        async fn run_mongo(&self, call: &MongoCall, row_cap: usize) -> StatementRun {
            self.calls.lock().unwrap().push((call.clone(), row_cap));
            let outcome = self.script.lock().unwrap().pop_front().unwrap_or_else(rows1);
            StatementRun { outcome, notices: vec![], elapsed_ms: 3 }
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

    fn rows1() -> RawOutcome {
        RawOutcome::Rows {
            columns: vec![ResultColumn { name: "a".into(), data_type: "int32".into() }],
            rows: vec![vec![Some("1".into())]],
            truncated: false,
            cell_types: Some(vec!["i".into()]),
        }
    }

    fn req(text: &str, target: RunTarget) -> RunRequest {
        RunRequest {
            sql: text.into(),
            target,
            params: ParamValues::new(),
            schema: None,
            confirmed: false,
            fetch_all: false,
            saved_query_id: None,
            database: None,
        }
    }

    fn utf16(text: &str, needle: &str) -> usize {
        text[..text.find(needle).unwrap()].encode_utf16().count()
    }

    async fn go(fake: &FakeMongo, r: &RunRequest) -> RunReport {
        run(fake, &Environment::Local, r, &|_| {}).await.unwrap()
    }

    fn shapes(report: &RunReport) -> Vec<&'static str> {
        report
            .statements
            .iter()
            .map(|s| match s.outcome {
                Outcome::Rows { .. } => "rows",
                Outcome::Affected { .. } => "affected",
                Outcome::Done => "done",
                Outcome::Error { .. } => "error",
                Outcome::Canceled => "canceled",
                Outcome::Skipped => "skipped",
            })
            .collect()
    }

    #[tokio::test]
    async fn targets_cursor_range_all() {
        let text = "db.a.find()\ndb.b.find({ n: \"é😀\" })\n  .limit(2)\ndb.c.find()";
        let fake = FakeMongo::default();
        let report = go(&fake, &req(text, RunTarget::All)).await;
        assert_eq!(fake.collections(), ["a", "b", "c"]);
        let b = &report.statements[1];
        let b_end = utf16(text, "\ndb.c");
        assert_eq!((b.from, b.to), (utf16(text, "db.b"), b_end));
        assert_eq!((b.verb.as_str(), b.kind), ("find", StatementKind::Read));
        assert_eq!(b.sql, "db.b.find({ n: \"é😀\" })\n  .limit(2)");
        assert!(matches!(report.outcome, RunOutcome::Completed { statement_count: 3, tx_status: TxStatus::Idle, .. }));

        // The cursor on the chain's second line picks the whole chain; in the gap after a
        // statement, that statement; before the first, the first.
        for (at, want) in [(utf16(text, ".limit"), "b"), (b_end, "b"), (0, "a")] {
            let fake = FakeMongo::default();
            go(&fake, &req(text, RunTarget::Cursor { at })).await;
            assert_eq!(fake.collections(), [want], "cursor at {at}");
        }

        // A selection runs what it covers, with spans in the whole text.
        let fake = FakeMongo::default();
        let (from, to) = (utf16(text, "db.b"), text.encode_utf16().count());
        let report = go(&fake, &req(text, RunTarget::Range { from, to })).await;
        assert_eq!(fake.collections(), ["b", "c"]);
        assert_eq!(report.statements[1].from, utf16(text, "db.c"));
        let fake = FakeMongo::default();
        go(&fake, &req(text, RunTarget::Range { from: 5, to: 2 })).await;
        assert!(fake.collections().is_empty());
    }

    #[tokio::test]
    async fn missing_param_fails_before_running() {
        let fake = FakeMongo::default();
        let mut r = req("db.a.find({ x: :a }); db.b.find({ x: :a, y: :b })", RunTarget::All);
        r.params.insert("a".into(), Some("1".into()));
        let res = run(&fake, &Environment::Local, &r, &|_| {}).await;
        assert_eq!(res.unwrap_err(), AppError::Query("No value for :b".into()));
        assert!(fake.collections().is_empty());

        // With every value: each statement records only the names it uses, and gets the values.
        r.params.insert("b".into(), None);
        r.params.insert("unused".into(), Some("x".into()));
        let report = go(&fake, &r).await;
        let mut want = ParamValues::new();
        want.insert("a".into(), Some("1".into()));
        assert_eq!(report.statements[0].params, want);
        assert_eq!(report.statements[0].sql, "db.a.find({ x: :a })");
        let calls = fake.calls.lock().unwrap();
        let crate::mongoquery::MongoQueryOp::Find { filter, .. } = &calls[1].0.op else { panic!("{:?}", calls[1]) };
        assert_eq!(filter, &mongodb::bson::doc! { "x": 1, "y": null });
    }

    #[tokio::test]
    async fn empty_param_fails_before_running() {
        let fake = FakeMongo::default();
        let mut r = req("db.a.find(); db.b.find({ age: { $gt: :min } })", RunTarget::All);
        for empty in ["", "  "] {
            r.params.insert("min".into(), Some(empty.into()));
            let res = run(&fake, &Environment::Local, &r, &|_| {}).await;
            assert_eq!(res.unwrap_err(), AppError::Query("No value for :min".into()));
            assert!(fake.collections().is_empty());
        }

        // `""` is an empty string, and NULL is null.
        r.params.insert("min".into(), Some("\"\"".into()));
        go(&fake, &r).await;
        r.params.insert("min".into(), None);
        go(&fake, &r).await;
        let calls = fake.calls.lock().unwrap();
        let filters: Vec<_> = calls
            .iter()
            .filter_map(|(call, _)| match &call.op {
                crate::mongoquery::MongoQueryOp::Find { filter, .. } if !filter.is_empty() => Some(filter.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            filters,
            [mongodb::bson::doc! { "age": { "$gt": "" } }, mongodb::bson::doc! { "age": { "$gt": null } }]
        );
    }

    #[test]
    fn bind_params_rule() {
        let values: ParamValues = [("a".to_string(), Some("1".to_string())), ("b".to_string(), Some(String::new()))]
            .into_iter()
            .collect();
        let mut want = ParamValues::new();
        want.insert("a".into(), Some("1".into()));
        assert_eq!(bind_params("db.x.find({ a: :a })", &values).unwrap(), want);
        assert_eq!(bind_params("db.x.find({ a: :a, b: :b })", &values).unwrap_err(), AppError::Query("No value for :b".into()));
        assert_eq!(bind_params("db.x.find({ c: :c })", &values).unwrap_err(), AppError::Query("No value for :c".into()));
        assert_eq!(bind_params("db.x.find()", &values).unwrap(), ParamValues::new());
    }

    #[tokio::test]
    async fn production_gate_holds_a_mongo_write() {
        let text = "db.a.find()\ndb.a.aggregate([{ $match: {} },   { $out: \"x\" }])\ndb.a.insertOne({ n: 1 })";
        let fake = FakeMongo::default();
        let r = req(text, RunTarget::All);
        let report = run(&fake, &Environment::Production, &r, &|_| {}).await.unwrap();
        let pending = |verb: &str, preview: &str| PendingWrite { verb: verb.into(), preview: preview.into() };
        assert_eq!(
            report.outcome,
            RunOutcome::NeedsConfirmation {
                statements: vec![
                    pending("aggregate", "db.a.aggregate([{ $match: {} }, { $out: \"x\" }])"),
                    pending("insertOne", "db.a.insertOne({ n: 1 })"),
                ]
            }
        );
        assert!(report.statements.is_empty());
        assert!(fake.collections().is_empty());

        let mut confirmed = r.clone();
        confirmed.confirmed = true;
        run(&fake, &Environment::Production, &confirmed, &|_| {}).await.unwrap();
        assert_eq!(fake.collections().len(), 3);

        let fake = FakeMongo::default();
        run(&fake, &Environment::Local, &r, &|_| {}).await.unwrap();
        assert_eq!(fake.collections().len(), 3);

        // Reads alone never ask, a read aggregate included.
        let fake = FakeMongo::default();
        let reads = req("db.a.find()\ndb.a.aggregate([{ $match: {} }])", RunTarget::All);
        run(&fake, &Environment::Production, &reads, &|_| {}).await.unwrap();
        assert_eq!(fake.collections().len(), 2);
    }

    /// On Production, drop and createCollection wait for confirmation, as any
    /// write; getIndexes is a read and doesn't.
    #[tokio::test]
    async fn production_gate_holds_a_drop() {
        let text = "db.a.getIndexes()\ndb.a.drop()\ndb.createCollection(\"x\")";
        let fake = FakeMongo::default();
        let r = req(text, RunTarget::All);
        let report = run(&fake, &Environment::Production, &r, &|_| {}).await.unwrap();
        let pending = |verb: &str, preview: &str| PendingWrite { verb: verb.into(), preview: preview.into() };
        assert_eq!(
            report.outcome,
            RunOutcome::NeedsConfirmation {
                statements: vec![pending("drop", "db.a.drop()"), pending("createCollection", "db.createCollection(\"x\")")]
            }
        );
        assert!(report.statements.is_empty());
        assert!(fake.collections().is_empty());

        let mut confirmed = r.clone();
        confirmed.confirmed = true;
        run(&fake, &Environment::Production, &confirmed, &|_| {}).await.unwrap();
        assert_eq!(fake.collections(), ["a", "a", "x"]);

        let fake = FakeMongo::default();
        let reads = req("db.a.getIndexes()", RunTarget::All);
        run(&fake, &Environment::Production, &reads, &|_| {}).await.unwrap();
        assert_eq!(fake.collections(), ["a"]);
    }

    #[tokio::test]
    async fn production_gate_sees_an_out_stage_in_a_parameter() {
        let fake = FakeMongo::default();
        let mut r = req("db.a.aggregate(:p)", RunTarget::All);
        r.params.insert("p".into(), Some("[{ $out: \"x\" }]".into()));
        let report = run(&fake, &Environment::Production, &r, &|_| {}).await.unwrap();
        assert_eq!(
            report.outcome,
            RunOutcome::NeedsConfirmation {
                statements: vec![PendingWrite { verb: "aggregate".into(), preview: "db.a.aggregate(:p)".into() }]
            }
        );
        assert!(fake.collections().is_empty());

        // A write that doesn't parse is still held, by its tokens.
        let r = req("db.a.deleteMany({ a: })", RunTarget::All);
        let report = run(&fake, &Environment::Production, &r, &|_| {}).await.unwrap();
        assert!(matches!(report.outcome, RunOutcome::NeedsConfirmation { .. }), "{:?}", report.outcome);
    }

    #[tokio::test]
    async fn parse_error_position_is_utf16_after_multibyte_text() {
        let text = "db.a.find({})\ndb.b.find({ name: \"Zoë 😀\", age: })";
        let fake = FakeMongo::default();
        let report = go(&fake, &req(text, RunTarget::All)).await;
        let second_brace = text.match_indices('}').nth(1).unwrap().0;
        let want = text[..second_brace].encode_utf16().count();
        match &report.statements[1].outcome {
            Outcome::Error { code, position, .. } => {
                assert_eq!(code.as_deref(), Some(PARSE_ERROR_CODE));
                assert_eq!(*position, Some(want));
            }
            o => panic!("{o:?}"),
        }
        assert_eq!(fake.collections(), ["a"]);
    }

    #[tokio::test]
    async fn parse_error_stops_and_skips_the_rest() {
        let text = "db.a.find(); db.b.frob(); db.c.find()";
        let fake = FakeMongo::default();
        let events = Mutex::new(Vec::new());
        let report = run(&fake, &Environment::Local, &req(text, RunTarget::All), &|e| {
            events.lock().unwrap().push(match e {
                RunEvent::Started { index, .. } => format!("started {index}"),
                RunEvent::Finished { result } => format!("finished {}", result.index),
            })
        })
        .await
        .unwrap();
        assert_eq!(shapes(&report), ["rows", "error", "skipped"]);
        assert_eq!(fake.collections(), ["a"]);
        let Outcome::Error { message, code, .. } = &report.statements[1].outcome else { panic!() };
        assert!(message.starts_with("frob isn't supported in query tabs. Supported: find, findOne"), "{message}");
        assert_eq!(code.as_deref(), Some(PARSE_ERROR_CODE));
        // A statement that doesn't parse keeps the verb its tokens give.
        assert_eq!(report.statements[1].verb, "frob");
        assert_eq!(
            *events.lock().unwrap(),
            ["started 0", "finished 0", "started 1", "finished 1", "finished 2"]
        );
    }

    #[tokio::test]
    async fn server_errors_and_cancels_stop_the_run() {
        let failed = RawOutcome::Error { message: "bad".into(), code: Some("MONGO_2".into()), position: None };
        let fake = FakeMongo::scripted(vec![failed]);
        let report = go(&fake, &req("db.a.find({ $foo: 1 }); db.b.find()", RunTarget::All)).await;
        assert_eq!(
            report.statements[0].outcome,
            Outcome::Error { message: "bad".into(), code: Some("MONGO_2".into()), position: None }
        );
        assert_eq!(report.statements[1].outcome, Outcome::Skipped);

        let canceled = RawOutcome::Error { message: "Canceled".into(), code: Some(CANCELED_CODE.into()), position: None };
        let fake = FakeMongo::scripted(vec![canceled]);
        let report = go(&fake, &req("db.a.find(); db.b.find()", RunTarget::All)).await;
        assert_eq!(shapes(&report), ["canceled", "skipped"]);
        assert_eq!(fake.collections(), ["a"]);
    }

    #[tokio::test]
    async fn mongo_write_verbs_map_to_affected() {
        let text = "db.a.insertOne({})\ndb.a.insertMany([{}])\ndb.a.updateOne({}, { $set: { a: 1 } })\n\
                    db.a.updateMany({}, { $set: { a: 1 } })\ndb.a.replaceOne({}, { a: 1 })\n\
                    db.a.deleteOne({})\ndb.a.deleteMany({})\ndb.a.aggregate([{ $out: \"x\" }])";
        let fake = FakeMongo::scripted((1..=8).map(|n| RawOutcome::Command { rows_affected: n }).collect());
        let report = go(&fake, &req(text, RunTarget::All)).await;
        let outcomes: Vec<&Outcome> = report.statements.iter().map(|s| &s.outcome).collect();
        let affected: Vec<Outcome> = (1..=7).map(|rows| Outcome::Affected { rows }).collect();
        assert_eq!(outcomes[..7], affected.iter().collect::<Vec<_>>()[..]);
        assert_eq!(outcomes[7], &Outcome::Done);
        let verbs: Vec<&str> = report.statements.iter().map(|s| s.verb.as_str()).collect();
        assert_eq!(
            verbs,
            ["insertOne", "insertMany", "updateOne", "updateMany", "replaceOne", "deleteOne", "deleteMany", "aggregate"]
        );
        assert!(report.statements.iter().all(|s| s.kind == StatementKind::Write));
    }

    #[tokio::test]
    async fn fetch_all_raises_the_cap() {
        let fake = FakeMongo::default();
        go(&fake, &req("db.a.find()", RunTarget::All)).await;
        let mut r = req("db.a.find()", RunTarget::All);
        r.fetch_all = true;
        go(&fake, &r).await;
        let caps: Vec<usize> = fake.calls.lock().unwrap().iter().map(|c| c.1).collect();
        assert_eq!(caps, [1000, 100_000]);
    }
}
