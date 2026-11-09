import { create } from "zustand";
import * as api from "../api/tauri";
import { MOCK_MONGO_QUERY_SQL, MOCK_QUERY_SQL } from "../api/mockSql";
import { hasOtherDatabases, hasQueryTabs, schemaIsDatabase, targetName } from "../connections/profile";
import { EDITS_NOT_RESTORED, editsRestored, flushTabState, restoreTabs, TABS_NOT_RESTORED } from "./persist";
import * as pending from "../table/pending";
import type { PendingChanges, RowKey, RowTypes } from "../table/pending";
import type {
  ConnectedInfo,
  ConnectionProfile,
  ExportOutcome,
  PendingWrite,
  RunEvent,
  RunOutcome,
  RunTarget,
  SavedQuery,
  SchemaTree,
  SortSpec,
  StatementResult,
  TableRef,
  TxStatus,
} from "../api/types";

// Mirrors the gate in `api/tauri.ts`: dev server with `?mock`, never in tests or production.
const mockMode =
  import.meta.env.DEV && import.meta.env.MODE !== "test" && window.location.search.includes("mock");

/** The text of an `AppError` (a plain object from Rust), an `Error`, or anything else. */
export const errorMessage = (e: unknown) => (e as { message?: string })?.message ?? String(e);

export const LOST_TX_NOTE = "The connection was lost. Uncommitted changes were rolled back.";
/** Shown when a run, COMMIT or ROLLBACK is asked for while the tab's export holds its session. */
export const EXPORT_RUNNING = "An export is running in this tab. Wait for it to finish, or cancel it.";
/** The workspace notice when File ▸ New Window (menu or palette) fails. */
export const newWindowFailed = (message: string) => `Couldn't open a new window: ${message}`;
/** How long a completed run waits for missing `Finished` events before it settles anyway. */
export const RESULTS_GRACE_MS = 2_000;

export interface TableViewState {
  mode: "data" | "structure";
  filter: string | null;
  sort: SortSpec | null;
}

export interface TableTab {
  kind: "table";
  id: string;
  table: TableRef;
  view: TableViewState;
  /** Unsaved row changes. Restored after a crash; leaving a tab with changes still asks. */
  pending: PendingChanges;
  /** Muted footer note after a save (`Saved 3 changes`), until the next edit or user-triggered fetch. */
  saveNote?: string | null;
  /** `Nothing was saved: …`, the uncertain-commit message, or a partial save's `Saved {k} of {n}
   *  changes, then: …` (MongoDB without transactions), shown in the pending bar until the next edit
   *  or save attempt. */
  saveError?: string | null;
  /** True while a save is in flight; the grid is read-only meanwhile. */
  saving?: boolean;
  /** The last save's COMMIT got no answer and the server could not be asked what became of it.
   *  Until a save succeeds or the changes are gone, saving again first shows the SQL and asks. */
  uncertainSave?: boolean;
  /** Bumped after a save so the view refetches its current page. */
  reloadNonce?: number;
}

export interface QueryRunState {
  status: "idle" | "running" | "confirm";
  results: StatementResult[];
  selected: number;
  startedAt: number | null;
  sqlAtRun: string | null;
  pending: PendingWrite[] | null;
  error: string | null;
  /** From `Completed`; the run is finished once this many results have arrived. */
  statementCount: number | null;
  /** Positions in `results` that were replaced by a Fetch all. */
  fetchedAll?: number[];
  /** The target schema this run used, so Fetch all and Run anyway re-run against the same one. */
  schemaAtRun?: string | null;
  /** The database this run used (`null` = the connection's own), for Fetch all and Run anyway. */
  databaseAtRun?: string | null;
}

export interface QueryTab {
  kind: "query";
  id: string;
  title: string;
  sql: string;
  savedQueryId: string | null;
  savedSql: string | null;
  params: Record<string, string | null>;
  schema: string | null;
  /** The database this tab runs on; `null` is the connection's own. */
  database: string | null;
  run: QueryRunState;
  txStatus: TxStatus;
  note: string | null;
}

export type Tab = TableTab | QueryTab;

const initialRun = (): QueryRunState => ({
  status: "idle",
  results: [],
  selected: 0,
  startedAt: null,
  sqlAtRun: null,
  pending: null,
  error: null,
  statementCount: null,
});

/** `Query N`, where N is 1 + the highest N among open tabs titled `Query N`. */
function nextQueryTitle(tabs: Tab[]): string {
  let max = 0;
  for (const t of tabs) {
    const m = t.kind === "query" ? /^Query (\d+)$/.exec(t.title) : null;
    if (m) max = Math.max(max, Number(m[1]));
  }
  return `Query ${max + 1}`;
}

export interface RunOptions {
  confirmed?: boolean;
  fetchAll?: boolean;
  /** Overrides the editor text; used by Fetch all and by Run anyway. */
  sql?: string;
  /** Exact values to bind (Fetch all re-uses its result's own). Otherwise the SQL is analyzed
   *  right before the run and every parameter it names gets the tab's value, a missing one `""`. */
  params?: Record<string, string | null>;
  /** Overrides the tab's target schema (`null` = the connection's default). */
  schema?: string | null;
  /** Overrides the tab's database (`null` = the connection's own). */
  database?: string | null;
}

/** One value per name; a name without a value is bound as an empty string. */
export function paramsFor(names: string[], values: Record<string, string | null>): Record<string, string | null> {
  return Object.fromEntries(names.map((n) => [n, values[n] === undefined ? "" : values[n]]));
}

/** Latest run per tab, so events from an older run never touch a newer one. */
const runTokens = new Map<string, number>();

/** Cache key of a completion map: `"{database ?? ''}|{schema}"`. */
export const completionKey = (database: string | null, schema: string) => `${database ?? ""}|${schema}`;

/** The schema unqualified names resolve to in a tab with no schema picked. Another database's own
 *  default is not known without a round trip, so it is assumed to be `public`. SQLite looks in
 *  `main` first. */
export const defaultSchemaFor = (database: string | null, active: ConnectedInfo | null) => {
  if (active?.profile && !hasOtherDatabases(active.profile)) return schemaIsDatabase(active.profile.kind) ? (active.schema ?? "") : "main";
  return database === null || database === (active?.profile && targetName(active.profile)) ? (active?.schema ?? "public") : "public";
};

/** The database a query tab runs on (`null` = the connection's own). Always `null` on a connection
 *  with no other databases, whatever the tab names (a tab state saved by an earlier build may). */
export const tabDatabase = (database: string | null, active: ConnectedInfo | null): string | null =>
  active?.profile && !hasOtherDatabases(active.profile) ? null : database;

/** The schema a query tab runs (and exports) on. Where the schema is a database (MySQL), a tab
 *  with none runs on the window's current one, the database its chip names; elsewhere `null`
 *  stays the connection's default. */
export const runSchema = (schema: string | null, active: ConnectedInfo | null): string | null =>
  // A server with no databases (and a profile naming none) leaves the window on "": no database, not `USE ``.
  schema === null && active?.profile && schemaIsDatabase(active.profile.kind) ? active.schema || null : schema;

/** Statements that can change what the sidebar lists. */
const CATALOG_VERBS = new Set(["CREATE", "DROP", "ALTER", "RENAME"]);
/** Query tabs that ran such a statement since the sidebar last refreshed for them. The sidebar
 *  refreshes once the tab's transaction is over, as only then do other sessions see the change. */
const catalogDirty = new Set<string>();
/** Bumped by each refresh and by connecting; only the latest refresh applies its result. */
let refreshGen = 0;
/** Refreshes the sidebar when `tabId` changed the catalog and its transaction is now over. */
function refreshAfter(tabId: string, txStatus: TxStatus) {
  if (txStatus !== "idle" || !catalogDirty.delete(tabId)) return;
  void useApp.getState().refreshCatalog();
}

/** Loads in flight, by key, so two tabs asking for the same map make one call (each call to another
 *  database opens a server connection). Replaced on connect, which also drops stale results. */
let completionLoads = new Map<string, Promise<void>>();

/** A MongoDB database's collections and views, each with no columns. */
async function collectionNames(schema: string): Promise<Record<string, string[]>> {
  const { tables, views } = await api.schemaTree(schema);
  return Object.fromEntries([...tables, ...views].map((n) => [n, []]));
}

/** Autocomplete is non-critical: a failure is logged, leaves the key missing and never blocks the caller. */
function ensureCompletion(database: string | null, schema: string): Promise<void> {
  const key = completionKey(database, schema);
  if (useApp.getState().completionCache[key]) return Promise.resolve();
  const loads = completionLoads;
  const running = loads.get(key);
  if (running) return running;
  const load = (async () => {
    try {
      // MongoDB's collections have no column list (its schema_columns is empty): complete the names alone.
      const columns = useApp.getState().active?.profile?.kind === "mongodb" ? await collectionNames(schema) : await api.targetColumns(database, schema);
      if (loads === completionLoads) useApp.setState((s) => ({ completionCache: { ...s.completionCache, [key]: columns } }));
    } catch (e) {
      // Not cached: a later ask (remount, retarget, schema switch) retries.
      console.error(useApp.getState().active?.profile?.kind === "mongodb" ? "schema_tree failed" : "target_columns failed", e);
    } finally {
      loads.delete(key);
    }
  })();
  loads.set(key, load);
  return load;
}

/** MongoDB field paths for key completion, by `${schema}|${collection}`: sampled once per collection,
 *  until the next connect, schema switch or catalog refresh replaces the map. */
let fieldCache = new Map<string, string[]>();
/** Field loads in flight, by the same key, so two asks make one call. */
let fieldLoads = new Map<string, Promise<string[]>>();

/** Drops every cached field list; a load still in flight then caches into the old map, unseen. */
function clearFields() {
  fieldCache = new Map();
  fieldLoads = new Map();
}

/** The field paths of `collection` in MongoDB database `schema`, for completion. Non-critical, as
 *  `ensureCompletion`: a failure is logged, gives `[]` and isn't cached, so the next ask retries. */
export function loadFields(schema: string, collection: string): Promise<string[]> {
  const key = `${schema}|${collection}`;
  const cached = fieldCache.get(key);
  if (cached) return Promise.resolve(cached);
  const cache = fieldCache;
  const loads = fieldLoads;
  const running = loads.get(key);
  if (running) return running;
  const load = (async () => {
    try {
      const fields = await api.collectionFields(schema, collection);
      cache.set(key, fields);
      return fields;
    } catch (e) {
      console.error("collection_fields failed", e);
      return [];
    } finally {
      loads.delete(key);
    }
  })();
  loads.set(key, load);
  return load;
}

export type LeaveReason = "closeTab" | "switch" | "edit" | "window" | "quit" | "reload" | "retarget";
export type LeaveChoice = "commit" | "rollback" | "save" | "discard" | "cancel";
export interface LeaveRequest {
  reason: LeaveReason;
  tabIds: string[];
  then: () => void;
  /** Runs when the flow ends without going ahead: the user cancels, or another request replaces
   *  this one (quit / window close take over). Never runs after `then`. */
  onCancel?: () => void;
  /** `waiting`: a listed tab's save, or a Commit / Roll back of a listed query tab, is still running
   *  (Cancel only); `edits`: unsaved grid edits (Save / Discard); `tx`: open transactions
   *  (Commit / Roll back). */
  stage: "waiting" | "edits" | "tx";
}

/** All tab ids: what Edit / Switch connection, window close and quit hand to `requestLeave`. */
export const leaveTabIds = (tabs: Tab[]) => tabs.map((t) => t.id);
/** Table tabs among `tabIds` that have unsaved edits. */
export const tabsWithEdits = (tabs: Tab[], tabIds: string[]): TableTab[] =>
  tabs.filter((t): t is TableTab => t.kind === "table" && tabIds.includes(t.id) && pending.counts(t.pending).total > 0);

/** Open, failed, or unknown (the session could not tell): leaving such a tab must ask first. */
export const txNeedsAsk = (s: TxStatus) => s === "open" || s === "failed" || s === "unknown";
const hasOpenTx = (t: Tab): t is QueryTab => t.kind === "query" && txNeedsAsk(t.txStatus);

/** Whether quitting this window would ask anything or wait: a prompt is up, a grid has unsaved
 *  edits or a save running, or a query tab's transaction is open (or unknown). Reported to Rust so a
 *  system quit (Dock ▸ Quit, logout) with nothing to ask can end the app at once. */
export const quitNeedsAsk = (s: Pick<AppState, "screen" | "tabs" | "leave">): boolean =>
  s.leave !== null ||
  (s.screen === "workspace" &&
    (tabsWithEdits(s.tabs, leaveTabIds(s.tabs)).length > 0 ||
      s.tabs.some(hasOpenTx) ||
      s.tabs.some((t) => t.kind === "table" && t.saving === true)));

export interface AppState {
  screen: "connections" | "workspace";
  connections: ConnectionProfile[];
  loadError: string | null;
  selectedConnectionId: string | null;
  active: ConnectedInfo | null;
  tree: SchemaTree | null;
  /** Table name → column names for the editor's autocomplete, per `completionKey`; `{}` when unavailable. */
  completionCache: Record<string, Record<string, string[]>>;
  /** Loads the map for `database` (`null` = the connection's own) and `schema` unless cached or loading. */
  ensureCompletion: (database: string | null, schema: string) => Promise<void>;
  /** Points a query tab at another database (its schema resets); asks first when its transaction is open. */
  retargetQueryTab: (tabId: string, database: string | null) => void;
  /** Set when switching schema or refreshing the sidebar fails; shown in the sidebar. */
  schemaError: string | null;
  /** True while `refreshCatalog` runs. */
  catalogRefreshing: boolean;
  /** Reloads the schema (MySQL: database) list and the current schema's tree. When the current one
   *  is no longer listed, moves to the first listed. A failure keeps both and sets `schemaError`. */
  refreshCatalog: () => Promise<void>;
  /** Saved queries of the connected connection, sorted by name. */
  savedQueries: SavedQuery[];
  tabs: Tab[];
  activeTabId: string | null;
  /** The open leave prompt (waiting for a save, unsaved edits, open transactions); `then` runs once
   *  it is resolved. While one is open, new requests are ignored, except quit / window close, which
   *  replace it (they cover every tab). */
  leave: LeaveRequest | null;
  /** Shown inside the prompt when Save, Commit or Roll back fails. */
  leaveError: string | null;
  /** Set when the open tabs could not be restored on connect; the tab state is then not saved
   *  (so the stored one is not overwritten) until the next successful load. */
  tabsSaveBlocked: boolean;
  /** Dismissible workspace notice for the same failure. */
  tabsNotice: string | null;
  dismissTabsNotice: () => void;
  /** Dismissible workspace notice for a failure with nowhere else to show (e.g. New window). */
  notice: string | null;
  setNotice: (notice: string | null) => void;
  /** ⌘K palette. */
  paletteOpen: boolean;
  setPaletteOpen: (open: boolean) => void;
  /** The History popover (opened from the toolbar or the palette). */
  historyOpen: boolean;
  setHistoryOpen: (open: boolean) => void;
  /** A one-shot request from the palette; the table tab with this id acts on it and clears it. */
  exportRequest: { tabId: string; format: "csv" | "json" } | null;
  setExportRequest: (r: { tabId: string; format: "csv" | "json" } | null) => void;
  /** Bumped by the palette's New connection; the Connections screen opens a blank form. */
  newConnectionNonce: number;
  requestNewConnection: () => void;
  requestLeave: (reason: LeaveReason, tabIds: string[], then: () => void, onCancel?: () => void) => void;
  resolveLeave: (choice: LeaveChoice) => Promise<void>;
  endTransaction: (tabId: string, commit: boolean) => Promise<void>;
  loadConnections: () => Promise<void>;
  openConnections: (selectId?: string) => void;
  connectTo: (id: string) => Promise<void>;
  loadSavedQueries: () => Promise<void>;
  openSavedQuery: (q: SavedQuery) => void;
  /** A saved tab upserts; an unsaved one needs `name` and becomes saved. Rejects when the backend fails. */
  saveQueryTab: (tabId: string, name?: string) => Promise<void>;
  renameSavedQuery: (id: string, name: string) => Promise<void>;
  duplicateSavedQuery: (id: string) => Promise<void>;
  deleteSavedQuery: (id: string) => Promise<void>;
  setSchema: (schema: string) => Promise<void>;
  openTable: (table: TableRef) => void;
  /** The new tab's id; `""` (and no tab) where the connection's kind has no query tabs. */
  newQueryTab: (
    init?: Partial<Pick<QueryTab, "sql" | "title" | "savedQueryId" | "savedSql" | "params" | "schema" | "database">>,
  ) => string;
  updateQueryTab: (id: string, patch: Partial<QueryTab>) => void;
  updateTableView: (id: string, patch: Partial<TableViewState>) => void;
  /** `types`: the row's original cell types, when the page has them (`rowTypes`). */
  editCell: (
    tabId: string,
    key: RowKey,
    column: string,
    value: string | null,
    original: string | null,
    types?: RowTypes,
  ) => void;
  revertCell: (tabId: string, key: RowKey, column: string, types?: RowTypes) => void;
  toggleDeleteRow: (tabId: string, key: RowKey, types?: RowTypes) => void;
  /** Appends an empty new row and returns its tempId (`""` when the tab is not a table tab). */
  addRow: (tabId: string) => string;
  editNewRow: (tabId: string, tempId: string, column: string, value: string | null) => void;
  removeNewRow: (tabId: string, tempId: string) => void;
  discardPending: (tabId: string) => void;
  /** Applies the tab's pending changes in one transaction; resolves whether they were saved. */
  saveTableChanges: (tabId: string) => Promise<boolean>;
  /** Sets (or clears, with `null`) the tab's footer note. */
  setSaveNote: (tabId: string, note: string | null) => void;
  runQuery: (tabId: string, target: RunTarget, opts?: RunOptions) => Promise<void>;
  /** Runs an export that runs a result's statement again on the tab's session. While it
   * runs, the tab's Run, COMMIT and ROLLBACK are refused with `EXPORT_RUNNING`. */
  trackQueryExport: (tabId: string, work: () => Promise<ExportOutcome>) => Promise<ExportOutcome>;
  stopQuery: (tabId: string) => Promise<void>;
  cancelConfirm: (tabId: string) => void;
  closeTab: (id: string) => void;
  setActiveTab: (id: string) => void;
}

export const useApp = create<AppState>((set, get) => ({
  screen: "connections",
  connections: [],
  loadError: null,
  selectedConnectionId: null,
  active: null,
  tree: null,
  completionCache: {},
  ensureCompletion,
  schemaError: null,
  catalogRefreshing: false,
  savedQueries: [],
  tabs: [],
  activeTabId: null,
  leave: null,
  leaveError: null,
  tabsSaveBlocked: false,
  tabsNotice: null,

  dismissTabsNotice: () => set({ tabsNotice: null }),
  notice: null,
  setNotice: (notice) => set({ notice }),

  paletteOpen: false,
  setPaletteOpen: (open) => set({ paletteOpen: open }),
  historyOpen: false,
  setHistoryOpen: (open) => set({ historyOpen: open }),
  exportRequest: null,
  setExportRequest: (r) => set({ exportRequest: r }),
  newConnectionNonce: 0,
  requestNewConnection: () => set((s) => ({ screen: "connections", selectedConnectionId: null, newConnectionNonce: s.newConnectionNonce + 1 })),

  requestLeave: (reason, tabIds, then, onCancel) => {
    // One prompt at a time: a second request would silently drop the first one's `then`. Quit and
    // window close cover every tab, so they take over (the earlier `then` is superseded). Either way
    // the flow that does not go ahead gets its `onCancel`, synchronously (a quit must be cancelled
    // in Rust before the window that was asking can be destroyed).
    const earlier = get().leave;
    if (earlier && reason !== "quit" && reason !== "window") {
      runOnCancel(onCancel);
      return;
    }
    const request: LeaveRequest = { reason, tabIds, then, onCancel, stage: "waiting" };
    if (earlier) runOnCancel(earlier.onCancel);
    const begin = () => {
      // Cancelled or replaced while waiting.
      if (get().leave !== request) return;
      // A save, Commit or Roll back in flight finishes first: its tab's state is not known yet, and
      // leaving must not cut it off. Meanwhile the sheet shows the waiting state.
      const running = [...inFlightSaves(tabIds), ...inFlightTxEnds(tabIds)];
      if (running.length > 0) {
        void Promise.all(running).then(begin);
        return;
      }
      const { tabs } = get();
      const stage = tabsWithEdits(tabs, tabIds).length > 0 ? "edits" : tabs.some((t) => tabIds.includes(t.id) && hasOpenTx(t)) ? "tx" : null;
      if (!stage) {
        closeLeave();
        then();
        return;
      }
      // A Commit that failed under a prompt this one replaced is shown here (nothing is lost silently).
      set({ leave: { ...request, stage }, leaveError: stage === "tx" ? takeTxEndErrors(tabIds) : null });
    };
    set({ leave: request, leaveError: null });
    begin();
  },

  endTransaction: async (tabId, commit) => {
    if (queryExports.has(tabId)) {
      const e = { kind: "query", message: EXPORT_RUNNING };
      if (commit && get().leave) txEndErrors.set(tabId, e.message);
      throw e;
    }
    let done!: () => void;
    const entry = new Promise<void>((resolve) => (done = resolve));
    txEndsInFlight.set(tabId, entry);
    // Settled before the tab's state changes, so a leave waiting on it sees the new state.
    const settle = () => {
      if (txEndsInFlight.get(tabId) === entry) txEndsInFlight.delete(tabId);
      done();
    };
    let txStatus: TxStatus;
    try {
      txStatus = await api.endTransaction(tabId, commit);
    } catch (e) {
      // Kept for the prompt that is up now, or for the one that replaces it meanwhile.
      if (commit && get().leave) txEndErrors.set(tabId, errorMessage(e));
      settle();
      // Nothing running any more may be all an open transaction prompt was waiting for.
      advanceLeave();
      throw e;
    }
    settle();
    get().updateQueryTab(tabId, { txStatus });
    refreshAfter(tabId, txStatus);
  },

  resolveLeave: async (choice) => {
    const leave = get().leave;
    if (!leave) return;
    if (choice === "cancel") {
      closeLeave();
      runOnCancel(leave.onCancel);
      return;
    }
    // Waiting for a save: the sheet offers only Cancel.
    if (leave.stage === "waiting") return;
    if (leave.stage === "edits") {
      if (choice !== "save" && choice !== "discard") return;
      // Never discard or re-save under a running save: wait, then see what is still unsaved.
      await Promise.all(inFlightSaves(leave.tabIds));
      // Replaced meanwhile (quit / window close): that request decides now.
      if (get().leave !== leave) return;
      // Tabs already saved by an earlier attempt are skipped, so a retry resumes where it stopped.
      const edited = tabsWithEdits(get().tabs, leave.tabIds);
      for (const t of edited) {
        if (choice === "discard") {
          get().discardPending(t.id);
          continue;
        }
        const saved = await get().saveTableChanges(t.id);
        // Advanced by the save itself (the last tab), or replaced by quit / window close.
        if (get().leave !== leave) return;
        if (!saved) {
          const failed = get().tabs.find((x) => x.id === t.id);
          const saveError = failed?.kind === "table" ? failed.saveError : null;
          // No saveError: the save was refused because one for that tab is already running.
          const message = saveError
            ? edited.length > 1
              ? `“${t.table.name}”: ${saveError}`
              : saveError
            : `A save for “${t.table.name}” is still running. Try again.`;
          set({ leaveError: message });
          return;
        }
      }
      // Stored now, not after the usual delay: a crash just after must not bring them back.
      if (choice === "discard" && edited.length > 0) void flushTabState();
      advanceLeave();
      return;
    }
    if (choice !== "commit" && choice !== "rollback") return;
    // Tabs already ended by an earlier attempt are skipped, so a retry resumes where it stopped.
    const open = get().tabs.filter((t): t is QueryTab => leave.tabIds.includes(t.id) && hasOpenTx(t));
    for (const t of open) {
      // A failed transaction cannot be committed; it is always rolled back. An unknown one is
      // committed when asked: COMMIT outside a transaction does nothing.
      const commit = choice === "commit" && t.txStatus !== "failed";
      try {
        await get().endTransaction(t.id, commit);
      } catch (e) {
        // Replaced meanwhile (quit / window close): a failed COMMIT is shown by that prompt instead.
        if (get().leave !== leave) return;
        if (commit) {
          // Only a failed COMMIT keeps the prompt: nothing may be lost silently.
          txEndErrors.delete(t.id);
          set({ leaveError: errorMessage(e) });
          return;
        }
        // A failed ROLLBACK must not trap the user; closing the session discards the transaction.
        console.error("rollback failed", e);
      }
      // Replaced meanwhile (quit / window close): its `then` supersedes this one. Also advanced by
      // the tab's new state when it was the last open one.
      if (get().leave !== leave) return;
    }
    closeLeave();
    leave.then();
  },

  loadConnections: async () => {
    try {
      set({ connections: await api.listConnections(), loadError: null });
    } catch (e) {
      set({ loadError: errorMessage(e) });
      throw e;
    }
  },

  openConnections: (selectId) => {
    set({ screen: "connections", selectedConnectionId: selectId ?? null, historyOpen: false });
  },

  connectTo: async (id) => {
    const info = await api.connect(id);
    let tree: SchemaTree;
    try {
      tree = await api.schemaTree(info.schema);
    } catch (e) {
      // The backend session is open; close it so nothing dangles.
      try {
        await api.disconnect();
      } catch {
        // Already gone or unreachable; the original error matters more.
      }
      set({ active: null, tree: null, screen: "connections" });
      throw e;
    }
    // Saved queries first (restore needs their ids), then the tab state; both before entering the
    // workspace so the persistence subscriber never sees an empty tab list and overwrites the saved one.
    let savedQueries: SavedQuery[] = [];
    // `null` when the list failed: restore then keeps every tab's saved-query link.
    let savedIds: Set<string> | null = null;
    try {
      savedQueries = await api.listSavedQueries();
      savedIds = new Set(savedQueries.map((q) => q.id));
    } catch (e) {
      console.error("list_saved_queries failed", e);
    }
    let json: string | null = null;
    let loadFailed = false;
    try {
      json = await api.loadTabState();
    } catch (e) {
      console.error("load_tab_state failed", e);
      loadFailed = true;
    }
    completionLoads = new Map();
    clearFields();
    refreshGen++;
    catalogDirty.clear();
    const { tabs, activeTabId, failed, restoredEdits, droppedEdits } = restoreTabs(json, savedIds, {
      queryTabs: hasQueryTabs(info.profile?.kind),
    });
    const blocked = loadFailed || failed;
    // Some changes dropped and others restored: both are said.
    const editsNotice = [droppedEdits ? EDITS_NOT_RESTORED : null, restoredEdits ? editsRestored(restoredEdits) : null]
      .filter((n) => n !== null)
      .join(" ");
    const tabsNotice = blocked ? TABS_NOT_RESTORED : editsNotice || null;
    set({
      active: info, tree, completionCache: {}, schemaError: null, catalogRefreshing: false, screen: "workspace", tabs, activeTabId, savedQueries, historyOpen: false, notice: null,
      tabsSaveBlocked: blocked, tabsNotice,
    });
    void ensureCompletion(null, info.schema);
  },

  loadSavedQueries: async () => {
    const connectionId = get().active?.profile?.id;
    try {
      const list = await api.listSavedQueries();
      // A newer connection may have replaced this one while the request was in flight.
      if (get().active?.profile?.id === connectionId) set({ savedQueries: list });
    } catch (e) {
      console.error("list_saved_queries failed", e);
    }
  },

  openSavedQuery: (q) => {
    if (!hasQueryTabs(get().active?.profile?.kind)) return;
    const open = get().tabs.find((t) => t.kind === "query" && t.savedQueryId === q.id);
    if (open) {
      set({ activeTabId: open.id });
      return;
    }
    get().newQueryTab({ title: q.name, sql: q.sql, savedQueryId: q.id, savedSql: q.sql, params: q.lastParams });
  },

  saveQueryTab: async (tabId, name) => {
    const tab = get().tabs.find((t) => t.id === tabId);
    const connectionId = get().active?.profile?.id;
    if (tab?.kind !== "query" || !connectionId) return;
    const sql = tab.sql;
    if (tab.savedQueryId !== null) {
      const known = get().savedQueries.find((q) => q.id === tab.savedQueryId);
      await api.saveQuery({
        id: tab.savedQueryId,
        connectionId,
        name: tab.title,
        sql,
        lastParams: known?.lastParams ?? tab.params,
        updatedAt: known?.updatedAt ?? "",
      });
      get().updateQueryTab(tabId, { savedSql: sql });
    } else {
      const trimmed = name?.trim();
      if (!trimmed) return;
      const saved = await api.saveQuery({ id: "", connectionId, name: trimmed, sql, lastParams: tab.params, updatedAt: "" });
      get().updateQueryTab(tabId, { savedQueryId: saved.id, title: trimmed, savedSql: sql });
    }
    await get().loadSavedQueries();
  },

  renameSavedQuery: async (id, name) => {
    const q = get().savedQueries.find((x) => x.id === id);
    const trimmed = name.trim();
    if (!q || !trimmed || trimmed === q.name) return;
    await api.saveQuery({ ...q, name: trimmed });
    set((s) => ({ tabs: s.tabs.map((t) => (t.kind === "query" && t.savedQueryId === id ? { ...t, title: trimmed } : t)) }));
    await get().loadSavedQueries();
  },

  duplicateSavedQuery: async (id) => {
    const q = get().savedQueries.find((x) => x.id === id);
    if (!q) return;
    await api.saveQuery({ ...q, id: "", name: `${q.name} copy` });
    await get().loadSavedQueries();
  },

  deleteSavedQuery: async (id) => {
    await api.deleteSavedQuery(id);
    set((s) => ({
      tabs: s.tabs.map((t) => (t.kind === "query" && t.savedQueryId === id ? { ...t, savedQueryId: null, savedSql: null } : t)),
    }));
    await get().loadSavedQueries();
  },

  setSchema: async (schema) => {
    try {
      const tree = await api.schemaTree(schema);
      // Switching always refetches the window's own schema (the sidebar's old behaviour).
      const key = completionKey(null, schema);
      clearFields();
      set((s) => {
        const { [key]: _stale, ...completionCache } = s.completionCache;
        return { tree, completionCache, schemaError: null, active: s.active && { ...s.active, schema } };
      });
      void ensureCompletion(null, schema);
    } catch (e) {
      set({ schemaError: errorMessage(e) });
    }
  },

  refreshCatalog: async () => {
    const start = get().active;
    if (!start) return;
    // The latest refresh wins: an older one still in flight applies nothing, not even its error.
    const gen = ++refreshGen;
    // The collections' fields are sampled again on the next ask.
    clearFields();
    const errorAtStart = get().schemaError;
    set({ catalogRefreshing: true });
    // A schema switch meanwhile wins over this refresh's tree (a reconnect bumps `refreshGen`).
    const current = () => gen === refreshGen && get().active?.schema === start.schema;
    try {
      const [schemas, tree0] = await Promise.all([api.targetSchemas(null), api.schemaTree(start.schema)]);
      // Move only off a schema that was listed and is gone, or off "" (a server that had no
      // databases). One never listed (a profile's default `pg_catalog`, MySQL's `mysql`) stays.
      const gone = start.schema === "" || (start.schemas.includes(start.schema) && !schemas.includes(start.schema));
      const schema = gone && schemas.length > 0 ? schemas[0] : start.schema;
      const tree = schema === start.schema ? tree0 : await api.schemaTree(schema);
      if (gen !== refreshGen) return;
      if (!current()) {
        set((s) => ({ active: s.active && { ...s.active, schemas } }));
        return;
      }
      const key = completionKey(null, schema);
      set((s) => {
        const { [key]: _stale, ...completionCache } = s.completionCache;
        // An error that arrived during the refresh (a failed switch) is not this refresh's to clear.
        const schemaError = s.schemaError === errorAtStart ? null : s.schemaError;
        return { tree, completionCache, schemaError, active: s.active && { ...s.active, schemas, schema } };
      });
      void ensureCompletion(null, schema);
    } catch (e) {
      console.error("refresh failed", e);
      if (current()) set({ schemaError: `Couldn't refresh: ${errorMessage(e)}` });
    } finally {
      if (gen === refreshGen) set({ catalogRefreshing: false });
    }
  },

  openTable: (table) => {
    const id = `${table.schema}.${table.name}`;
    set((s) => ({
      tabs: s.tabs.some((t) => t.id === id) ? s.tabs : [...s.tabs, { kind: "table", id, table, view: { mode: "data", filter: null, sort: null }, pending: pending.emptyPending }],
      activeTabId: id,
    }));
  },

  newQueryTab: (init) => {
    // The entry points are hidden or disabled where the kind has no query tabs (none now); this is the backstop.
    if (!hasQueryTabs(get().active?.profile?.kind)) return "";
    const id = `q:${crypto.randomUUID()}`;
    set((s) => ({
      tabs: [
        ...s.tabs,
        {
          kind: "query",
          id,
          title: init?.title ?? nextQueryTitle(s.tabs),
          sql: init?.sql ?? (mockMode ? (get().active?.profile?.kind === "mongodb" ? MOCK_MONGO_QUERY_SQL : MOCK_QUERY_SQL) : ""),
          savedQueryId: init?.savedQueryId ?? null,
          savedSql: init?.savedSql ?? null,
          params: init?.params ?? {},
          schema: init?.schema ?? null,
          database: init?.database ?? null,
          run: initialRun(),
          txStatus: "idle",
          note: null,
        },
      ],
      activeTabId: id,
    }));
    return id;
  },

  retargetQueryTab: (tabId, database) => {
    const tab = get().tabs.find((t): t is QueryTab => t.kind === "query" && t.id === tabId);
    if (!tab) return;
    // A running or confirming run is pinned to its database; the picker is disabled meanwhile too.
    if (tab.run.status !== "idle") return;
    const profile = get().active?.profile;
    // A SQLite tab stays on its file.
    if (profile && !hasOtherDatabases(profile)) return;
    // The connection's own database is `null`, however the picker spelled it.
    const own = (profile ? targetName(profile) : "") || null;
    const next = database !== null && database === own ? null : database;
    if (next === tab.database) return;
    // The old session is left alone: Rust replaces it on the tab's next run.
    get().requestLeave("retarget", [tabId], () => get().updateQueryTab(tabId, { database: next, schema: null }));
  },

  updateQueryTab: (id, patch) => {
    set((s) => ({
      tabs: s.tabs.map((t) => (t.id === id && t.kind === "query" ? { ...t, ...patch } : t)),
    }));
    // A transaction that ended (here or in the editor) may have emptied an open transaction prompt.
    if ("txStatus" in patch) advanceLeave();
  },

  runQuery: async (tabId, target, opts) => {
    const find = () => {
      const t = get().tabs.find((x) => x.id === tabId);
      return t?.kind === "query" ? t : undefined;
    };
    const tab = find();
    if (!tab || tab.run.status === "running") return;
    if (queryExports.has(tabId)) {
      get().updateQueryTab(tabId, { run: { ...tab.run, error: EXPORT_RUNNING } });
      return;
    }
    const sql = opts?.sql ?? tab.sql;
    const schema = runSchema(opts?.schema !== undefined ? opts.schema : tab.schema, get().active);
    const database = tabDatabase(opts?.database !== undefined ? opts.database : tab.database, get().active);
    const fetchAll = opts?.fetchAll ?? false;
    const token = (runTokens.get(tabId) ?? 0) + 1;
    runTokens.set(tabId, token);
    const patchRun = (patch: Partial<QueryRunState>) => {
      if (runTokens.get(tabId) !== token) return;
      const t = find();
      if (t) get().updateQueryTab(tabId, { run: { ...t.run, ...patch } });
    };

    // Fetch all re-runs one statement and replaces only that result.
    const replaceIndex = fetchAll ? tab.run.selected : null;
    const received: StatementResult[] = [];
    let expected: number | null = null;
    let outcome: Extract<RunOutcome, { kind: "completed" }> | null = null;
    let settled = false;

    patchRun(
      fetchAll
        ? { status: "running", error: null, pending: null }
        : {
            status: "running", results: [], selected: 0, startedAt: null, sqlAtRun: sql, schemaAtRun: schema, databaseAtRun: database,
            pending: null, error: null, statementCount: null, fetchedAll: [],
          },
    );

    /** Ends the run; `error` reports results that never arrived. */
    const finish = (error?: string) => {
      settled = true;
      const extra = error ? { error } : {};
      if (fetchAll) {
        patchRun({ status: "idle", ...extra });
        return;
      }
      const results = find()?.run.results ?? [];
      let selected = results.length - 1;
      for (let i = results.length - 1; i >= 0; i--) {
        if (results[i].outcome.kind === "rows") {
          selected = i;
          break;
        }
      }
      patchRun({ status: "idle", selected: Math.max(0, selected), ...extra });
    };
    const settle = () => {
      if (settled || !outcome || received.length < (expected ?? 0)) return;
      finish();
    };

    const onEvent = (e: RunEvent) => {
      if (e.type === "started" && CATALOG_VERBS.has(e.verb)) catalogDirty.add(tabId);
      if (e.type !== "finished" || runTokens.get(tabId) !== token) return;
      // A MongoDB write can create a collection (an insert into a new one); its tab has no transaction to wait for.
      if (e.result.kind === "write" && get().active?.profile?.kind === "mongodb") {
        catalogDirty.add(tabId);
        if (outcome) refreshAfter(tabId, "idle");
      }
      received.push(e.result);
      const t = find();
      if (!t) return;
      if (replaceIndex !== null) {
        const old = t.run.results[replaceIndex];
        if (!old) return;
        const o = e.result.outcome;
        if (o.kind !== "rows") {
          // Keep the rows already shown; surface the failure inline.
          patchRun({ error: o.kind === "error" ? o.message : o.kind === "canceled" ? "Fetch all was canceled." : "Fetch all did not run." });
        } else {
          const results = t.run.results.map((r, i) => (i === replaceIndex ? { ...e.result, index: old.index, from: old.from, to: old.to } : r));
          patchRun({ results, fetchedAll: [...new Set([...(t.run.fetchedAll ?? []), replaceIndex])] });
        }
      } else {
        patchRun({ results: [...t.run.results, e.result] });
      }
      settle();
    };

    let params = opts?.params;
    if (!params) {
      // The editor's analysis is debounced and may predate the last edit; analyze the exact text.
      try {
        params = paramsFor((await api.analyzeSql(sql)).params, tab.params);
      } catch (e) {
        console.error("analyze_sql failed", e);
        params = tab.params;
      }
      if (runTokens.get(tabId) !== token) return;
    }

    try {
      const res = await api.runQuery(
        tabId,
        { sql, target, params, schema, confirmed: opts?.confirmed ?? false, fetchAll, savedQueryId: tab.savedQueryId, database },
        onEvent,
      );
      if (runTokens.get(tabId) !== token) return;
      if (res.kind === "needsConfirmation") {
        patchRun({ status: "confirm", pending: res.statements });
        return;
      }
      outcome = res;
      expected = res.statementCount;
      if (!fetchAll) {
        patchRun({ startedAt: res.startedAt, statementCount: res.statementCount });
      }
      const t = find();
      if (t) get().updateQueryTab(tabId, { txStatus: res.txStatus });
      refreshAfter(tabId, res.txStatus);
      // The runner remembered this run's values; reload so reopening the query shows them.
      if (tab.savedQueryId && !fetchAll) void get().loadSavedQueries();
      settle();
      if (!settled) {
        // A lost `Finished` event must not leave the tab running forever.
        setTimeout(() => {
          const missing = (expected ?? 0) - received.length;
          if (settled || missing <= 0 || runTokens.get(tabId) !== token) return;
          finish(`${missing} statement result(s) did not arrive.`);
        }, RESULTS_GRACE_MS);
      }
    } catch (e) {
      patchRun({ status: "idle", error: errorMessage(e) });
      // The tab's session is gone, and with it any transaction (the server rolled it back).
      const t = find();
      if ((e as { kind?: string })?.kind === "connection" && t && runTokens.get(tabId) === token) {
        get().updateQueryTab(tabId, { txStatus: "idle", ...(t.txStatus !== "idle" ? { note: LOST_TX_NOTE } : {}) });
      }
    }
  },

  trackQueryExport: async (tabId, work) => {
    queryExports.add(tabId);
    try {
      return await work();
    } catch (e) {
      // As for a run: the tab's session is gone, and with it any transaction.
      const t = get().tabs.find((x) => x.id === tabId);
      if ((e as { kind?: string })?.kind === "connection" && t?.kind === "query") {
        get().updateQueryTab(tabId, { txStatus: "idle", ...(t.txStatus !== "idle" ? { note: LOST_TX_NOTE } : {}) });
      }
      throw e;
    } finally {
      queryExports.delete(tabId);
    }
  },

  stopQuery: async (tabId) => {
    try {
      await api.cancelQuery(tabId);
    } catch (e) {
      const t = get().tabs.find((x) => x.id === tabId);
      if (t?.kind === "query") {
        get().updateQueryTab(tabId, { run: { ...t.run, error: `Couldn't stop the query: ${errorMessage(e)}` } });
      }
    }
  },

  cancelConfirm: (tabId) => {
    const t = get().tabs.find((x) => x.id === tabId);
    if (t?.kind === "query" && t.run.status === "confirm") {
      get().updateQueryTab(tabId, { run: { ...t.run, status: "idle", pending: null } });
    }
  },

  updateTableView: (id, patch) => {
    set((s) => ({
      tabs: s.tabs.map((t) => (t.id === id && t.kind === "table" ? { ...t, view: { ...t.view, ...patch } } : t)),
    }));
  },

  editCell: (tabId, key, column, value, original, types) =>
    updatePending(tabId, (p) => pending.setCell(p, key, column, value, original, types)),
  revertCell: (tabId, key, column, types) => updatePending(tabId, (p) => pending.revertCell(p, key, column, types)),
  toggleDeleteRow: (tabId, key, types) => updatePending(tabId, (p) => pending.toggleDelete(p, key, types)),
  addRow: (tabId) => {
    let tempId = "";
    updatePending(tabId, (p) => {
      const r = pending.addInsert(p);
      tempId = r.tempId;
      return r.pending;
    });
    return tempId;
  },
  editNewRow: (tabId, tempId, column, value) =>
    updatePending(tabId, (p) => pending.setInsertCell(p, tempId, column, value)),
  removeNewRow: (tabId, tempId) => updatePending(tabId, (p) => pending.removeInsert(p, tempId)),
  discardPending: (tabId) => updatePending(tabId, () => pending.emptyPending),
  saveTableChanges: async (tabId) => {
    const tab = get().tabs.find((t) => t.id === tabId);
    if (!tab || tab.kind !== "table") return false;
    const sent = tab.pending;
    const total = pending.counts(sent).total;
    if (total === 0) return true;
    if (savesInFlight.has(tabId)) return false;
    const run = runSave(tabId, tab.table, sent, total);
    savesInFlight.set(tabId, run);
    let saved: boolean;
    try {
      saved = await run;
    } finally {
      savesInFlight.delete(tabId);
    }
    // A save made under an open edits prompt (⌘S) may have emptied it.
    if (saved) advanceLeave();
    return saved;
  },
  setSaveNote: (tabId, note) => patchTableTab(tabId, { saveNote: note }),

  closeTab: (id) => {
    const tab = get().tabs.find((t) => t.id === id);
    if (!tab) return;
    get().requestLeave("closeTab", [id], () => removeTab(id));
  },

  setActiveTab: (id) => {
    if (get().tabs.some((t) => t.id === id)) set({ activeTabId: id });
  },
}));

/**
 * Moves the open leave request on once its stage has nothing left to ask about: at the edits stage,
 * nothing left to save and no save running; at the transaction stage, no listed tab with an open
 * transaction and no Commit / Roll back running. Next comes the transaction prompt when a listed
 * query tab needs it, else `then`. The state changes before `then` runs, so a second call (a click
 * racing a ⌘S) finds nothing to do and `then` runs once.
 */
function advanceLeave() {
  const { leave, tabs } = useApp.getState();
  if (!leave) return;
  const openTx = tabs.some((t) => leave.tabIds.includes(t.id) && hasOpenTx(t));
  if (leave.stage === "edits") {
    if (tabsWithEdits(tabs, leave.tabIds).length > 0 || inFlightSaves(leave.tabIds).length > 0) return;
    // The two prompts never show together.
    if (openTx) {
      useApp.setState({ leave: { ...leave, stage: "tx" }, leaveError: takeTxEndErrors(leave.tabIds) });
      return;
    }
  } else if (leave.stage === "tx") {
    if (openTx || inFlightTxEnds(leave.tabIds).length > 0) return;
  } else {
    return;
  }
  closeLeave();
  leave.then();
}

function runOnCancel(onCancel: (() => void) | undefined) {
  try {
    onCancel?.();
  } catch (e) {
    console.error("leave onCancel failed", e);
  }
}

/** Clears the leave prompt and anything it was keeping to show. */
function closeLeave() {
  txEndErrors.clear();
  useApp.setState({ leave: null, leaveError: null });
}

/** Query tabs with a Commit / Roll back running; leaving waits for them. Never rejects. */
const txEndsInFlight = new Map<string, Promise<void>>();
/** Tabs whose session an export holds (see `trackQueryExport`). */
const queryExports = new Set<string>();
const inFlightTxEnds = (tabIds: string[]) => tabIds.flatMap((id) => txEndsInFlight.get(id) ?? []);
/** COMMIT failures under an open leave prompt, by tab, until a transaction prompt shows them. */
const txEndErrors = new Map<string, string>();

/** The recorded COMMIT failures of `tabIds` as one message (`null` when none), forgetting them. */
function takeTxEndErrors(tabIds: string[]): string | null {
  const failed = tabIds.filter((id) => txEndErrors.has(id));
  if (failed.length === 0) return null;
  const { tabs } = useApp.getState();
  const message = failed
    .map((id) => {
      const text = txEndErrors.get(id)!;
      txEndErrors.delete(id);
      const tab = tabs.find((t) => t.id === id);
      return failed.length > 1 && tab?.kind === "query" ? `“${tab.title}”: ${text}` : text;
    })
    .join(" ");
  return message;
}

/** Tabs with a save running, with its outcome; a second save for the same tab is refused. */
const savesInFlight = new Map<string, Promise<boolean>>();
/** The running saves among `tabIds`; leaving waits for them. */
const inFlightSaves = (tabIds: string[]) => tabIds.flatMap((id) => savesInFlight.get(id) ?? []);

/**
 * Sends `sent` in one transaction (one change at a time on a server without them); on success
 * removes only what was sent. Never rejects.
 */
async function runSave(tabId: string, table: TableRef, sent: PendingChanges, total: number): Promise<boolean> {
  patchTableTab(tabId, { saveError: null, saveNote: null, saving: true });
  try {
    await api.applyChanges(pending.toChangeSet(sent, table));
  } catch (e) {
    const err = e as { kind?: string; applied?: number; uncertain?: boolean } | null;
    if (err?.kind === "partlySaved") {
      // The first `applied` changes stayed saved (no transaction to roll them back): they leave the
      // pending list, the rest stay. The message already says what happened. Reload to show them.
      // When the next write got no answer it may have been applied: saving again first asks, as
      // after an uncertain commit, so an insert isn't made twice.
      const saved = pending.firstChanges(sent, err.applied ?? 0);
      patchTableTab(tabId, (t) => ({
        pending: pending.subtractSent(t.pending, saved),
        saveError: errorMessage(e),
        saving: false,
        reloadNonce: (t.reloadNonce ?? 0) + 1,
        ...(err.uncertain === true ? { uncertainSave: true } : {}),
      }));
      return false;
    }
    if (err?.kind === "uncertain") {
      // The commit's outcome is unknown: don't claim nothing was saved. Keep the changes so nothing
      // is lost, and reload so the grid shows what the table holds now.
      patchTableTab(tabId, (t) => ({
        saveError: errorMessage(e),
        saving: false,
        uncertainSave: true,
        reloadNonce: (t.reloadNonce ?? 0) + 1,
      }));
    } else {
      patchTableTab(tabId, { saveError: `Nothing was saved: ${errorMessage(e)}`, saving: false });
    }
    return false;
  }
  patchTableTab(tabId, (t) => ({
    saving: false,
    pending: pending.subtractSent(t.pending, sent),
    saveError: null,
    uncertainSave: false,
    saveNote: `Saved ${total} ${total === 1 ? "change" : "changes"}`,
    reloadNonce: (t.reloadNonce ?? 0) + 1,
  }));
  return true;
}

function updatePending(tabId: string, f: (p: PendingChanges) => PendingChanges) {
  useApp.setState((s) => ({
    tabs: s.tabs.map((t) => {
      if (t.id !== tabId || t.kind !== "table") return t;
      const next = f(t.pending);
      if (next === t.pending) return t;
      // With nothing left to save, an earlier uncertain save no longer needs asking about.
      const settled = pending.counts(next).total === 0 ? { uncertainSave: false } : {};
      return { ...t, pending: next, saveError: null, saveNote: null, ...settled };
    }),
  }));
  // A revert, restore, discard or removed new row may have emptied an open edits prompt.
  advanceLeave();
}

function patchTableTab(tabId: string, patch: Partial<TableTab> | ((t: TableTab) => Partial<TableTab>)) {
  useApp.setState((s) => ({
    tabs: s.tabs.map((t) =>
      t.id === tabId && t.kind === "table" ? { ...t, ...(typeof patch === "function" ? patch(t) : patch) } : t,
    ),
  }));
}

/** Removes a tab; a query tab's session is closed afterwards (which also stops any running query). */
function removeTab(id: string) {
  catalogDirty.delete(id);
  const { tabs, activeTabId } = useApp.getState();
  const index = tabs.findIndex((t) => t.id === id);
  if (index < 0) return;
  const closing = tabs[index];
  const next = activeTabId === id ? (tabs[index + 1] ?? tabs[index - 1])?.id ?? null : activeTabId;
  useApp.setState({ tabs: tabs.filter((t) => t.id !== id), activeTabId: next });
  if (closing.kind !== "query") return;
  runTokens.delete(id);
  try {
    api.closeQuerySession(id).catch((e) => console.error("close_query_session failed", e));
  } catch (e) {
    console.error("close_query_session failed", e);
  }
}
