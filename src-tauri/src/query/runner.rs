//! Runs the statements of a `RunRequest` one by one on a tab session.

use std::time::{SystemTime, UNIX_EPOCH};

use super::{
    in_failed_transaction, Outcome, PendingWrite, QuerySession, RawOutcome, RunEvent, RunOutcome, RunReport, RunRequest,
    QueryLanguage, RunTarget, StatementResult, CANCELED_CODE, COMMIT_IN_FAILED_TX,
};
use crate::error::{AppError, AppResult};
use crate::model::{Environment, NewHistoryEntry, ParamValues};
use crate::sqltext::{rewrite_params_in, split_statements_in, Rewritten, SqlDialect, Statement, StatementKind, Utf16Index};

pub(super) const DEFAULT_CAP: usize = 1_000;
pub(super) const FETCH_ALL_CAP: usize = 100_000;

/// A target statement with everything needed to run it.
struct Prepared {
    /// Char span in `req.sql`.
    start: usize,
    end: usize,
    stmt: Statement,
    text: String,
    rewritten: Rewritten,
    values: Vec<Option<String>>,
    params: ParamValues,
}

/// The statements `req.target` picks, each with its char span in `req.sql`. `split` gives a
/// text's statements with their char spans in that text: a selection is split on its own, so a
/// part of a statement runs as that part.
pub(super) fn pick_targets<T>(
    req: &RunRequest,
    idx: &Utf16Index,
    split: impl Fn(&str) -> Vec<(usize, usize, T)>,
) -> Vec<(usize, usize, T)> {
    let chars: Vec<char> = req.sql.chars().collect();
    let slice = |from: usize, to: usize| -> (usize, Vec<(usize, usize, T)>) {
        let from = from.min(chars.len());
        let to = to.clamp(from, chars.len());
        let text: String = chars[from..to].iter().collect();
        (from, split(&text))
    };
    let shift = |base: usize, v: Vec<(usize, usize, T)>| -> Vec<(usize, usize, T)> {
        v.into_iter().map(|(start, end, s)| (base + start, base + end, s)).collect()
    };
    match &req.target {
        RunTarget::All => {
            let (base, v) = slice(0, chars.len());
            shift(base, v)
        }
        // The offsets come from the UI: `to_char` clamps them to the text, and a
        // reversed range is an empty selection.
        RunTarget::Range { from, to } if from > to => vec![],
        RunTarget::Range { from, to } => {
            let (base, v) = slice(idx.to_char(*from), idx.to_char(*to));
            shift(base, v)
        }
        RunTarget::Cursor { at } => {
            let (base, v) = slice(0, chars.len());
            let all = shift(base, v);
            let cursor = idx.to_char(*at);
            let pick = all.iter().rposition(|(start, _, _)| *start <= cursor).unwrap_or(0);
            all.into_iter().nth(pick).into_iter().collect()
        }
    }
}

/// The Production gate: on Production, unless the run is confirmed, the writes among
/// `statements` (kind, verb, text) come back for confirmation and nothing runs. `None`: run.
pub(super) fn production_gate<'a>(
    env: &Environment,
    req: &RunRequest,
    statements: impl Iterator<Item = (StatementKind, &'a str, &'a str)>,
) -> Option<RunReport> {
    if *env != Environment::Production || req.confirmed {
        return None;
    }
    let writes: Vec<PendingWrite> = statements
        .filter(|(kind, _, _)| *kind == StatementKind::Write)
        .map(|(_, verb, text)| PendingWrite { verb: verb.to_string(), preview: preview(text) })
        .collect();
    if writes.is_empty() {
        return None;
    }
    let outcome = RunOutcome::NeedsConfirmation { statements: writes };
    Some(RunReport { outcome, statements: vec![] })
}

pub(super) fn preview(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(120).collect()
}

pub(super) fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Runs `req` on a tab's session: first points it at `req.schema`
/// (`QueryTabSession::prepare_schema`; where a failed switch fails the run, nothing runs), then
/// `run_in`.
pub async fn run_on_tab(
    lang: &QueryLanguage,
    tab: &super::sessions::QueryTabSession,
    env: &Environment,
    req: &RunRequest,
    on_event: &(dyn Fn(RunEvent) + Send + Sync),
) -> AppResult<RunReport> {
    tab.prepare_schema(req.schema.as_deref()).await?;
    run_in(lang, tab.session.as_ref(), env, req, on_event).await
}

/// `run_in` for Postgres.
pub async fn run(
    session: &dyn QuerySession,
    env: &Environment,
    req: &RunRequest,
    on_event: &(dyn Fn(RunEvent) + Send + Sync),
) -> AppResult<RunReport> {
    run_in(&QueryLanguage::Sql(SqlDialect::Postgres), session, env, req, on_event).await
}

pub async fn run_in(
    lang: &QueryLanguage,
    session: &dyn QuerySession,
    env: &Environment,
    req: &RunRequest,
    on_event: &(dyn Fn(RunEvent) + Send + Sync),
) -> AppResult<RunReport> {
    match *lang {
        QueryLanguage::Sql(dialect) => run_sql(dialect, session, env, req, on_event).await,
        QueryLanguage::Mongo => super::mongo_runner::run(session, env, req, on_event).await,
    }
}

async fn run_sql(
    dialect: SqlDialect,
    session: &dyn QuerySession,
    env: &Environment,
    req: &RunRequest,
    on_event: &(dyn Fn(RunEvent) + Send + Sync),
) -> AppResult<RunReport> {
    let idx = Utf16Index::new(&req.sql);
    let chars: Vec<char> = req.sql.chars().collect();

    // Params: resolve every statement before anything runs.
    let mut prepared = Vec::new();
    let split = |text: &str| -> Vec<(usize, usize, Statement)> {
        split_statements_in(dialect, text).into_iter().map(|s| (s.start, s.end, s)).collect()
    };
    for (start, end, stmt) in pick_targets(req, &idx, split) {
        let text: String = chars[start..end].iter().collect();
        let rewritten = rewrite_params_in(dialect, &text);
        // `values` follows the placeholders (`bind_order`); `params` is keyed by distinct name.
        let mut values = Vec::new();
        let mut params = ParamValues::new();
        for name in &rewritten.bind_order {
            let value = req
                .params
                .get(name)
                .ok_or_else(|| AppError::Query(format!("No value for :{name}")))?;
            values.push(value.clone());
            params.insert(name.clone(), value.clone());
        }
        prepared.push(Prepared { start, end, stmt, text, rewritten, values, params });
    }

    let statements = prepared.iter().map(|p| (p.stmt.kind, p.stmt.verb.as_str(), p.text.as_str()));
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
            verb: p.stmt.verb.clone(),
            kind: p.stmt.kind,
            sql: p.text.clone(),
            params: p.params.clone(),
            outcome: Outcome::Skipped,
            elapsed_ms: 0,
            notices: vec![],
        };
        if !stopped {
            on_event(RunEvent::Started { index, total, from, to, verb: p.stmt.verb.clone() });
            let commit_in_failed_tx =
                matches!(p.stmt.verb.as_str(), "COMMIT" | "END") && in_failed_transaction(session).await;
            let run = session.run(&p.rewritten.sql, &p.values, cap).await;
            result.elapsed_ms = run.elapsed_ms;
            result.notices = run.notices;
            result.outcome = match run.outcome {
                RawOutcome::Rows { columns, rows, truncated, cell_types } => {
                    Outcome::Rows { columns, rows, truncated, cell_types }
                }
                RawOutcome::Command { rows_affected } => match p.stmt.verb.as_str() {
                    "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "REPLACE" => Outcome::Affected { rows: rows_affected },
                    _ => Outcome::Done,
                },
                RawOutcome::Error { code, .. } if code.as_deref() == Some(CANCELED_CODE) => Outcome::Canceled,
                RawOutcome::Error { message, code, position } => {
                    let position = position.map(|pos| {
                        let in_stmt = p.rewritten.original_pos(pos.saturating_sub(1));
                        idx.to_utf16(p.start + in_stmt)
                    });
                    Outcome::Error { message, code, position }
                }
            };
            // The statement still ran (it ends the failed transaction), but nothing was committed.
            if commit_in_failed_tx && !matches!(result.outcome, Outcome::Error { .. } | Outcome::Canceled) {
                result.outcome = Outcome::Error { message: COMMIT_IN_FAILED_TX.into(), code: None, position: None };
            }
            stopped = matches!(result.outcome, Outcome::Error { .. } | Outcome::Canceled);
        }
        on_event(RunEvent::Finished { result: result.clone() });
        results.push(result);
    }

    let tx_status = session.tx_status().await;
    Ok(RunReport {
        outcome: RunOutcome::Completed { statement_count: total, started_at, tx_status },
        statements: results,
    })
}

fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn count(n: usize, unit: &str, suffix: &str) -> String {
    format!("{} {unit}{}{suffix}", thousands(n), if n == 1 { "" } else { "s" })
}

/// `history_entries` with the database and schema the run targeted set on every entry
/// (an empty name counts as none).
pub fn history_for_run(
    statements: &[StatementResult],
    started_at: i64,
    database: Option<&str>,
    schema: Option<&str>,
) -> Vec<NewHistoryEntry> {
    let named = |n: Option<&str>| n.filter(|n| !n.is_empty()).map(str::to_string);
    let (database, schema) = (named(database), named(schema));
    let mut entries = history_entries(statements, started_at);
    for e in &mut entries {
        e.target_database.clone_from(&database);
        e.target_schema.clone_from(&schema);
    }
    entries
}

/// One entry per statement that ran (not Skipped).
pub fn history_entries(statements: &[StatementResult], started_at: i64) -> Vec<NewHistoryEntry> {
    statements
        .iter()
        .filter_map(|s| {
            let (ok, summary) = match &s.outcome {
                Outcome::Skipped => return None,
                Outcome::Rows { rows, truncated: true, .. } => {
                    (true, format!("{}+ rows", thousands(rows.len())))
                }
                Outcome::Rows { rows, .. } => (true, count(rows.len(), "row", "")),
                Outcome::Affected { rows } => (true, count(*rows as usize, "row", " affected")),
                Outcome::Done => (true, "Done".to_string()),
                Outcome::Error { message, .. } => (false, message.clone()),
                Outcome::Canceled => (false, "Canceled".to_string()),
            };
            Some(NewHistoryEntry {
                sql: s.sql.clone(),
                params: s.params.clone(),
                verb: s.verb.clone(),
                ok,
                summary,
                started_at,
                elapsed_ms: s.elapsed_ms,
                target_database: None,
                target_schema: None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;
    use crate::error::AppError;
    use crate::model::ParamValues;
    use crate::query::{end_tab_transaction, Outcome, RawOutcome, RunTarget, StatementRun, TxStatus};

    type Call = (String, Vec<Option<String>>, usize);

    #[derive(Default)]
    struct FakeSession {
        calls: Mutex<Vec<Call>>,
        script: Mutex<VecDeque<RawOutcome>>,
        /// What `tx_status` reports; `None` = Open.
        tx: Option<TxStatus>,
    }

    impl FakeSession {
        fn scripted(outcomes: Vec<RawOutcome>) -> Self {
            Self { script: Mutex::new(outcomes.into()), ..Self::default() }
        }
        fn sqls(&self) -> Vec<String> {
            self.calls.lock().unwrap().iter().map(|c| c.0.clone()).collect()
        }
    }

    #[async_trait]
    impl QuerySession for FakeSession {
        async fn run(&self, sql: &str, params: &[Option<String>], row_cap: usize) -> StatementRun {
            self.calls.lock().unwrap().push((sql.to_string(), params.to_vec(), row_cap));
            let outcome = self
                .script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(RawOutcome::Command { rows_affected: 0 });
            StatementRun { outcome, notices: vec![], elapsed_ms: 5 }
        }
        async fn run_simple(&self, sql: &str) -> RawOutcome {
            self.run(sql, &[], 1).await.outcome
        }
        async fn cancel(&self) -> AppResult<()> {
            Ok(())
        }
        async fn tx_status(&self) -> TxStatus {
            self.tx.unwrap_or(TxStatus::Open)
        }
        fn is_closed(&self) -> bool {
            false
        }
        async fn set_search_path(&self, _: Option<&str>) -> AppResult<()> {
            Ok(())
        }
        async fn close(&self) {}
    }

    fn req(sql: &str, target: RunTarget) -> RunRequest {
        RunRequest {
            sql: sql.into(),
            target,
            params: ParamValues::new(),
            schema: None,
            confirmed: false,
            fetch_all: false,
            saved_query_id: None,
            database: None,
        }
    }

    fn err(message: &str, code: Option<&str>, position: Option<usize>) -> RawOutcome {
        RawOutcome::Error { message: message.into(), code: code.map(Into::into), position }
    }

    fn rows1() -> RawOutcome {
        RawOutcome::Rows {
            columns: vec![crate::query::ResultColumn { name: "a".into(), data_type: "int4".into() }],
            rows: vec![vec![Some("1".into())]],
            truncated: false,
            cell_types: None,
        }
    }

    async fn go(fake: &FakeSession, r: &RunRequest) -> RunReport {
        run(fake, &Environment::Local, r, &|_| {}).await.unwrap()
    }

    #[tokio::test]
    async fn cursor_picks_statement_under_cursor() {
        let sql = "select 1;\nselect 2;\nselect 3";
        let fake = FakeSession::default();
        go(&fake, &req(sql, RunTarget::Cursor { at: 15 })).await;
        assert_eq!(fake.sqls(), vec!["select 2"]);
    }

    #[tokio::test]
    async fn cursor_in_gap_picks_previous_and_before_first_picks_first() {
        let sql = "  select 1;\n\n  select 2";
        let fake = FakeSession::default();
        // In the gap after the first statement's `;`.
        go(&fake, &req(sql, RunTarget::Cursor { at: 12 })).await;
        assert_eq!(fake.sqls(), vec!["select 1"]);
        let fake = FakeSession::default();
        go(&fake, &req(sql, RunTarget::Cursor { at: 0 })).await;
        assert_eq!(fake.sqls(), vec!["select 1"]);
    }

    #[tokio::test]
    async fn range_runs_selection_with_editor_offsets() {
        let sql = "select 0;\nselect 1; select nope;\nselect 3";
        let from = sql.find("select 1").unwrap();
        let to = sql.find(";\nselect 3").unwrap();
        let fake = FakeSession::scripted(vec![rows1(), err("bad", Some("42703"), Some(8))]);
        let report = go(&fake, &req(sql, RunTarget::Range { from, to })).await;
        assert_eq!(fake.sqls(), vec!["select 1", "select nope"]);
        let r0 = &report.statements[0];
        assert_eq!((r0.from, r0.to), (from, from + 8));
        let r1 = &report.statements[1];
        let start1 = sql.find("select nope").unwrap();
        assert_eq!((r1.from, r1.to), (start1, start1 + 11));
        match &r1.outcome {
            // position 8 (1-based) -> char 7 of "select nope" -> "nope" starts at 7.
            Outcome::Error { position, .. } => assert_eq!(*position, Some(start1 + 7)),
            o => panic!("{o:?}"),
        }
    }

    #[tokio::test]
    async fn range_after_astral_prefix_reports_full_text_offsets() {
        let sql = "-- 😀é\nselect 1; select :xyz, nope; select 3";
        let sel = "select :xyz, nope";
        let from_byte = sql.find(sel).unwrap();
        let from = sql[..from_byte].encode_utf16().count();
        let to = from + sel.encode_utf16().count();
        // Rewritten "select $1, nope": `nope` is char 11 -> 1-based 12.
        let fake = FakeSession::scripted(vec![err("bad", Some("42703"), Some(12))]);
        let mut r = req(sql, RunTarget::Range { from, to });
        r.params.insert("xyz".into(), Some("1".into()));
        let report = go(&fake, &r).await;
        assert_eq!(fake.sqls(), vec!["select $1, nope"]);
        let res = &report.statements[0];
        assert_eq!((res.from, res.to), (from, to));
        let nope = from + "select :xyz, ".encode_utf16().count();
        match &res.outcome {
            Outcome::Error { position, .. } => assert_eq!(*position, Some(nope)),
            o => panic!("{o:?}"),
        }
    }

    #[tokio::test]
    async fn run_all_stops_at_first_error() {
        let fake = FakeSession::scripted(vec![rows1(), err("boom", Some("22012"), None)]);
        let report = go(&fake, &req("select 1; select 1/0; select 3", RunTarget::All)).await;
        let kinds: Vec<&str> = report
            .statements
            .iter()
            .map(|s| match s.outcome {
                Outcome::Rows { .. } => "rows",
                Outcome::Error { .. } => "error",
                Outcome::Skipped => "skipped",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["rows", "error", "skipped"]);
        assert_eq!(fake.sqls().len(), 2);
        let RunOutcome::Completed { statement_count, tx_status, .. } = report.outcome else {
            panic!("expected Completed");
        };
        assert_eq!((statement_count, tx_status), (3, TxStatus::Open));
    }

    #[tokio::test]
    async fn cancel_maps_to_canceled_and_skips_rest() {
        let fake = FakeSession::scripted(vec![err("canceling statement", Some("57014"), None)]);
        let report = go(&fake, &req("select pg_sleep(9); select 2", RunTarget::All)).await;
        assert_eq!(report.statements[0].outcome, Outcome::Canceled);
        assert_eq!(report.statements[1].outcome, Outcome::Skipped);
        assert_eq!(fake.sqls().len(), 1);
    }

    #[tokio::test]
    async fn params_rewritten_and_bound() {
        let fake = FakeSession::default();
        let mut r = req("select :a, :b, :a", RunTarget::All);
        r.params.insert("a".into(), Some("1".into()));
        r.params.insert("b".into(), None);
        r.params.insert("unused".into(), Some("x".into()));
        let report = go(&fake, &r).await;
        let calls = fake.calls.lock().unwrap();
        assert_eq!(calls[0].0, "select $1, $2, $1");
        assert_eq!(calls[0].1, vec![Some("1".to_string()), None]);
        let mut want = ParamValues::new();
        want.insert("a".into(), Some("1".into()));
        want.insert("b".into(), None);
        assert_eq!(report.statements[0].params, want);
        assert_eq!(report.statements[0].sql, "select :a, :b, :a");
    }

    #[tokio::test]
    async fn missing_param_errors_before_running() {
        let fake = FakeSession::default();
        let mut r = req("select 1; select :a, :b", RunTarget::All);
        r.params.insert("a".into(), Some("1".into()));
        let res = run(&fake, &Environment::Local, &r, &|_| {}).await;
        assert_eq!(res.unwrap_err(), AppError::Query("No value for :b".into()));
        assert!(fake.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn production_write_needs_confirmation() {
        let fake = FakeSession::default();
        let r = req("select 1; update t set a=1", RunTarget::All);
        let report = run(&fake, &Environment::Production, &r, &|_| {}).await.unwrap();
        assert_eq!(
            report.outcome,
            RunOutcome::NeedsConfirmation {
                statements: vec![PendingWrite { verb: "UPDATE".into(), preview: "update t set a=1".into() }]
            }
        );
        assert!(fake.calls.lock().unwrap().is_empty());

        let mut confirmed = r.clone();
        confirmed.confirmed = true;
        run(&fake, &Environment::Production, &confirmed, &|_| {}).await.unwrap();
        assert_eq!(fake.sqls().len(), 2);

        let fake = FakeSession::default();
        run(&fake, &Environment::Local, &r, &|_| {}).await.unwrap();
        assert_eq!(fake.sqls().len(), 2);

        let fake = FakeSession::default();
        let read = req("select 1", RunTarget::All);
        run(&fake, &Environment::Production, &read, &|_| {}).await.unwrap();
        assert_eq!(fake.sqls().len(), 1);
    }

    #[tokio::test]
    async fn preview_collapses_whitespace_and_truncates() {
        let fake = FakeSession::default();
        let sql = format!("update   t\n\tset a = 1 {}", "x".repeat(200));
        let report = run(&fake, &Environment::Production, &req(&sql, RunTarget::All), &|_| {}).await.unwrap();
        let RunOutcome::NeedsConfirmation { statements } = report.outcome else { panic!() };
        assert!(statements[0].preview.starts_with("update t set a = 1 xxx"));
        assert_eq!(statements[0].preview.chars().count(), 120);
    }

    #[tokio::test]
    async fn error_position_maps_to_editor_utf16() {
        let sql = "select 1;\nselect '😀', :xyz, nope";
        let rewritten = "select '😀', $1, nope";
        let char_pos = rewritten[..rewritten.find("nope").unwrap()].chars().count();
        let fake = FakeSession::scripted(vec![rows1(), err("no column", Some("42703"), Some(1 + char_pos))]);
        let mut r = req(sql, RunTarget::All);
        r.params.insert("xyz".into(), Some("1".into()));
        let report = go(&fake, &r).await;
        let want: usize = sql[..sql.find("nope").unwrap()].encode_utf16().count();
        match &report.statements[1].outcome {
            Outcome::Error { position, .. } => assert_eq!(*position, Some(want)),
            o => panic!("{o:?}"),
        }
        // Spans are UTF-16 too.
        assert_eq!(report.statements[1].to, sql.encode_utf16().count());
    }

    #[tokio::test]
    async fn events_in_order() {
        let fake = FakeSession::default();
        let events = Mutex::new(Vec::new());
        let r = req("select 1; select 2", RunTarget::All);
        run(&fake, &Environment::Local, &r, &|e| events.lock().unwrap().push(e)).await.unwrap();
        let ev = events.lock().unwrap();
        let shape: Vec<(&str, usize)> = ev
            .iter()
            .map(|e| match e {
                RunEvent::Started { index, total, .. } => {
                    assert_eq!(*total, 2);
                    ("started", *index)
                }
                RunEvent::Finished { result } => ("finished", result.index),
            })
            .collect();
        assert_eq!(shape, [("started", 0), ("finished", 0), ("started", 1), ("finished", 1)]);
    }

    #[tokio::test]
    async fn command_outcomes() {
        let fake = FakeSession::scripted(vec![
            RawOutcome::Command { rows_affected: 3 },
            RawOutcome::Command { rows_affected: 0 },
        ]);
        let report = go(&fake, &req("update t set a=1; create table z(a int)", RunTarget::All)).await;
        assert_eq!(report.statements[0].outcome, Outcome::Affected { rows: 3 });
        assert_eq!(report.statements[1].outcome, Outcome::Done);
    }

    #[tokio::test]
    async fn fetch_all_raises_cap() {
        let fake = FakeSession::default();
        go(&fake, &req("select 1", RunTarget::All)).await;
        let mut r = req("select 1", RunTarget::All);
        r.fetch_all = true;
        go(&fake, &r).await;
        let caps: Vec<usize> = fake.calls.lock().unwrap().iter().map(|c| c.2).collect();
        assert_eq!(caps, [1000, 100_000]);
    }

    fn probe_failed() -> RawOutcome {
        err("current transaction is aborted", Some("25P02"), None)
    }

    #[tokio::test]
    async fn commit_in_failed_transaction_runs_but_reports_an_error() {
        // The status is Unknown (pg_stat_activity hidden): only the probe tells.
        for sql in ["commit; select 2", "END; select 2"] {
            let fake = FakeSession {
                tx: Some(TxStatus::Unknown),
                ..FakeSession::scripted(vec![probe_failed(), RawOutcome::Command { rows_affected: 0 }])
            };
            let report = go(&fake, &req(sql, RunTarget::All)).await;
            // Postgres still has to see it: it ends the failed transaction.
            assert_eq!(fake.sqls().len(), 2, "{sql}");
            assert_eq!(fake.sqls()[0], "SELECT 1");
            assert_eq!(
                report.statements[0].outcome,
                Outcome::Error { message: COMMIT_IN_FAILED_TX.into(), code: None, position: None }
            );
            assert_eq!(report.statements[1].outcome, Outcome::Skipped);
        }
        // In an open (or no) transaction COMMIT is an ordinary command.
        let fake = FakeSession::default();
        let report = go(&fake, &req("commit", RunTarget::All)).await;
        assert_eq!(report.statements[0].outcome, Outcome::Done);
        // Only COMMIT/END probe.
        let fake = FakeSession::default();
        go(&fake, &req("select 1", RunTarget::All)).await;
        assert_eq!(fake.sqls(), ["select 1"]);
    }

    #[tokio::test]
    async fn range_beyond_text_or_reversed_never_panics() {
        let sql = "select 1; select 2";
        let n = sql.encode_utf16().count();
        let fake = FakeSession::default();
        go(&fake, &req(sql, RunTarget::Range { from: 10, to: n + 50 })).await;
        assert_eq!(fake.sqls(), vec!["select 2"]);

        let fake = FakeSession::default();
        go(&fake, &req(sql, RunTarget::Range { from: n + 5, to: n + 50 })).await;
        assert!(fake.sqls().is_empty());

        let fake = FakeSession::default();
        let report = go(&fake, &req(sql, RunTarget::Range { from: 8, to: 2 })).await;
        assert!(fake.sqls().is_empty());
        assert!(matches!(report.outcome, RunOutcome::Completed { statement_count: 0, .. }));
    }

    #[tokio::test]
    async fn no_statements_completes_empty() {
        let fake = FakeSession::default();
        let report = go(&fake, &req("  ;; ", RunTarget::All)).await;
        assert!(matches!(report.outcome, RunOutcome::Completed { statement_count: 0, .. }));
        assert!(fake.calls.lock().unwrap().is_empty());
    }

    fn result(index: usize, outcome: Outcome) -> StatementResult {
        StatementResult {
            index,
            from: 0,
            to: 1,
            verb: "SELECT".into(),
            kind: crate::sqltext::StatementKind::Read,
            sql: format!("s{index}"),
            params: ParamValues::new(),
            outcome,
            elapsed_ms: 7,
            notices: vec![],
        }
    }

    fn rows_outcome(n: usize, truncated: bool) -> Outcome {
        Outcome::Rows { columns: vec![], rows: vec![vec![]; n], truncated, cell_types: None }
    }

    #[test]
    fn history_entries_skip_skipped() {
        let stmts = vec![
            result(0, rows_outcome(20, false)),
            result(1, rows_outcome(1, false)),
            result(2, rows_outcome(1000, true)),
            result(3, Outcome::Affected { rows: 37 }),
            result(4, Outcome::Affected { rows: 1 }),
            result(5, Outcome::Done),
            result(6, Outcome::Error { message: "boom".into(), code: None, position: None }),
            result(7, Outcome::Canceled),
            result(8, Outcome::Skipped),
        ];
        let h = history_entries(&stmts, 99);
        let summaries: Vec<(&str, bool)> = h.iter().map(|e| (e.summary.as_str(), e.ok)).collect();
        assert_eq!(
            summaries,
            [
                ("20 rows", true),
                ("1 row", true),
                ("1,000+ rows", true),
                ("37 rows affected", true),
                ("1 row affected", true),
                ("Done", true),
                ("boom", false),
                ("Canceled", false),
            ]
        );
        assert_eq!(h[0].started_at, 99);
        assert_eq!((h[0].elapsed_ms, h[0].sql.as_str(), h[0].verb.as_str()), (7, "s0", "SELECT"));
        let capped = history_entries(&[result(0, rows_outcome(100_000, true))], 1);
        assert_eq!(capped[0].summary, "100,000+ rows");
    }

    #[test]
    fn history_for_run_sets_the_target_on_every_entry() {
        let stmts = vec![result(0, rows_outcome(1, false)), result(1, Outcome::Done)];
        let h = history_for_run(&stmts, 5, Some("fx_other"), Some("sales"));
        assert_eq!(h.len(), 2);
        for e in &h {
            assert_eq!(e.target_database.as_deref(), Some("fx_other"));
            assert_eq!(e.target_schema.as_deref(), Some("sales"));
        }
        let blank = history_for_run(&stmts, 5, None, Some(""));
        assert!(blank.iter().all(|e| e.target_database.is_none() && e.target_schema.is_none()));
    }

    #[tokio::test]
    async fn end_tab_transaction_rolls_back_a_failed_commit_and_errors() {
        let fake = FakeSession {
            tx: Some(TxStatus::Unknown),
            ..FakeSession::scripted(vec![probe_failed(), RawOutcome::Command { rows_affected: 0 }])
        };
        let res = end_tab_transaction(&fake, true).await;
        assert_eq!(res, Err(AppError::Query(COMMIT_IN_FAILED_TX.into())));
        assert_eq!(fake.sqls(), ["SELECT 1", "ROLLBACK"]);
    }

    #[tokio::test]
    async fn end_tab_transaction_commits_when_healthy() {
        let fake = FakeSession { tx: Some(TxStatus::Idle), ..FakeSession::default() };
        assert_eq!(end_tab_transaction(&fake, true).await, Ok(TxStatus::Idle));
        assert_eq!(fake.sqls(), ["SELECT 1", "COMMIT"]);
    }

    #[tokio::test]
    async fn end_tab_transaction_maps_a_commit_error() {
        let fake = FakeSession::scripted(vec![RawOutcome::Command { rows_affected: 0 }, err("boom", None, None)]);
        assert_eq!(end_tab_transaction(&fake, true).await, Err(AppError::Query("boom".into())));
    }

    #[tokio::test]
    async fn end_tab_transaction_rollback_never_probes() {
        let fake = FakeSession { tx: Some(TxStatus::Idle), ..FakeSession::default() };
        assert_eq!(end_tab_transaction(&fake, false).await, Ok(TxStatus::Idle));
        assert_eq!(fake.sqls(), ["ROLLBACK"]);
    }
}
