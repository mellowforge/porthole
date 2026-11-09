-- Test objects for manual check steps 131–133 (docs/manual-check.md). Load into "Local PG17" (localhost:54317, app_test):
--   docker compose -f docker-compose.test.yml exec -T pg17 psql -U postgres -d app_test < src-tauri/tests/fixtures/manual/manual_pg.sql
-- Safe to run again: it recreates schema manual10. The integration tests never touch it.
DROP SCHEMA IF EXISTS manual10 CASCADE;
CREATE SCHEMA manual10;

-- 131: an expression index and an INCLUDE index.
CREATE TABLE manual10.idx_demo (id int PRIMARY KEY, email text, total numeric, status text);
INSERT INTO manual10.idx_demo VALUES (1, 'Ann@example.com', 10, 'paid'), (2, 'bob@example.com', 25, 'open');
CREATE INDEX idx_demo_lower_email ON manual10.idx_demo (lower(email));
CREATE INDEX idx_demo_id_incl ON manual10.idx_demo (id) INCLUDE (total, status);

-- 132: a table without a primary key (ordered by ctid), one with a unique NOT NULL index, and a view.
CREATE TABLE manual10.nokey (n int, grp text);
INSERT INTO manual10.nokey SELECT n, 'g' || (n % 5) FROM generate_series(1, 250) n;
CREATE TABLE manual10.nokey_uniq (n int NOT NULL UNIQUE, grp text);
INSERT INTO manual10.nokey_uniq SELECT n, 'g' || (n % 5) FROM generate_series(250, 1, -1) n;
CREATE VIEW manual10.nokey_view AS SELECT * FROM manual10.nokey;

-- 133: a view whose page takes 30 s.
CREATE VIEW manual10.slow AS SELECT pg_sleep(30)::text AS s;
