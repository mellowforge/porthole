export type DbKind = "postgres" | "mysql" | "sqlite" | "sqlserver" | "mongodb";
export type Environment = "local" | "staging" | "production";
export type SslMode = "disable" | "prefer" | "require" | "verify-ca" | "verify-full";

export interface ProfileBase {
  id: string;
  name: string;
  kind: DbKind;
  defaultSchema?: string | null;
  environment: Environment;
  connectTimeoutSecs: number;
  readOnly: boolean;
  startupSql?: string | null;
  /** MongoDB query tabs' statement time limit; absent for every other kind, and for no limit. */
  statementTimeoutSecs?: number;
}

export interface NetworkProfile {
  transport: "network";
  host: string;
  port: number;
  user: string;
  database: string;
  sslMode: SslMode;
  sslRootCert?: string | null;
  /** MongoDB's authentication database. */
  authSource?: string | null;
}

export interface FileProfile {
  transport: "file";
  filePath: string;
}

export interface UriProfile {
  transport: "uri";
  uri: string;
}

export type ConnectionProfile = ProfileBase & (NetworkProfile | FileProfile | UriProfile);

export interface TableRef {
  schema: string;
  name: string;
}

export interface FunctionInfo {
  name: string;
  signature: string;
  /** Absent for Postgres and SQLite: every routine is a function. */
  kind?: "function" | "procedure";
}

export interface SchemaTree {
  tables: string[];
  views: string[];
  functions: FunctionInfo[];
}

export interface ColumnInfo {
  name: string;
  dataType: string;
  fullType: string;
  /** The type parameters are cast to (no typmod). */
  castType: string;
  nullable: boolean;
  default: string | null;
  isPrimaryKey: boolean;
  /** Identity ALWAYS or stored generated: the database fills it in. */
  readOnly: boolean;
}

export type RelationKind = "table" | "partitioned" | "foreign" | "view" | "materialized";

export type Editing = { kind: "allowed" | "noPrimaryKey" | "view" | "floatKey" };

export interface IndexInfo {
  name: string;
  columns: string[];
  included?: string[];
  unique: boolean;
  primary: boolean;
}

export interface ForeignKeyInfo {
  name: string;
  columns: string[];
  refTable: TableRef;
  refColumns: string[];
}

export interface TableStructure {
  columns: ColumnInfo[];
  /** Primary-key column names in key order. */
  primaryKey: string[];
  relation: RelationKind;
  editing: Editing;
  indexes: IndexInfo[];
  foreignKeys: ForeignKeyInfo[];
}

export interface SortSpec {
  column: string;
  descending: boolean;
}

/** A table with the grid's filter and sort, for exporting every matching row. */
export interface TableQuery {
  table: TableRef;
  filter?: string | null;
  sort?: SortSpec | null;
}

export interface PageRequest {
  table: TableRef;
  filter?: string | null;
  sort?: SortSpec | null;
  /** 0-based page index. */
  page: number;
  pageSize: number;
}

export type RowCount =
  | { kind: "exact" | "estimated"; value: number }
  | { kind: "unknown" };

/** A cell's own storage type, for databases whose cells carry one: SQLite and MySQL send null,
 * integer, real, text, blob; MongoDB adds int64 (l), decimal (m), bool (y), ObjectId (o), date (d),
 * document or array (j) and any other BSON type (x, not editable). Postgres never sends these. */
export type CellTypeCode = "n" | "i" | "r" | "t" | "b" | "l" | "m" | "y" | "o" | "d" | "j" | "x";

export interface TablePage {
  columns: ColumnInfo[];
  primaryKey: string[];
  editing: Editing;
  rows: (string | null)[][];
  total: RowCount;
  matches: RowCount | null;
  elapsedMs: number;
  /** One `CellTypeCode` per column for each row (read with `cellType` in `table/cells.ts`); absent for Postgres. */
  cellTypes?: string[];
  /** True when the page query has no tie-break: rows equal on the sort (all rows without a sort) have no fixed order. Absent when false. */
  unstableOrder?: boolean;
}

export interface TestResult {
  latencyMs: number;
  serverVersion: string;
}

export interface ConnectedInfo {
  profile: ConnectionProfile;
  serverVersion: string;
  schemas: string[];
  schema: string;
  /** True where a save runs one change at a time (a server without transactions). */
  oneByOneSaves?: boolean;
}

export type AppErrorKind =
  | "connection"
  | "query"
  | "filter"
  | "notFound"
  | "storage"
  | "secret"
  | "unsupported"
  /** The save may or may not have been applied (the connection failed during COMMIT). */
  | "uncertain"
  /** A save without transactions (a standalone MongoDB) stopped part-way: the first `applied`
   * changes, in the order sent, stayed saved. */
  | "partlySaved";

export interface AppError {
  kind: AppErrorKind;
  message: string;
  position?: number;
  /** `partlySaved` only. */
  applied?: number;
  /** `partlySaved` only, and only when true: the write after the saved changes got no answer, so
   * it may or may not have been applied. */
  uncertain?: boolean;
}

export type StatementKind = "read" | "write";

/** `from`/`to` are 0-based UTF-16 offsets into the analyzed text; `to` is exclusive. */
export interface StatementSpan {
  from: number;
  to: number;
  kind: StatementKind;
  verb: string;
}

export interface SqlAnalysis {
  statements: StatementSpan[];
  params: string[];
}

/** Query parameter values by name. `null` binds NULL. */
export type ParamValues = Record<string, string | null>;

export interface SavedQuery {
  id: string;
  connectionId: string;
  name: string;
  sql: string;
  lastParams: ParamValues;
  updatedAt: string;
}

export interface NewHistoryEntry {
  sql: string;
  params: ParamValues;
  verb: string;
  ok: boolean;
  summary: string;
  /** Unix milliseconds. */
  startedAt: number;
  elapsedMs: number;
  /** The database the query ran on, when not the connection's own. */
  targetDatabase?: string;
  /** The schema (a database, on MySQL and MongoDB) the query ran on. */
  targetSchema?: string;
}

export interface HistoryEntry extends NewHistoryEntry {
  id: number;
}

export interface ResultColumn {
  name: string;
  /** Postgres type name, e.g. `int4`. */
  dataType: string;
}

export interface Notice {
  severity: string;
  message: string;
}

export type TxStatus = "idle" | "open" | "failed" | "unknown";

/** Offsets are 0-based UTF-16 code units into `RunRequest.sql`. */
export type RunTarget =
  | { kind: "all" }
  | { kind: "range"; from: number; to: number }
  | { kind: "cursor"; at: number };

export interface RunRequest {
  sql: string;
  target: RunTarget;
  params: ParamValues;
  schema: string | null;
  confirmed: boolean;
  fetchAll: boolean;
  savedQueryId: string | null;
  /** The database to run on; `null` is the connection's own. */
  database: string | null;
}

export type Outcome =
  | {
      kind: "rows";
      columns: ResultColumn[];
      rows: (string | null)[][];
      truncated: boolean;
      /** As in `TablePage.cellTypes`. */
      cellTypes?: string[];
    }
  | { kind: "affected"; rows: number }
  | { kind: "done" }
  /** `position`: 0-based UTF-16 offset in `RunRequest.sql`. */
  | { kind: "error"; message: string; code: string | null; position: number | null }
  | { kind: "canceled" }
  | { kind: "skipped" };

export interface StatementResult {
  index: number;
  from: number;
  to: number;
  verb: string;
  kind: StatementKind;
  /** As written, with `:names`. */
  sql: string;
  /** Only the names this statement uses. */
  params: ParamValues;
  outcome: Outcome;
  elapsedMs: number;
  notices: Notice[];
}

export type RunEvent =
  | { type: "started"; index: number; total: number; from: number; to: number; verb: string }
  | { type: "finished"; result: StatementResult };

export interface PendingWrite {
  verb: string;
  preview: string;
}

/** Results travel only in `finished` events. */
export type RunOutcome =
  | { kind: "completed"; statementCount: number; startedAt: number; txStatus: TxStatus }
  | { kind: "needsConfirmation"; statements: PendingWrite[] };

/** Column name to new text value (`null` is NULL). */
export type CellValues = Record<string, string | null>;

/** The original cell types of a change's key columns (and an update's edited columns), by column.
 * Sent only when the page had `cellTypes`. */
export type OriginalTypes = Record<string, CellTypeCode>;

export type RowChange =
  /** `key` holds the original primary-key values, in primary-key order. */
  | { kind: "update"; key: string[]; values: CellValues; originalTypes?: OriginalTypes }
  /** Only the columns the user set; empty means DEFAULT VALUES. */
  | { kind: "insert"; values: CellValues }
  | { kind: "delete"; key: string[]; originalTypes?: OriginalTypes };

export interface ChangeSet {
  table: TableRef;
  changes: RowChange[];
}

export interface ApplyResult {
  applied: number;
}

export type ExportFormat = "csv" | "json";

/** `dismissed` means the save dialog was closed without choosing a file. */
export type ExportOutcome =
  | { kind: "done"; rows: number; path: string; fileName: string }
  | { kind: "canceled" }
  | { kind: "dismissed" };

export interface ExportProgress {
  rows: number;
}
