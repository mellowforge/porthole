import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../api/tauri", () => ({ saveTabState: vi.fn(), loadTabState: vi.fn(), applyChanges: vi.fn() }));

import * as api from "../api/tauri";
import { discardStoredTabs, flushTabState, onTabStatePendingChange, restoreTabs, RESTORE_NOTE, serializeTabs, startTabPersistence, tabStatePending, tabsNotSaved } from "./persist";
import { addInsert, emptyPending, setCell, setInsertCell, toggleDelete } from "../table/pending";
import type { RowTypes } from "../table/pending";
import { useApp } from "./store";
import type { QueryTab, Tab, TableTab } from "./store";

const run: QueryTab["run"] = { status: "idle", results: [], selected: 0, startedAt: null, sqlAtRun: null, pending: null, error: null, statementCount: null };
const q = (over: Partial<QueryTab> = {}): QueryTab => ({
  kind: "query", id: "q:1", title: "Query 1", sql: "select :a", savedQueryId: null, savedSql: null,
  params: { a: null }, schema: "public", run: { ...run }, txStatus: "idle", note: null, database: null, ...over,
});
const table: Tab = { kind: "table", id: "public.users", table: { schema: "public", name: "users" }, view: { mode: "data", filter: "id > 1", sort: null }, pending: emptyPending };

beforeEach(() => {
  vi.resetAllMocks();
  vi.mocked(api.saveTabState).mockResolvedValue(undefined);
  useApp.setState(useApp.getInitialState(), true);
});
afterEach(() => vi.useRealTimers());

describe("persist", () => {
  it("pending changes and the uncertain-save mark round-trip; a clean tab is written as before", () => {
    const types: RowTypes = { key: { id: "i" }, cells: { id: "i", name: "t" } };
    let p = setCell(emptyPending, ["1"], "name", "x", "a");
    p = setCell(p, ["5"], "name", "y", "b", types);
    p = toggleDelete(p, ["2"]);
    const ins = addInsert(p);
    p = setInsertCell(ins.pending, ins.tempId, "name", "new");
    const dirty: Tab = { ...table, id: "public.orders", table: { schema: "public", name: "orders" }, pending: p, uncertainSave: true };
    const r = restoreTabs(JSON.stringify(serializeTabs([dirty, table], "public.orders")), new Set());
    expect(r.tabs[0]).toEqual(dirty);
    expect(r.tabs[1]).toEqual(table);
    expect(JSON.stringify(serializeTabs([table], "public.users"))).toBe(
      '{"v":1,"activeTabId":"public.users","tabs":[{"kind":"table","id":"public.users","table":{"schema":"public","name":"users"},"view":{"mode":"data","filter":"id > 1","sort":null}}]}',
    );
    // A clean tab with a stale uncertain mark is written clean too.
    expect(JSON.stringify(serializeTabs([{ ...table, uncertainSave: true } as Tab], "public.users"))).not.toContain("uncertainSave");
  });

  it("an unreadable stored pending is dropped, the tab restores clean and droppedEdits is set", () => {
    const stored = (pending: unknown, ref: unknown = table.table) =>
      JSON.stringify({ v: 1, activeTabId: null, tabs: [{ kind: "table", id: table.id, table: ref, view: table.view, pending, uncertainSave: true }] });
    const edit = { '["1"]': { key: ["1"], values: { name: "x" } } };
    const cases: Record<string, [unknown, unknown?]> = {
      "a non-array key": [{ edits: { '"1"': { key: "1", values: { name: "x" } } }, deletes: {}, inserts: [] }],
      "a numeric value": [{ edits: { '["1"]': { key: ["1"], values: { name: 5 } } }, deletes: {}, inserts: [] }],
      "a record key that doesn't match its key": [{ edits: {}, deletes: { '["1"]': ["2"] }, inserts: [] }],
      "an edit without values": [{ edits: { '["1"]': { key: ["1"], values: {} } }, deletes: { '["2"]': ["2"] }, inserts: [] }],
      "a repeated new-row id": [{ edits: {}, deletes: {}, inserts: [{ tempId: "a", values: {} }, { tempId: "a", values: {} }] }],
      "a table without a name": [{ edits: edit, deletes: {}, inserts: [] }, { schema: "public" }],
    };
    for (const [name, [pending, ref]] of Object.entries(cases)) {
      const r = restoreTabs(stored(pending, ref), new Set());
      expect(r.tabs[0], name).toEqual(ref ? { ...table, table: ref } : table);
      expect(r.droppedEdits, name).toBe(true);
      expect("restoredEdits" in r, name).toBe(false);
    }
    // A row marked for deletion keeps its edits (struck through): that is kept.
    const both = restoreTabs(stored({ edits: edit, deletes: { '["1"]': ["1"] }, inserts: [] }), new Set());
    expect(both.restoredEdits).toEqual(["users"]);
    expect("droppedEdits" in both).toBe(false);
    const clean = restoreTabs(JSON.stringify(serializeTabs([table], table.id)), new Set());
    expect("droppedEdits" in clean).toBe(false);
    expect("restoredEdits" in clean).toBe(false);
  });

  it("a tab with a save running is stored with the uncertain mark", () => {
    const dirty: Tab = { ...table, pending: setCell(emptyPending, ["1"], "name", "x", "a"), saving: true } as Tab;
    const r = restoreTabs(JSON.stringify(serializeTabs([dirty], table.id)), new Set());
    expect(r.tabs[0]).toEqual({ ...table, pending: (dirty as TableTab).pending, uncertainSave: true });
    // Without changes nothing new is written, save running or not.
    expect(JSON.stringify(serializeTabs([{ ...table, saving: true } as Tab], table.id))).toBe(JSON.stringify(serializeTabs([table], table.id)));
  });

  it("a save starting and a successful save write the tab state right away", async () => {
    vi.useFakeTimers();
    let commit!: () => void;
    vi.mocked(api.applyChanges).mockReturnValue(new Promise((r) => (commit = () => r({ applied: 1 }))));
    const stop = startTabPersistence();
    const dirty: Tab = { ...table, pending: setCell(emptyPending, ["1"], "name", "x", "a") } as Tab;
    useApp.setState({ screen: "workspace", tabs: [dirty], activeTabId: table.id });
    await vi.advanceTimersByTimeAsync(1000);
    expect(api.saveTabState).toHaveBeenCalledTimes(1);
    const last = () => {
      const calls = vi.mocked(api.saveTabState).mock.calls;
      return JSON.parse(calls[calls.length - 1][0]).tabs[0];
    };

    const saved = useApp.getState().saveTableChanges(table.id);
    expect(api.saveTabState).toHaveBeenCalledTimes(2);
    expect(last()).toMatchObject({ pending: (dirty as TableTab).pending, uncertainSave: true });
    commit();
    expect(await saved).toBe(true);
    expect(api.saveTabState).toHaveBeenCalledTimes(3);
    expect(last()).not.toHaveProperty("pending");
    expect(last()).not.toHaveProperty("uncertainSave");
    stop();
  });

  it("a save that fails while starting or cut off by a crash writes what to restore", async () => {
    vi.useFakeTimers();
    const stop = startTabPersistence();
    const p = setCell(emptyPending, ["1"], "name", "x", "a");
    const dirty: Tab = { ...table, pending: p } as Tab;
    const last = () => {
      const calls = vi.mocked(api.saveTabState).mock.calls;
      return JSON.parse(calls[calls.length - 1][0]).tabs[0];
    };
    useApp.setState({ screen: "workspace", tabs: [dirty], activeTabId: table.id });
    await vi.advanceTimersByTimeAsync(1000);

    // A definite failure: nothing was saved, so the changes come back unmarked.
    vi.mocked(api.applyChanges).mockRejectedValueOnce({ kind: "query", message: "duplicate key" });
    expect(await useApp.getState().saveTableChanges(table.id)).toBe(false);
    expect(last()).toMatchObject({ pending: p });
    expect(last()).not.toHaveProperty("uncertainSave");
    const restored = restoreTabs(JSON.stringify({ v: 1, activeTabId: table.id, tabs: [last()] }), new Set());
    expect(restored.tabs[0]).toEqual({ ...table, pending: p });

    // The commit's outcome is unknown: they come back marked, so saving again asks first.
    vi.mocked(api.applyChanges).mockRejectedValueOnce({ kind: "uncertain", message: "connection lost during COMMIT" });
    expect(await useApp.getState().saveTableChanges(table.id)).toBe(false);
    expect(last()).toMatchObject({ pending: p, uncertainSave: true });

    // A partial save whose next write got no answer (`uncertain: true`): marked too.
    useApp.setState({ tabs: [dirty] });
    await vi.advanceTimersByTimeAsync(1000);
    expect(last()).not.toHaveProperty("uncertainSave");
    vi.mocked(api.applyChanges).mockRejectedValueOnce({ kind: "partlySaved", message: "Saved 0 of 1 changes", applied: 0, uncertain: true });
    expect(await useApp.getState().saveTableChanges(table.id)).toBe(false);
    expect(last()).toMatchObject({ pending: p, uncertainSave: true });
    stop();
  });

  it("a stored uncertain mark without pending changes restores clean", () => {
    const json = JSON.stringify({ v: 1, activeTabId: table.id, tabs: [{ kind: "table", id: table.id, table: (table as TableTab).table, view: table.view, uncertainSave: true }] });
    const r = restoreTabs(json, new Set());
    expect(r.tabs[0]).toEqual(table);
    expect("restoredEdits" in r).toBe(false);
    expect("droppedEdits" in r).toBe(false);
  });

  it("a failed tab-state write says so without blocking; the next change retries and clears it", async () => {
    vi.useFakeTimers();
    vi.spyOn(console, "error").mockImplementation(() => {});
    const stop = startTabPersistence();
    vi.mocked(api.saveTabState).mockRejectedValueOnce({ kind: "storage", message: "disk full" });
    useApp.setState({ screen: "workspace", tabs: [q()], activeTabId: "q:1" });
    await vi.advanceTimersByTimeAsync(500);
    const notice = "Couldn't save your open tabs: disk full. Unsaved changes won't be restored after a crash.";
    expect(tabsNotSaved("disk full")).toBe(notice);
    expect(useApp.getState()).toMatchObject({ tabsNotice: notice, tabsSaveBlocked: false });

    useApp.setState({ tabs: [q({ sql: "select 2" })] });
    await vi.advanceTimersByTimeAsync(500);
    expect(api.saveTabState).toHaveBeenCalledTimes(2);
    expect(useApp.getState().tabsNotice).toBeNull();

    // A newer notice is left alone by the next successful save.
    vi.mocked(api.saveTabState).mockRejectedValueOnce({ kind: "storage", message: "disk full" });
    useApp.setState({ tabs: [q({ sql: "select 3" })] });
    await vi.advanceTimersByTimeAsync(500);
    expect(useApp.getState().tabsNotice).toBe(notice);
    useApp.setState({ tabsNotice: "something else" });
    useApp.setState({ tabs: [q({ sql: "select 4" })] });
    await vi.advanceTimersByTimeAsync(500);
    expect(api.saveTabState).toHaveBeenCalledTimes(4);
    expect(useApp.getState().tabsNotice).toBe("something else");
    stop();
  });

  it("discarding edits in the leave prompt writes the tab state at once", async () => {
    vi.useFakeTimers();
    const stop = startTabPersistence();
    const dirty: Tab = { ...table, pending: setCell(emptyPending, ["1"], "name", "x", "a") } as Tab;
    useApp.setState({ screen: "workspace", tabs: [dirty, q()], activeTabId: table.id });
    await vi.advanceTimersByTimeAsync(1000);
    vi.mocked(api.saveTabState).mockClear();

    useApp.getState().closeTab(table.id);
    expect(useApp.getState().leave?.stage).toBe("edits");
    await useApp.getState().resolveLeave("discard");
    // No 500 ms wait: a crash right after must not bring the discarded changes back.
    expect(api.saveTabState).toHaveBeenCalledTimes(1);
    expect(vi.mocked(api.saveTabState).mock.calls[0][0]).not.toContain('"pending"');
    expect(useApp.getState().tabs.map((t) => t.id)).toEqual(["q:1"]);
    stop();
  });

  it("restoredEdits names the tables whose changes came back", () => {
    const dirty = (name: string): Tab => ({
      ...table, id: `public.${name}`, table: { schema: "public", name }, pending: setCell(emptyPending, ["1"], "name", "x", "a"),
    });
    const r = restoreTabs(JSON.stringify(serializeTabs([dirty("accounts"), q(), table, dirty("orders")], "q:1")), new Set());
    expect(r.restoredEdits).toEqual(["accounts", "orders"]);
    expect("droppedEdits" in r).toBe(false);
  });

  it("roundtrip keeps query text, params, table filter and active tab", () => {
    const json = JSON.stringify(serializeTabs([table, q()], "q:1"));
    const r = restoreTabs(json, new Set());
    expect(r.activeTabId).toBe("q:1");
    expect(r.tabs[0]).toEqual(table);
    expect(r.tabs[1]).toMatchObject({ kind: "query", sql: "select :a", params: { a: null }, schema: "public", txStatus: "idle", note: null });
  });

  it("invalid json or other version restores nothing and reports a failure; nothing stored is fine", () => {
    expect(restoreTabs("{nope", new Set())).toEqual({ tabs: [], activeTabId: null, failed: true });
    expect(restoreTabs(JSON.stringify({ v: 2, activeTabId: null, tabs: [] }), new Set())).toMatchObject({ tabs: [], failed: true });
    expect(restoreTabs(null, new Set())).toEqual({ tabs: [], activeTabId: null, failed: false });
    expect(restoreTabs(JSON.stringify(serializeTabs([q()], "q:1")), new Set()).failed).toBe(false);
  });

  it("without the saved-query list, saved links are kept", () => {
    const json = JSON.stringify(serializeTabs([q({ savedQueryId: "s1", savedSql: "x" }), q({ id: "q:2" })], "q:1"));
    const tabs = restoreTabs(json, null).tabs;
    expect(tabs[0]).toMatchObject({ savedQueryId: "s1", savedSql: "x" });
    expect(tabs[1]).toMatchObject({ savedQueryId: null, savedSql: null });
  });

  it("an unknown transaction state is persisted as possibly open", () => {
    const json = JSON.stringify(serializeTabs([q({ txStatus: "unknown" })], "q:1"));
    expect((restoreTabs(json, new Set()).tabs[0] as QueryTab).note).toBe(RESTORE_NOTE);
  });

  it("discarding stored tabs overwrites them with the current tabs and unblocks saving", async () => {
    vi.useFakeTimers();
    const stop = startTabPersistence();
    useApp.setState({ screen: "workspace", tabsSaveBlocked: true, tabsNotice: "broken", tabs: [q()], activeTabId: "q:1" });
    await discardStoredTabs();
    expect(api.saveTabState).toHaveBeenCalledTimes(1);
    expect(api.saveTabState).toHaveBeenCalledWith(JSON.stringify(serializeTabs([q()], "q:1")));
    expect(useApp.getState().tabsSaveBlocked).toBe(false);
    expect(useApp.getState().tabsNotice).toBeNull();
    useApp.setState({ tabs: [q({ sql: "select 2" })] });
    await vi.advanceTimersByTimeAsync(1000);
    expect(api.saveTabState).toHaveBeenCalledTimes(2);
    stop();
  });

  it("a change made while discarding stored tabs is saved after the write", async () => {
    vi.useFakeTimers();
    const stop = startTabPersistence();
    let finish!: () => void;
    vi.mocked(api.saveTabState).mockImplementationOnce(() => new Promise<void>((r) => (finish = r)));
    useApp.setState({ screen: "workspace", tabsSaveBlocked: true, tabsNotice: "broken", tabs: [q()], activeTabId: "q:1" });
    const discarding = discardStoredTabs();
    // Still blocked: the debounced save skips this change.
    useApp.setState({ tabs: [q({ sql: "select 2" })] });
    await vi.advanceTimersByTimeAsync(1000);
    expect(api.saveTabState).toHaveBeenCalledTimes(1);
    finish();
    await discarding;
    await vi.advanceTimersByTimeAsync(0);
    expect(api.saveTabState).toHaveBeenCalledTimes(2);
    expect(JSON.parse(vi.mocked(api.saveTabState).mock.calls[1][0]).tabs[0].sql).toBe("select 2");
    stop();
  });

  it("a failed discard keeps saving blocked and says why", async () => {
    vi.mocked(api.saveTabState).mockRejectedValue({ kind: "internal", message: "disk full" });
    useApp.setState({ screen: "workspace", tabsSaveBlocked: true, tabsNotice: "broken", tabs: [q()], activeTabId: "q:1" });
    await discardStoredTabs();
    expect(useApp.getState().tabsSaveBlocked).toBe(true);
    expect(useApp.getState().tabsNotice).toBe("Couldn't discard the saved tabs: disk full");
  });

  it("saving is skipped while blocked after a failed restore", async () => {
    vi.useFakeTimers();
    const stop = startTabPersistence();
    useApp.setState({ screen: "workspace", tabsSaveBlocked: true, tabs: [q()], activeTabId: "q:1" });
    vi.advanceTimersByTime(1000);
    await flushTabState();
    expect(api.saveTabState).not.toHaveBeenCalled();
    useApp.setState({ tabsSaveBlocked: false });
    await flushTabState();
    expect(api.saveTabState).toHaveBeenCalledTimes(1);
    stop();
  });

  it("deleted saved query becomes unsaved", () => {
    const json = JSON.stringify(serializeTabs([q({ savedQueryId: "s1", savedSql: "x" })], "q:1"));
    expect(restoreTabs(json, new Set(["s1"])).tabs[0]).toMatchObject({ savedQueryId: "s1", savedSql: "x" });
    expect(restoreTabs(json, new Set()).tabs[0]).toMatchObject({ savedQueryId: null, savedSql: null, sql: "select :a" });
  });

  it("txOpen sets the restore note", () => {
    const json = JSON.stringify(serializeTabs([q({ txStatus: "failed" }), q({ id: "q:2" })], "q:1"));
    const tabs = restoreTabs(json, new Set()).tabs as QueryTab[];
    expect(tabs[0].note).toBe(RESTORE_NOTE);
    expect(tabs[0].txStatus).toBe("idle");
    expect(tabs[1].note).toBeNull();
  });

  it("a tab-state write waiting or running counts as pending, and listeners hear each change", async () => {
    vi.useFakeTimers();
    const heard = vi.fn();
    const off = onTabStatePendingChange(heard);
    const stop = startTabPersistence();
    expect(tabStatePending()).toBe(false);
    useApp.setState({ screen: "workspace", tabs: [q()], activeTabId: "q:1" });
    expect(tabStatePending()).toBe(true);
    expect(heard).toHaveBeenCalled();
    let finish!: () => void;
    vi.mocked(api.saveTabState).mockImplementation(() => new Promise<void>((resolve) => (finish = resolve)));
    vi.advanceTimersByTime(500);
    // The write is running: still pending until it finishes.
    expect(tabStatePending()).toBe(true);
    finish();
    await vi.waitFor(() => expect(tabStatePending()).toBe(false));
    off();
    stop();
  });

  it("debounced save fires once after 500 ms of edits", () => {
    vi.useFakeTimers();
    const stop = startTabPersistence();
    useApp.setState({ screen: "workspace", tabs: [q()], activeTabId: "q:1" });
    vi.advanceTimersByTime(300);
    useApp.setState({ tabs: [q({ sql: "select 2" })] });
    vi.advanceTimersByTime(499);
    expect(api.saveTabState).not.toHaveBeenCalled();
    vi.advanceTimersByTime(1);
    expect(api.saveTabState).toHaveBeenCalledTimes(1);
    expect(JSON.parse(vi.mocked(api.saveTabState).mock.calls[0][0]).tabs[0].sql).toBe("select 2");
    stop();
  });

  it("flush saves immediately", async () => {
    vi.useFakeTimers();
    const stop = startTabPersistence();
    useApp.setState({ screen: "workspace", tabs: [q()], activeTabId: "q:1" });
    await flushTabState();
    expect(api.saveTabState).toHaveBeenCalledTimes(1);
    vi.advanceTimersByTime(1000);
    expect(api.saveTabState).toHaveBeenCalledTimes(1);
    stop();
  });

  it("flush outside the workspace saves nothing and a failed save does not throw", async () => {
    await flushTabState();
    expect(api.saveTabState).not.toHaveBeenCalled();
    vi.spyOn(console, "error").mockImplementation(() => {});
    vi.mocked(api.saveTabState).mockRejectedValue(new Error("disk"));
    useApp.setState({ screen: "workspace", tabs: [q()], activeTabId: "q:1" });
    await expect(flushTabState()).resolves.toBeUndefined();
  });
});

describe("query tab database", () => {
  const base = { kind: "query", id: "q1", title: "Q", sql: "select 1", savedQueryId: null, savedSql: null, params: {}, schema: null, txOpen: false };
  it("round-trips", () => {
    const tab = { ...base, kind: "query" as const, database: "analytics" };
    const json = JSON.stringify({ v: 1, activeTabId: "q1", tabs: [tab] });
    const restored = restoreTabs(json, null).tabs[0] as QueryTab;
    expect(restored.database).toBe("analytics");
    expect(serializeTabs([restored], "q1").tabs[0]).toMatchObject({ database: "analytics" });
  });
  it("a stored tab without the field restores with null", () => {
    const json = JSON.stringify({ v: 1, activeTabId: "q1", tabs: [base] });
    expect((restoreTabs(json, null).tabs[0] as QueryTab).database).toBeNull();
  });
});
