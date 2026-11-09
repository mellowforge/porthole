-- The SQLite test database. Default (rollback) journal; nothing here sets a journal mode.
-- `users` holds the rows of fx.users from pg_fixture.sql, value for value.

CREATE TABLE users (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  email TEXT,
  status TEXT,
  plan TEXT,
  created_at TEXT DEFAULT CURRENT_TIMESTAMP,
  bio TEXT,
  avatar BLOB
);

INSERT INTO users (id, name, email, status, plan) VALUES
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

-- repeat('x', 10000): 10,000 zero bytes are 20,000 hex zeros, every pair becomes one 'x'.
UPDATE users SET bio = replace(hex(zeroblob(10000)), '00', 'x') WHERE id = 3;
UPDATE users SET avatar = X'DEADBEEF' WHERE id = 4;

CREATE TABLE typed (
  id INTEGER PRIMARY KEY,
  i INTEGER,
  r REAL,
  n NUMERIC,
  t TEXT,
  b BLOB,
  anyv,
  d DATE
);
INSERT INTO typed VALUES
  (1, 42, 1.0, 1.5, 'x', X'DEAD', 5, '2026-01-01'),
  (2, -7, 0.1, 10, '', X'', '5', NULL),
  (3, NULL, 1e300, NULL, NULL, NULL, 2.5, NULL),
  (4, 0, 9e999, 0, 'y', NULL, NULL, NULL);

CREATE TABLE loose (k PRIMARY KEY, v TEXT);
INSERT INTO loose VALUES (5, 'int five'), ('5', 'text five');

CREATE TABLE big (id INTEGER PRIMARY KEY, label TEXT);
INSERT INTO big (id, label)
  WITH RECURSIVE g(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM g WHERE x < 60000)
  SELECT x, 'v' || x FROM g;

CREATE TABLE nopk (a TEXT, b TEXT);
INSERT INTO nopk VALUES ('1', 'one'), ('2', 'two');

CREATE TABLE floatkey (k REAL PRIMARY KEY, v TEXT);
INSERT INTO floatkey VALUES (1.5, 'one and a half'), (2.25, 'two and a quarter');

CREATE TABLE gen (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED);
INSERT INTO gen (id, a) VALUES (1, 21);

CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER REFERENCES users(id), total NUMERIC);
CREATE INDEX orders_user ON orders(user_id);
INSERT INTO orders (user_id, total) VALUES (1, 10.50), (1, 20.00), (2, 5.25);

CREATE TABLE "Order Items" (id INTEGER PRIMARY KEY, name TEXT);
INSERT INTO "Order Items" (name) VALUES ('b'), ('a');

CREATE VIEW active_users AS SELECT * FROM users WHERE status = 'active';
