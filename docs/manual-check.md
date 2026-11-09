# Manual check

A walk through the app by hand, against the test databases. It covers what the automated tests can't: the real window, menus, dialogs and the Keychain. Steps are numbered once from 1 to 134, and later steps refer back to earlier ones by number. Run the commands from the repository root.

## Setup

Start the test databases (see [CONTRIBUTING.md](../CONTRIBUTING.md#tests)), load the PostgreSQL fixture, and start the app. The fixture creates the schemas `fx` and `"Mixed Case"` in `app_test`; load it again to start over.

```sh
docker compose -f docker-compose.test.yml up -d --wait
docker compose -f docker-compose.test.yml exec -T pg17 psql -U postgres -d app_test < src-tauri/tests/fixtures/pg_fixture.sql
npm run tauri dev
```

## PostgreSQL and the app window

1. The app opens on Connections, and the window can be dragged on the Connections screen.
2. Create the "Local PG17" connection: Host `localhost`, Port `54317`, User `postgres`, Password `test`, Database `app_test`, environment Staging.
3. **Test connection** shows "Test passed · N ms".
4. Quit and relaunch → the connection is still listed and no password is asked.
5. **Connect** → the workspace opens with an amber strip.
6. Schema `fx` shows its tables, views and functions.
7. Open `users` → 17 rows, and NULL looks different from empty.
8. Filter `status = "active" and name like "%ann%"` → "15 matches".
9. Filter `nope = 1` → an inline error.
10. Open `big` → "~60K rows", and paging works.
11. Click a header → the sort cycles.
12. **Structure** shows the primary key, indexes and foreign keys.
13. Open the `"Mixed Case"` schema → `Order Items` opens.
14. **Edit** → back to Connections with the connection selected.
15. Set the environment to Production → the strip turns red.
16. Turn on read-only and reconnect → nothing breaks while browsing.
17. Open a view (`active_users`) or a partitioned table (`events`) → it returns quickly with an exact row count.
18. With a table tab open, press ⌘W → the tab closes, not the window.
19. Select a saved connection and click **Delete** → it asks inline ("Delete “name”?"); confirming removes the connection.
20. ⌘T opens `Query 1`.
21. Run `select * from fx.users; update fx.users set plan = plan where id = 1` with ⇧⌘↵. The statement bar shows `SELECT 17 rows` and `UPDATE 1 row affected`.
22. `select pg_sleep(30)`, then **Stop** → `Canceled` within a second, and the next run works.
23. `DO $$ BEGIN RAISE NOTICE 'hi'; END $$` → Messages shows `NOTICE: hi`.
24. `begin; update fx.users set plan = 'x' where id = 1` → `Transaction open`. Closing the tab asks; **Roll back** leaves the plan unchanged.
25. With the connection set to Production, `update …` asks first. **Cancel** runs nothing.
26. Write `select * from fx.users where status = :status and created_at > :since limit :limit`, set the values, press ⌘S, type "Active since" and press Enter. Reopen it from Saved queries; the values are remembered.
27. Set a field to NULL → the run summary shows `NULL`.
28. ⌘Y lists the runs (newest first, grouped by day); clicking one opens its SQL and parameter values in a new query tab.
29. Quit with an unsaved query tab and relaunch, then connect. The tab and its text come back.
30. On a read-only connection, `update` in a query tab fails with the database's read-only error.
31. Connect to a real TLS server (sslmode `require`) and run a query in a query tab.
32. ⌘Q with an open transaction asks first.
33. Open two table tabs and a query tab with results, then switch between them several times. Every grid redraws with its rows, which shows that hidden grids repaint when they are shown again.
34. In a query tab, ⌘A highlights the whole editor text, including the current statement's lines. With the autocomplete list open, Tab accepts the suggestion; otherwise Tab indents and ⇧Tab outdents.
35. Open `fx.users`, double-click a name, type a new one, press Enter → the cell turns amber and the bar shows `1 pending change`.
36. Right-click a `plan` cell → **Set NULL** → it shows `NULL` in amber. Right-click **another** row → **Delete row** → the row turns red with strikethrough.
37. **Show SQL** → `Will run on … · in one transaction` with the UPDATE and DELETE.
38. ⌘S → the bar disappears, the footer shows `Saved 3 changes`, and the grid reloads showing the new values.
39. **+ Add row**, fill `name`, ⌘S → the new row is saved with its generated `id`.
40. Edit a cell, then click the next page → it asks to Save / Discard / Cancel.
41. Edit a cell, then close the tab, Edit the connection, or press ⌘Q → each asks first.
42. Open `fx.typed`, set `v` to `abcdefg`, ⌘S → `Nothing was saved: UPDATE id = 1: value too long for type character varying(5)`, and the edit stays amber.
43. Each of these opens read-only (no editing, no **+ Add row**) with its note under the header:
    - `fx.no_pk` → `This table has no primary key, so it opens read-only.`
    - `fx.active_users` → `Views are read-only.`
    - `fx.floaty` → `Rows can't be matched exactly by a floating-point primary key, so this table opens read-only.`
    - any table on a read-only connection → `This connection is read-only. Turn read-only off in the connection settings to edit.`
44. With the connection set to Production, ⌘S opens the preview and asks `This writes to Production.` Edit a cell again and click the next page → the unsaved-changes prompt shows the SQL too, and **Save to Production** is enabled once it has loaded.
45. In a query tab run `begin; select * from fx.users where id = 1 for update`, then edit `fx.users` id 1 in the grid and ⌘S → after about 5 s `Nothing was saved: Another session is holding a lock on this row (an open transaction in a query tab?).` Roll back the query tab and save again → it saves.
46. In a query tab run `begin; select 1`, click **Commit** and press ⌘Q at once → `Waiting for the transaction to end…`, then the app quits without asking about a transaction.
47. **Export:** filter a table (for example `id > 100`), choose **Export ▸ Export as CSV** and save. Open the file in Numbers: the header row, filtered rows only, NULL cells empty. The footer shows `Exported {N} rows to {name}.csv` with **Show in Finder**; click it and Finder reveals the file.
48. Run a query in a query tab and choose **Export ▸ Export as JSON**: an array of objects, one per line, numbers as numbers, NULL as `null`. Close the save dialog without choosing → nothing is shown. With unsaved grid edits the menu starts with `Unsaved changes are not included.` and the file has the saved values.
49. Export every row of a big result: run `select g from generate_series(1, 250000) g` (the grid shows 1,000 rows; Fetch all would stop at 100,000). **Export** shows `Runs the query again to export every row.`; **Export as CSV** writes all 250,000 rows plus the header. Then cancel a large export: run `select g from generate_series(1, 50000000) g`, **Export as CSV** to the Desktop and click **Cancel** while `Exporting… {N} rows` shows → `Export canceled.` within a second, and no file (and no `.part` leftover) at the chosen path. Choosing an existing file name and canceling leaves that file untouched. Inside a transaction: run `begin; create temp table keep(x int); insert into keep values (7)`, then the 50-million-row select, export and cancel it → `select * from keep` still returns 7 and **Commit** / **Rollback** still show (the transaction is intact).
50. **⌘K:** press ⌘K → `Search tables, queries, connections and actions`. Type a table name, Enter → it opens. Do the same with a saved query (opens in a tab), `Switch connection` (back to Connections) and `New window`. Type `zzzz` → `Nothing matches “zzzz”.`
51. **New window:** ⇧⌘N (and File ▸ New Window) opens a second window on the Connections screen with the same size and titlebar as the first. The **Window** menu lists every open window by title: `{connection} · {database}` once connected, `Porthole` before; choosing one brings it to the front.
52. **Two windows:** connect each window (two different connections, or the same one) and leave an unsaved grid edit in each. ⌘Q asks in the focused window, then in the other. Cancel in the second keeps both windows open. With the same connection in both, only the first window restores and saves tabs. Closing one window leaves the other's query tabs running.
53. During that ⌘Q prompt, close the asking window with its red button → the quit is canceled: the other window stays open and the app keeps running.
54. **Target chip:** in a query tab click the chip (`PG · app_dev ▾`), pick another database under **Database** → the chip reads `PG · {database}` and the schema resets. Run `select current_database()` → that database. Pick a schema and run `select current_schema()` or an unqualified table name → it resolves there. Autocomplete suggests the other database's tables.
55. With `begin; select 1` open in the tab, switch the chip to another database → the prompt `“Query 1” has an open transaction.` appears first; Cancel keeps the chip as it was, Roll back switches. Switch to a database that was dropped meanwhile and run → an inline error, and the tab can switch back.
56. Press ⌘Q right after ⇧⌘N: if nothing happens (the new window wasn't listening yet), ⌘Q again asks. A minimized or hidden window that is asked comes to the front.
57. Rename a connection in window A: window B's Connections list shows the new name.
58. Close a window while it is still connecting to a slow host, then connect another window to the same connection: its tabs restore and save. Likewise, after **Switch connection** in window A, window B on the same connection saves its tabs.
59. ⇧⌘N right after closing a window works, and each new window opens down and right of the focused one. If opening fails, the workspace shows `Couldn't open a new window: …`. Closing the last window leaves the app running (its menu bar and Dock icon stay); clicking the Dock icon or ⇧⌘N opens a window on the Connections screen, and ⌘Q quits.
60. During a big export, memory stays flat (Activity Monitor), and switching connection doesn't hang (the export is cancelled). ⌘Q during an export quits within a second and leaves no `.part` file.

## SQLite

Make the test file first (steps 61–72 use it; step 66 changes it, so to start over delete the file and make it again):

```sh
sqlite3 ~/porthole-fx.sqlite < src-tauri/tests/fixtures/sqlite_fixture.sql
```

61. **+ New**, pick **SQLite** (Host, Port, SSL / TLS and SSH are gone), name it, click **Choose…** and pick `~/porthole-fx.sqlite`, then **Test connection** → `Test passed · N ms`; **Save** → the list entry shows `SQ` and `~/porthole-fx.sqlite`.
62. **Connect** → the window title is `{connection} · porthole-fx.sqlite`, schema `main` shows tables and views with no Functions section, and ⌘T's chip reads `SQ · porthole-fx.sqlite ▾` with only a **Schema** list.
63. Open `users` → 17 rows; filter `status = "active" and name like "%ann%"` → "15 matches" (the same as step 8).
64. Open `loose`: filter `k = 5` → only `int five`; filter `k = "5"` → only `text five`.
65. Open `typed` → row 4's `r` shows `Inf` and row 1's `b` shows `\xdead`; double-clicking that `b` cell doesn't edit it, hovering it shows `Blob values can't be edited yet.`, and its **Set NULL** is disabled.
66. In `users`, edit a name, delete row 16 (`Bob`), **+ Add row** with a `name`, then ⌘S → `Saved 3 changes`, and the grid reloads with the new row's `id`.
67. In a query tab run `begin; update users set plan = 'x' where id = 1` → `Transaction open`; **Rollback** → `select plan from users where id = 1` shows it unchanged.
68. Run `with recursive c(x) as (select 1 union all select x+1 from c) select count(*) from c`, then **Stop** → `Canceled` within a second, and the next run works.
69. Open `typed` and choose **Export ▸ Export as JSON** → one object per row: `"i":42` and `"r":1.0` as numbers, `"r":"Inf"` and `"b":"\\xdead"` as strings, NULL as `null`.
70. Turn on read-only and reconnect → `update users set plan = 'x'` in a query tab fails with `attempt to write a readonly database`, and `users` opens with `This connection is read-only. Turn read-only off in the connection settings to edit.`
71. Set the connection's **Database file** to `/tmp/missing.sqlite` and **Connect** → `The file doesn't exist: /tmp/missing.sqlite` inline, and no file is created.
72. In a query tab run `begin immediate`, then edit a `users` cell and ⌘S → within about 6 s `Nothing was saved: The database is locked by another connection (an open transaction in a query tab?).` **Rollback** the tab and save again → it saves.
73. **Dock ▸ Quit asks too:** leave an unsaved grid edit (or an open transaction), then right-click the Dock icon ▸ **Quit** → the same `… has 1 unsaved change.` prompt as ⌘Q; **Cancel** keeps the app running. With nothing unsaved and no open transaction, Dock ▸ Quit quits at once. Hide the app (⌘H) with an unsaved edit and Dock ▸ Quit → the app comes to the front with the prompt. Logging out or restarting with nothing unsaved goes through as usual; with unsaved changes the app interrupts it (macOS says so) and shows its prompt, then log out again.

## MySQL and MariaDB

Load the fixture first (steps 74–88 use it; step 80 changes `users`, so load it again to start over). It creates the databases `fx` and `fx_other`:

```sh
docker compose -f docker-compose.test.yml exec -T mysql84 mysql -uroot -ptest < src-tauri/tests/fixtures/mysql_fixture.sql
```

74. **+ New**, pick **MySQL / MariaDB** (the port becomes `3306`; there is no Schema field and the database field reads `Database (optional)`), name it, and enter host `localhost`, port `33084`, user `root`, password `test`, database `fx`, then **Test connection** → `Test passed · N ms`.
75. **Connect** → the chip reads `MY · fx`, the sidebar picker is labelled `Database` and lists `fx` and `fx_other`, and Functions shows `calc_tax` and `two_sets`.
76. Switch the picker to `fx_other` → the tables list shows `other_t`, and the window title ends with `· fx_other`.
77. With the picker still on `fx_other`, open a new query tab (its chip reads `MY · fx_other`) and run `select database()` → `fx_other`.
78. Open `users`; filter `status = "active" and name like "%ann%"` → "15 matches" (the same on MariaDB, and the same as steps 8 and 63).
79. Open `typed` → it shows `1.23457`, `1e300`, `\xdead` and `00101`; double-clicking the binary cell doesn't edit it, and hovering it shows `Blob values can't be edited yet.`
80. In `users`, edit a name, delete row 16 (`Bob`), **+ Add row** with a `name`, then ⌘S → `Saved 3 changes`. Set a name to its own value and ⌘S → it saves.
81. In a query tab run `begin` → `Transaction open`; **Rollback** → the tab is idle again.
82. Run `select count(*) from big a, big b where a.id + b.id > 0`, then **Stop** → `Canceled` within a second, and the next run works.
83. Run `call two_sets(1, @t)` → the first result set shows, with the notice `The procedure returned 1 more result sets; only the first is shown.`
84. Run a script that starts with `DELIMITER //`, holds a `create procedure … begin … end //` body and ends with `DELIMITER ;` → the procedure is created as one statement, and appears under Functions in the sidebar without reconnecting.
85. Open `typed` and choose **Export ▸ Export as JSON** → numbers come out as numbers (`1e300` as a number, `"b":"\\xdead"` as a string).
86. Turn on read-only and reconnect → `update users set name = 'x'` in a query tab fails with `Cannot execute statement in a READ ONLY transaction`, and `users` opens with `This connection is read-only. Turn read-only off in the connection settings to edit.`
87. In a query tab run `begin; update users set name = name where id = 1`, then edit that row's cell in `users` and ⌘S → within about 6 s `Another session is holding a lock on this row (an open transaction in a query tab?).` **Rollback** the tab and save again → it saves.
88. Repeat steps 74–75 against MariaDB (`mariadb114`, port `33114`; load the fixture with `docker compose -f docker-compose.test.yml exec -T mariadb114 mariadb -uroot -ptest < src-tauri/tests/fixtures/mysql_fixture.sql`) → **Test connection** passes, the sidebar card shows the version under the connection's address: `MariaDB 11.4.…`, and the editor still highlights SQL.
89. Create a database from outside the app (`docker compose -f docker-compose.test.yml exec -T mysql84 mysql -uroot -ptest -e 'create database fx_new'`), then press the ↻ button beside the `Database` picker (or ⌘R) → `fx_new` is listed in the picker.

## MongoDB

Load the fixture first (steps 90–103 use it; steps 95–98 change `keys` and `users`, so load it again to start over). The MongoDB integration tests load it into all three test servers: `mongo60` (port `27060`) and `mongo80` (port `27080`), both with user `root`, password `test` and the read-only user `ro` / `ropw`, and the replica set `mongo80rs` (port `27082`, no user). It creates the databases `fx` and `fx_other`:

```sh
(cd src-tauri && cargo test --test mongo_integration)
```

90. **+ New**, pick **MongoDB** (the port becomes `27017`; there is no Schema field, an `Auth database (optional)` field appears, SSL / TLS has no `prefer`, and **Advanced** has no startup SQL), name it, and enter host `localhost`, port `27080`, user `root`, password `test`, database `fx`, then **Test connection** → `Test passed · N ms`.
91. **Connect** → the sidebar card shows `MG`, the picker is labelled `Database` and lists `fx` and `fx_other`, the sections read `COLLECTIONS` (`big`, `keys`, `typed`, `users`) and `VIEWS` (`active_users`, `metrics`), the search reads `Search collections, views…`, the empty workspace says `Double-click or click a collection to open it.`, and the sidebar card shows the version under the connection's address: `MongoDB 8.0.…`.
92. Open `typed` → its one row shows `1.0`, `1e+300`, `NaN`, `-0.0`, `1.10`, `9007199254740993`, the date `2024-01-02T03:04:05.678Z` (with its milliseconds), the year-10000 date as `{"$date":{"$numberLong":"253402300800000"}}`, and `nested_za` as `{"z":1,"a":{"$numberLong":"2"}}`, its keys in that order. The binary, UUID, timestamp, regex, min and max key, undefined, code and symbol cells show extended JSON and can't be edited.
93. Filter `users` (each shows its match count): `age = 30` → Ann, Bob and Cid (an int32, an int64 and a double 30), not Dee's string `"30"`; `age = '30'` → only Dee; `` `address.city` like '%osl%' `` → Ann and Eve; `nope = 1` → no rows and no error; `` `$where` = 1 `` → `A field name can't start with $.` In `keys`, `_id = '65a1b2c3d4e5f60718293a4b'` → two rows, the string key and the ObjectId key.
94. Open the Structure of `users` → the indexes `_id_` and `email_1`, both unique. In `typed`, `_id`, `a.b` and `$c` are read-only: double-clicking them doesn't edit.
95. In `keys`, set `v` to `1` on the row whose `_id` is the string `65a1b2c3d4e5f60718293a4b`, then ⌘S → only that row changes; the ObjectId row with the same hex keeps `0`.
96. Connect to `mongo80rs` (port `27082`, no user or password, database `fx`). In `users`, edit a name, delete `Fay`, **+ Add row** with a `name` and an `email`, then **Show SQL** → `Will run on … · in one transaction` with an `updateOne`, a `deleteOne` and an `insertOne` line; ⌘S → `Saved 3 changes`, and the grid reloads with the new row's `_id`.
97. Back on `mongo80`, set the connection's environment to Production, edit a `users` name and ⌘S → the preview's title reads `Will run on … · one change at a time (this server has no transactions)` and asks `This writes to Production.`; **Save to Production** saves it.
98. Partial save on `mongo80`: **+ Add row** twice, both with the email `new@example.com`, then ⌘S → `Saved 1 of 2 changes, then: E11000 duplicate key error … This server has no transactions, so the saved changes stay saved; the rest are still pending.` The first new row leaves the pending list; the second stays. The same on `mongo80rs` saves nothing: `Nothing was saved: E11000 duplicate key error …`.
99. Turn on read-only and reconnect (the form notes that MongoDB has no read-only sessions) → pages still load, and saving an edited name fails with `Nothing was saved: This connection is read-only. Turn read-only off in the connection settings to edit.` Turn read-only off and connect as `ro` / `ropw` on `mongo80` → saving an edit fails with `Nothing was saved: Not authorized to write to fx.users.`
100. Open `typed` and choose **Export ▸ Export as JSON** → the whole document with its types kept as extended JSON (`{"$numberLong":"9007199254740993"}`, `{"$numberDouble":"NaN"}`, the nested `{"z":1,"a":{"$numberLong":"2"}}` with its keys in order); **Export ▸ Export as CSV** → the grid's columns only, each cell as the grid shows it.
101. **+ New query** is enabled for MongoDB: ⌘T opens `Query 1`, its chip reads `MG · fx`, and its editor highlights the text as JavaScript.
102. Set the port to `27999` and **Test connection** → within the timeout `Couldn't reach the MongoDB server at localhost:27999. Connection refused.`
103. Repeat steps 90–91 against `mongo60` (port `27060`) → the sidebar card shows the version under the connection's address: `MongoDB 6.0.…`.
104. On a Postgres connection, the sidebar card shows `PostgreSQL 17.…` on its own line under the connection's address (no build details); hovering it shows the server's full `version()` text. A version too long for the card ends in `…`.

## MongoDB query tabs

Run `cargo test --test mongo_integration` in `src-tauri` first: it reloads the fixture on every server, and manual edits make the `mongo80rs` data drift.

105. On `mongo80`, in a query tab, run `db.users.find({}, { name: 1, age: 1 }).sort({ name: 1 }).limit(3)` → the columns `_id`, `name` and `age`, the rows Ann, Bob and Cid (ages `30`, `30` and `30.0`: an int32, an int64 and a double), and the statement bar `find 3 rows · N ms`; Messages shows the statement and `3 rows · N ms`.
106. `db.big.find()` → `find 1,000+ rows` and `Showing the first 1,000 rows.`; **Fetch all** → `20,000 rows`.
107. `db.users.find({ age: { $gt: :min } }).sort({ name: 1 })` with `min` set to `29` → Ann, Bob, Cid and Eve (not Dee, whose age is the string `"30"`, nor Fay, who has none). With `min` left empty, the run stops with `No value for :min` and nothing runs.
108. `db.users.insertOne({ name: "Gus" })` → `insertOne 1 row affected`, and Messages shows `info: Inserted 1 document (_id: ObjectId("…")).` Then `db.notes.insertMany([{ t: "a" }, { t: "b" }])` → `info: Inserted 2 documents.`, and the sidebar lists the new collection `notes` under `COLLECTIONS`.
109. `db.users.updateMany({ active: false }, { $set: { flagged: true } })` → `2 rows affected` and `info: Matched 2, modified 2.`; running it again → `info: Matched 2, modified 0.`
110. Set the connection's environment to Production and run `db.users.deleteMany({ name: "Gus" })` → `This looks like it changes data on Production: deleteMany`; **Cancel** runs nothing (`db.users.countDocuments({ name: "Gus" })` still gives `1`); **Run anyway** → `1 row affected`. Set the environment back.
111. Turn read-only on and reconnect → `db.users.find()` still runs, and `db.users.insertOne({ name: "Hal" })` fails with `This connection is read-only. Turn read-only off in the connection settings to run insertOne.`, and nothing is inserted. Turn read-only off.
112. In `users`, edit Ann's name to `Ann B`, open **Show SQL**, copy the `db.getCollection("users").updateOne(…)` line, then discard the edit. Paste the line into a query tab and run it → `1 row affected` and `info: Matched 1, modified 1.`; reopening `users` shows `Ann B`.
113. Run `db.big.find({ $where: "sleep(100) || true" })`, then **Stop** → within a second the statement bar shows `Canceled` and Results reads `Stopped.`; the next run works. Then `db.big.updateMany({ $where: "sleep(100) || true" }, { $set: { s: 1 } })`, **Stop** after a second or two → Results reads `Stopped.` and, under it, `Stopped. Some documents may already have been changed.` (Messages: `warning: Stopped. Some documents may already have been changed.`); `db.big.countDocuments({ s: 1 })` gives a small number that stays the same when run again.
114. Run `db.big.find()` and choose **Export ▸ Export as JSON** → the menu notes `Runs the query again to export every row.`, and the file holds all 20,000 documents. `db.big.distinct("_id")` (`1,000+ rows`) exports the same way, one `{"_id": …}` document per value.
115. Type `db.users.find({ name: "Ann B" })` on line 1 and `db.users.find({ age: })` on line 2, then ⇧⌘↵ → the first returns its row; the second fails with `Expected a value.`, and Messages shows `Line 2, column 22`; clicking it puts the cursor before the `}`.
116. Repeat step 105 on `mongo60` (port `27060`) and on `mongo80rs` (port `27082`, no user or password) → the same rows.
117. On `mongo80`, run `db.users.findOneAndUpdate({ name: "Bob" }, { $set: { age: 31 } }, { returnDocument: "after", projection: { name: 1, age: 1 } })` → one row with the columns `_id`, `name` and `age`, Bob with age `31`, and Messages shows `info: Updated 1 document.`; the same call with `{ $set: { age: 32 } }` and no options returns the document before the change (age `31`). With `{ name: "Nobody" }` as the filter → no rows and `info: No document matched.` With `{ name: "Zed" }`, `{ $set: { email: "zed@x" } }` and `{ upsert: true }` → no rows and `info: Upserted _id: ObjectId("…").`. `db.users.findOneAndDelete({ name: "Zed" })` → the Zed row and `info: Deleted 1 document.`
118. `db.p9_bulk.bulkWrite([{ insertOne: { document: { _id: 1 } } }, { insertOne: { document: { _id: 1 } } }, { insertOne: { document: { _id: 2 } } }])` → an error starting `E11000 duplicate key error` with the code `MONGO_11000`, and Messages shows `info: Inserted 1, matched 0, modified 0, upserted 0, deleted 0.` then `info: Ran 1 of 3 operations before the error.`; `db.p9_bulk.find()` shows only `_id` 1 (the third operation never ran). Then the same with `p9_bulk2` and `{ ordered: false }` as the second argument → the same error, `info: Inserted 2, matched 0, modified 0, upserted 0, deleted 0.` then `info: 1 of 3 operations failed; the others ran.`, and `db.p9_bulk2.find()` shows `_id` 1 and 2. Clean up with `db.p9_bulk.drop()` and `db.p9_bulk2.drop()`.
119. `db.createCollection("p9_logs")` → `Done` and `info: Created collection fx.p9_logs.`; the sidebar lists `p9_logs` under `COLLECTIONS`. `db.p9_logs.createIndex({ at: -1 })` → `info: Created index at_-1 on fx.p9_logs.`; running it again → `info: Index at_-1 already exists.` `db.p9_logs.getIndexes()` → two rows, `_id_` and `at_-1`. `db.p9_logs.dropIndex("at_-1")` → `info: Dropped index at_-1.`; running it again fails with the code `MONGO_27`.
120. Set the connection's environment to Production and run `db.p9_logs.drop()` → `This looks like it changes data on Production: drop`; **Cancel** runs nothing (`p9_logs` is still in the sidebar); **Run anyway** → `Done` and `info: Dropped collection fx.p9_logs.`, and `p9_logs` leaves the sidebar. Run it again (**Run anyway**) → `info: There was no collection fx.p9_logs to drop.`, not an error. Set the environment back.
121. `db.createCollection("p9_logs")`, then `db.p9_logs.renameCollection("p9_logs2")` → `info: Renamed fx.p9_logs to fx.p9_logs2.`, and the sidebar lists `p9_logs2` instead of `p9_logs`. Clean up with `db.p9_logs2.drop()` → `info: Dropped collection fx.p9_logs2.` A view drops the same way: `db.createCollection("p9_v", { viewOn: "users", pipeline: [] })` → `info: Created collection fx.p9_v.`, then `db.p9_v.drop()` → `info: Dropped collection fx.p9_v.`
122. `db.users.find({ email: "ann@example.com" }).explain()` → the columns `stage`, `index` and `details`, one row per plan stage with `email_1` in the `index` column of the index scan (the stage name varies by server version: `EXPRESS_IXSCAN` on `mongo80`, and `IXSCAN` under a `FETCH` on `mongo60`), and Messages shows `info: Planned only. explain("executionStats") also runs it and shows what it read.` `db.users.find({ email: "ann@example.com" }).explain("executionStats")` → Messages shows `info: Returned 1 document; examined 1 index key and 1 document in N ms.`. `db.users.find({ $where: "throw new Error('boom')" }).explain("executionStats")` fails with an error starting `Error: boom` and the code `MONGO_139`, as running the find does. In a read-only connection these still run.
123. Type `db.users.updateMany({ active: false, "address.city": "Oslo" }, { $set: { flagged: true, reviewedBy: "someone@example.com" } })` on one line and press **Format** (⇧⌘F) → it becomes `db.users.updateMany({ active: false, "address.city": "Oslo" }, {` / `  $set: { flagged: true, reviewedBy: "someone@example.com" }` / `})`; ⌘Z puts back the one line in a single step. Then `db.orders.aggregate([{ $match: { status: "A" } }, { $group: { _id: "$cust", total: { $sum: "$amount" } } }, { $sort: { total: -1 } }])` → `db.orders.aggregate([` then one stage per line (`  { $match: { status: "A" } },`, `  { $group: { _id: "$cust", total: { $sum: "$amount" } } },`, `  { $sort: { total: -1 } }`) and `])`. Format twice gives the same text, and a comment in the statement stays where it was.
124. Type `db.users.find({ ` and then `ad` → completion offers `address` and `address.city`; choosing the second writes `"address.city"`. Inside `db.users.updateMany({}, { $set: { ` typing `na` offers `name`. Before the first completion in a tab a collection's fields are fetched once, so a moment's delay is normal; a collection that doesn't exist offers nothing and typing is not blocked.
125. In the connection form, open **Advanced**, set `Statement time limit (seconds)` to `2`, save and reconnect. Run `db.big.find({ $where: "sleep(100) || true" })` → after about 2 seconds the statement fails with `Stopped after 2 s, the connection's statement time limit.` and the code `MONGO_TIMEOUT` (not `Canceled`); `db.users.find()` still works. Then `db.big.updateMany({ $where: "sleep(100) || true" }, { $set: { s: 1 } })` → the same error and, under it, `Stopped. Some documents may already have been changed.` Clear the field and reconnect → `db.big.find({ $where: "sleep(100) || true" })` runs until it ends or **Stop**.
126. Repeat steps 117–122 on `mongo60` (port `27060`) and on `mongo80rs` (port `27082`, no user or password) → the same rows and texts, except the explain stage names in step 122.

## Keyboard, restored tabs and history

127. In the connection form, tick a checkbox and press ⌘N → a new connection starts. Click Delete on a profile and press ⌘↵ while "Delete “…”?" shows → nothing connects.
128. Open ⌘K, type `us`, then press ⌘T, ⌘W, ⌘Y and ⌘S → nothing happens behind the palette, and ⌘A / ⌘C / ⌘V still edit the text.
129. Quit the app. In the app database (`~/Library/Application Support/io.github.mellowforge.porthole/app.db`), find the connection's id with `sqlite3 "$HOME/Library/Application Support/io.github.mellowforge.porthole/app.db" "SELECT id, name FROM connections"`, then run `sqlite3 "$HOME/Library/Application Support/io.github.mellowforge.porthole/app.db" "UPDATE tab_state SET json = '{nope' WHERE connection_id = '<id>'"` (the connection needs a stored row, so open a tab and quit first). Reopen the app and connect → `Couldn't restore your open tabs. They won't be saved until you reconnect.` with **Discard saved tabs**. Click it, open a table, reconnect → the table tab comes back.
130. On Postgres, run a query with the chip on `fx_other`, then open History (⌘Y) → the entry shows `fx_other`, and clicking it opens a tab whose chip names `fx_other`. On MySQL an entry shows its database. Older entries with no database recorded still open.

## Structure details, paging and Stop

Steps 131–133 use extra test objects. Load them once (each script is safe to run again):

```sh
docker compose -f docker-compose.test.yml exec -T pg17 psql -U postgres -d app_test < src-tauri/tests/fixtures/manual/manual_pg.sql
docker compose -f docker-compose.test.yml exec -T mysql84 mysql -uroot -ptest < src-tauri/tests/fixtures/manual/manual_mysql.sql
sqlite3 ~/porthole-fx.sqlite < src-tauri/tests/fixtures/manual/manual_sqlite.sql
```

131. In "Local PG17", pick schema `manual10` in the sidebar (press ↻ if it isn't listed), open `idx_demo` and click **Structure** → under Indexes, `idx_demo_lower_email` reads `lower(email)` and `idx_demo_id_incl` reads `id INCLUDE (total, status)` (the `INCLUDE (…)` part greyed). (SQLite and MySQL expression parts are still not shown.)
132. Paging without a primary key (100 rows a page, 250 rows each; the `n` column should equal the row number on every page):
    - "Local PG17", schema `manual10`: open `nokey` and page through → `n` runs 1–100, 101–200, 201–250, no note (ordered by `ctid`). Click the `grp` header to sort → page 1 is `g0` then `g1`, page 2 starts at `g2` with `n` = 2, and within each group `n` keeps rising with no repeats. Open `nokey_uniq` (inserted 250 down to 1) → row 1 has `n` = 1 (ordered by its unique index), no note. Open the view `nokey_view` → `Rows here have no fixed order, so paging can repeat or skip a row.` under the filter bar.
    - SQLite (`~/porthole-fx.sqlite`): `p10_nokey` → `n` equals the row number on every page, no note (ordered by `rowid`); `p10_nokey_view` → the note.
    - MySQL 8.4 (port `33084`), database `manual10` in the sidebar: `nokey_uniq` → `n` equals the row number, no note (ordered by its unique NOT NULL index); `nokey_plain` (no key at all) and `nokey_view` → the note. A table whose rows all fit on one page never shows it.
133. Stop (Postgres only). In "Local PG17", schema `manual10`, open the view `slow` (each page takes 30 s) → after a second **Stop** shows beside the pager; click it → `Stopped. The page wasn't loaded.` at once. In a query tab run `SELECT pid, state, left(query, 60) AS query FROM pg_stat_activity WHERE query LIKE '/* ts-page:%';` → no rows (without Stop you'd see one `active` row for 30 s). **Retry** loads again (and can be stopped again). A page stuck behind a lock stops the same way: close the `nokey` tab, then in a query tab run `BEGIN; LOCK TABLE manual10.nokey IN ACCESS EXCLUSIVE MODE;`, open `nokey` from the sidebar → it keeps loading; **Stop** → `Stopped. The page wasn't loaded.`; then run `ROLLBACK;` in the query tab and **Retry** → the page loads. On SQLite, MySQL and MongoDB there is no **Stop** on a page.

## Crash recovery

134. Edit two cells in a table, wait a second, then force-quit the app (`kill -9` the process) and reopen it → the edits are back with `Unsaved changes from your last session were restored in “…”. Check them before saving.` Save works. Then edit again, quit with ⌘Q, choose Discard, and reopen → no edits come back. If a crash cuts off a save, the edits come back marked uncertain: saving again shows the SQL and asks first. Edits that can't be restored show `Some unsaved changes from your last session couldn't be restored.`

## Notes

On MongoDB, `verify-ca` also checks the server's host name (the driver's TLS can't skip only that check), so it behaves like `verify-full`. A CSV export has only the sampled columns (the first and last 500 documents by `_id`); a field seen only in other documents is in the JSON export.

Databases attached in a query tab (`attach database … as aux`) stay on that tab's connection: they don't show in the sidebar or in other tabs. Databases attached by the connection's startup SQL show as schemas next to `main`.

A number typed into a column with no declared type (`loose.k`, `typed.anyv`) is stored as text, both in a new row and in a cell that was NULL; only a cell that already held a number keeps a typed number numeric. This keeps it possible to enter the text `'5'` next to the integer `5`; to store a number in a new row, use a query tab (`insert into loose values (6, 'six')`).

## Keychain and stored profile

- Keychain Access lists `io.github.mellowforge.porthole`, `connection:{id}`.
- Run the command below and confirm there is no password in the output:

```sh
sqlite3 "$HOME/Library/Application Support/io.github.mellowforge.porthole/app.db" 'select profile_json from connections'
```
