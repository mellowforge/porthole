import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ConnectedInfo, RunEvent, RunOutcome, SchemaTree, TxStatus } from "../api/types";

vi.mock("../api/tauri", () => ({
  connect: vi.fn(),
  listSavedQueries: vi.fn(),
  loadTabState: vi.fn(),
  schemaTree: vi.fn(),
  targetSchemas: vi.fn(),
  targetColumns: vi.fn(),
  analyzeSql: vi.fn(),
  runQuery: vi.fn(),
  endTransaction: vi.fn(),
}));

import * as api from "../api/tauri";
import { runSchema, useApp } from "./store";
import type { QueryTab } from "./store";

const pg = { profile: { id: "c1", kind: "postgres", transport: "network", database: "app" }, serverVersion: "17", schemas: ["public", "x"], schema: "public" } as unknown as ConnectedInfo;
const my = (schema: string, schemas: string[]) =>
  ({ profile: { id: "m1", kind: "mysql", transport: "network", database: "" }, serverVersion: "MySQL 8.4.13", schemas, schema }) as unknown as ConnectedInfo;
const tree: SchemaTree = { tables: ["users"], views: [], functions: [] };
const fresh: SchemaTree = { tables: ["users"], views: [], functions: [{ name: "two_sets", signature: "IN n int", kind: "procedure" }] };

const queryTab = (over: Partial<QueryTab> = {}): QueryTab => ({
  kind: "query", id: "q1", title: "Query 1", sql: "create procedure p() begin end", savedQueryId: null, savedSql: null, params: {}, schema: null,
  run: { status: "idle", results: [], selected: 0, startedAt: null, sqlAtRun: null, pending: null, error: null, statementCount: null },
  ...over,
} as QueryTab);

beforeEach(() => {
  vi.resetAllMocks();
  vi.mocked(api.analyzeSql).mockResolvedValue({ statements: [], params: [] });
  vi.mocked(api.targetColumns).mockResolvedValue({});
  useApp.setState(useApp.getInitialState(), true);
});

describe("runSchema on a MySQL server with no database", () => {
  it("a tab with no database runs on none, not on the empty name", () => {
    expect(runSchema(null, my("", []))).toBeNull();
  });

  it("otherwise unchanged", () => {
    expect(runSchema(null, my("fx", ["fx"]))).toBe("fx");
    expect(runSchema("fx_other", my("fx", ["fx"]))).toBe("fx_other");
    expect(runSchema(null, pg)).toBeNull();
    expect(runSchema("x", pg)).toBe("x");
  });
});

describe("refreshCatalog", () => {
  it("reloads the schema list and the current schema's tree", async () => {
    useApp.setState({ active: pg, tree, schemaError: "old" });
    vi.mocked(api.targetSchemas).mockResolvedValue(["audit", "public", "x"]);
    vi.mocked(api.schemaTree).mockResolvedValue(fresh);
    await useApp.getState().refreshCatalog();
    expect(api.targetSchemas).toHaveBeenCalledWith(null);
    expect(api.schemaTree).toHaveBeenCalledWith("public");
    const s = useApp.getState();
    expect(s.active?.schemas).toEqual(["audit", "public", "x"]);
    expect(s.active?.schema).toBe("public");
    expect(s.tree).toEqual(fresh);
    expect(s.schemaError).toBeNull();
    expect(s.catalogRefreshing).toBe(false);
  });

  it("moves to the first listed schema when the current one is gone", async () => {
    useApp.setState({ active: my("", []), tree: { tables: [], views: [], functions: [] } });
    vi.mocked(api.targetSchemas).mockResolvedValue(["shop"]);
    vi.mocked(api.schemaTree).mockResolvedValue(tree);
    await useApp.getState().refreshCatalog();
    expect(api.schemaTree).toHaveBeenCalledWith("shop");
    expect(useApp.getState().active?.schema).toBe("shop");
    expect(useApp.getState().active?.schemas).toEqual(["shop"]);
    expect(useApp.getState().tree).toEqual(tree);
  });

  it("keeps a current schema that was never listed (a profile's pg_catalog, MySQL's mysql)", async () => {
    useApp.setState({ active: my("mysql", ["fx", "shop"]), tree });
    vi.mocked(api.targetSchemas).mockResolvedValue(["fx", "shop"]);
    vi.mocked(api.schemaTree).mockResolvedValue(fresh);
    await useApp.getState().refreshCatalog();
    expect(api.schemaTree).toHaveBeenCalledTimes(1);
    expect(api.schemaTree).toHaveBeenCalledWith("mysql");
    expect(useApp.getState().active?.schema).toBe("mysql");
    expect(useApp.getState().tree).toEqual(fresh);
  });

  it("moves off a listed schema that was dropped", async () => {
    useApp.setState({ active: pg, tree });
    vi.mocked(api.targetSchemas).mockResolvedValue(["public"]);
    useApp.setState({ active: { ...pg, schema: "x" } });
    vi.mocked(api.schemaTree).mockResolvedValue(fresh);
    await useApp.getState().refreshCatalog();
    expect(api.schemaTree).toHaveBeenLastCalledWith("public");
    expect(useApp.getState().active?.schema).toBe("public");
  });

  it("only the latest of two overlapping refreshes applies, and the busy flag waits for it", async () => {
    useApp.setState({ active: pg, tree });
    const older: SchemaTree = { tables: ["old"], views: [], functions: [] };
    let releaseFirst!: (t: SchemaTree) => void;
    vi.mocked(api.targetSchemas).mockResolvedValue(["public", "x"]);
    vi.mocked(api.schemaTree).mockImplementationOnce(() => new Promise((r) => (releaseFirst = r)));
    const first = useApp.getState().refreshCatalog();
    vi.mocked(api.schemaTree).mockResolvedValueOnce(fresh);
    await useApp.getState().refreshCatalog();
    expect(useApp.getState().tree).toEqual(fresh);
    expect(useApp.getState().catalogRefreshing).toBe(false);
    releaseFirst(older);
    await first;
    expect(useApp.getState().tree).toEqual(fresh);
    expect(useApp.getState().catalogRefreshing).toBe(false);
  });

  it("a reconnect to the same connection during a refresh wins", async () => {
    vi.mocked(api.listSavedQueries).mockResolvedValue([]);
    vi.mocked(api.loadTabState).mockResolvedValue(null);
    useApp.setState({ active: pg, tree });
    let fail!: (e: unknown) => void;
    vi.mocked(api.targetSchemas).mockResolvedValue(["public"]);
    vi.mocked(api.schemaTree).mockImplementationOnce(() => new Promise((_, reject) => (fail = reject)));
    const refresh = useApp.getState().refreshCatalog();
    expect(useApp.getState().catalogRefreshing).toBe(true);
    vi.mocked(api.connect).mockResolvedValue({ ...pg });
    vi.mocked(api.schemaTree).mockResolvedValueOnce(fresh);
    await useApp.getState().connectTo("c1");
    expect(useApp.getState().catalogRefreshing).toBe(false);
    fail({ kind: "connection", message: "timed out" });
    await refresh;
    expect(useApp.getState().schemaError).toBeNull();
    expect(useApp.getState().tree).toEqual(fresh);
  });

  it("does not clear a switch error that arrived during the refresh", async () => {
    useApp.setState({ active: pg, tree });
    let release!: (t: SchemaTree) => void;
    vi.mocked(api.targetSchemas).mockResolvedValue(["public", "x"]);
    vi.mocked(api.schemaTree).mockImplementationOnce(() => new Promise((r) => (release = r)));
    const refresh = useApp.getState().refreshCatalog();
    vi.mocked(api.schemaTree).mockRejectedValueOnce({ kind: "query", message: "denied" });
    await useApp.getState().setSchema("x");
    release(fresh);
    await refresh;
    expect(useApp.getState().schemaError).toBe("denied");
  });

  it("keeps the current schema when nothing is listed", async () => {
    useApp.setState({ active: my("", []), tree });
    vi.mocked(api.targetSchemas).mockResolvedValue([]);
    vi.mocked(api.schemaTree).mockResolvedValue(tree);
    await useApp.getState().refreshCatalog();
    expect(api.schemaTree).toHaveBeenCalledWith("");
    expect(useApp.getState().active?.schema).toBe("");
  });

  it("a failure keeps everything and records the error", async () => {
    useApp.setState({ active: pg, tree });
    vi.mocked(api.targetSchemas).mockResolvedValue(["public"]);
    vi.mocked(api.schemaTree).mockRejectedValue({ kind: "connection", message: "gone" });
    await useApp.getState().refreshCatalog();
    const s = useApp.getState();
    expect(s.schemaError).toBe("Couldn't refresh: gone");
    expect(s.active).toEqual(pg);
    expect(s.tree).toEqual(tree);
    expect(s.catalogRefreshing).toBe(false);
  });

  it("a schema switch during the refresh wins", async () => {
    useApp.setState({ active: pg, tree });
    let release!: (t: SchemaTree) => void;
    vi.mocked(api.targetSchemas).mockResolvedValue(["public", "x"]);
    vi.mocked(api.schemaTree).mockImplementationOnce(() => new Promise((r) => (release = r)));
    const refresh = useApp.getState().refreshCatalog();
    const other: SchemaTree = { tables: ["z"], views: [], functions: [] };
    vi.mocked(api.schemaTree).mockResolvedValueOnce(other);
    await useApp.getState().setSchema("x");
    release(fresh);
    await refresh;
    const s = useApp.getState();
    expect(s.active?.schema).toBe("x");
    expect(s.tree).toEqual(other);
    expect(s.active?.schemas).toEqual(["public", "x"]);
  });

  it("drops the window schema's autocomplete so new tables complete", async () => {
    useApp.setState({ active: pg, tree, completionCache: { "|public": { old: ["a"] } } });
    vi.mocked(api.targetSchemas).mockResolvedValue(["public"]);
    vi.mocked(api.schemaTree).mockResolvedValue(fresh);
    vi.mocked(api.targetColumns).mockResolvedValue({ users: ["id"] });
    await useApp.getState().refreshCatalog();
    expect(api.targetColumns).toHaveBeenCalledWith(null, "public");
  });
});

describe("refresh after a query tab changes the schema", () => {
  const run = (verbs: string[], txStatus: TxStatus) => {
    vi.mocked(api.runQuery).mockImplementation(async (_tab: string, _req: unknown, onEvent: (e: RunEvent) => void) => {
      verbs.forEach((verb, index) => {
        onEvent({ type: "started", index, total: verbs.length, from: 0, to: 1, verb });
        onEvent({ type: "finished", result: { index, from: 0, to: 1, outcome: { kind: "command", rowsAffected: 0 }, elapsedMs: 1 } as never });
      });
      const done: RunOutcome = { kind: "completed", statementCount: verbs.length, startedAt: 0, txStatus };
      return done;
    });
  };
  const settle = () => new Promise((r) => setTimeout(r, 0));

  beforeEach(() => {
    useApp.setState({ active: pg, tree, tabs: [queryTab()], activeTabId: "q1" });
    vi.mocked(api.targetSchemas).mockResolvedValue(["public", "x"]);
    vi.mocked(api.schemaTree).mockResolvedValue(fresh);
  });

  it("CREATE, DROP, ALTER or RENAME refreshes the sidebar", async () => {
    for (const verb of ["CREATE", "DROP", "ALTER", "RENAME"]) {
      vi.mocked(api.schemaTree).mockClear();
      run(["SELECT", verb], "idle");
      await useApp.getState().runQuery("q1", { kind: "all" });
      await settle();
      expect(api.schemaTree, verb).toHaveBeenCalledWith("public");
    }
    expect(useApp.getState().tree).toEqual(fresh);
  });

  it("a run without DDL does not", async () => {
    run(["SELECT", "INSERT", "UPDATE"], "idle");
    await useApp.getState().runQuery("q1", { kind: "all" });
    await settle();
    expect(api.schemaTree).not.toHaveBeenCalled();
  });

  it("DDL in an open transaction refreshes once the transaction commits", async () => {
    run(["BEGIN", "CREATE"], "open");
    await useApp.getState().runQuery("q1", { kind: "all" });
    await settle();
    expect(api.schemaTree).not.toHaveBeenCalled();
    vi.mocked(api.endTransaction).mockResolvedValue("idle");
    await useApp.getState().endTransaction("q1", true);
    await settle();
    expect(api.schemaTree).toHaveBeenCalledTimes(1);
  });

  it("DDL in a failed transaction refreshes only after the rollback", async () => {
    run(["BEGIN", "CREATE"], "failed");
    await useApp.getState().runQuery("q1", { kind: "all" });
    await settle();
    expect(api.schemaTree).not.toHaveBeenCalled();
    vi.mocked(api.endTransaction).mockResolvedValue("idle");
    await useApp.getState().endTransaction("q1", false);
    await settle();
    expect(api.schemaTree).toHaveBeenCalledTimes(1);
  });

  it("DDL in an open transaction refreshes once a COMMIT run closes it", async () => {
    run(["BEGIN", "CREATE"], "open");
    await useApp.getState().runQuery("q1", { kind: "all" });
    run(["COMMIT"], "idle");
    await useApp.getState().runQuery("q1", { kind: "all" });
    await settle();
    expect(api.schemaTree).toHaveBeenCalledTimes(1);
  });
});
