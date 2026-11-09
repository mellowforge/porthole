CREATE TABLE saved_queries (id TEXT PRIMARY KEY, connection_id TEXT NOT NULL, name TEXT NOT NULL, sql TEXT NOT NULL,
  last_params_json TEXT NOT NULL DEFAULT '{}', updated_at TEXT NOT NULL);
CREATE INDEX saved_queries_conn ON saved_queries(connection_id);
CREATE TABLE query_history (id INTEGER PRIMARY KEY AUTOINCREMENT, connection_id TEXT NOT NULL, sql TEXT NOT NULL,
  params_json TEXT NOT NULL, verb TEXT NOT NULL, ok INTEGER NOT NULL, summary TEXT NOT NULL,
  started_at INTEGER NOT NULL, elapsed_ms INTEGER NOT NULL);
CREATE INDEX query_history_conn ON query_history(connection_id, id);
CREATE TABLE tab_state (connection_id TEXT PRIMARY KEY, json TEXT NOT NULL, updated_at TEXT NOT NULL);
