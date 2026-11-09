use std::path::Path;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use crate::error::{AppError, AppResult};
use crate::model::{ConnectionProfile, HistoryEntry, NewHistoryEntry, ParamValues, SavedQuery};

#[derive(Clone)]
pub struct AppDb {
    pool: SqlitePool,
}

fn storage<E: std::fmt::Display>(e: E) -> AppError {
    AppError::Storage(e.to_string())
}

impl AppDb {
    pub async fn open(path: &Path) -> AppResult<AppDb> {
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(opts)
            .await
            .map_err(storage)?;
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(storage)?;
        Ok(AppDb { pool })
    }

    /// Lists saved profiles. A row that cannot be read is skipped (and logged),
    /// so one bad row does not hide the others.
    pub async fn list_connections(&self) -> AppResult<Vec<ConnectionProfile>> {
        let rows = sqlx::query("SELECT id, profile_json FROM connections ORDER BY name COLLATE NOCASE, id")
            .fetch_all(&self.pool)
            .await
            .map_err(storage)?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            let id: String = r.try_get(0).map_err(storage)?;
            let json: String = r.try_get(1).map_err(storage)?;
            match serde_json::from_str(&json) {
                Ok(p) => out.push(p),
                Err(e) => eprintln!("skipping unreadable connection {id}: {e}"),
            }
        }
        Ok(out)
    }

    pub async fn get_connection(&self, id: &str) -> AppResult<ConnectionProfile> {
        let row = sqlx::query("SELECT profile_json FROM connections WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(storage)?
            .ok_or_else(|| AppError::NotFound(format!("connection {id}")))?;
        serde_json::from_str(&row.try_get::<String, _>(0).map_err(storage)?).map_err(storage)
    }

    pub async fn upsert_connection(&self, mut profile: ConnectionProfile) -> AppResult<ConnectionProfile> {
        if profile.id.is_empty() {
            profile.id = uuid::Uuid::new_v4().to_string();
        }
        let json = serde_json::to_string(&profile).map_err(storage)?;
        sqlx::query(
            "INSERT INTO connections (id, name, profile_json, updated_at) \
             VALUES (?, ?, ?, strftime('%Y-%m-%dT%H:%M:%fZ','now')) \
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, \
             profile_json = excluded.profile_json, updated_at = excluded.updated_at",
        )
        .bind(&profile.id)
        .bind(&profile.name)
        .bind(json)
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        Ok(profile)
    }

    /// Deletes the profile and everything stored for it, in one transaction.
    pub async fn delete_connection(&self, id: &str) -> AppResult<()> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        for sql in [
            "DELETE FROM saved_queries WHERE connection_id = ?",
            "DELETE FROM query_history WHERE connection_id = ?",
            "DELETE FROM tab_state WHERE connection_id = ?",
            "DELETE FROM connections WHERE id = ?",
        ] {
            sqlx::query(sql).bind(id).execute(&mut *tx).await.map_err(storage)?;
        }
        tx.commit().await.map_err(storage)
    }

    pub async fn list_saved_queries(&self, connection_id: &str) -> AppResult<Vec<SavedQuery>> {
        let rows = sqlx::query(
            "SELECT id, connection_id, name, sql, last_params_json, updated_at FROM saved_queries \
             WHERE connection_id = ? ORDER BY name COLLATE NOCASE, id",
        )
        .bind(connection_id)
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        rows.iter().map(saved_query_from_row).collect()
    }

    /// An empty id gets a new UUID. An existing row keeps its `last_params`.
    pub async fn upsert_saved_query(&self, mut q: SavedQuery) -> AppResult<SavedQuery> {
        if q.id.is_empty() {
            q.id = uuid::Uuid::new_v4().to_string();
        }
        let params = serde_json::to_string(&q.last_params).map_err(storage)?;
        let row = sqlx::query(
            "INSERT INTO saved_queries (id, connection_id, name, sql, last_params_json, updated_at) \
             VALUES (?, ?, ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%fZ','now')) \
             ON CONFLICT(id) DO UPDATE SET connection_id = excluded.connection_id, name = excluded.name, \
             sql = excluded.sql, updated_at = excluded.updated_at \
             RETURNING id, connection_id, name, sql, last_params_json, updated_at",
        )
        .bind(&q.id)
        .bind(&q.connection_id)
        .bind(&q.name)
        .bind(&q.sql)
        .bind(params)
        .fetch_one(&self.pool)
        .await
        .map_err(storage)?;
        saved_query_from_row(&row)
    }

    pub async fn delete_saved_query(&self, id: &str) -> AppResult<()> {
        sqlx::query("DELETE FROM saved_queries WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(storage)?;
        Ok(())
    }

    pub async fn set_saved_query_params(&self, id: &str, params: &ParamValues) -> AppResult<()> {
        let json = serde_json::to_string(params).map_err(storage)?;
        sqlx::query("UPDATE saved_queries SET last_params_json = ? WHERE id = ?")
            .bind(json)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(storage)?;
        Ok(())
    }

    /// Inserts the entries, then keeps only the newest 500 rows of this connection.
    pub async fn add_history(&self, connection_id: &str, entries: &[NewHistoryEntry]) -> AppResult<()> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        for e in entries {
            sqlx::query(
                "INSERT INTO query_history (connection_id, sql, params_json, verb, ok, summary, started_at, elapsed_ms, target_database, target_schema) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(connection_id)
            .bind(&e.sql)
            .bind(serde_json::to_string(&e.params).map_err(storage)?)
            .bind(&e.verb)
            .bind(e.ok)
            .bind(&e.summary)
            .bind(e.started_at)
            .bind(i64::try_from(e.elapsed_ms).unwrap_or(i64::MAX))
            .bind(&e.target_database)
            .bind(&e.target_schema)
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        }
        sqlx::query(
            "DELETE FROM query_history WHERE connection_id = ?1 AND id NOT IN \
             (SELECT id FROM query_history WHERE connection_id = ?1 ORDER BY id DESC LIMIT 500)",
        )
        .bind(connection_id)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        tx.commit().await.map_err(storage)
    }

    pub async fn list_history(&self, connection_id: &str, limit: u32) -> AppResult<Vec<HistoryEntry>> {
        let rows = sqlx::query(
            "SELECT id, sql, params_json, verb, ok, summary, started_at, elapsed_ms, target_database, target_schema FROM query_history \
             WHERE connection_id = ? ORDER BY id DESC LIMIT ?",
        )
        .bind(connection_id)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        rows.iter()
            .map(|r| {
                let params: String = r.try_get(2).map_err(storage)?;
                let elapsed: i64 = r.try_get(7).map_err(storage)?;
                Ok(HistoryEntry {
                    id: r.try_get(0).map_err(storage)?,
                    entry: NewHistoryEntry {
                        sql: r.try_get(1).map_err(storage)?,
                        params: serde_json::from_str(&params).map_err(storage)?,
                        verb: r.try_get(3).map_err(storage)?,
                        ok: r.try_get(4).map_err(storage)?,
                        summary: r.try_get(5).map_err(storage)?,
                        started_at: r.try_get(6).map_err(storage)?,
                        elapsed_ms: u64::try_from(elapsed).unwrap_or(0),
                        target_database: r.try_get(8).map_err(storage)?,
                        target_schema: r.try_get(9).map_err(storage)?,
                    },
                })
            })
            .collect()
    }

    pub async fn load_tab_state(&self, connection_id: &str) -> AppResult<Option<String>> {
        let row = sqlx::query("SELECT json FROM tab_state WHERE connection_id = ?")
            .bind(connection_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(storage)?;
        row.map(|r| r.try_get(0).map_err(storage)).transpose()
    }

    pub async fn save_tab_state(&self, connection_id: &str, json: &str) -> AppResult<()> {
        sqlx::query(
            "INSERT INTO tab_state (connection_id, json, updated_at) \
             VALUES (?, ?, strftime('%Y-%m-%dT%H:%M:%fZ','now')) \
             ON CONFLICT(connection_id) DO UPDATE SET json = excluded.json, updated_at = excluded.updated_at",
        )
        .bind(connection_id)
        .bind(json)
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        Ok(())
    }
}

fn saved_query_from_row(r: &sqlx::sqlite::SqliteRow) -> AppResult<SavedQuery> {
    let params: String = r.try_get(4).map_err(storage)?;
    Ok(SavedQuery {
        id: r.try_get(0).map_err(storage)?,
        connection_id: r.try_get(1).map_err(storage)?,
        name: r.try_get(2).map_err(storage)?,
        sql: r.try_get(3).map_err(storage)?,
        last_params: serde_json::from_str(&params).map_err(storage)?,
        updated_at: r.try_get(5).map_err(storage)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AppError;
    use crate::model::*;

    fn profile(name: &str) -> ConnectionProfile {
        ConnectionProfile {
            id: String::new(),
            name: name.into(),
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
        }
    }

    async fn db() -> (tempfile::TempDir, AppDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = AppDb::open(&dir.path().join("app.db")).await.unwrap();
        (dir, db)
    }

    #[tokio::test]
    async fn upsert_assigns_id_and_roundtrips() {
        let (_d, db) = db().await;
        let saved = db.upsert_connection(profile("x")).await.unwrap();
        assert_eq!(saved.id.len(), 36);
        assert_eq!(db.get_connection(&saved.id).await.unwrap(), saved);
    }

    #[tokio::test]
    async fn list_is_sorted_by_name_case_insensitive() {
        let (_d, db) = db().await;
        for n in ["b-db", "A-db", "c-db"] {
            db.upsert_connection(profile(n)).await.unwrap();
        }
        let names: Vec<String> = db.list_connections().await.unwrap().into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["A-db", "b-db", "c-db"]);
    }

    #[tokio::test]
    async fn upsert_existing_updates_in_place() {
        let (_d, db) = db().await;
        let mut p = db.upsert_connection(profile("old")).await.unwrap();
        p.name = "new".into();
        db.upsert_connection(p).await.unwrap();
        let list = db.list_connections().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "new");
    }

    #[tokio::test]
    async fn delete_then_get_is_not_found() {
        let (_d, db) = db().await;
        let p = db.upsert_connection(profile("x")).await.unwrap();
        db.delete_connection(&p.id).await.unwrap();
        let err = db.get_connection(&p.id).await.unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)));
    }

    #[tokio::test]
    async fn list_skips_unreadable_rows() {
        let (_d, db) = db().await;
        let good = db.upsert_connection(profile("good")).await.unwrap();
        sqlx::query("INSERT INTO connections (id, name, profile_json, updated_at) VALUES ('bad', 'bad', '{not json', '')")
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO connections (id, name, profile_json, updated_at) VALUES ('old', 'old', ?, '')")
            .bind(r#"{"id":"old","name":"old","kind":"postgres","host":"h","port":5432,"user":"u","database":"d","environment":"local","sslMode":"prefer","connectTimeoutSecs":10}"#)
            .execute(&db.pool)
            .await
            .unwrap();
        let list = db.list_connections().await.unwrap();
        let ids: Vec<&str> = list.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, [good.id.as_str(), "old"]);
        let old = &list[1];
        assert_eq!(old.default_schema, None);
        assert!(!old.read_only);
        assert_eq!(old.startup_sql, None);
    }

    #[tokio::test]
    async fn list_connections_reads_a_profile_saved_by_an_older_version() {
        let (_d, db) = db().await;
        sqlx::query("INSERT INTO connections (id, name, profile_json, updated_at) VALUES ('c1', 'Local', ?, '')")
            .bind(r#"{"id":"c1","name":"Local","kind":"postgres","host":"localhost","port":5432,"user":"app","database":"app_dev","defaultSchema":null,"environment":"local","sslMode":"prefer","sslRootCert":null,"connectTimeoutSecs":10,"readOnly":false,"startupSql":null,"filePath":null}"#)
            .execute(&db.pool)
            .await
            .unwrap();
        let list = db.list_connections().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(
            list[0].transport,
            Transport::Network(NetworkTarget {
                host: "localhost".into(),
                port: 5432,
                user: "app".into(),
                database: "app_dev".into(),
                ssl_mode: SslMode::Prefer,
                ssl_root_cert: None,
                auth_source: None,
            })
        );
    }

    fn sq(conn: &str, name: &str) -> SavedQuery {
        SavedQuery {
            id: String::new(),
            connection_id: conn.into(),
            name: name.into(),
            sql: "select 1".into(),
            last_params: ParamValues::new(),
            updated_at: String::new(),
        }
    }

    fn hist(n: i64) -> NewHistoryEntry {
        NewHistoryEntry {
            sql: format!("select {n}"),
            params: ParamValues::new(),
            verb: "SELECT".into(),
            ok: true,
            summary: "1 row".into(),
            started_at: n,
            elapsed_ms: 3,
            target_database: None,
            target_schema: None,
        }
    }

    #[tokio::test]
    async fn saved_query_crud() {
        let (_d, db) = db().await;
        let q = db.upsert_saved_query(sq("c1", "first")).await.unwrap();
        assert_eq!(q.id.len(), 36);
        assert!(q.updated_at.ends_with('Z'));
        assert_eq!(db.list_saved_queries("c1").await.unwrap(), vec![q.clone()]);
        let mut renamed = q.clone();
        renamed.name = "second".into();
        let r = db.upsert_saved_query(renamed).await.unwrap();
        assert_eq!(r.id, q.id);
        let list = db.list_saved_queries("c1").await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "second");
        db.delete_saved_query(&q.id).await.unwrap();
        assert!(db.list_saved_queries("c1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn saved_query_params_roundtrip() {
        let (_d, db) = db().await;
        let q = db.upsert_saved_query(sq("c1", "p")).await.unwrap();
        let mut params = ParamValues::new();
        params.insert("since".into(), Some("2026-10-01".into()));
        params.insert("lim".into(), None);
        db.set_saved_query_params(&q.id, &params).await.unwrap();
        assert_eq!(db.list_saved_queries("c1").await.unwrap()[0].last_params, params);
        let mut renamed = q.clone();
        renamed.name = "p2".into();
        let r = db.upsert_saved_query(renamed).await.unwrap();
        assert_eq!(r.last_params, params);
        let list = db.list_saved_queries("c1").await.unwrap();
        assert_eq!(list[0].name, "p2");
        assert_eq!(list[0].last_params, params);
    }

    #[tokio::test]
    async fn saved_queries_are_per_connection() {
        let (_d, db) = db().await;
        db.upsert_saved_query(sq("c1", "a")).await.unwrap();
        db.upsert_saved_query(sq("c2", "b")).await.unwrap();
        let a = db.list_saved_queries("c1").await.unwrap();
        let b = db.list_saved_queries("c2").await.unwrap();
        assert_eq!((a.len(), a[0].name.as_str()), (1, "a"));
        assert_eq!((b.len(), b[0].name.as_str()), (1, "b"));
    }

    #[tokio::test]
    async fn history_is_newest_first_and_capped_at_500() {
        let (_d, db) = db().await;
        for chunk in (1..=505).collect::<Vec<i64>>().chunks(100) {
            let entries: Vec<_> = chunk.iter().map(|n| hist(*n)).collect();
            db.add_history("c1", &entries).await.unwrap();
        }
        db.add_history("c2", &[hist(1)]).await.unwrap();
        let list = db.list_history("c1", 1000).await.unwrap();
        assert_eq!(list.len(), 500);
        assert_eq!(list[0].entry.started_at, 505);
        assert_eq!(list[499].entry.started_at, 6);
        assert_eq!(db.list_history("c1", 10).await.unwrap().len(), 10);
        assert_eq!(db.list_history("c2", 10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn history_keeps_the_target() {
        let (_d, db) = db().await;
        let mut with = hist(1);
        with.target_database = Some("fx_other".into());
        with.target_schema = Some("sales".into());
        db.add_history("c1", &[with.clone(), hist(2)]).await.unwrap();
        // A row from before the columns existed has NULL targets.
        sqlx::query("INSERT INTO query_history (connection_id, sql, params_json, verb, ok, summary, started_at, elapsed_ms) VALUES ('c1', 's', '{}', 'SELECT', 1, 'x', 3, 1)")
            .execute(&db.pool)
            .await
            .unwrap();
        let list = db.list_history("c1", 10).await.unwrap();
        assert_eq!((list[0].entry.target_database.clone(), list[0].entry.target_schema.clone()), (None, None));
        assert_eq!((list[1].entry.target_database.clone(), list[1].entry.target_schema.clone()), (None, None));
        assert_eq!(list[2].entry, with);
    }

    #[tokio::test]
    async fn tab_state_upserts() {
        let (_d, db) = db().await;
        db.save_tab_state("c1", "a").await.unwrap();
        db.save_tab_state("c1", "b").await.unwrap();
        assert_eq!(db.load_tab_state("c1").await.unwrap(), Some("b".into()));
        assert_eq!(db.load_tab_state("nope").await.unwrap(), None);
    }

    #[tokio::test]
    async fn delete_connection_cascades() {
        let (_d, db) = db().await;
        let p = db.upsert_connection(profile("x")).await.unwrap();
        db.upsert_saved_query(sq(&p.id, "q")).await.unwrap();
        db.add_history(&p.id, &[hist(1)]).await.unwrap();
        db.save_tab_state(&p.id, "{}").await.unwrap();
        db.delete_connection(&p.id).await.unwrap();
        assert!(db.list_saved_queries(&p.id).await.unwrap().is_empty());
        assert!(db.list_history(&p.id, 10).await.unwrap().is_empty());
        assert_eq!(db.load_tab_state(&p.id).await.unwrap(), None);
    }

    #[test]
    fn app_error_serializes_with_kind() {
        let v = serde_json::to_value(AppError::Filter { message: "x".into(), position: 3 }).unwrap();
        assert_eq!(v, serde_json::json!({"kind":"filter","message":"x","position":3}));
    }
}
