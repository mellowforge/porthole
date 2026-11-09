//! Registry of query-tab sessions, keyed by (window label, tab id).

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use tokio::sync::Mutex;

use super::{in_transaction, QuerySession, SWITCH_DATABASE_IN_TX};
use crate::error::{AppError, AppResult};

pub struct QueryTabSession {
    pub session: Arc<dyn QuerySession>,
    /// Held for the duration of a run (or COMMIT/ROLLBACK); taken with `try_lock`.
    pub run_lock: Mutex<()>,
    /// The last value applied by `apply_schema` (`None` = as opened). Only used to detect
    /// the Some -> None transition; a `Some` is re-applied on every run because ROLLBACK
    /// undoes it.
    pub search_path: Mutex<Option<String>>,
}

impl QueryTabSession {
    /// Points the session at `schema` before a run. `set_config(.., false)` is undone by a
    /// ROLLBACK, so a `Some` schema is applied on every run. `None` only restores the
    /// session's own search_path once, on the Some -> None transition.
    ///
    /// A failure never blocks the run (inside an aborted transaction Postgres rejects the
    /// SET with 25P02, and the user must still be able to send ROLLBACK). It is logged and
    /// the stored state is left as it was, so the next run tries again.
    pub async fn apply_schema(&self, schema: Option<&str>) {
        if let Err(e) = self.prepare_schema(schema).await {
            eprintln!("could not switch the database: {e}");
        }
    }

    /// As `apply_schema`, but for a session whose failed switch fails the run
    /// (`QuerySession::schema_switch_fails_run`, MySQL's `USE`) the failure is returned, and
    /// the caller doesn't run the statement. Any other session's failure is only logged, as
    /// `apply_schema` describes.
    pub async fn prepare_schema(&self, schema: Option<&str>) -> AppResult<()> {
        let mut current = self.search_path.lock().await;
        let result = match schema {
            Some(s) => self.session.set_search_path(Some(s)).await.map(|()| Some(s.to_string())),
            None if current.is_some() => self.session.set_search_path(None).await.map(|()| None),
            None => return Ok(()),
        };
        match result {
            Ok(applied) => *current = applied,
            Err(e) if self.session.schema_switch_fails_run() => return Err(e),
            Err(e) => eprintln!("could not set the search_path: {e}"),
        }
        Ok(())
    }
}

type Key = (String, String);

/// A tab's open session and the database it is on (`None` = the profile's own).
struct Open {
    database: Option<String>,
    tab: Arc<QueryTabSession>,
}

/// One slot per tab. Opening holds only that slot's lock, never the registry's.
type Slot = Arc<Mutex<Option<Open>>>;

/// Returned while a run (or COMMIT/ROLLBACK) holds the tab's `run_lock`.
pub const QUERY_RUNNING: &str = "A query is already running in this tab";

#[derive(Default)]
pub struct QuerySessions {
    slots: std::sync::Mutex<HashMap<Key, Slot>>,
}

impl QuerySessions {
    fn slots(&self) -> std::sync::MutexGuard<'_, HashMap<Key, Slot>> {
        self.slots.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Returns the tab's live session on `database` (`None` = the profile's own; callers
    /// pass `None` for the profile's database by name too). Opens one when there is none,
    /// the old one is closed, or it is on another database. A session on another database
    /// is closed first, unless it is inside a transaction (or running a query): then the
    /// switch is refused and the session kept.
    pub async fn get_or_open(
        &self,
        label: &str,
        tab_id: &str,
        database: Option<&str>,
        open: impl Future<Output = AppResult<Arc<dyn QuerySession>>>,
    ) -> AppResult<Arc<QueryTabSession>> {
        let key = (label.to_string(), tab_id.to_string());
        let slot = self.slots().entry(key.clone()).or_default().clone();
        let mut guard = slot.lock().await;
        if let Some(existing) = guard.as_ref()
            && !existing.tab.session.is_closed()
        {
            if existing.database.as_deref() == database {
                return Ok(existing.tab.clone());
            }
            let Ok(_running) = existing.tab.run_lock.try_lock() else {
                return Err(AppError::Query(QUERY_RUNNING.into()));
            };
            // Bounded: it runs under the slot lock. No answer counts as a failure, so the
            // switch is refused and the session kept.
            let session = existing.tab.session.as_ref();
            let limit = session.probe_timeout();
            let in_tx = tokio::time::timeout(limit, in_transaction(session)).await.map_err(|_| {
                AppError::Query(format!(
                    "Couldn't tell within {} s whether this tab is in a transaction, so it stays on its database. Try again.",
                    limit.as_secs().max(1)
                ))
            })??;
            if in_tx {
                return Err(AppError::Query(SWITCH_DATABASE_IN_TX.into()));
            }
        }
        if let Some(old) = guard.take() {
            old.tab.session.close().await;
        }
        let session = open.await?;
        // The tab may have been closed while we were opening.
        let still_registered = self.slots().get(&key).is_some_and(|s| Arc::ptr_eq(s, &slot));
        if !still_registered {
            session.close().await;
            return Err(AppError::Connection("The query tab was closed.".into()));
        }
        let tab = Arc::new(QueryTabSession {
            session,
            run_lock: Mutex::new(()),
            search_path: Mutex::new(None),
        });
        *guard = Some(Open { database: database.map(str::to_string), tab: tab.clone() });
        Ok(tab)
    }

    /// Never waits: a tab that is still opening has no session yet.
    pub async fn get(&self, label: &str, tab_id: &str) -> Option<Arc<QueryTabSession>> {
        let slot = self.slots().get(&(label.to_string(), tab_id.to_string()))?.clone();
        let open = slot.try_lock().ok()?;
        open.as_ref().map(|o| o.tab.clone())
    }

    pub async fn close(&self, label: &str, tab_id: &str) {
        let slot = self.slots().remove(&(label.to_string(), tab_id.to_string()));
        if let Some(slot) = slot {
            close_slot(&slot).await;
        }
    }

    pub async fn close_window(&self, label: &str) {
        let removed: Vec<Slot> = {
            let mut map = self.slots();
            let keys: Vec<Key> = map.keys().filter(|(l, _)| l == label).cloned().collect();
            keys.iter().filter_map(|k| map.remove(k)).collect()
        };
        for slot in removed {
            close_slot(&slot).await;
        }
    }
}

async fn close_slot(slot: &Slot) {
    let taken = slot.lock().await.take();
    if let Some(open) = taken {
        open.tab.session.close().await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use async_trait::async_trait;

    use super::*;
    use crate::query::{RawOutcome, StatementRun, TxStatus};

    #[derive(Default)]
    struct Dummy {
        closed: AtomicBool,
    }

    #[async_trait]
    impl QuerySession for Dummy {
        async fn run(&self, _: &str, _: &[Option<String>], _: usize) -> StatementRun {
            StatementRun { outcome: RawOutcome::Command { rows_affected: 0 }, notices: vec![], elapsed_ms: 0 }
        }
        /// Never in a transaction.
        async fn run_simple(&self, _: &str) -> RawOutcome {
            RawOutcome::Rows { columns: vec![], rows: vec![vec![Some("f".into())]], truncated: false, cell_types: None }
        }
        async fn cancel(&self) -> AppResult<()> {
            Ok(())
        }
        async fn tx_status(&self) -> TxStatus {
            TxStatus::Idle
        }
        fn is_closed(&self) -> bool {
            self.closed.load(Ordering::SeqCst)
        }
        async fn set_search_path(&self, _: Option<&str>) -> AppResult<()> {
            Ok(())
        }
        async fn close(&self) {
            self.closed.store(true, Ordering::SeqCst);
        }
    }

    async fn open_dummy(opens: &AtomicUsize) -> AppResult<Arc<dyn QuerySession>> {
        opens.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(Dummy::default()) as Arc<dyn QuerySession>)
    }

    #[tokio::test]
    async fn get_or_open_opens_once() {
        let reg = QuerySessions::default();
        let opens = AtomicUsize::new(0);
        let a = reg.get_or_open("w", "t1", None, open_dummy(&opens)).await.unwrap();
        let b = reg.get_or_open("w", "t1", None, open_dummy(&opens)).await.unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(opens.load(Ordering::SeqCst), 1);
        assert!(reg.get("w", "t1").await.is_some());
        assert!(reg.get("w", "t2").await.is_none());
    }

    #[tokio::test]
    async fn close_window_closes_only_that_window() {
        let reg = QuerySessions::default();
        let opens = AtomicUsize::new(0);
        let a = reg.get_or_open("w1", "t1", None, open_dummy(&opens)).await.unwrap();
        let b = reg.get_or_open("w1", "t2", None, open_dummy(&opens)).await.unwrap();
        let c = reg.get_or_open("w2", "t1", None, open_dummy(&opens)).await.unwrap();
        reg.close_window("w1").await;
        assert!(a.session.is_closed() && b.session.is_closed());
        assert!(!c.session.is_closed());
        assert!(reg.get("w1", "t1").await.is_none());
        assert!(reg.get("w2", "t1").await.is_some());
    }

    #[tokio::test]
    async fn closed_session_is_replaced_on_next_open() {
        let reg = QuerySessions::default();
        let opens = AtomicUsize::new(0);
        let a = reg.get_or_open("w", "t", None, open_dummy(&opens)).await.unwrap();
        reg.close("w", "t").await;
        assert!(a.session.is_closed());
        let b = reg.get_or_open("w", "t", None, open_dummy(&opens)).await.unwrap();
        assert!(!Arc::ptr_eq(&a, &b));
        assert!(!b.session.is_closed());
        // A session that died on its own is replaced too.
        b.session.close().await;
        let c = reg.get_or_open("w", "t", None, open_dummy(&opens)).await.unwrap();
        assert!(!c.session.is_closed());
        assert_eq!(opens.load(Ordering::SeqCst), 3);
    }

    use tokio::time::{timeout, Duration};

    #[tokio::test]
    async fn pending_open_does_not_block_other_tabs() {
        let reg = Arc::new(QuerySessions::default());
        let other = reg.get_or_open("w2", "x", None, open_dummy(&AtomicUsize::new(0))).await.unwrap();
        let gate = Arc::new(tokio::sync::Notify::new());
        let slow = {
            let (reg, gate) = (reg.clone(), gate.clone());
            tokio::spawn(async move {
                reg.get_or_open("w1", "slow", None, async move {
                    gate.notified().await;
                    Ok(Arc::new(Dummy::default()) as Arc<dyn QuerySession>)
                })
                .await
            })
        };
        tokio::task::yield_now().await;
        let quick = Duration::from_millis(500);
        timeout(quick, async {
            assert!(reg.get("w2", "x").await.is_some());
            assert!(reg.get("w1", "slow").await.is_none());
            reg.get_or_open("w1", "fast", None, open_dummy(&AtomicUsize::new(0))).await.unwrap();
            reg.close("w1", "fast").await;
            reg.close_window("w2").await;
        })
        .await
        .expect("other tabs and windows must not wait for a pending open");
        assert!(other.session.is_closed());
        gate.notify_one();
        assert!(slow.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn concurrent_get_or_open_same_key_opens_once() {
        let reg = Arc::new(QuerySessions::default());
        let opens = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..4 {
            let (reg, opens) = (reg.clone(), opens.clone());
            tasks.push(tokio::spawn(async move {
                reg.get_or_open("w", "t", None, async {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    open_dummy(&opens).await
                })
                .await
                .unwrap()
            }));
        }
        let mut got = Vec::new();
        for t in tasks {
            got.push(t.await.unwrap());
        }
        assert_eq!(opens.load(Ordering::SeqCst), 1);
        assert!(got.iter().all(|g| Arc::ptr_eq(g, &got[0])));
    }

    #[derive(Default)]
    struct PathLog(std::sync::Mutex<Vec<Option<String>>>, std::sync::atomic::AtomicBool);

    #[async_trait]
    impl QuerySession for PathLog {
        async fn run(&self, _: &str, _: &[Option<String>], _: usize) -> StatementRun {
            StatementRun { outcome: RawOutcome::Command { rows_affected: 0 }, notices: vec![], elapsed_ms: 0 }
        }
        async fn run_simple(&self, _: &str) -> RawOutcome {
            unimplemented!()
        }
        async fn cancel(&self) -> AppResult<()> {
            Ok(())
        }
        async fn tx_status(&self) -> TxStatus {
            TxStatus::Idle
        }
        fn is_closed(&self) -> bool {
            false
        }
        async fn set_search_path(&self, s: Option<&str>) -> AppResult<()> {
            self.0.lock().unwrap().push(s.map(String::from));
            if self.1.load(Ordering::SeqCst) {
                return Err(AppError::Query("current transaction is aborted".into()));
            }
            Ok(())
        }
        async fn close(&self) {}
    }

    #[tokio::test]
    async fn schema_is_set_every_run_and_restored_once() {
        let log = Arc::new(PathLog::default());
        let tab = QueryTabSession {
            session: log.clone(),
            run_lock: Mutex::new(()),
            search_path: Mutex::new(None),
        };
        tab.apply_schema(None).await; // None -> None: nothing
        tab.apply_schema(Some("a")).await;
        tab.apply_schema(Some("a")).await; // a ROLLBACK may have undone it
        tab.apply_schema(None).await;
        tab.apply_schema(None).await;
        let want: Vec<Option<String>> = vec![Some("a".into()), Some("a".into()), None];
        assert_eq!(*log.0.lock().unwrap(), want);
    }

    #[tokio::test]
    async fn failed_set_search_path_does_not_block_and_is_retried() {
        let log = Arc::new(PathLog::default());
        log.1.store(true, Ordering::SeqCst);
        let tab = QueryTabSession {
            session: log.clone(),
            run_lock: Mutex::new(()),
            search_path: Mutex::new(None),
        };
        tab.apply_schema(Some("a")).await; // returns: the failure is only logged
        assert!(tab.session.run("select 1", &[], 1).await.notices.is_empty()); // the run still goes out
        tab.apply_schema(Some("a")).await;
        assert_eq!(log.0.lock().unwrap().len(), 2);
        // A failed restore is retried on the next run.
        log.1.store(false, Ordering::SeqCst);
        tab.apply_schema(Some("a")).await;
        log.1.store(true, Ordering::SeqCst);
        tab.apply_schema(None).await;
        tab.apply_schema(None).await;
        let last: Vec<Option<String>> = log.0.lock().unwrap().iter().rev().take(2).cloned().collect();
        assert_eq!(last, vec![None, None]);
    }

    /// A session whose schema switch always fails, and which says such a failure fails the run.
    #[derive(Default)]
    struct StrictSwitch(PathLog);

    #[async_trait]
    impl QuerySession for StrictSwitch {
        async fn run(&self, sql: &str, p: &[Option<String>], cap: usize) -> StatementRun {
            self.0.run(sql, p, cap).await
        }
        async fn run_simple(&self, _: &str) -> RawOutcome {
            unimplemented!()
        }
        async fn cancel(&self) -> AppResult<()> {
            Ok(())
        }
        async fn tx_status(&self) -> TxStatus {
            TxStatus::Idle
        }
        fn is_closed(&self) -> bool {
            false
        }
        async fn set_search_path(&self, s: Option<&str>) -> AppResult<()> {
            self.0.set_search_path(s).await
        }
        fn schema_switch_fails_run(&self) -> bool {
            true
        }
        async fn close(&self) {}
    }

    #[tokio::test]
    async fn a_failed_switch_fails_the_run_only_where_the_session_says_so() {
        let strict = Arc::new(StrictSwitch::default());
        strict.0 .1.store(true, Ordering::SeqCst);
        let tab = QueryTabSession { session: strict.clone(), run_lock: Mutex::new(()), search_path: Mutex::new(None) };
        assert_eq!(tab.prepare_schema(Some("a")).await, Err(AppError::Query("current transaction is aborted".into())));
        assert_eq!(*tab.search_path.lock().await, None);
        strict.0 .1.store(false, Ordering::SeqCst);
        assert_eq!(tab.prepare_schema(Some("a")).await, Ok(()));
        assert_eq!(tab.search_path.lock().await.as_deref(), Some("a"));

        // Postgres (the default): logged, and the run goes on.
        let lax = Arc::new(PathLog::default());
        lax.1.store(true, Ordering::SeqCst);
        let tab = QueryTabSession { session: lax, run_lock: Mutex::new(()), search_path: Mutex::new(None) };
        assert_eq!(tab.prepare_schema(Some("a")).await, Ok(()));
    }

    /// A session whose transaction probe answers from `probe`.
    struct TxFake {
        probe: std::sync::Mutex<RawOutcome>,
        closed: AtomicBool,
    }

    impl TxFake {
        fn answering(probe: RawOutcome) -> Arc<TxFake> {
            Arc::new(TxFake { probe: std::sync::Mutex::new(probe), closed: AtomicBool::new(false) })
        }
    }

    fn probe_rows(value: &str) -> RawOutcome {
        RawOutcome::Rows { columns: vec![], rows: vec![vec![Some(value.into())]], truncated: false, cell_types: None }
    }

    #[async_trait]
    impl QuerySession for TxFake {
        async fn run(&self, _: &str, _: &[Option<String>], _: usize) -> StatementRun {
            StatementRun { outcome: RawOutcome::Command { rows_affected: 0 }, notices: vec![], elapsed_ms: 0 }
        }
        async fn run_simple(&self, sql: &str) -> RawOutcome {
            assert_eq!(sql, "SELECT transaction_timestamp() <> statement_timestamp()");
            self.probe.lock().unwrap().clone()
        }
        async fn cancel(&self) -> AppResult<()> {
            Ok(())
        }
        async fn tx_status(&self) -> TxStatus {
            TxStatus::Unknown
        }
        fn is_closed(&self) -> bool {
            self.closed.load(Ordering::SeqCst)
        }
        async fn set_search_path(&self, _: Option<&str>) -> AppResult<()> {
            Ok(())
        }
        async fn close(&self) {
            self.closed.store(true, Ordering::SeqCst);
        }
    }

    async fn open_fake(opens: &AtomicUsize, fake: Arc<TxFake>) -> AppResult<Arc<dyn QuerySession>> {
        opens.fetch_add(1, Ordering::SeqCst);
        Ok(fake as Arc<dyn QuerySession>)
    }

    const REFUSED: &str = "Commit or roll back this tab's transaction before switching databases.";

    #[tokio::test]
    async fn same_database_reuses_session() {
        let reg = QuerySessions::default();
        let opens = AtomicUsize::new(0);
        let a = reg.get_or_open("w", "t", None, open_dummy(&opens)).await.unwrap();
        let b = reg.get_or_open("w", "t", None, open_dummy(&opens)).await.unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        let c = reg.get_or_open("w", "t", Some("other"), open_dummy(&opens)).await.unwrap();
        let d = reg.get_or_open("w", "t", Some("other"), open_dummy(&opens)).await.unwrap();
        assert!(Arc::ptr_eq(&c, &d));
        assert_eq!(opens.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn different_database_closes_and_reopens_when_idle() {
        let reg = QuerySessions::default();
        let opens = AtomicUsize::new(0);
        let idle = TxFake::answering(probe_rows("f"));
        let a = reg.get_or_open("w", "t", None, open_fake(&opens, idle.clone())).await.unwrap();
        let b = reg.get_or_open("w", "t", Some("other"), open_dummy(&opens)).await.unwrap();
        assert!(!Arc::ptr_eq(&a, &b));
        assert!(idle.is_closed());
        assert_eq!(opens.load(Ordering::SeqCst), 2);
        // And back to the profile's database.
        let c = reg.get_or_open("w", "t", None, open_dummy(&opens)).await.unwrap();
        assert!(!Arc::ptr_eq(&b, &c));
        assert!(b.session.is_closed());
        assert_eq!(opens.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn different_database_refused_when_tx_open() {
        for answer in [probe_rows("t"), RawOutcome::Error {
            message: "current transaction is aborted".into(),
            code: Some("25P02".into()),
            position: None,
        }] {
            let reg = QuerySessions::default();
            let opens = AtomicUsize::new(0);
            let in_tx = TxFake::answering(answer);
            let a = reg.get_or_open("w", "t", None, open_fake(&opens, in_tx.clone())).await.unwrap();
            let err = reg.get_or_open("w", "t", Some("other"), open_dummy(&opens)).await.err();
            assert_eq!(err, Some(AppError::Query(REFUSED.into())));
            assert!(!in_tx.is_closed());
            assert_eq!(opens.load(Ordering::SeqCst), 1);
            assert!(Arc::ptr_eq(&reg.get("w", "t").await.unwrap(), &a));
            // Once the transaction is over, the switch goes through.
            *in_tx.probe.lock().unwrap() = probe_rows("f");
            reg.get_or_open("w", "t", Some("other"), open_dummy(&opens)).await.unwrap();
            assert!(in_tx.is_closed());
        }
    }

    #[tokio::test]
    async fn different_database_keeps_session_when_probe_fails() {
        let reg = QuerySessions::default();
        let opens = AtomicUsize::new(0);
        let broken = TxFake::answering(RawOutcome::Error { message: "boom".into(), code: Some("XX000".into()), position: None });
        reg.get_or_open("w", "t", None, open_fake(&opens, broken.clone())).await.unwrap();
        let err = reg.get_or_open("w", "t", Some("other"), open_dummy(&opens)).await.err();
        assert_eq!(err, Some(AppError::Query("boom".into())));
        assert!(!broken.is_closed());
        assert_eq!(opens.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn different_database_refused_while_a_query_runs() {
        let reg = QuerySessions::default();
        let opens = AtomicUsize::new(0);
        let idle = TxFake::answering(probe_rows("f"));
        let a = reg.get_or_open("w", "t", None, open_fake(&opens, idle.clone())).await.unwrap();
        let running = a.run_lock.try_lock().unwrap();
        let err = reg.get_or_open("w", "t", Some("other"), open_dummy(&opens)).await.err();
        assert_eq!(err, Some(AppError::Query(QUERY_RUNNING.into())));
        assert!(!idle.is_closed());
        drop(running);
        reg.get_or_open("w", "t", Some("other"), open_dummy(&opens)).await.unwrap();
        assert!(idle.is_closed());
    }

    /// A session whose transaction probe never answers (a stuck server or network).
    #[derive(Default)]
    struct SilentProbe {
        closed: AtomicBool,
    }

    #[async_trait]
    impl QuerySession for SilentProbe {
        async fn run(&self, _: &str, _: &[Option<String>], _: usize) -> StatementRun {
            StatementRun { outcome: RawOutcome::Command { rows_affected: 0 }, notices: vec![], elapsed_ms: 0 }
        }
        async fn run_simple(&self, _: &str) -> RawOutcome {
            std::future::pending().await
        }
        async fn cancel(&self) -> AppResult<()> {
            Ok(())
        }
        async fn tx_status(&self) -> TxStatus {
            TxStatus::Unknown
        }
        fn is_closed(&self) -> bool {
            self.closed.load(Ordering::SeqCst)
        }
        fn probe_timeout(&self) -> Duration {
            Duration::from_millis(50)
        }
        async fn set_search_path(&self, _: Option<&str>) -> AppResult<()> {
            Ok(())
        }
        async fn close(&self) {
            self.closed.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn different_database_refused_when_the_probe_times_out() {
        let reg = QuerySessions::default();
        let opens = AtomicUsize::new(0);
        let silent = Arc::new(SilentProbe::default());
        let s = silent.clone();
        reg.get_or_open("w", "t", None, async move {
            opens.fetch_add(1, Ordering::SeqCst);
            Ok(s as Arc<dyn QuerySession>)
        })
        .await
        .unwrap();
        let reopened = AtomicUsize::new(0);
        let err = timeout(Duration::from_secs(5), reg.get_or_open("w", "t", Some("other"), open_dummy(&reopened)))
            .await
            .expect("the probe is bounded")
            .err();
        assert!(matches!(err, Some(AppError::Query(ref m)) if m.contains("transaction")), "{err:?}");
        assert!(!silent.is_closed());
        assert_eq!(reopened.load(Ordering::SeqCst), 0);
        // The session is kept for the tab's own database.
        assert!(reg.get("w", "t").await.is_some());
    }
}
