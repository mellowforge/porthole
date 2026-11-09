import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ConnectedInfo, RunOutcome, StatementResult } from "../api/types";

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
import { completionKey, useApp } from "./store";
import type { QueryTab } from "./store";

const mongo = { profile: { id: "g1", kind: "mongodb", transport: "network", database: "fx" }, serverVersion: "MongoDB 8.0.34", schemas: ["fx"], schema: "fx" } as unknown as ConnectedInfo;
const completed: RunOutcome = { kind: "completed", statementCount: 1, startedAt: 0, txStatus: "idle" };
const result = (verb: string, kind: "read" | "write"): StatementResult => ({
  index: 0, from: 0, to: 10, verb, kind, sql: "x", params: {},
  outcome: kind === "write" ? { kind: "affected", rows: 1 } : { kind: "rows", columns: [], rows: [], truncated: false },
  elapsedMs: 1, notices: [],
});

beforeEach(() => {
  vi.resetAllMocks();
  vi.mocked(api.analyzeSql).mockResolvedValue({ statements: [], params: [] });
  vi.mocked(api.targetColumns).mockResolvedValue({});
  useApp.setState(useApp.getInitialState(), true);
  useApp.setState({ active: mongo, tree: { tables: [], views: [], functions: [] } });
});

describe("MongoDB query tabs in the store", () => {
  it("ensureCompletion caches the database's collections and views from schema_tree", async () => {
    vi.mocked(api.schemaTree).mockResolvedValue({ tables: ["users", "orders"], views: ["active_users"], functions: [] });
    await useApp.getState().ensureCompletion(null, "fx");
    expect(api.schemaTree).toHaveBeenCalledWith("fx");
    expect(api.targetColumns).not.toHaveBeenCalled();
    expect(useApp.getState().completionCache[completionKey(null, "fx")]).toEqual({ users: [], orders: [], active_users: [] });
  });

  it("a finished insertOne refreshes the sidebar once the run returns", async () => {
    vi.mocked(api.targetSchemas).mockResolvedValue(["fx"]);
    vi.mocked(api.schemaTree).mockResolvedValue({ tables: ["users", "fresh"], views: [], functions: [] });
    vi.mocked(api.runQuery).mockImplementation(async (_id, _req, onEvent) => {
      onEvent({ type: "started", index: 0, total: 1, from: 0, to: 10, verb: "insertOne" });
      onEvent({ type: "finished", result: result("insertOne", "write") });
      return completed;
    });
    const id = useApp.getState().newQueryTab({ sql: 'db.fresh.insertOne({ a: 1 })' });
    await useApp.getState().runQuery(id, { kind: "all" });
    await vi.waitFor(() => expect(useApp.getState().tree?.tables).toEqual(["users", "fresh"]));
    expect(api.schemaTree).toHaveBeenCalledWith("fx");
    expect((useApp.getState().tabs[0] as QueryTab).txStatus).toBe("idle");
  });

  it("a finished read does not refresh the sidebar", async () => {
    vi.mocked(api.targetSchemas).mockResolvedValue(["fx"]);
    vi.mocked(api.schemaTree).mockResolvedValue({ tables: ["users"], views: [], functions: [] });
    vi.mocked(api.runQuery).mockImplementation(async (_id, _req, onEvent) => {
      onEvent({ type: "started", index: 0, total: 1, from: 0, to: 10, verb: "find" });
      onEvent({ type: "finished", result: result("find", "read") });
      return completed;
    });
    const id = useApp.getState().newQueryTab({ sql: "db.users.find()" });
    await useApp.getState().runQuery(id, { kind: "all" });
    await new Promise((r) => setTimeout(r, 20));
    expect(api.targetSchemas).not.toHaveBeenCalled();
  });
});
