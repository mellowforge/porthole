import { Channel, invoke as tauriInvoke } from "@tauri-apps/api/core";
import type {
  ApplyResult,
  ChangeSet,
  ConnectedInfo,
  ConnectionProfile,
  ExportFormat,
  ExportOutcome,
  ExportProgress,
  HistoryEntry,
  PageRequest,
  ParamValues,
  RunEvent,
  RunOutcome,
  RunRequest,
  SavedQuery,
  SchemaTree,
  SqlAnalysis,
  TablePage,
  TableQuery,
  TableRef,
  ResultColumn,
  TableStructure,
  TestResult,
  TxStatus,
} from "./types";

// Errors from Rust arrive as `AppError` objects; invoke rejects with them unchanged.

// Dev-only: `npm run dev` then open `/?mock` to render the UI with fixture data
// in a plain browser. `import.meta.env.DEV` is false in production builds, so the
// branch and the dynamic import are dropped; tests never set `?mock`.
const useDevMock =
  import.meta.env.DEV && import.meta.env.MODE !== "test" && window.location.search.includes("mock");
const invoke: typeof tauriInvoke = useDevMock
  ? <T>(cmd: string, args?: unknown) =>
      import("./devMock").then((m) => m.mockInvoke(cmd, args as Record<string, unknown>) as Promise<T>)
  : tauriInvoke;

export const listConnections = (): Promise<ConnectionProfile[]> => invoke("list_connections");

export const saveConnection = (
  profile: ConnectionProfile,
  password?: string | null,
): Promise<ConnectionProfile> => invoke("save_connection", { profile, password });

export const deleteConnection = (id: string): Promise<void> => invoke("delete_connection", { id });

/** Opens the file dialog for a SQLite database file; `null` when it was closed without a choice. */
export const pickDatabaseFile = (): Promise<string | null> => invoke("pick_database_file");

export const testConnection = (
  profile: ConnectionProfile,
  password?: string | null,
): Promise<TestResult> => invoke("test_connection", { profile, password });

export const connect = (id: string): Promise<ConnectedInfo> => invoke("connect", { id });

export const disconnect = (): Promise<void> => invoke("disconnect");

export const schemaTree = (schema: string): Promise<SchemaTree> =>
  invoke("schema_tree", { schema });

export const tableStructure = (table: TableRef): Promise<TableStructure> =>
  invoke("table_structure", { table });

export const fetchPage = (req: PageRequest): Promise<TablePage> => invoke("fetch_page", { req });

/** Stop for a slow table page: cancels the page queries running on `table`; resolves to how many the server cancelled. */
export const cancelPage = (table: TableRef): Promise<number> => invoke("cancel_page", { table });

export const previewChanges = (set: ChangeSet): Promise<string[]> => invoke("preview_changes", { set });

export const applyChanges = (set: ChangeSet): Promise<ApplyResult> => invoke("apply_changes", { set });

export const analyzeSql = (sql: string): Promise<SqlAnalysis> => invoke("analyze_sql", { sql });

/** MongoDB Format: `text` reprinted; text that doesn't lex rejects with the lexer's message. */
export const formatMongo = (text: string): Promise<string> => invoke("format_mongo", { text });

export const listSavedQueries = (): Promise<SavedQuery[]> => invoke("list_saved_queries");

export const saveQuery = (query: SavedQuery): Promise<SavedQuery> => invoke("save_query", { query });

export const deleteSavedQuery = (id: string): Promise<void> => invoke("delete_saved_query", { id });

export const listHistory = (limit: number): Promise<HistoryEntry[]> => invoke("list_history", { limit });

export const loadTabState = (): Promise<string | null> => invoke("load_tab_state");

export const saveTabState = (json: string): Promise<void> => invoke("save_tab_state", { json });

/** This window left the workspace: another window on the same connection saves the tabs from now on. */
export const releaseTabState = (): Promise<void> => invoke("release_tab_state");

export const runQuery = (
  tabId: string,
  req: RunRequest,
  onEvent: (e: RunEvent) => void,
): Promise<RunOutcome> => {
  if (useDevMock) {
    return import("./devMock").then((m) => m.mockRunQuery(tabId, req, onEvent));
  }
  const ch = new Channel<RunEvent>();
  ch.onmessage = onEvent;
  return tauriInvoke("run_query", { tabId, req, onEvent: ch });
};

/** Exports every row of the table (with the grid's filter and sort); Rust shows the save dialog. */
export const exportTable = (
  id: string,
  query: TableQuery,
  format: ExportFormat,
  onProgress: (p: ExportProgress) => void,
): Promise<ExportOutcome> => {
  if (useDevMock) {
    return import("./devMock").then((m) => m.mockExport(onProgress));
  }
  const ch = new Channel<ExportProgress>();
  ch.onmessage = onProgress;
  return tauriInvoke("export_table", { id, query, format, onProgress: ch });
};

/** Exports rows the UI already holds; `name` is the default file name without extension. `cellTypes`
 * (the result's, when it has them) is sent only when given. */
export const exportRows = (
  id: string,
  name: string,
  columns: ResultColumn[],
  rows: (string | null)[][],
  format: ExportFormat,
  onProgress: (p: ExportProgress) => void,
  cellTypes?: string[],
): Promise<ExportOutcome> => {
  if (useDevMock) {
    return import("./devMock").then((m) => m.mockExport(onProgress));
  }
  const ch = new Channel<ExportProgress>();
  ch.onmessage = onProgress;
  return tauriInvoke("export_rows", { id, name, columns, rows, format, onProgress: ch, ...(cellTypes ? { cellTypes } : {}) });
};

/** Where a query result's statement runs again for an export: the tab, and the database and
 * schema its run used (`null` = the connection's own / the session's search_path). */
export interface QueryExportTarget {
  tabId: string;
  database: string | null;
  schema: string | null;
}

/** Exports every row of a read result by running its statement again on the tab's session. */
export const exportQuery = (
  id: string,
  name: string,
  target: QueryExportTarget,
  sql: string,
  params: ParamValues,
  format: ExportFormat,
  onProgress: (p: ExportProgress) => void,
): Promise<ExportOutcome> => {
  if (useDevMock) {
    return import("./devMock").then((m) => m.mockExport(onProgress));
  }
  const ch = new Channel<ExportProgress>();
  ch.onmessage = onProgress;
  return tauriInvoke("export_query", {
    id, name, tabId: target.tabId, sql, params, database: target.database, schema: target.schema, format, onProgress: ch,
  });
};

export const cancelExport = (id: string): Promise<void> => invoke("cancel_export", { id });

/** Shows a finished export in Finder. */
export const revealExport = (path: string): Promise<void> => invoke("reveal_export", { path });

export const cancelQuery = (tabId: string): Promise<void> => invoke("cancel_query", { tabId });

export const endTransaction = (tabId: string, commit: boolean): Promise<TxStatus> =>
  invoke("end_transaction", { tabId, commit });

export const closeQuerySession = (tabId: string): Promise<void> =>
  invoke("close_query_session", { tabId });

/** Databases on the connection's server the user may connect to. */
export const listDatabases = (): Promise<string[]> => invoke("list_databases");

/** Schemas of `database` (`null` = the connection's own), for the target chip. */
export const targetSchemas = (database: string | null): Promise<string[]> =>
  invoke("target_schemas", { database });

/** Completion columns of `schema` in `database` (`null` = the connection's own). */
export const targetColumns = (database: string | null, schema: string): Promise<Record<string, string[]>> =>
  invoke("target_columns", { database, schema });

/** Completion field paths (`address.city`) of a MongoDB collection in database `schema`, from its sample. */
export const collectionFields = (schema: string, collection: string): Promise<string[]> =>
  invoke("collection_fields", { schema, collection });

/** Opens another window, on the Connections screen. */
export const openWindow = (): Promise<void> => invoke("open_window");
/** This window has dealt with the quit: Rust asks the next window, or exits. */
export const quitContinue = (): Promise<void> => invoke("quit_continue");
/** Tells Rust whether quitting this window would ask anything (see `quitNeedsAsk`). */
export const reportQuitPrompt = (asks: boolean): Promise<void> => invoke("report_quit_prompt", { asks });
/** The user cancelled (or turned to something else): the quit sequence ends. */
export const quitCancel = (): Promise<void> => invoke("quit_cancel");
