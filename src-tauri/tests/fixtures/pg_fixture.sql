DROP SCHEMA IF EXISTS fx CASCADE;
DROP SCHEMA IF EXISTS "Mixed Case" CASCADE;
CREATE SCHEMA fx;
CREATE SCHEMA "Mixed Case";

CREATE TABLE fx.users (
  id serial PRIMARY KEY,
  name text NOT NULL,
  email varchar(255),
  status varchar(20),
  plan text,
  created_at timestamptz DEFAULT now(),
  bio text,
  data jsonb,
  tags text[],
  avatar bytea
);

INSERT INTO fx.users (name, email, status, plan) VALUES
  ('Joanna',   'joanna@example.com',   'active', NULL),
  ('Hannah',   'hannah@example.com',   'active', ''),
  ('Shannon',  'shannon@example.com',  'active', 'pro'),
  ('Brianna',  'brianna@example.com',  'active', 'pro'),
  ('Johanna',  'johanna@example.com',  'active', 'free'),
  ('Leanne',   'leanne@example.com',   'active', 'free'),
  ('Dianna',   'dianna@example.com',   'active', 'free'),
  ('Roxanne',  'roxanne@example.com',  'active', 'pro'),
  ('Susanna',  'susanna@example.com',  'active', 'free'),
  ('Deanna',   'deanna@example.com',   'active', 'pro'),
  ('Savannah', 'savannah@example.com', 'active', 'free'),
  ('Rosanna',  'rosanna@example.com',  'active', 'pro'),
  ('Marianne', 'marianne@example.com', 'active', 'free'),
  ('Giovanna', 'giovanna@example.com', 'active', 'pro'),
  ('Joann',    'joann@example.com',    'active', 'free'),
  ('Bob',      'bob@example.com',      'inactive', 'free'),
  ('Carol',    'carol@example.com',    'active', 'pro');

UPDATE fx.users SET bio = repeat('x', 10000) WHERE id = 3;
UPDATE fx.users SET avatar = '\xDEADBEEF', data = '{"a":1}', tags = '{x,y}' WHERE id = 4;
SELECT setval(pg_get_serial_sequence('fx.users', 'id'), 17);

CREATE TABLE fx.orders (
  id serial PRIMARY KEY,
  user_id int REFERENCES fx.users(id),
  total numeric(10,2)
);
CREATE INDEX orders_user_id_idx ON fx.orders (user_id);
INSERT INTO fx.orders (user_id, total) VALUES (1, 10.50), (1, 20.00), (2, 5.25);

CREATE TABLE fx.no_pk (a int, b text);
INSERT INTO fx.no_pk VALUES (1, 'one'), (2, 'two');

CREATE TABLE fx.big (id int PRIMARY KEY, v text);
INSERT INTO fx.big SELECT g, 'v' || g FROM generate_series(1, 60000) g;
ANALYZE fx.big;

CREATE VIEW fx.active_users AS SELECT id, name FROM fx.users WHERE status = 'active';
CREATE MATERIALIZED VIEW fx.mv_totals AS SELECT user_id, sum(total) AS total FROM fx.orders GROUP BY user_id;
CREATE FUNCTION fx.calc_tax(numeric) RETURNS numeric LANGUAGE sql AS 'SELECT $1 * 0.2';

CREATE TABLE fx.events (id int, at date NOT NULL) PARTITION BY RANGE (at);
CREATE TABLE fx.events_2026 PARTITION OF fx.events FOR VALUES FROM ('2026-01-01') TO ('2027-01-01');

CREATE TABLE "Mixed Case"."Order Items" (
  id serial PRIMARY KEY,
  "select" text,
  "Weird ""Quote""" int
);
INSERT INTO "Mixed Case"."Order Items" ("select", "Weird ""Quote""") VALUES ('b', NULL), ('a', 7);

CREATE TABLE fx."select" (id int PRIMARY KEY, "from" text);
INSERT INTO fx."select" VALUES (1, 'x'), (2, 'y');

CREATE TABLE fx.typed (id int PRIMARY KEY, ts timestamptz, j jsonb, b bytea, arr int[], n numeric(5,2), v varchar(5), u uuid, flag bool, note text, c char(10), bits bit(3), cc char(3)[]);
INSERT INTO fx.typed VALUES (1, '2026-01-01 10:00+00', '{"a":1}', '\xdead', '{1,2}', 1.50, 'abc', '00000000-0000-0000-0000-000000000001', true, 'x', 'abc', B'101', '{ab,cd}');
CREATE TABLE fx.pair ("Key A" text, "select" int, val text, PRIMARY KEY ("select", "Key A"));
INSERT INTO fx.pair VALUES ('it''s', 1, 'one'), ('b', 2, 'two');
CREATE TABLE fx.gen (id int GENERATED ALWAYS AS IDENTITY PRIMARY KEY, name text NOT NULL, created timestamptz DEFAULT now());
INSERT INTO fx.gen (name) VALUES ('first');
DO $$
BEGIN
  IF current_setting('server_version_num')::int >= 120000 THEN
    EXECUTE 'ALTER TABLE fx.gen ADD COLUMN upper_name text GENERATED ALWAYS AS (upper(name)) STORED';
  END IF;
END
$$;
CREATE TABLE fx.floaty (k float8 PRIMARY KEY, v text);
-- A role that can't see other sessions' state in pg_stat_activity (for `SET ROLE` startup SQL).
-- Roles are cluster-global, so it is created only once.
DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'ts_limited') THEN CREATE ROLE ts_limited NOLOGIN; END IF; END $$;
GRANT ts_limited TO CURRENT_USER;
