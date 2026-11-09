-- Test objects for manual check step 132 (docs/manual-check.md). Load into the SQLite fixture file:
--   sqlite3 ~/porthole-fx.sqlite < src-tauri/tests/fixtures/manual/manual_sqlite.sql
-- Safe to run again: it recreates the p10_ objects.
DROP VIEW IF EXISTS p10_nokey_view;
DROP TABLE IF EXISTS p10_nokey;

-- A table without a primary key (ordered by rowid) and a view.
CREATE TABLE p10_nokey (n INTEGER, grp TEXT);
WITH RECURSIVE s(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM s WHERE n < 250)
INSERT INTO p10_nokey SELECT n, 'g' || (n % 5) FROM s;
CREATE VIEW p10_nokey_view AS SELECT * FROM p10_nokey;
