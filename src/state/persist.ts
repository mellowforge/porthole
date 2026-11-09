import * as api from "../api/tauri";
import type { TableRef } from "../api/types";
import { counts, emptyPending, validPending } from "../table/pending";
import type { PendingChanges } from "../table/pending";
import { errorMessage, txNeedsAsk, useApp } from "./store";
import type { QueryTab, Tab, TableViewState } from "./store";

export interface PersistedTabs {
  v: 1;
  activeTabId: string | null;
  tabs: PersistedTab[];
}

export type PersistedTab =
  | {
      kind: "table";
      id: string;
      table: TableRef;
      view: TableViewState;
      /** Only when the tab has unsaved changes. */
      pending?: PendingChanges;
      /** Written only alongside `pending`, never alone: when its last save's outcome is unknown or a
       *  save is running (a crash then leaves it unknown). Without `pending` it is ignored on restore. */
      uncertainSave?: true;
    }
  | {
      kind: "query";
      id: string;
      title: string;
      sql: string;
      savedQueryId: string | null;
      savedSql: string | null;
      params: Record<string, string | null>;
      schema: string | null;
      database?: string | null;
      txOpen: boolean;
    };

export const RESTORE_NOTE = "This tab had an open transaction when the app closed. Uncommitted changes were lost.";
export const TABS_NOT_RESTORED = "Couldn't restore your open tabs. They won't be saved until you reconnect.";
export const EDITS_NOT_RESTORED = "Some unsaved changes from your last session couldn't be restored.";
/** The connect notice for unsaved changes restored in the tables `names`. */
export const editsRestored = (names: string[]) =>
  `Unsaved changes from your last session were restored in ${names.length === 1 ? `“${names[0]}”` : `${names.length} tabs`}. Check them before saving.`;
/** The notice for a tab-state write that failed. */
export const tabsNotSaved = (message: string) =>
  `Couldn't save your open tabs: ${message}. Unsaved changes won't be restored after a crash.`;
const SAVE_DELAY_MS = 500;

export function serializeTabs(tabs: Tab[], activeTabId: string | null): PersistedTabs {
  return {
    v: 1,
    activeTabId,
    tabs: tabs.map((t): PersistedTab =>
      t.kind === "table"
        ? {
            kind: "table",
            id: t.id,
            table: t.table,
            view: t.view,
            // Clean tabs are written exactly as before (no new keys). A save cut off by a crash may
            // have committed: its changes come back marked so saving again asks first.
            ...(counts(t.pending).total > 0
              ? { pending: t.pending, ...(t.uncertainSave || t.saving ? { uncertainSave: true as const } : {}) }
              : {}),
          }
        : {
            kind: "query",
            id: t.id,
            title: t.title,
            sql: t.sql,
            savedQueryId: t.savedQueryId,
            savedSql: t.savedSql,
            params: t.params,
            schema: t.schema,
            database: t.database,
            txOpen: txNeedsAsk(t.txStatus),
          },
    ),
  };
}

export interface RestoredTabs {
  tabs: Tab[];
  activeTabId: string | null;
  /** The stored state exists but could not be read (invalid JSON or another version). */
  failed: boolean;
  /** The table names whose unsaved changes came back. Absent when none did. */
  restoredEdits?: string[];
  /** A tab's stored unsaved changes could not be read and were dropped. Absent otherwise. */
  droppedEdits?: true;
}

/**
 * Never throws: anything unreadable restores nothing and sets `failed`. `savedQueryIds` is
 * `null` when the saved queries could not be listed; links to saved queries are then kept.
 * `queryTabs: false` (a kind without query tabs) drops any stored query tab. A table tab's stored
 * unsaved changes come back when they are well-formed; otherwise the tab restores clean.
 */
export function restoreTabs(
  json: string | null,
  savedQueryIds: Set<string> | null,
  { queryTabs = true }: { queryTabs?: boolean } = {},
): RestoredTabs {
  const nothing = (failed: boolean): RestoredTabs => ({ tabs: [], activeTabId: null, failed });
  if (!json) return nothing(false);
  let data: PersistedTabs;
  try {
    data = JSON.parse(json) as PersistedTabs;
  } catch {
    return nothing(true);
  }
  if (!data || data.v !== 1 || !Array.isArray(data.tabs)) return nothing(true);
  const tabs: Tab[] = [];
  const restoredEdits: string[] = [];
  let droppedEdits = false;
  for (const p of data.tabs) {
    if (p?.kind === "table" && typeof p.id === "string" && p.table) {
      const stored: unknown = p.pending;
      // The notice names the table, so its changes come back only with a readable name.
      const valid = typeof p.table.schema === "string" && typeof p.table.name === "string" && validPending(stored);
      // Well-formed but empty (never written so) restores clean, without a notice.
      const kept = valid && counts(stored).total > 0 ? stored : null;
      if (kept) restoredEdits.push(p.table.name);
      else if (stored !== undefined && !valid) droppedEdits = true;
      tabs.push({
        kind: "table",
        id: p.id,
        table: p.table,
        view: { mode: p.view?.mode === "structure" ? "structure" : "data", filter: p.view?.filter ?? null, sort: p.view?.sort ?? null },
        pending: kept ?? emptyPending,
        ...(kept && p.uncertainSave === true ? { uncertainSave: true } : {}),
      });
    } else if (p?.kind === "query" && typeof p.id === "string" && queryTabs) {
      const saved = typeof p.savedQueryId === "string" && (savedQueryIds === null || savedQueryIds.has(p.savedQueryId));
      const tab: QueryTab = {
        kind: "query",
        id: p.id,
        title: p.title ?? "Query",
        sql: p.sql ?? "",
        savedQueryId: saved ? p.savedQueryId : null,
        savedSql: saved ? (p.savedSql ?? null) : null,
        params: p.params ?? {},
        schema: p.schema ?? null,
        database: typeof p.database === "string" ? p.database : null,
        run: { status: "idle", results: [], selected: 0, startedAt: null, sqlAtRun: null, pending: null, error: null, statementCount: null },
        txStatus: "idle",
        note: p.txOpen ? RESTORE_NOTE : null,
      };
      tabs.push(tab);
    }
  }
  const activeTabId = tabs.some((t) => t.id === data.activeTabId) ? data.activeTabId : (tabs[0]?.id ?? null);
  return {
    tabs,
    activeTabId,
    failed: false,
    ...(restoredEdits.length ? { restoredEdits } : {}),
    ...(droppedEdits ? { droppedEdits: true as const } : {}),
  };
}

/** A table tab's save started or ended: what a crash would leave behind must be stored now. */
const saveStartedOrEnded = (tabs: Tab[], prev: Tab[]) =>
  tabs.some((t) => {
    if (t.kind !== "table") return false;
    const before = prev.find((p) => p.id === t.id);
    return before?.kind === "table" && !!before.saving !== !!t.saving;
  });

let timer: ReturnType<typeof setTimeout> | null = null;
/** Saves started and not yet finished. */
let saving = 0;
const pendingListeners = new Set<() => void>();
const notifyPending = () => pendingListeners.forEach((l) => l());

/** Whether tab state has a write waiting or running: quitting right now would lose it. */
export const tabStatePending = () => timer !== null || saving > 0;

/** Calls `listener` whenever `tabStatePending()` may have changed. Returns the unsubscribe. */
export function onTabStatePendingChange(listener: () => void): () => void {
  pendingListeners.add(listener);
  return () => {
    pendingListeners.delete(listener);
  };
}
/** `connectionId:json` of the last successful save, to skip no-op saves (run progress changes tabs too). */
let lastSaved: string | null = null;
/** The `tabsNotice` the last failed save showed, cleared by the next successful one. */
let failedNotice: string | null = null;

function snapshot(): { key: string; json: string } | null {
  const { screen, tabs, activeTabId, active, tabsSaveBlocked } = useApp.getState();
  // The backend scopes the save to the current session; never write outside a workspace.
  if (screen !== "workspace") return null;
  // The stored tabs could not be restored: saving now would overwrite them.
  if (tabsSaveBlocked) return null;
  const json = JSON.stringify(serializeTabs(tabs, activeTabId));
  return { key: `${active?.profile?.id ?? ""}:${json}`, json };
}

async function save(): Promise<void> {
  const snap = snapshot();
  if (!snap || snap.key === lastSaved) return;
  saving++;
  notifyPending();
  try {
    await api.saveTabState(snap.json);
    lastSaved = snap.key;
    // Only when it still shows: a newer notice stays.
    if (failedNotice !== null && useApp.getState().tabsNotice === failedNotice) useApp.setState({ tabsNotice: null });
    failedNotice = null;
  } catch (e) {
    // Not blocking: the next change tries again.
    console.error("save_tab_state failed", e);
    failedNotice = tabsNotSaved(errorMessage(e));
    useApp.setState({ tabsNotice: failedNotice });
  } finally {
    saving--;
    notifyPending();
  }
}

/** Saves right now. Never rejects: a failure must not block leaving. */
export async function flushTabState(): Promise<void> {
  if (timer) {
    clearTimeout(timer);
    timer = null;
    notifyPending();
  }
  await save();
}

/**
 * Overwrites the stored tabs that could not be restored with this window's current tabs and
 * lifts the save block. Never rejects: a failed write keeps saving blocked and says why.
 */
export async function discardStoredTabs(): Promise<void> {
  const { tabs, activeTabId, active } = useApp.getState();
  const json = JSON.stringify(serializeTabs(tabs, activeTabId));
  saving++;
  notifyPending();
  try {
    await api.saveTabState(json);
    lastSaved = `${active?.profile?.id ?? ""}:${json}`;
    useApp.setState({ tabsSaveBlocked: false, tabsNotice: null });
    // A change made during the write was skipped while saving was blocked.
    void save();
  } catch (e) {
    useApp.setState({ tabsSaveBlocked: true, tabsNotice: `Couldn't discard the saved tabs: ${errorMessage(e)}` });
  } finally {
    saving--;
    notifyPending();
  }
}

export function startTabPersistence(): () => void {
  const unsubscribe = useApp.subscribe((s, prev) => {
    if (s.tabs === prev.tabs && s.activeTabId === prev.activeTabId) return;
    if (s.screen !== "workspace") return;
    if (timer) clearTimeout(timer);
    if (saveStartedOrEnded(s.tabs, prev.tabs)) {
      // Not debounced: a crash during the save or just after its commit must not restore the
      // changes unmarked (saved twice) or as they were before the commit.
      timer = null;
      void save();
      notifyPending();
      return;
    }
    timer = setTimeout(() => {
      timer = null;
      void save();
      notifyPending();
    }, SAVE_DELAY_MS);
    notifyPending();
  });
  return () => {
    unsubscribe();
    if (timer) clearTimeout(timer);
    timer = null;
    lastSaved = null;
    failedNotice = null;
    notifyPending();
  };
}
