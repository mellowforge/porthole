// Dev-only fixtures so the UI can be rendered in a plain browser (no Tauri).
// Loaded only through the gated dynamic import in `tauri.ts`
// (`import.meta.env.DEV` and `?mock` in the URL); never in tests or production builds.
import type {
  ApplyResult,
  ChangeSet,
  ColumnInfo,
  ConnectedInfo,
  ConnectionProfile,
  ExportOutcome,
  ExportProgress,
  HistoryEntry,
  Outcome,
  PageRequest,
  RunEvent,
  RunOutcome,
  RunRequest,
  SavedQuery,
  StatementResult,
  SchemaTree,
  SqlAnalysis,
  StatementSpan,
  TablePage,
  TableStructure,
  TestResult,
} from "./types";
import { capabilities } from "../connections/profile";

const base = {
  transport: "network",
  defaultSchema: null,
  sslMode: "prefer",
  sslRootCert: null,
  connectTimeoutSecs: 10,
  readOnly: false,
  startupSql: null,
} as const;

let connections: ConnectionProfile[] = [
  { ...base, id: "m1", name: "Local Postgres", kind: "postgres", host: "localhost", port: 5432, user: "postgres", database: "app_dev", environment: "local" },
  { ...base, id: "m2", name: "Local MySQL", kind: "mysql", host: "localhost", port: 3306, user: "root", database: "shop", environment: "local" },
  { ...base, id: "m3", name: "Staging Postgres", kind: "postgres", host: "stg-db.internal", port: 5432, user: "app", database: "app", environment: "staging" },
  {
    id: "m4", name: "Local SQLite", kind: "sqlite", transport: "file", filePath: "/Users/me/data/app.sqlite", environment: "local",
    defaultSchema: null, connectTimeoutSecs: 10, readOnly: false, startupSql: null,
  },
  { ...base, sslMode: "disable", id: "m6", name: "Local MongoDB", kind: "mongodb", host: "localhost", port: 27017, user: "", database: "fx", environment: "local" },
  { ...base, id: "m5", name: "Reports (MSSQL)", kind: "sqlserver", host: "reports.corp", port: 1433, user: "bi", database: "bi", environment: "production" },
];

const isSqlite = (p: ConnectionProfile | undefined) => p?.kind === "sqlite";
const isMysql = (p: ConnectionProfile | undefined) => p?.kind === "mysql";
const MYSQL_DATABASES = ["fx", "shop"];
const isMongo = (p: ConnectionProfile | undefined) => p?.kind === "mongodb";
const MONGO_VERSION = "MongoDB 8.0.34";
const MONGO_DATABASES = ["fx", "fx_other"];
const mongoTrees: Record<string, SchemaTree> = {
  fx: { tables: ["users", "orders", "typed", "events", "sessions"], views: ["active_users"], functions: [] },
  fx_other: { tables: ["archive", "logs"], views: [], functions: [] },
};
/** The profile the mock window is connected to. */
let connected: ConnectionProfile | undefined;

const tree: SchemaTree = {
  tables: ["users", "orders", "order_items", "products", "customers", "invoices", "payments", "sessions", "audit_log"],
  views: ["active_users", "monthly_revenue"],
  functions: [
    { name: "calc_tax", signature: "numeric" },
    { name: "refresh_stats", signature: "" },
  ],
};

const mysqlRoutines: SchemaTree["functions"] = [
  { name: "calc_tax", signature: "amount decimal(10,2)", kind: "function" },
  { name: "two_sets", signature: "IN n int, OUT total int", kind: "procedure" },
];

const col = (name: string, dataType: string, extra: Partial<ColumnInfo> = {}): ColumnInfo => ({
  name,
  dataType,
  fullType: dataType,
  castType: dataType,
  nullable: true,
  default: null,
  isPrimaryKey: false,
  readOnly: false,
  ...extra,
});

const columns: ColumnInfo[] = [
  col("id", "int4", { nullable: false, isPrimaryKey: true, default: "nextval('users_id_seq')" }),
  col("name", "varchar", { nullable: false }),
  col("email", "varchar"),
  col("status", "varchar"),
  col("plan", "varchar"),
  col("created_at", "timestamptz"),
];

const FIRST = ["Anna", "Joanna", "Hannah", "Annette", "Shannon", "Anneke", "Susanna", "Brianna", "Johanna", "Leanne"];
const LAST = ["Kowalski", "Lee", "Brooks", "Dubois", "Ortiz", "de Vries", "Rossi", "Chen", "Berg", "Walsh"];
const PLANS = ["pro", "free", "team", null];

const rows: (string | null)[][] = Array.from({ length: 17 }, (_, i) => {
  const first = FIRST[i % FIRST.length];
  const last = LAST[(i * 3) % LAST.length];
  const month = String((i % 9) + 1).padStart(2, "0");
  return [
    String(1003 + i * 37),
    `${first} ${last}`,
    `${first}.${last}`.toLowerCase().replace(/ /g, "") + "@example.com",
    "active",
    PLANS[i % PLANS.length],
    `2025-${month}-1${i % 10} 09:${String(10 + i).padStart(2, "0")}`,
  ];
});

const structure: TableStructure = {
  columns,
  primaryKey: ["id"],
  relation: "table",
  editing: { kind: "allowed" },
  indexes: [
    { name: "users_pkey", columns: ["id"], unique: true, primary: true },
    { name: "users_email_key", columns: ["email"], unique: true, primary: false },
    { name: "users_lower_name_idx", columns: ["lower((name)::text)"], unique: false, primary: false },
    { name: "users_status_idx", columns: ["status"], unique: false, primary: false, included: ["plan", "created_at"] },
  ],
  foreignKeys: [],
};

/** MongoDB's `typed`: one field per BSON type, with the cell codes the core sends (`x` is not editable). */
const typedColumns: ColumnInfo[] = [
  col("_id", "objectId", { nullable: false, isPrimaryKey: true, readOnly: true }),
  col("count", "int32"),
  col("big", "int64"),
  col("ratio", "double"),
  col("price", "decimal"),
  col("name", "string"),
  col("active", "bool"),
  col("created", "date"),
  col("address", "object"),
  col("tags", "array"),
  col("note", "string", { fullType: "string, null" }),
  col("blob", "binary"),
];
const typedRows: (string | null)[][] = [
  ["65a1b2c3d4e5f60718293a4b", "30", "9007199254740993", "1.0", "1.10", "Anna", "true", "2024-01-02T03:04:05.678Z", '{"z":1,"a":{"$numberLong":"2"}}', '["a","b"]', null, '{"$binary":{"base64":"AQID","subType":"00"}}'],
  ["65a1b2c3d4e5f60718293a4c", "-7", "12", "1e+300", "0.5", "Bo", "false", "1969-12-31T23:59:59Z", '{"city":"Oslo"}', "[]", "hello", '{"$binary":{"base64":"","subType":"04"}}'],
  ["65a1b2c3d4e5f60718293a4d", "0", "-1", "NaN", "100", "Cy", "true", '{"$date":{"$numberLong":"253402300800000"}}', "{}", "[1,2.5]", null, '{"$binary":{"base64":"/w==","subType":"00"}}'],
];
const typedCellTypes = ["oilrmtydjjnx", "oilrmtydjjtx", "oilrmtydjjnx"];
const mongoUserColumns: ColumnInfo[] = [
  col("_id", "objectId", { nullable: false, isPrimaryKey: true, readOnly: true }),
  col("name", "string"),
  col("email", "string"),
  col("age", "int32", { fullType: "int32, int64" }),
  col("plan", "string", { fullType: "string, null" }),
  col("created", "date"),
];
const mongoUserRows: (string | null)[][] = rows.map((r, i) => [
  `65a1b2c3d4e5f6071829${(0x3a00 + i).toString(16)}`,
  r[1],
  r[2],
  String(24 + ((i * 7) % 30)),
  r[4],
  `2025-${String((i % 9) + 1).padStart(2, "0")}-1${i % 10}T09:${String(10 + i).padStart(2, "0")}:00Z`,
]);
const mongoUserCellTypes = mongoUserRows.map((r, i) => `ott${i % 5 === 4 ? "l" : "i"}${r[4] === null ? "n" : "t"}d`);

const mongoPage = (req: PageRequest): TablePage => {
  const typed = req.table.name === "typed";
  const isView = req.table.name === "active_users";
  const cols = typed ? typedColumns : mongoUserColumns;
  const all = typed ? typedRows : mongoUserRows;
  const types = typed ? typedCellTypes : mongoUserCellTypes;
  const n = req.filter ? Math.min(all.length, 2) : all.length;
  return {
    columns: cols,
    primaryKey: ["_id"],
    editing: { kind: isView ? "view" : "allowed" },
    rows: all.slice(0, n),
    total: { kind: "estimated", value: typed ? all.length : 1284 },
    matches: req.filter ? { kind: "exact", value: n } : null,
    elapsedMs: 9,
    cellTypes: types.slice(0, n),
  };
};

/** A mongosh-like literal for the mock preview: numbers bare, an `_id` as ObjectId, the rest quoted. */
const mongoLiteral = (v: string | null, field?: string) =>
  v === null ? "null" : field === "_id" && /^[0-9a-f]{24}$/.test(v) ? `ObjectId("${v}")` : /^-?\d+(\.\d+)?$/.test(v) ? v : JSON.stringify(v);

function mongoPlan(set: ChangeSet): { previews: string[]; failsAt: number | null } {
  let failsAt: number | null = null;
  const coll = `db.getCollection(${JSON.stringify(set.table.name)})`;
  const doc = (values: Record<string, string | null>) =>
    `{ ${Object.entries(values).map(([k, v]) => `${k}: ${mongoLiteral(v, k)}`).join(", ")} }`;
  const previews = set.changes.map((c, i) => {
    const filter = c.kind === "insert" ? "" : `{ _id: ${mongoLiteral(c.key[0], "_id")} }`;
    if (c.kind !== "delete" && failsAt === null && Object.values(c.values).includes("fail")) failsAt = i;
    if (c.kind === "delete") return `${coll}.deleteOne(${filter})`;
    if (c.kind === "insert") return `${coll}.insertOne(${doc(c.values)})`;
    return `${coll}.updateOne(${filter}, { $set: ${doc(c.values)} })`;
  });
  return { previews, failsAt };
}

const ORDERS_SQL = `-- Orders per active user since a date
SELECT u.id, u.name, count(o.id) AS orders
FROM users u
LEFT JOIN orders o ON o.user_id = u.id
WHERE u.status = :status
  AND o.created_at > :since
GROUP BY u.id, u.name
ORDER BY orders DESC
LIMIT :limit;`;

let savedQueries: SavedQuery[] = [
  { id: "sq1", connectionId: "m1", name: "Orders per user since", sql: ORDERS_SQL, lastParams: { status: "active", since: "2026-10-01", limit: "20" }, updatedAt: "2026-11-01T09:00:00.000Z" },
  { id: "sq2", connectionId: "m1", name: "Failed payments today", sql: "SELECT id, user_id, amount, failure_reason\nFROM payments\nWHERE status = 'failed'\n  AND created_at >= current_date\nORDER BY created_at DESC;", lastParams: {}, updatedAt: "2026-11-01T09:00:00.000Z" },
  { id: "sq3", connectionId: "m1", name: "Slow sessions", sql: "SELECT id, user_id, started_at, duration_ms\nFROM sessions\nWHERE duration_ms > 5000\nORDER BY duration_ms DESC\nLIMIT 50;", lastParams: {}, updatedAt: "2026-11-01T09:00:00.000Z" },
];

const mockClock = (daysAgo: number, h: number, m: number) => {
  const d = new Date();
  return new Date(d.getFullYear(), d.getMonth(), d.getDate() - daysAgo, h, m, 5).getTime();
};
const histEntry = (id: number, sql: string, verb: string, ok: boolean, summary: string, startedAt: number, elapsedMs: number, params: Record<string, string | null> = {}): HistoryEntry =>
  ({ id, sql, params, verb, ok, summary, startedAt, elapsedMs });
const history: HistoryEntry[] = [
  histEntry(8, "SELECT * FROM users LIMIT 20;", "SELECT", true, "20 rows", mockClock(0, 14, 32), 18),
  histEntry(7, "UPDATE users SET plan = 'pro' WHERE id = 1003;", "UPDATE", true, "1 row affected", mockClock(0, 11, 5), 4),
  histEntry(6, "SELECT nope FROM users;", "SELECT", false, 'column "nope" does not exist', mockClock(0, 9, 47), 3),
  { ...histEntry(5, "SELECT o.id, o.total\nFROM orders o\nWHERE o.user_id = :user_id;", "SELECT", true, "12 rows", mockClock(1, 17, 20), 11, { user_id: "1003" }), targetDatabase: "fx_other", targetSchema: "sales" },
  histEntry(4, "DELETE FROM sessions WHERE expires_at < now();", "DELETE", true, "37 rows affected", mockClock(1, 10, 2), 9),
  histEntry(3, "SELECT count(*) FROM orders;", "SELECT", true, "1 row", mockClock(2, 16, 41), 6),
  histEntry(2, "INSERT INTO plans (name) VALUES ('team');", "INSERT", true, "1 row affected", mockClock(2, 13, 15), 5),
  histEntry(1, "CREATE INDEX users_email_idx ON users (email);", "CREATE", true, "Done", mockClock(2, 8, 30), 42),
];

let tabState: string | null = null;

const delay = <T>(value: T, ms = 120) => new Promise<T>((r) => setTimeout(() => r(value), ms));

/** Literal rendering for the mock preview; a value `fail` makes apply fail. */
function mockLiteral(v: string | null): string {
  if (v === null) return "NULL";
  return /^-?\d+(\.\d+)?$/.test(v) ? v : `'${v.replace(/'/g, "''")}'`;
}

function mockPlan(set: ChangeSet): { previews: string[]; fails: boolean } {
  let fails = false;
  const where = (key: string[]) => `WHERE id = ${mockLiteral(key[0])}`;
  const previews = set.changes.map((c) => {
    if (c.kind === "delete") return `DELETE FROM ${set.table.name} ${where(c.key)};`;
    const entries = Object.entries(c.values);
    if (entries.some(([, v]) => v === "fail")) fails = true;
    if (c.kind === "insert") {
      return entries.length === 0
        ? `INSERT INTO ${set.table.name} DEFAULT VALUES;`
        : `INSERT INTO ${set.table.name} (${entries.map(([k]) => k).join(", ")}) VALUES (${entries.map(([, v]) => mockLiteral(v)).join(", ")});`;
    }
    return `UPDATE ${set.table.name} SET ${entries.map(([k, v]) => `${k} = ${mockLiteral(v)}`).join(", ")} ${where(c.key)};`;
  });
  return { previews, fails };
}

function fetchPage(req: PageRequest): Promise<TablePage> {
  if (req.filter && req.filter.includes("nope")) {
    const position = req.filter.indexOf("nope");
    return Promise.reject({ kind: "filter", message: 'Unknown column "nope"', position });
  }
  const isView = req.table.name === "active_users" || req.table.name === "monthly_revenue";
  return delay({
    columns,
    primaryKey: isView ? [] : ["id"],
    editing: { kind: isView ? "view" : "allowed" },
    rows: req.filter ? rows.slice(0, 12) : rows,
    total: { kind: "exact", value: 1284 },
    matches: req.filter ? { kind: "exact", value: 12 } : null,
    elapsedMs: 12,
    // A view has no fixed order: with more than one page, the grid says so.
    ...(isView ? { unstableOrder: true } : {}),
  });
}

// Rough approximation of Rust's `sqltext::analyze`: splits on `;` outside quotes
// and comments, skips leading comments, and finds `:name` params.
const MONGO_WRITES = ["insertOne", "insertMany", "updateOne", "updateMany", "replaceOne", "deleteOne", "deleteMany", "findOneAndUpdate", "findOneAndReplace", "findOneAndDelete", "bulkWrite", "drop", "createIndex", "dropIndex", "renameCollection"];
/** `collection_fields` for the mock's `users`. */
const MONGO_USER_FIELDS = ["_id", "name", "email", "age", "plan", "created", "address", "address.city", "address.zip"];
/** A MongoDB statement's verb: the first method called on the collection (`find` in `db.users.find(...)`). */
const mongoVerb = (sql: string) => sql.match(/^\s*db(?:\.\w+|\[[^\]]*\])\.(\w+)\(/)?.[1] ?? sql.match(/^\s*db\.getCollection\([^)]*\)\.(\w+)\(/)?.[1] ?? "";

function mockAnalyze(sql: string): SqlAnalysis {
  if (isMongo(connected)) {
    // The mock treats the whole text as one statement.
    const from = sql.length - sql.trimStart().length;
    const to = sql.trimEnd().length;
    if (to <= from) return { statements: [], params: [] };
    const verb = mongoVerb(sql);
    return { statements: [{ from, to, kind: MONGO_WRITES.includes(verb) ? "write" : "read", verb }], params: [] };
  }
  const statements: StatementSpan[] = [];
  const push = (start: number, end: number) => {
    // Move `start` past whitespace and comments.
    let from = start;
    for (;;) {
      while (from < end && /\s/.test(sql[from])) from++;
      if (sql.startsWith("--", from)) {
        const nl = sql.indexOf("\n", from);
        from = nl < 0 || nl > end ? end : nl;
      } else if (sql.startsWith("/*", from)) {
        const close = sql.indexOf("*/", from + 2);
        from = close < 0 || close + 2 > end ? end : close + 2;
      } else break;
    }
    let to = end;
    while (to > from && /\s/.test(sql[to - 1])) to--;
    if (to <= from) return;
    const verb = sql.slice(from, to).match(/^\(*\s*([A-Za-z]+)/)?.[1]?.toUpperCase() ?? "";
    const read = ["SELECT", "WITH", "VALUES", "TABLE", "SHOW", "EXPLAIN", "BEGIN", "COMMIT", "ROLLBACK"].includes(verb);
    statements.push({ from, to, kind: read ? "read" : "write", verb });
  };
  let quote: string | null = null;
  let start = 0;
  for (let i = 0; i < sql.length; i++) {
    const c = sql[i];
    if (quote) {
      if (c === quote) quote = null;
    } else if (sql.startsWith("--", i)) {
      const nl = sql.indexOf("\n", i);
      i = nl < 0 ? sql.length : nl;
    } else if (c === "'" || c === '"') {
      quote = c;
    } else if (c === ";") {
      push(start, i);
      start = i + 1;
    }
  }
  push(start, sql.length);
  const params = [...new Set([...sql.matchAll(/(?<![:\w\])])[:]([A-Za-z_]\w*)/g)].map((m) => m[1]))];
  return { statements, params };
}

const MOCK_SELECT = "select id, name, count(*) as orders from customers group by id, name order by orders desc";
const MOCK_UPDATE = "update customers set tier = 'gold' where orders > 10";

const MOCK_ROWS: [number, string, number][] = [
  [1077, "Hannah Brooks", 42],
  [1003, "Anna Kowalski", 39],
  [1188, "Anneke de Vries", 35],
  [1262, "Brianna Chen", 31],
  [1336, "Leanne Walsh", 28],
  [1040, "Joanna Lee", 26],
  [1447, "Roxanne Patel", 24],
  [1114, "Annette Dubois", 21],
  [1521, "Marianne Holt", 19],
  [1299, "Johanna Berg", 17],
  [1382, "Susanna Rossi", 15],
  [1209, "Rosanna Kim", 14],
  [1455, "Diana Novak", 12],
  [1066, "Lucianne Martin", 11],
  [1573, "Stefanie Olsen", 9],
  [1128, "Juliana Costa", 8],
  [1490, "Ariana Schmidt", 6],
  [1231, "Elena Ivanova", 5],
  [1604, "Brianne Walker", 3],
  [1015, "Gianna Ferrari", 2],
];

const selectOutcome = (): Outcome => ({
  kind: "rows",
  columns: [
    { name: "id", dataType: "int4" },
    { name: "name", dataType: "varchar" },
    { name: "orders", dataType: "int8" },
  ],
  rows: MOCK_ROWS.map(([id, name, orders]) => [String(id), name, String(orders)]),
  truncated: false,
});

/** Dev-only stand-in for `run_query`: frame 2 (two statements), or frame 4 (one SELECT) with params. */
export async function mockRunQuery(
  _tabId: string,
  req: RunRequest,
  onEvent: (e: RunEvent) => void,
): Promise<RunOutcome> {
  if (isMongo(connected)) {
    const verb = mongoVerb(req.sql);
    const write = MONGO_WRITES.includes(verb);
    const sql = req.sql.trim();
    const result: StatementResult = {
      index: 0, from: 0, to: sql.length, verb, kind: write ? "write" : "read", sql, params: {},
      outcome: write
        ? { kind: "affected", rows: 1 }
        : { kind: "rows", columns: mongoUserColumns.map((c) => ({ name: c.name, dataType: c.dataType })), rows: mongoUserRows, truncated: false, cellTypes: mongoUserCellTypes },
      elapsedMs: 9,
      notices: write ? [{ severity: "info", message: "Inserted 1 document (_id: ObjectId(\"65a1b2c3d4e5f60718293a4b\"))." }] : [],
    };
    onEvent({ type: "started", index: 0, total: 1, from: 0, to: sql.length, verb });
    await delay(undefined, 20);
    onEvent({ type: "finished", result });
    return { kind: "completed", statementCount: 1, startedAt: Date.now(), txStatus: "idle" };
  }
  const withParams = Object.keys(req.params).length > 0;
  const results: StatementResult[] = [
    {
      index: 0, from: 0, to: MOCK_SELECT.length, verb: "SELECT", kind: "read", sql: MOCK_SELECT,
      params: withParams ? req.params : {}, outcome: selectOutcome(), elapsedMs: 18, notices: [],
    },
  ];
  if (!withParams) {
    const from = MOCK_SELECT.length + 2;
    results.push({
      index: 1, from, to: from + MOCK_UPDATE.length, verb: "UPDATE", kind: "write", sql: MOCK_UPDATE,
      params: {}, outcome: { kind: "affected", rows: 37 }, elapsedMs: 4, notices: [],
    });
  }
  for (const result of results) {
    onEvent({ type: "started", index: result.index, total: results.length, from: result.from, to: result.to, verb: result.verb });
    await delay(undefined, 20);
    onEvent({ type: "finished", result });
  }
  const opensTx = req.sql.trim().toLowerCase().startsWith("begin");
  return { kind: "completed", statementCount: results.length, startedAt: Date.now(), txStatus: opensTx ? "open" : "idle" };
}

/** Dev-only stand-in for `export_table` / `export_rows`: two progress events, then done. */
export async function mockExport(onProgress: (p: ExportProgress) => void): Promise<ExportOutcome> {
  onProgress({ rows: 8 });
  await delay(undefined, 150);
  onProgress({ rows: 17 });
  await delay(undefined, 150);
  return { kind: "done", rows: 17, path: "/tmp/users.csv", fileName: "users.csv" };
}

/** Databases on the mock server: schema → table → columns. `app_dev` is the connection's own. */
const mockDatabases: Record<string, Record<string, Record<string, string[]>>> = {
  app_dev: {
    public: {
      users: ["id", "name", "email", "status", "last_login"],
      orders: ["id", "user_id", "total", "created_at"],
    },
    audit: { audit_log: ["id", "actor", "action", "at"] },
  },
  analytics: {
    public: {
      events: ["id", "user_id", "name", "occurred_at"],
      daily_active: ["day", "users"],
    },
    rollups: { monthly_revenue: ["month", "revenue"] },
  },
};

const mockColumns = (database: string | null, schema: string): Record<string, string[]> =>
  mockDatabases[database ?? "app_dev"]?.[schema] ?? {};

export async function mockInvoke(cmd: string, args?: Record<string, unknown>): Promise<unknown> {
  switch (cmd) {
    case "list_connections":
      return delay(connections);
    case "save_connection": {
      const p = args?.profile as ConnectionProfile;
      const saved = { ...p, id: p.id || `m${Date.now()}` };
      connections = [...connections.filter((c) => c.id !== saved.id), saved];
      return delay(saved);
    }
    case "delete_connection":
      connections = connections.filter((c) => c.id !== args?.id);
      return delay(undefined);
    case "test_connection":
      return delay<TestResult>(
        isSqlite(args?.profile as ConnectionProfile)
          ? { latencyMs: 2, serverVersion: "SQLite 3.50.4" }
          : isMongo(args?.profile as ConnectionProfile)
            ? { latencyMs: 4, serverVersion: MONGO_VERSION }
            : { latencyMs: 23, serverVersion: isMysql(args?.profile as ConnectionProfile) ? "MySQL 8.4.13" : "PostgreSQL 17" },
      );
    case "pick_database_file":
      return delay("/Users/me/data/picked.sqlite");
    case "connect": {
      const profile = connections.find((c) => c.id === args?.id) ?? connections[0];
      connected = profile;
      if (isSqlite(profile)) return delay<ConnectedInfo>({ profile, serverVersion: "SQLite 3.50.4", schemas: ["main"], schema: "main" });
      if (isMongo(profile)) {
        const schema = profile.transport === "network" && profile.database ? profile.database : "fx";
        // A standalone server: saves run one change at a time.
        return delay<ConnectedInfo>({ profile, serverVersion: MONGO_VERSION, schemas: MONGO_DATABASES, schema, oneByOneSaves: true });
      }
      if (isMysql(profile)) return delay<ConnectedInfo>({ profile, serverVersion: "MySQL 8.4.13", schemas: MYSQL_DATABASES, schema: profile.transport === "network" && profile.database ? profile.database : "fx" });
      return delay<ConnectedInfo>({ profile, serverVersion: "17", schemas: ["public", "audit"], schema: "public" });
    }
    case "disconnect":
    case "release_tab_state":
      return delay(undefined);
    case "schema_tree":
      // SQLite and MongoDB have no functions.
      if (isMongo(connected)) return delay(mongoTrees[String(args?.schema)] ?? { tables: [], views: [], functions: [] });
      if (isMysql(connected)) return delay<SchemaTree>({ ...tree, functions: mysqlRoutines });
      return delay(isSqlite(connected) ? { ...tree, functions: [] } : tree);
    case "table_structure":
      if (isMongo(connected)) {
        const cols = (args?.table as { name?: string } | undefined)?.name === "typed" ? typedColumns : mongoUserColumns;
        return delay<TableStructure>({
          columns: cols,
          primaryKey: ["_id"],
          relation: "table",
          editing: { kind: "allowed" },
          indexes: [{ name: "_id_", columns: ["_id"], unique: true, primary: true }],
          foreignKeys: [],
        });
      }
      return delay(structure);
    case "fetch_page":
      if (isMongo(connected)) return delay(mongoPage(args?.req as PageRequest));
      return fetchPage(args?.req as PageRequest);
    case "cancel_page":
      return delay(0);
    case "preview_changes":
      if (isMongo(connected)) return delay(mongoPlan(args?.set as ChangeSet).previews);
      return delay(mockPlan(args?.set as ChangeSet).previews);
    case "apply_changes": {
      if (isMongo(connected)) {
        // A value `fail` stops the save there, as a unique index would; the changes before it stay saved.
        const { previews, failsAt } = mongoPlan(args?.set as ChangeSet);
        const error = 'E11000 duplicate key error collection: fx.users index: email_1 dup key: { email: "fail" }';
        if (failsAt === 0) return Promise.reject({ kind: "query", message: error });
        if (failsAt !== null) {
          return Promise.reject({
            kind: "partlySaved",
            message: `Saved ${failsAt} of ${previews.length} changes, then: ${error} This server has no transactions, so the saved changes stay saved; the rest are still pending.`,
            applied: failsAt,
          });
        }
        return delay<ApplyResult>({ applied: previews.length });
      }
      const plan = mockPlan(args?.set as ChangeSet);
      if (plan.fails) return Promise.reject({ kind: "query", message: "UPDATE id = 1: value too long for type character varying(5)" });
      return delay<ApplyResult>({ applied: plan.previews.length });
    }
    case "analyze_sql":
      return delay(mockAnalyze(String(args?.sql ?? "")));
    case "format_mongo":
      return delay(String(args?.text ?? ""));
    case "list_saved_queries":
      return delay(savedQueries.filter((q) => q.connectionId === "m1").sort((x, y) => x.name.localeCompare(y.name)));
    case "save_query": {
      const q = args?.query as SavedQuery;
      const prev = savedQueries.find((x) => x.id === q.id);
      const saved = { ...q, id: q.id || `sq${Date.now()}`, connectionId: "m1", lastParams: prev?.lastParams ?? q.lastParams, updatedAt: new Date().toISOString() };
      savedQueries = [...savedQueries.filter((x) => x.id !== saved.id), saved];
      return delay(saved);
    }
    case "delete_saved_query":
      savedQueries = savedQueries.filter((q) => q.id !== args?.id);
      return delay(undefined);
    case "list_history":
      return delay(history.slice(0, Number(args?.limit ?? 200)));
    case "load_tab_state":
      return delay(tabState);
    case "save_tab_state":
      tabState = String(args?.json ?? "");
      return delay(undefined);
    case "cancel_query":
    case "cancel_export":
    case "reveal_export":
    case "close_query_session":
    case "open_window":
    case "report_quit_prompt":
    case "quit_continue":
    case "quit_cancel":
      return delay(undefined);
    case "end_transaction":
      return delay("idle");
    case "list_databases":
      return delay(connected && !capabilities(connected.kind).otherDatabases ? [] : Object.keys(mockDatabases).sort());
    case "target_schemas": {
      if (isMysql(connected)) return delay(MYSQL_DATABASES);
      if (connected && !capabilities(connected.kind).otherDatabases) return delay(["main"]);
      const db = mockDatabases[(args?.database as string | null) ?? "app_dev"];
      if (!db) return Promise.reject({ kind: "connection", message: `database "${String(args?.database)}" does not exist` });
      return delay(Object.keys(db).sort());
    }
    case "target_columns":
      return delay(mockColumns((args?.database as string | null) ?? null, String(args?.schema ?? "public")));
    case "collection_fields":
      return delay(args?.collection === "users" ? MONGO_USER_FIELDS : []);
    default:
      throw new Error(`devMock: unknown command ${cmd}`);
  }
}
