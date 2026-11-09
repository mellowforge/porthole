-- The MySQL / MariaDB fixture: recreates the databases `fx` and `fx_other` on every load.
-- Every name is qualified, so the load doesn't depend on the connection's database.
DROP DATABASE IF EXISTS fx;
DROP DATABASE IF EXISTS fx_other;
CREATE DATABASE fx CHARACTER SET utf8mb4;
CREATE DATABASE fx_other CHARACTER SET utf8mb4;

-- `users` holds the rows of fx.users from pg_fixture.sql, value for value.
CREATE TABLE fx.users (
  id INT AUTO_INCREMENT PRIMARY KEY,
  name VARCHAR(100) NOT NULL,
  email VARCHAR(255),
  status VARCHAR(20),
  plan TEXT,
  created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
  bio TEXT,
  avatar BLOB
);
INSERT INTO fx.users (id, name, email, status, plan) VALUES
  (1,  'Joanna',   'joanna@example.com',   'active', NULL),
  (2,  'Hannah',   'hannah@example.com',   'active', ''),
  (3,  'Shannon',  'shannon@example.com',  'active', 'pro'),
  (4,  'Brianna',  'brianna@example.com',  'active', 'pro'),
  (5,  'Johanna',  'johanna@example.com',  'active', 'free'),
  (6,  'Leanne',   'leanne@example.com',   'active', 'free'),
  (7,  'Dianna',   'dianna@example.com',   'active', 'free'),
  (8,  'Roxanne',  'roxanne@example.com',  'active', 'pro'),
  (9,  'Susanna',  'susanna@example.com',  'active', 'free'),
  (10, 'Deanna',   'deanna@example.com',   'active', 'pro'),
  (11, 'Savannah', 'savannah@example.com', 'active', 'free'),
  (12, 'Rosanna',  'rosanna@example.com',  'active', 'pro'),
  (13, 'Marianne', 'marianne@example.com', 'active', 'free'),
  (14, 'Giovanna', 'giovanna@example.com', 'active', 'pro'),
  (15, 'Joann',    'joann@example.com',    'active', 'free'),
  (16, 'Bob',      'bob@example.com',      'inactive', 'free'),
  (17, 'Carol',    'carol@example.com',    'active', 'pro');
UPDATE fx.users SET bio = REPEAT('x', 10000) WHERE id = 3;
UPDATE fx.users SET avatar = x'DEADBEEF' WHERE id = 4;

CREATE TABLE fx.typed (
  id INT PRIMARY KEY,
  x DOUBLE,
  f FLOAT,
  dt DATETIME(3),
  tm TIME(2),
  dc DECIMAL(10,2),
  vb VARBINARY(4),
  bt BIT(5),
  j JSON,
  y YEAR,
  e ENUM('a','b'),
  st SET('x','y'),
  u BIGINT UNSIGNED,
  ti TINYINT(1),
  d DATE
);
INSERT INTO fx.typed VALUES
  (1, 1e300, 1.23456789, '2026-01-02 03:04:05.678', '-100:02:03.45', 12.30, x'dead', b'00101', '{"a": [1, 2]}', 2026, 'b', 'x,y', 18446744073709551615, 1, '2026-01-02'),
  (2, 0.1, 0.1, '2026-01-02 03:04:05', '00:00:00', -0.5, x'', b'0', 'null', 1999, 'a', '', 0, 0, '0001-01-01'),
  (3, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL);

CREATE TABLE fx.bigkey (id BIGINT PRIMARY KEY, v VARCHAR(10));
INSERT INTO fx.bigkey VALUES (9007199254740992, 'a'), (9007199254740993, 'b');

CREATE TABLE fx.binkey (id BINARY(2) PRIMARY KEY, v VARCHAR(10));
INSERT INTO fx.binkey VALUES (x'0001', 'one'), (x'0002', 'two');

CREATE TABLE fx.nopk (a VARCHAR(10), b VARCHAR(10));
INSERT INTO fx.nopk VALUES ('1', 'one'), ('2', 'two');

CREATE TABLE fx.floatkey (k DOUBLE PRIMARY KEY, v TEXT);
INSERT INTO fx.floatkey VALUES (1.5, 'a');

CREATE TABLE fx.gen (
  id INT AUTO_INCREMENT PRIMARY KEY,
  a INT DEFAULT 3,
  b INT AS (a * 2) STORED,
  c INT AS (a + 1) VIRTUAL,
  ts TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE fx.orders (
  id INT PRIMARY KEY,
  user_id INT,
  total DECIMAL(10,2),
  INDEX orders_user (user_id),
  CONSTRAINT orders_user_fk FOREIGN KEY (user_id) REFERENCES fx.users(id)
);
INSERT INTO fx.orders VALUES (1, 1, 10.50), (2, 1, 20.00), (3, 2, 5.25);

CREATE TABLE fx.`Order Items` (id INT PRIMARY KEY, name TEXT);
INSERT INTO fx.`Order Items` VALUES (1, 'b'), (2, 'a');

CREATE TABLE fx.latin (id INT PRIMARY KEY, s VARCHAR(10) CHARACTER SET latin1);
INSERT INTO fx.latin VALUES (1, 'café');

-- 60,000 rows from a cross join of digit tables: a recursive CTE past 1,000 rows fails on
-- MySQL and is silently cut short on MariaDB.
CREATE TABLE fx.big (id INT PRIMARY KEY, label VARCHAR(20));
INSERT INTO fx.big (id, label)
SELECT n, CONCAT('v', n) FROM (
  SELECT 1 + d0.d + 10 * d1.d + 100 * d2.d + 1000 * d3.d + 10000 * d4.d AS n
  FROM (SELECT 0 AS d UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4
        UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) d0,
       (SELECT 0 AS d UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4
        UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) d1,
       (SELECT 0 AS d UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4
        UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) d2,
       (SELECT 0 AS d UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4
        UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) d3,
       (SELECT 0 AS d UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4
        UNION ALL SELECT 5) d4
) digits;

CREATE VIEW fx.active_users AS SELECT * FROM fx.users WHERE status = 'active';

CREATE FUNCTION fx.calc_tax(amount DECIMAL(10,2)) RETURNS DECIMAL(10,2) DETERMINISTIC RETURN amount * 0.2;

DELIMITER //
CREATE PROCEDURE fx.two_sets(IN n INT, OUT total INT) BEGIN SELECT n; SELECT n + 1; SET total = n; END//
DELIMITER ;

CREATE TABLE fx_other.other_t (id INT PRIMARY KEY, label TEXT);
INSERT INTO fx_other.other_t VALUES (1, 'one');

DROP USER IF EXISTS 'ts_app'@'%';
CREATE USER 'ts_app'@'%' IDENTIFIED BY 'apppw';
GRANT ALL ON fx.* TO 'ts_app'@'%';
FLUSH PRIVILEGES;
