-- Test objects for manual check step 132 (docs/manual-check.md). Load into the MySQL 8.4 test server (localhost:33084):
--   docker compose -f docker-compose.test.yml exec -T mysql84 mysql -uroot -ptest < src-tauri/tests/fixtures/manual/manual_mysql.sql
-- Safe to run again: it recreates database manual10. The integration tests never touch it.
DROP DATABASE IF EXISTS manual10;
CREATE DATABASE manual10;
USE manual10;
SET SESSION cte_max_recursion_depth = 1000;

-- A table without a primary key but with a unique NOT NULL index (ordered by it), inserted in reverse.
CREATE TABLE nokey_uniq (n INT NOT NULL, grp VARCHAR(10), UNIQUE KEY n_uniq (n));
INSERT INTO nokey_uniq WITH RECURSIVE s(n) AS (SELECT 250 UNION ALL SELECT n - 1 FROM s WHERE n > 1) SELECT n, CONCAT('g', n % 5) FROM s;

-- A table without any key (no fixed order) and a view.
CREATE TABLE nokey_plain (n INT, grp VARCHAR(10));
INSERT INTO nokey_plain WITH RECURSIVE s(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM s WHERE n < 250) SELECT n, CONCAT('g', n % 5) FROM s;
CREATE VIEW nokey_view AS SELECT * FROM nokey_plain;
