use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::driver::Driver;
use crate::error::{AppError, AppResult};
use crate::model::ConnectionProfile;

type Session = (ConnectionProfile, Arc<dyn Driver>);

/// Connect order across all windows: a smaller number connected earlier.
static NEXT_SEQ: AtomicU64 = AtomicU64::new(0);

struct Entry {
    profile: ConnectionProfile,
    driver: Arc<dyn Driver>,
    seq: u64,
    /// False once the window has left the workspace: it no longer saves or restores tabs,
    /// and does not keep another window on the same connection from doing so.
    owns_tab_state: bool,
}

/// Active connection per window label. The lock is a plain mutex, never held across an
/// await, so a window's entry can be taken synchronously when the window is destroyed.
#[derive(Default)]
pub struct Sessions {
    inner: Mutex<HashMap<String, Entry>>,
}

impl Sessions {
    fn map(&self) -> MutexGuard<'_, HashMap<String, Entry>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Stores the session; the one it replaces is closed in the background. The window joins
    /// the end of the connect order, even when it reconnects to the same connection.
    pub async fn set(&self, label: &str, profile: ConnectionProfile, driver: Arc<dyn Driver>) {
        self.insert(label, profile, driver);
    }

    /// Inserts and returns the new entry's seq. The replaced driver is closed in a spawned
    /// task, never awaited here: closing a pool waits for its connections to come back, and
    /// connecting must not wait on, say, a long export still holding one.
    fn insert(&self, label: &str, profile: ConnectionProfile, driver: Arc<dyn Driver>) -> u64 {
        let seq = NEXT_SEQ.fetch_add(1, Ordering::SeqCst);
        let previous = self.map().insert(label.to_string(), Entry { profile, driver, seq, owns_tab_state: true });
        if let Some(old) = previous {
            tokio::spawn(async move { old.driver.close().await });
        }
        seq
    }

    /// Like `set`, for a window that may have been destroyed while it was connecting.
    /// `alive` is checked after the insert: if the window is gone, the entry is taken
    /// back out and its driver closed, so no dead window keeps a pool or the tab-state
    /// ownership of its connection. Returns whether the session was kept.
    ///
    /// A destroyed window is no longer alive by the time its `Destroyed` handler runs,
    /// so either that handler's `take` or this check removes the entry, whichever is later.
    pub async fn set_if_alive(
        &self,
        label: &str,
        profile: ConnectionProfile,
        driver: Arc<dyn Driver>,
        alive: impl FnOnce() -> bool,
    ) -> bool {
        let seq = self.insert(label, profile, driver);
        if alive() {
            return true;
        }
        let ghost = {
            let mut map = self.map();
            // Only our own entry: a later `set` for this label may have replaced it.
            match map.get(label) {
                Some(e) if e.seq == seq => map.remove(label),
                _ => None,
            }
        };
        if let Some(ghost) = ghost {
            ghost.driver.close().await;
        }
        false
    }

    pub async fn get(&self, label: &str) -> AppResult<Session> {
        self.map()
            .get(label)
            .map(|e| (e.profile.clone(), e.driver.clone()))
            .ok_or_else(|| AppError::NotFound("No active connection".into()))
    }

    /// Takes the window's session out of the map without waiting; the caller closes the
    /// returned driver. Used when a window is destroyed, before anything else can reuse
    /// its label.
    pub fn take(&self, label: &str) -> Option<Arc<dyn Driver>> {
        self.map().remove(label).map(|e| e.driver)
    }

    pub async fn remove(&self, label: &str) {
        if let Some(driver) = self.take(label) {
            driver.close().await;
        }
    }

    /// Whether this window restores and saves tab state for its connection: true when no
    /// other window has been on the same connection longer. False without a session.
    pub async fn is_tab_state_owner(&self, label: &str) -> bool {
        matches!(self.tab_state_connection(label).await, Ok(Some(_)))
    }

    /// The connection id whose tab state this window owns, or `None` when another window
    /// on the same connection connected first, or this one has released it. `NotFound`
    /// without a session.
    pub async fn tab_state_connection(&self, label: &str) -> AppResult<Option<String>> {
        let map = self.map();
        let mine = map.get(label).ok_or_else(|| AppError::NotFound("No active connection".into()))?;
        if !mine.owns_tab_state {
            return Ok(None);
        }
        let id = &mine.profile.id;
        let earlier = map.values().any(|e| e.owns_tab_state && e.profile.id == *id && e.seq < mine.seq);
        Ok((!earlier).then(|| id.clone()))
    }

    /// The window left the workspace for the Connections screen (after its last tab-state
    /// save): it stops owning its connection's tab state, so the next window on that
    /// connection saves. The session itself stays until the window connects again.
    pub fn release_tab_state(&self, label: &str) {
        if let Some(entry) = self.map().get_mut(label) {
            entry.owns_tab_state = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use async_trait::async_trait;

    use super::*;
    use crate::driver::{Dialect, PostgresDialect};
    use crate::error::AppError;
    use crate::model::*;

    struct FakeDriver {
        closes: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Driver for FakeDriver {
        fn kind(&self) -> DbKind {
            DbKind::Postgres
        }
        fn dialect(&self) -> &dyn Dialect {
            &PostgresDialect
        }
        fn query_language(&self) -> crate::query::QueryLanguage {
            crate::query::QueryLanguage::Sql(crate::sqltext::SqlDialect::Postgres)
        }
        async fn server_version(&self) -> AppResult<String> {
            Ok("fake".into())
        }
        async fn list_databases(&self) -> AppResult<Vec<String>> {
            Ok(vec![])
        }
        async fn list_schemas(&self, _: Option<&str>) -> AppResult<Vec<String>> {
            Ok(vec![])
        }
        async fn schema_tree(&self, _: &str) -> AppResult<SchemaTree> {
            unimplemented!()
        }
        async fn table_structure(&self, _: &TableRef) -> AppResult<TableStructure> {
            unimplemented!()
        }
        async fn fetch_page(&self, _: &PageRequest) -> AppResult<TablePage> {
            unimplemented!()
        }
        async fn export_table(&self, _: &TableQuery, _: &mut dyn crate::export::RowSink) -> AppResult<()> {
            unimplemented!()
        }
        async fn apply_changes(&self, _: &ChangeSet) -> AppResult<ApplyResult> {
            unimplemented!()
        }
        async fn schema_columns(
            &self,
            _: Option<&str>,
            _: &str,
        ) -> AppResult<std::collections::BTreeMap<String, Vec<String>>> {
            unimplemented!()
        }
        async fn open_query_session(&self, _: Option<&str>) -> AppResult<Arc<dyn crate::query::QuerySession>> {
            unimplemented!()
        }
        async fn close(&self) {
            self.closes.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn profile(id: &str) -> ConnectionProfile {
        ConnectionProfile {
            id: id.into(),
            name: id.into(),
            kind: DbKind::Postgres,
            default_schema: None,
            environment: Environment::Local,
            connect_timeout_secs: 10,
            read_only: false,
            startup_sql: None,
            statement_timeout_secs: None,
            transport: Transport::Network(NetworkTarget {
                host: "h".into(),
                port: 5432,
                user: "u".into(),
                database: "d".into(),
                ssl_mode: SslMode::Prefer,
                ssl_root_cert: None,
                auth_source: None,
            }),
        }
    }

    /// Lets the spawned close of a replaced driver run (the test runtime is single-threaded).
    async fn closed_in_background() {
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }

    fn fake() -> (Arc<dyn Driver>, Arc<AtomicUsize>) {
        let closes = Arc::new(AtomicUsize::new(0));
        (Arc::new(FakeDriver { closes: closes.clone() }), closes)
    }

    #[tokio::test]
    async fn get_without_connect_is_not_found() {
        let s = Sessions::default();
        match s.get("main").await {
            Err(AppError::NotFound(m)) => assert_eq!(m, "No active connection"),
            _ => panic!("expected NotFound"),
        }
    }

    #[tokio::test]
    async fn set_replaces_and_closes_previous() {
        let s = Sessions::default();
        let (d1, c1) = fake();
        let (d2, c2) = fake();
        s.set("main", profile("a"), d1).await;
        s.set("main", profile("b"), d2).await;
        closed_in_background().await;
        assert_eq!(c1.load(Ordering::SeqCst), 1);
        assert_eq!(c2.load(Ordering::SeqCst), 0);
        assert_eq!(s.get("main").await.unwrap().0.id, "b");
    }

    /// A driver whose close never finishes, like a pool waiting on a long export.
    struct StuckDriver;

    #[async_trait]
    impl Driver for StuckDriver {
        fn kind(&self) -> DbKind {
            DbKind::Postgres
        }
        fn dialect(&self) -> &dyn Dialect {
            &PostgresDialect
        }
        fn query_language(&self) -> crate::query::QueryLanguage {
            crate::query::QueryLanguage::Sql(crate::sqltext::SqlDialect::Postgres)
        }
        async fn server_version(&self) -> AppResult<String> {
            Ok("stuck".into())
        }
        async fn list_databases(&self) -> AppResult<Vec<String>> {
            Ok(vec![])
        }
        async fn list_schemas(&self, _: Option<&str>) -> AppResult<Vec<String>> {
            Ok(vec![])
        }
        async fn schema_tree(&self, _: &str) -> AppResult<SchemaTree> {
            unimplemented!()
        }
        async fn table_structure(&self, _: &TableRef) -> AppResult<TableStructure> {
            unimplemented!()
        }
        async fn fetch_page(&self, _: &PageRequest) -> AppResult<TablePage> {
            unimplemented!()
        }
        async fn export_table(&self, _: &TableQuery, _: &mut dyn crate::export::RowSink) -> AppResult<()> {
            unimplemented!()
        }
        async fn apply_changes(&self, _: &ChangeSet) -> AppResult<ApplyResult> {
            unimplemented!()
        }
        async fn schema_columns(
            &self,
            _: Option<&str>,
            _: &str,
        ) -> AppResult<std::collections::BTreeMap<String, Vec<String>>> {
            unimplemented!()
        }
        async fn open_query_session(&self, _: Option<&str>) -> AppResult<Arc<dyn crate::query::QuerySession>> {
            unimplemented!()
        }
        async fn close(&self) {
            std::future::pending::<()>().await;
        }
    }

    #[tokio::test]
    async fn replacing_a_session_does_not_wait_for_the_old_driver_to_close() {
        let s = Sessions::default();
        s.set("main", profile("a"), Arc::new(StuckDriver)).await;
        let (d, _) = fake();
        tokio::time::timeout(std::time::Duration::from_secs(1), s.set("main", profile("b"), d))
            .await
            .expect("set must not wait for the replaced driver's close");
        assert_eq!(s.get("main").await.unwrap().0.id, "b");
        let (d, _) = fake();
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            s.set_if_alive("w2", profile("c"), d, || true),
        )
        .await
        .expect("set_if_alive does not wait either");
    }

    #[tokio::test]
    async fn remove_closes() {
        let s = Sessions::default();
        let (d, c) = fake();
        s.set("main", profile("a"), d).await;
        s.remove("main").await;
        assert_eq!(c.load(Ordering::SeqCst), 1);
        assert!(s.get("main").await.is_err());
    }

    #[tokio::test]
    async fn windows_are_isolated() {
        let s = Sessions::default();
        let (d1, c1) = fake();
        let (d2, _c2) = fake();
        s.set("main", profile("a"), d1).await;
        s.set("w2", profile("b"), d2).await;
        assert_eq!(s.get("main").await.unwrap().0.id, "a");
        assert_eq!(s.get("w2").await.unwrap().0.id, "b");
        s.remove("w2").await;
        assert_eq!(c1.load(Ordering::SeqCst), 0);
        assert!(s.get("main").await.is_ok());
    }

    #[tokio::test]
    async fn tab_state_owner_is_the_first_window_on_a_connection() {
        let s = Sessions::default();
        s.set("w1", profile("p"), fake().0).await;
        s.set("w2", profile("p"), fake().0).await;
        s.set("w3", profile("q"), fake().0).await;
        assert!(s.is_tab_state_owner("w1").await);
        assert!(!s.is_tab_state_owner("w2").await);
        assert!(s.is_tab_state_owner("w3").await);
        assert!(!s.is_tab_state_owner("nobody").await);

        s.remove("w1").await;
        assert!(s.is_tab_state_owner("w2").await);
        assert!(s.is_tab_state_owner("w3").await);
    }

    #[tokio::test]
    async fn tab_state_owner_moves_on_after_a_reconnect() {
        let s = Sessions::default();
        s.set("w1", profile("p"), fake().0).await;
        s.set("w2", profile("p"), fake().0).await;
        // w1 reconnects: w2 has been on `p` longer now.
        s.set("w1", profile("p"), fake().0).await;
        assert!(s.is_tab_state_owner("w2").await);
        assert!(!s.is_tab_state_owner("w1").await);
        assert_eq!(s.tab_state_connection("w1").await.unwrap(), None);
        assert_eq!(s.tab_state_connection("w2").await.unwrap(), Some("p".to_string()));
        assert!(s.tab_state_connection("nobody").await.is_err());
    }

    #[tokio::test]
    async fn after_release_the_other_window_owns_the_tab_state() {
        let s = Sessions::default();
        s.set("w1", profile("p"), fake().0).await;
        s.set("w2", profile("p"), fake().0).await;
        assert!(s.is_tab_state_owner("w1").await);
        assert!(!s.is_tab_state_owner("w2").await);
        // w1 leaves for the Connections screen: its session stays, its ownership doesn't.
        s.release_tab_state("w1");
        assert!(!s.is_tab_state_owner("w1").await);
        assert!(s.is_tab_state_owner("w2").await);
        assert!(s.get("w1").await.is_ok());
        // w1 connects again: w2 has been on `p` longer, so it keeps owning.
        s.set("w1", profile("p"), fake().0).await;
        assert!(s.is_tab_state_owner("w2").await);
        assert!(!s.is_tab_state_owner("w1").await);
        // A window alone on its connection owns it again after reconnecting.
        s.set("w3", profile("q"), fake().0).await;
        s.release_tab_state("w3");
        s.set("w3", profile("q"), fake().0).await;
        assert!(s.is_tab_state_owner("w3").await);
        // Nothing to release without a session.
        s.release_tab_state("nobody");
    }

    #[tokio::test]
    async fn destroy_closes_only_that_window() {
        let s = Sessions::default();
        let (d1, c1) = fake();
        let (d2, c2) = fake();
        s.set("w1", profile("p"), d1).await;
        s.set("w2", profile("p"), d2).await;
        s.remove("w1").await;
        assert_eq!(c1.load(Ordering::SeqCst), 1);
        assert_eq!(c2.load(Ordering::SeqCst), 0);
        assert_eq!(s.get("w2").await.unwrap().0.id, "p");
        assert!(s.get("w1").await.is_err());
    }

    #[tokio::test]
    async fn set_if_alive_drops_a_session_for_a_closed_window() {
        let s = Sessions::default();
        let (d, c) = fake();
        assert!(!s.set_if_alive("w1", profile("p"), d, || false).await);
        assert_eq!(c.load(Ordering::SeqCst), 1);
        assert!(s.get("w1").await.is_err());
        // The ghost doesn't own `p`: the next window on it does.
        s.set("w2", profile("p"), fake().0).await;
        assert!(s.is_tab_state_owner("w2").await);
    }

    #[tokio::test]
    async fn set_if_alive_keeps_a_session_for_a_live_window() {
        let s = Sessions::default();
        let (d, c) = fake();
        assert!(s.set_if_alive("w1", profile("p"), d, || true).await);
        assert_eq!(c.load(Ordering::SeqCst), 0);
        assert!(s.is_tab_state_owner("w1").await);
    }

    #[tokio::test]
    async fn set_if_alive_leaves_a_newer_session_under_the_same_label() {
        let s = Sessions::default();
        let (old, old_closes) = fake();
        let (new, new_closes) = fake();
        let alive = || {
            // While the dead window's connect is checking, a new window reuses the
            // label and connects. Simulated by a set from inside the check.
            futures_util::FutureExt::now_or_never(s.set("w2", profile("q"), new.clone()))
                .expect("set does not wait");
            false
        };
        assert!(!s.set_if_alive("w2", profile("p"), old, alive).await);
        closed_in_background().await;
        assert_eq!(old_closes.load(Ordering::SeqCst), 1);
        assert_eq!(new_closes.load(Ordering::SeqCst), 0);
        assert_eq!(s.get("w2").await.unwrap().0.id, "q");
    }

    #[tokio::test]
    async fn take_then_new_session_then_close_old_keeps_the_new_one() {
        let s = Sessions::default();
        let (old, old_closes) = fake();
        s.set("w2", profile("p"), old).await;
        // Destroyed: taken at once, closed later.
        let taken = s.take("w2").expect("an entry");
        // A new window reuses the label and connects before the old driver is closed.
        let (new, new_closes) = fake();
        s.set("w2", profile("q"), new).await;
        taken.close().await;
        assert_eq!(old_closes.load(Ordering::SeqCst), 1);
        assert_eq!(new_closes.load(Ordering::SeqCst), 0);
        assert_eq!(s.get("w2").await.unwrap().0.id, "q");
        assert!(s.take("nobody").is_none());
    }
}
