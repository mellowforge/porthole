import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ConnectedInfo, SchemaTree, StatementResult } from "../api/types";

vi.mock("../api/tauri", () => ({
  listConnections: vi.fn(),
  connect: vi.fn(),
  disconnect: vi.fn(),
  schemaTree: vi.fn(),
  targetColumns: vi.fn(),
  listSavedQueries: vi.fn(),
  loadTabState: vi.fn(),
  analyzeSql: vi.fn(),
  runQuery: vi.fn(),
  cancelQuery: vi.fn(),
  applyChanges: vi.fn(),
  endTransaction: vi.fn(),
}));

import * as api from "../api/tauri";
import { EDITS_NOT_RESTORED, serializeTabs, TABS_NOT_RESTORED } from "./persist";
import { defaultSchemaFor, EXPORT_RUNNING, LOST_TX_NOTE, quitNeedsAsk, useApp } from "./store";
import type { QueryTab, Tab, TableTab } from "./store";
import { emptyPending, keyId, setCell } from "../table/pending";

const tree: SchemaTree = { tables: ["users"], views: [], functions: [] };
const info = { profile: { id: "c1" }, serverVersion: "17", schemas: ["public", "x"], schema: "public" } as unknown as ConnectedInfo;
const t = (name: string) => ({ schema: "public", name });

beforeEach(() => {
  vi.resetAllMocks();
  vi.useRealTimers();
  vi.mocked(api.listSavedQueries).mockResolvedValue([]);
  vi.mocked(api.loadTabState).mockResolvedValue(null);
  vi.mocked(api.analyzeSql).mockResolvedValue({ statements: [], params: [] });
  useApp.setState(useApp.getInitialState(), true);
});

const queryTab = (over: Partial<QueryTab> = {}): QueryTab => ({
  kind: "query", id: "q1", title: "Query 1", sql: "select 1", savedQueryId: null, savedSql: null, params: {}, schema: null,
  run: { status: "idle", results: [], selected: 0, startedAt: null, sqlAtRun: null, pending: null, error: null, statementCount: null },
  txStatus: "idle", note: null, database: null, ...over,
});
const qt = () => useApp.getState().tabs[0] as QueryTab;
const savedTabJson = JSON.stringify({
  v: 1,
  activeTabId: "q:1",
  tabs: [{ kind: "query", id: "q:1", title: "Active users", sql: "select 1", savedQueryId: "s1", savedSql: "select 1", params: {}, schema: null, txOpen: false }],
});

describe("store", () => {
  it("openTable twice keeps one tab", () => {
    useApp.getState().openTable(t("users"));
    useApp.getState().openTable(t("users"));
    expect(useApp.getState().tabs).toHaveLength(1);
    expect(useApp.getState().activeTabId).toBe("public.users");
  });

  it("closeTab activates right neighbour", () => {
    const s = useApp.getState();
    s.openTable(t("a"));
    s.openTable(t("b"));
    s.openTable(t("c"));
    useApp.getState().closeTab("public.b");
    expect(useApp.getState().activeTabId).toBe("public.c");
    useApp.getState().closeTab("public.c");
    expect(useApp.getState().activeTabId).toBe("public.a");
    useApp.getState().closeTab("public.a");
    expect(useApp.getState().activeTabId).toBeNull();
  });

  it("setActiveTab activates an existing tab and ignores unknown ids", () => {
    const s = useApp.getState();
    s.openTable(t("a"));
    s.openTable(t("b"));
    useApp.getState().setActiveTab("public.a");
    expect(useApp.getState().activeTabId).toBe("public.a");
    useApp.getState().setActiveTab("public.zzz");
    expect(useApp.getState().activeTabId).toBe("public.a");
  });

  it("connectTo switches to workspace and loads tree", async () => {
    vi.mocked(api.connect).mockResolvedValue(info);
    vi.mocked(api.schemaTree).mockResolvedValue(tree);
    useApp.getState().openTable(t("old"));
    await useApp.getState().connectTo("c1");
    const s = useApp.getState();
    expect(api.schemaTree).toHaveBeenCalledWith("public");
    expect(s.screen).toBe("workspace");
    expect(s.active).toEqual(info);
    expect(s.tree).toEqual(tree);
    expect(s.tabs).toEqual([]);
  });

  it("connectTo restores tabs", async () => {
    vi.mocked(api.connect).mockResolvedValue(info);
    vi.mocked(api.schemaTree).mockResolvedValue(tree);
    vi.mocked(api.targetColumns).mockResolvedValue({});
    vi.mocked(api.listSavedQueries).mockResolvedValue([{ id: "s1" }] as never);
    vi.mocked(api.loadTabState).mockResolvedValue(
      JSON.stringify({
        v: 1,
        activeTabId: "q:1",
        tabs: [
          { kind: "table", id: "public.users", table: t("users"), view: { mode: "data", filter: "id > 1", sort: null } },
          { kind: "query", id: "q:1", title: "Query 1", sql: "select 1", savedQueryId: "gone", savedSql: "x", params: {}, schema: null, txOpen: true },
        ],
      }),
    );
    await useApp.getState().connectTo("c1");
    const s = useApp.getState();
    expect(s.activeTabId).toBe("q:1");
    expect(s.tabs).toHaveLength(2);
    expect(s.tabs[1]).toMatchObject({ sql: "select 1", savedQueryId: null, txStatus: "idle" });
    expect(s.tabs[1].kind === "query" && s.tabs[1].note).toContain("open transaction");
  });

  it("connectTo failure keeps connections screen and rethrows", async () => {
    const err = { kind: "connection", message: "boom" };
    vi.mocked(api.connect).mockImplementation(() => Promise.reject(err));
    await expect(useApp.getState().connectTo("c1")).rejects.toEqual(err);
    expect(useApp.getState().screen).toBe("connections");
    expect(useApp.getState().active).toBeNull();
  });

  it("connectTo disconnects and stays on connections when the schema tree fails", async () => {
    const err = { kind: "query", message: "permission denied for schema public" };
    vi.mocked(api.connect).mockResolvedValue(info);
    vi.mocked(api.schemaTree).mockImplementation(() => Promise.reject(err));
    vi.mocked(api.disconnect).mockImplementation(() => Promise.reject({ kind: "connection", message: "gone" }));
    await expect(useApp.getState().connectTo("c1")).rejects.toEqual(err);
    const s = useApp.getState();
    expect(api.disconnect).toHaveBeenCalledTimes(1);
    expect(s.screen).toBe("connections");
    expect(s.active).toBeNull();
    expect(s.tree).toBeNull();
  });

  it("setSchema failure keeps the current schema and records the error", async () => {
    vi.mocked(api.connect).mockResolvedValue(info);
    vi.mocked(api.schemaTree).mockResolvedValue(tree);
    await useApp.getState().connectTo("c1");
    vi.mocked(api.schemaTree).mockImplementation(() => Promise.reject({ kind: "query", message: "denied" }));
    await useApp.getState().setSchema("x");
    expect(useApp.getState().schemaError).toBe("denied");
    expect(useApp.getState().active?.schema).toBe("public");
    expect(useApp.getState().tree).toEqual(tree);
    vi.mocked(api.schemaTree).mockResolvedValue(tree);
    await useApp.getState().setSchema("x");
    expect(useApp.getState().schemaError).toBeNull();
  });

  it("setSchema reloads tree and updates active.schema", async () => {
    vi.mocked(api.connect).mockResolvedValue(info);
    vi.mocked(api.schemaTree).mockResolvedValue(tree);
    await useApp.getState().connectTo("c1");
    const other: SchemaTree = { tables: ["z"], views: [], functions: [] };
    vi.mocked(api.schemaTree).mockResolvedValue(other);
    await useApp.getState().setSchema("x");
    expect(useApp.getState().active?.schema).toBe("x");
    expect(useApp.getState().tree).toEqual(other);
  });

  it("loadConnections stores profiles; openConnections returns to list", async () => {
    vi.mocked(api.listConnections).mockResolvedValue([{ id: "c1" }] as never);
    await useApp.getState().loadConnections();
    expect(useApp.getState().connections).toHaveLength(1);
    useApp.setState({ screen: "workspace" });
    useApp.getState().openConnections("c1");
    expect(useApp.getState().screen).toBe("connections");
    expect(useApp.getState().selectedConnectionId).toBe("c1");
  });

  it("History is closed after switching connection and connecting again", async () => {
    vi.mocked(api.connect).mockResolvedValue(info);
    vi.mocked(api.schemaTree).mockResolvedValue(tree);
    vi.mocked(api.targetColumns).mockResolvedValue({});
    await useApp.getState().connectTo("c1");
    useApp.getState().setHistoryOpen(true);
    useApp.getState().openConnections("c1");
    expect(useApp.getState().historyOpen).toBe(false);
    useApp.getState().setHistoryOpen(true);
    await useApp.getState().connectTo("c1");
    expect(useApp.getState()).toMatchObject({ screen: "workspace", historyOpen: false });
  });

  it("newQueryTab numbers Query 1, Query 2, reuses after close", () => {
    const a = useApp.getState().newQueryTab();
    const b = useApp.getState().newQueryTab();
    expect(a.startsWith("q:")).toBe(true);
    expect(useApp.getState().tabs.map((t) => (t.kind === "query" ? t.title : ""))).toEqual(["Query 1", "Query 2"]);
    useApp.getState().closeTab(b);
    useApp.getState().newQueryTab();
    expect(useApp.getState().tabs.map((t) => (t.kind === "query" ? t.title : ""))).toEqual(["Query 1", "Query 2"]);
  });

  it("newQueryTab activates it", () => {
    useApp.getState().openTable(t("a"));
    const id = useApp.getState().newQueryTab({ sql: "select 1" });
    expect(useApp.getState().activeTabId).toBe(id);
    const tab = useApp.getState().tabs.find((x) => x.id === id);
    expect(tab?.kind === "query" && tab.sql).toBe("select 1");
  });

  it("updateTableView patches only that tab", () => {
    useApp.getState().openTable(t("a"));
    useApp.getState().openTable(t("b"));
    useApp.getState().updateTableView("public.a", { filter: "x = 1" });
    const [a, b] = useApp.getState().tabs;
    expect(a.kind === "table" && a.view).toEqual({ mode: "data", filter: "x = 1", sort: null });
    expect(b.kind === "table" && b.view).toEqual({ mode: "data", filter: null, sort: null });
  });

  it("loads completion after connect and after setSchema, into the cache", async () => {
    vi.mocked(api.connect).mockResolvedValue(info);
    vi.mocked(api.schemaTree).mockResolvedValue(tree);
    vi.mocked(api.targetColumns).mockImplementation(async (_db, schema) => ({ [`t_${schema}`]: ["id"] }));
    await useApp.getState().connectTo("c1");
    await vi.waitFor(() => expect(useApp.getState().completionCache["|public"]).toEqual({ t_public: ["id"] }));
    await useApp.getState().setSchema("x");
    await vi.waitFor(() => expect(useApp.getState().completionCache["|x"]).toEqual({ t_x: ["id"] }));
    expect(api.targetColumns).toHaveBeenCalledWith(null, "x");
  });

  it("a targetColumns failure leaves the key missing, does not block connect, and the next ask retries", async () => {
    const log = vi.spyOn(console, "error").mockImplementation(() => {});
    vi.mocked(api.connect).mockResolvedValue(info);
    vi.mocked(api.schemaTree).mockResolvedValue(tree);
    vi.mocked(api.targetColumns).mockRejectedValueOnce({ kind: "db", message: "boom" }).mockResolvedValue({ users: ["id"] });
    await useApp.getState().connectTo("c1");
    await vi.waitFor(() => expect(log).toHaveBeenCalled());
    expect(useApp.getState().screen).toBe("workspace");
    expect(useApp.getState().completionCache["|public"]).toBeUndefined();
    await useApp.getState().ensureCompletion(null, "public");
    expect(api.targetColumns).toHaveBeenCalledTimes(2);
    expect(useApp.getState().completionCache["|public"]).toEqual({ users: ["id"] });
    log.mockRestore();
  });

  it("switching the sidebar schema A to B to A refetches A", async () => {
    vi.mocked(api.connect).mockResolvedValue(info);
    vi.mocked(api.schemaTree).mockResolvedValue(tree);
    vi.mocked(api.targetColumns).mockResolvedValue({});
    await useApp.getState().connectTo("c1");
    await useApp.getState().setSchema("x");
    await useApp.getState().setSchema("public");
    await vi.waitFor(() => expect(vi.mocked(api.targetColumns).mock.calls.filter((c) => c[1] === "public")).toHaveLength(2));
  });

  it("ensureCompletion calls targetColumns once per key, even for concurrent asks", async () => {
    vi.mocked(api.targetColumns).mockImplementation(async (db, schema) => ({ [`${db}_${schema}`]: ["id"] }));
    const { ensureCompletion } = useApp.getState();
    await Promise.all([ensureCompletion("analytics", "public"), ensureCompletion("analytics", "public"), ensureCompletion("analytics", "rollups")]);
    await ensureCompletion("analytics", "public");
    expect(api.targetColumns).toHaveBeenCalledTimes(2);
    const cache = useApp.getState().completionCache;
    expect(cache["analytics|public"]).toEqual({ analytics_public: ["id"] });
    expect(cache["analytics|rollups"]).toEqual({ analytics_rollups: ["id"] });
    expect(cache["analytics|public"]).not.toEqual(cache["analytics|rollups"]);
  });

  describe("retargetQueryTab", () => {
    it("an idle tab switches database and resets the schema", () => {
      useApp.setState({ tabs: [queryTab({ schema: "audit" })], activeTabId: "q1" });
      useApp.getState().retargetQueryTab("q1", "analytics");
      expect(qt()).toMatchObject({ database: "analytics", schema: null });
      expect(useApp.getState().leave).toBeNull();
    });

    it("the same database does nothing", () => {
      useApp.setState({ tabs: [queryTab({ database: "analytics", schema: "rollups" })], activeTabId: "q1" });
      useApp.getState().retargetQueryTab("q1", "analytics");
      expect(qt()).toMatchObject({ database: "analytics", schema: "rollups" });
    });

    it("an open transaction asks first and the database is unchanged until Commit or Roll back", async () => {
      vi.mocked(api.endTransaction).mockResolvedValue("idle");
      useApp.setState({ tabs: [queryTab({ txStatus: "open" })], activeTabId: "q1" });
      useApp.getState().retargetQueryTab("q1", "analytics");
      expect(useApp.getState().leave).toMatchObject({ reason: "retarget", stage: "tx", tabIds: ["q1"] });
      expect(qt().database).toBeNull();
      await useApp.getState().resolveLeave("rollback");
      expect(qt()).toMatchObject({ database: "analytics", schema: null, txStatus: "idle" });
    });

    it("is refused while the tab runs or waits for a Production confirmation", () => {
      for (const status of ["running", "confirm"] as const) {
        useApp.setState({ tabs: [queryTab({ run: { ...queryTab().run, status }, schema: "audit" })], activeTabId: "q1", leave: null });
        useApp.getState().retargetQueryTab("q1", "analytics");
        expect(qt()).toMatchObject({ database: null, schema: "audit" });
        expect(useApp.getState().leave).toBeNull();
      }
    });

    it("Cancel leaves it unchanged", async () => {
      useApp.setState({ tabs: [queryTab({ txStatus: "open", schema: "audit" })], activeTabId: "q1" });
      useApp.getState().retargetQueryTab("q1", "analytics");
      await useApp.getState().resolveLeave("cancel");
      expect(useApp.getState().leave).toBeNull();
      expect(qt()).toMatchObject({ database: null, schema: "audit" });
    });
  });

  describe("on a SQLite connection", () => {
    const sqliteInfo = {
      profile: { id: "s1", name: "Local SQLite", kind: "sqlite", transport: "file", filePath: "/Users/me/data/app.sqlite" },
      serverVersion: "SQLite 3.50.4",
      schemas: ["main"],
      schema: "main",
    } as unknown as ConnectedInfo;
    const completed = { kind: "completed", statementCount: 0, startedAt: 1, txStatus: "idle" } as never;

    it("a query tab's runQuery sends database: null, whatever the tab or the caller names", async () => {
      vi.mocked(api.runQuery).mockResolvedValue(completed);
      useApp.setState({ active: sqliteInfo });
      const id = useApp.getState().newQueryTab();
      await useApp.getState().runQuery(id, { kind: "all" });
      expect(vi.mocked(api.runQuery).mock.calls[0][1]).toMatchObject({ database: null });
      // A tab state saved by an earlier build may name the file as the tab's database.
      useApp.setState({ tabs: [queryTab({ database: "app.sqlite" })], activeTabId: "q1" });
      await useApp.getState().runQuery("q1", { kind: "all" });
      await useApp.getState().runQuery("q1", { kind: "all" }, { database: "other" });
      expect(vi.mocked(api.runQuery).mock.calls[1][1]).toMatchObject({ database: null });
      expect(vi.mocked(api.runQuery).mock.calls[2][1]).toMatchObject({ database: null });
      expect(qt().run.databaseAtRun).toBeNull();
    });

    it("retargeting keeps the tab on the file", () => {
      useApp.setState({ active: sqliteInfo, tabs: [queryTab({ schema: "aux" })], activeTabId: "q1" });
      useApp.getState().retargetQueryTab("q1", "app.sqlite");
      useApp.getState().retargetQueryTab("q1", "other");
      expect(qt()).toMatchObject({ database: null, schema: "aux" });
      expect(useApp.getState().leave).toBeNull();
    });

    it("the default schema is main", () => {
      expect(defaultSchemaFor(null, sqliteInfo)).toBe("main");
      expect(defaultSchemaFor("app.sqlite", sqliteInfo)).toBe("main");
      expect(defaultSchemaFor(null, { ...sqliteInfo, schema: "" } as ConnectedInfo)).toBe("main");
    });
  });

  it("runQuery sends the tab's database", async () => {
    vi.mocked(api.runQuery).mockResolvedValue({ kind: "completed", statementCount: 0, startedAt: 1, txStatus: "idle" } as never);
    useApp.setState({ tabs: [queryTab({ database: "analytics" })], activeTabId: "q1" });
    await useApp.getState().runQuery("q1", { kind: "all" });
    expect(vi.mocked(api.runQuery).mock.calls[0][1]).toMatchObject({ database: "analytics" });
  });

  it("a run records its database; an explicit database overrides the tab's", async () => {
    vi.mocked(api.runQuery).mockResolvedValue({ kind: "completed", statementCount: 0, startedAt: 1, txStatus: "idle" } as never);
    useApp.setState({ tabs: [queryTab({ database: "analytics" })], activeTabId: "q1" });
    await useApp.getState().runQuery("q1", { kind: "all" });
    expect(qt().run.databaseAtRun).toBe("analytics");
    await useApp.getState().runQuery("q1", { kind: "all" }, { database: null });
    expect(vi.mocked(api.runQuery).mock.calls[1][1]).toMatchObject({ database: null });
    expect(qt().run.databaseAtRun).toBeNull();
  });

  it("while a tab's export runs its statement again, Run, COMMIT and ROLLBACK there are refused", async () => {
    const done = { kind: "done", rows: 5, path: "/tmp/r.csv", fileName: "r.csv" } as const;
    let finish!: () => void;
    const work = () => new Promise<typeof done>((resolve) => (finish = () => resolve(done)));
    const result = { ...queryTab().run, results: [{ index: 0 } as StatementResult] };
    useApp.setState({ tabs: [queryTab({ txStatus: "open", run: result }), queryTab({ id: "q2" })] });
    const exporting = useApp.getState().trackQueryExport("q1", work);

    await useApp.getState().runQuery("q1", { kind: "all" });
    expect(api.runQuery).not.toHaveBeenCalled();
    // The results stay; only the error is set.
    expect(qt().run).toMatchObject({ status: "idle", error: EXPORT_RUNNING, results: [{ index: 0 }] });
    await expect(useApp.getState().endTransaction("q1", true)).rejects.toMatchObject({ message: EXPORT_RUNNING });
    expect(api.endTransaction).not.toHaveBeenCalled();
    // Other tabs are not affected.
    vi.mocked(api.runQuery).mockResolvedValue({ kind: "completed", startedAt: 1, statementCount: 0, txStatus: "idle" } as never);
    await useApp.getState().runQuery("q2", { kind: "all" });
    expect(api.runQuery).toHaveBeenCalledTimes(1);

    finish();
    await expect(exporting).resolves.toEqual(done);
    vi.mocked(api.endTransaction).mockResolvedValue("idle");
    await useApp.getState().endTransaction("q1", true);
    expect(api.endTransaction).toHaveBeenCalledWith("q1", true);
  });

  it("a connection error during a query export ends the transaction and says so", async () => {
    useApp.setState({ tabs: [queryTab({ txStatus: "open" })] });
    const lost = { kind: "connection", message: "The connection to the database was lost." };
    await expect(useApp.getState().trackQueryExport("q1", () => Promise.reject(lost))).rejects.toBe(lost);
    expect(qt()).toMatchObject({ txStatus: "idle", note: LOST_TX_NOTE });
    // The export is over: the tab runs again.
    vi.mocked(api.runQuery).mockResolvedValue({ kind: "completed", startedAt: 1, statementCount: 0, txStatus: "idle" } as never);
    await useApp.getState().runQuery("q1", { kind: "all" });
    expect(api.runQuery).toHaveBeenCalledTimes(1);
  });

  it("a connection error during a run ends the transaction and says so", async () => {
    vi.mocked(api.runQuery).mockRejectedValue({ kind: "connection", message: "The connection to the database was lost. Run again to reconnect." });
    useApp.setState({ tabs: [queryTab({ txStatus: "open" }), queryTab({ id: "q2" })] });
    await useApp.getState().runQuery("q1", { kind: "all" });
    expect(qt()).toMatchObject({ txStatus: "idle", note: LOST_TX_NOTE });
    expect(qt().run).toMatchObject({ status: "idle", error: "The connection to the database was lost. Run again to reconnect." });
    // Without a transaction there is nothing to report beyond the run error.
    await useApp.getState().runQuery("q2", { kind: "all" });
    expect(useApp.getState().tabs[1]).toMatchObject({ txStatus: "idle", note: null });
  });

  it("a query error during a run keeps the transaction state", async () => {
    vi.mocked(api.runQuery).mockRejectedValue({ kind: "query", message: "A query is already running in this tab" });
    useApp.setState({ tabs: [queryTab({ txStatus: "failed" })] });
    await useApp.getState().runQuery("q1", { kind: "all" });
    expect(qt()).toMatchObject({ txStatus: "failed", note: null });
  });

  it("a run whose results never all arrive settles after the grace period", async () => {
    vi.useFakeTimers();
    const result: StatementResult = {
      index: 0, from: 0, to: 8, verb: "SELECT", kind: "read", sql: "select 1", params: {},
      outcome: { kind: "done" }, elapsedMs: 1, notices: [],
    };
    vi.mocked(api.runQuery).mockImplementation(async (_id, _req, onEvent) => {
      onEvent({ type: "finished", result });
      return { kind: "completed", statementCount: 3, startedAt: 1, txStatus: "idle" };
    });
    useApp.setState({ tabs: [queryTab()] });
    await useApp.getState().runQuery("q1", { kind: "all" });
    expect(qt().run.status).toBe("running");
    vi.advanceTimersByTime(1_999);
    expect(qt().run.status).toBe("running");
    vi.advanceTimersByTime(1);
    expect(qt().run).toMatchObject({ status: "idle", error: "2 statement result(s) did not arrive." });
    expect(qt().run.results).toHaveLength(1);
  });

  it("a run that completes normally sets no grace error", async () => {
    vi.useFakeTimers();
    vi.mocked(api.runQuery).mockResolvedValue({ kind: "completed", statementCount: 0, startedAt: 1, txStatus: "idle" });
    useApp.setState({ tabs: [queryTab()] });
    await useApp.getState().runQuery("q1", { kind: "all" });
    vi.advanceTimersByTime(5_000);
    expect(qt().run).toMatchObject({ status: "idle", error: null });
  });

  it("a failed Stop shows inline", async () => {
    vi.mocked(api.cancelQuery).mockRejectedValue({ kind: "connection", message: "cancel refused" });
    useApp.setState({ tabs: [queryTab()] });
    await useApp.getState().stopQuery("q1");
    expect(qt().run.error).toBe("Couldn't stop the query: cancel refused");
  });

  it("a run analyzes the exact SQL and sends a value for every param", async () => {
    vi.mocked(api.analyzeSql).mockResolvedValue({ statements: [], params: ["a", "b", "c"] });
    vi.mocked(api.runQuery).mockResolvedValue({ kind: "completed", statementCount: 0, startedAt: 1, txStatus: "idle" });
    useApp.setState({ tabs: [queryTab({ sql: "select :a, :b, :c", params: { a: "1", b: null, stale: "x" } })] });
    await useApp.getState().runQuery("q1", { kind: "all" });
    expect(api.analyzeSql).toHaveBeenCalledWith("select :a, :b, :c");
    expect(vi.mocked(api.runQuery).mock.calls[0][1].params).toEqual({ a: "1", b: null, c: "" });
  });

  it("a load_tab_state failure blocks saving and shows a notice until a good load", async () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    vi.mocked(api.connect).mockResolvedValue(info);
    vi.mocked(api.schemaTree).mockResolvedValue(tree);
    vi.mocked(api.targetColumns).mockResolvedValue({});
    vi.mocked(api.loadTabState).mockRejectedValue({ kind: "storage", message: "disk" });
    await useApp.getState().connectTo("c1");
    expect(useApp.getState()).toMatchObject({ tabsSaveBlocked: true, tabsNotice: TABS_NOT_RESTORED, tabs: [] });
    useApp.getState().dismissTabsNotice();
    expect(useApp.getState()).toMatchObject({ tabsSaveBlocked: true, tabsNotice: null });

    vi.mocked(api.loadTabState).mockResolvedValue("{not json");
    await useApp.getState().connectTo("c1");
    expect(useApp.getState()).toMatchObject({ tabsSaveBlocked: true, tabsNotice: TABS_NOT_RESTORED });

    vi.mocked(api.loadTabState).mockResolvedValue(null);
    await useApp.getState().connectTo("c1");
    expect(useApp.getState()).toMatchObject({ tabsSaveBlocked: false, tabsNotice: null });
  });

  it("connect says which unsaved changes came back", async () => {
    vi.mocked(api.connect).mockResolvedValue(info);
    vi.mocked(api.schemaTree).mockResolvedValue(tree);
    vi.mocked(api.targetColumns).mockResolvedValue({});
    const dirty = (name: string): TableTab => ({
      kind: "table", id: `public.${name}`, table: t(name), view: { mode: "data", filter: null, sort: null },
      pending: setCell(emptyPending, ["1"], "name", "x", "a"),
    });
    const stored = (tabs: Tab[]) => JSON.stringify(serializeTabs(tabs, tabs[0].id));

    vi.mocked(api.loadTabState).mockResolvedValue(stored([dirty("users")]));
    await useApp.getState().connectTo("c1");
    expect(useApp.getState().tabsNotice).toBe("Unsaved changes from your last session were restored in “users”. Check them before saving.");
    expect((useApp.getState().tabs[0] as TableTab).pending).toEqual(dirty("users").pending);
    expect(useApp.getState().tabsSaveBlocked).toBe(false);

    vi.mocked(api.loadTabState).mockResolvedValue(stored([dirty("users"), dirty("orders")]));
    await useApp.getState().connectTo("c1");
    expect(useApp.getState().tabsNotice).toBe("Unsaved changes from your last session were restored in 2 tabs. Check them before saving.");

    const broken = JSON.parse(stored([dirty("users"), dirty("orders")]));
    broken.tabs[1].pending.inserts = "nope";
    vi.mocked(api.loadTabState).mockResolvedValue(JSON.stringify(broken));
    await useApp.getState().connectTo("c1");
    // Some dropped, some restored: both are said.
    expect(useApp.getState()).toMatchObject({
      tabsNotice: `${EDITS_NOT_RESTORED} Unsaved changes from your last session were restored in “users”. Check them before saving.`,
      tabsSaveBlocked: false,
    });
    expect((useApp.getState().tabs[1] as TableTab).pending).toEqual(emptyPending);

    const allBroken = JSON.parse(stored([dirty("orders")]));
    allBroken.tabs[0].pending.inserts = "nope";
    vi.mocked(api.loadTabState).mockResolvedValue(JSON.stringify(allBroken));
    await useApp.getState().connectTo("c1");
    expect(useApp.getState()).toMatchObject({ tabsNotice: EDITS_NOT_RESTORED, tabsSaveBlocked: false });
  });

  it("a list_saved_queries failure keeps restored tabs linked to their saved query", async () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    vi.mocked(api.connect).mockResolvedValue(info);
    vi.mocked(api.schemaTree).mockResolvedValue(tree);
    vi.mocked(api.targetColumns).mockResolvedValue({});
    vi.mocked(api.listSavedQueries).mockRejectedValue({ kind: "storage", message: "locked" });
    vi.mocked(api.loadTabState).mockResolvedValue(savedTabJson);
    await useApp.getState().connectTo("c1");
    expect(qt()).toMatchObject({ savedQueryId: "s1", savedSql: "select 1" });
    expect(useApp.getState().tabsSaveBlocked).toBe(false);
  });
});

describe("pending row changes", () => {
  const pendingOf = (id: string) => {
    const tab = useApp.getState().tabs.find((x) => x.id === id);
    return tab?.kind === "table" ? tab.pending : undefined;
  };

  it("editCell then discardPending clears", () => {
    useApp.getState().openTable(t("users"));
    const id = "public.users";
    useApp.getState().editCell(id, ["1"], "name", "x", "a");
    useApp.getState().toggleDeleteRow(id, ["2"]);
    const tempId = useApp.getState().addRow(id);
    useApp.getState().editNewRow(id, tempId, "name", "n");
    expect(pendingOf(id)?.edits).not.toEqual({});
    expect(pendingOf(id)?.inserts).toHaveLength(1);
    useApp.getState().revertCell(id, ["1"], "name");
    expect(pendingOf(id)?.edits).toEqual({});
    useApp.getState().removeNewRow(id, tempId);
    expect(pendingOf(id)?.inserts).toEqual([]);
    useApp.getState().discardPending(id);
    expect(pendingOf(id)).toEqual({ edits: {}, deletes: {}, inserts: [] });
  });

  it("pending is per tab", () => {
    useApp.getState().openTable(t("users"));
    useApp.getState().openTable(t("orders"));
    useApp.getState().editCell("public.users", ["1"], "name", "x", "a");
    expect(Object.keys(pendingOf("public.users")!.edits)).toHaveLength(1);
    expect(pendingOf("public.orders")!.edits).toEqual({});
  });
});

describe("saveTableChanges", () => {
  const tid = "public.users";
  const tt = () => useApp.getState().tabs.find((x) => x.id === tid) as TableTab;
  const open = () => {
    useApp.setState({ tabs: [{ kind: "table", id: tid, table: t("users"), view: { mode: "data", filter: null, sort: null }, pending: emptyPending }] });
    useApp.getState().editCell(tid, ["1"], "status", "x", "a");
  };

  it("keeps an edit made while the save is in flight", async () => {
    open();
    let release!: () => void;
    vi.mocked(api.applyChanges).mockReturnValue(new Promise((r) => (release = () => r({ applied: 1 }))));
    const saving = useApp.getState().saveTableChanges(tid);
    useApp.getState().editCell(tid, ["2"], "status", "late", "b");
    useApp.getState().toggleDeleteRow(tid, ["3"]);
    release();
    expect(await saving).toBe(true);
    expect(Object.keys(tt().pending.edits)).toEqual([keyId(["2"])]);
    expect(Object.keys(tt().pending.deletes)).toEqual([keyId(["3"])]);
    expect(tt().saveNote).toBe("Saved 1 change");
  });

  it("refuses a second save for the tab while one is in flight", async () => {
    open();
    let release!: () => void;
    vi.mocked(api.applyChanges).mockReturnValue(new Promise((r) => (release = () => r({ applied: 1 }))));
    const first = useApp.getState().saveTableChanges(tid);
    expect(await useApp.getState().saveTableChanges(tid)).toBe(false);
    expect(api.applyChanges).toHaveBeenCalledTimes(1);
    release();
    expect(await first).toBe(true);
    // The flag is released afterwards, also after a failure.
    useApp.getState().editCell(tid, ["1"], "status", "y", "a");
    vi.mocked(api.applyChanges).mockRejectedValue({ kind: "query", message: "boom" });
    expect(await useApp.getState().saveTableChanges(tid)).toBe(false);
    vi.mocked(api.applyChanges).mockResolvedValue({ applied: 1 });
    expect(await useApp.getState().saveTableChanges(tid)).toBe(true);
  });

  it("clears the Saved note when pending changes again or a save starts", async () => {
    open();
    vi.mocked(api.applyChanges).mockResolvedValue({ applied: 1 });
    await useApp.getState().saveTableChanges(tid);
    expect(tt().saveNote).toBe("Saved 1 change");
    useApp.getState().editCell(tid, ["1"], "status", "z", "a");
    expect(tt().saveNote).toBeNull();
    vi.mocked(api.applyChanges).mockRejectedValue({ kind: "query", message: "no" });
    await useApp.getState().saveTableChanges(tid);
    expect(tt().saveNote).toBeNull();
    expect(tt().saveError).toBe("Nothing was saved: no");
  });
});

describe("quitNeedsAsk", () => {
  const base = { screen: "workspace" as const, tabs: [] as Tab[], leave: null };
  const table = (over: Partial<TableTab> = {}): TableTab =>
    ({ kind: "table", id: "public.users", table: t("users"), view: { mode: "data", filter: "", sort: null }, pending: emptyPending, ...over }) as TableTab;

  it("asks nothing on an idle workspace or off the workspace", () => {
    expect(quitNeedsAsk(base)).toBe(false);
    expect(quitNeedsAsk({ ...base, tabs: [queryTab()] })).toBe(false);
    expect(quitNeedsAsk({ ...base, screen: "connections", tabs: [queryTab({ txStatus: "open" })] })).toBe(false);
  });

  it("asks for an open or unknown transaction, unsaved edits, a running save, or a prompt that is up", () => {
    expect(quitNeedsAsk({ ...base, tabs: [queryTab({ txStatus: "open" })] })).toBe(true);
    expect(quitNeedsAsk({ ...base, tabs: [queryTab({ txStatus: "unknown" })] })).toBe(true);
    const edited = setCell(emptyPending, ["1"], "name", "x", "a");
    expect(quitNeedsAsk({ ...base, tabs: [table({ pending: edited })] })).toBe(true);
    expect(quitNeedsAsk({ ...base, tabs: [table({ saving: true })] })).toBe(true);
    expect(quitNeedsAsk({ ...base, leave: { reason: "quit" } as never })).toBe(true);
  });
});
