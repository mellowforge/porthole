import { beforeEach, describe, expect, it, vi } from "vitest";

// A plain function (not a vi.fn spy) backs the mock: vitest's spy bookkeeping
// reports object rejections as unhandled errors.
const calls: unknown[][] = [];
let impl: (...args: unknown[]) => Promise<unknown> = () => Promise.resolve(undefined);

vi.mock("@tauri-apps/api/core", () => ({
  Channel: class {
    onmessage: (e: unknown) => void = () => {};
  },
  invoke: (...args: unknown[]) => {
    calls.push(args);
    return impl(...args);
  },
}));

import { fetchPage, listDatabases, pickDatabaseFile, runQuery, targetColumns, targetSchemas } from "./tauri";
import type { AppError, PageRequest, RunOutcome, RunRequest, TablePage } from "./types";

const req: PageRequest = {
  table: { schema: "public", name: "users" },
  filter: null,
  sort: null,
  page: 0,
  pageSize: 100,
};

describe("tauri api", () => {
  beforeEach(() => {
    calls.length = 0;
  });

  it("fetchPage passes {req} and returns the value", async () => {
    const page: TablePage = {
      columns: [],
      primaryKey: [],
      editing: { kind: "view" },
      rows: [],
      total: { kind: "exact", value: 0 },
      matches: null,
      elapsedMs: 1,
    };
    impl = () => Promise.resolve(page);
    await expect(fetchPage(req)).resolves.toBe(page);
    expect(calls).toEqual([["fetch_page", { req }]]);
  });

  it("runQuery passes a channel wired to the callback as onEvent", async () => {
    const outcome: RunOutcome = { kind: "completed", statementCount: 0, startedAt: 1, txStatus: "idle" };
    impl = () => Promise.resolve(outcome);
    const run: RunRequest = {
      sql: "select 1", target: { kind: "all" }, params: {}, schema: null,
      confirmed: false, fetchAll: false, savedQueryId: null, database: null,
    };
    const seen: unknown[] = [];
    await expect(runQuery("t1", run, (e) => seen.push(e))).resolves.toBe(outcome);
    const [cmd, args] = calls[0] as [string, { tabId: string; req: RunRequest; onEvent: { onmessage: (e: unknown) => void } }];
    expect(cmd).toBe("run_query");
    expect(args.tabId).toBe("t1");
    expect(args.req).toBe(run);
    args.onEvent.onmessage({ type: "started" });
    expect(seen).toEqual([{ type: "started" }]);
  });

  it("target lookups pass {database} and {database, schema}", async () => {
    impl = () => Promise.resolve([]);
    await listDatabases();
    await targetSchemas("analytics");
    await targetSchemas(null);
    await targetColumns("analytics", "public");
    expect(calls).toEqual([
      ["list_databases"],
      ["target_schemas", { database: "analytics" }],
      ["target_schemas", { database: null }],
      ["target_columns", { database: "analytics", schema: "public" }],
    ]);
  });

  it("pickDatabaseFile invokes pick_database_file and returns the path or null", async () => {
    impl = () => Promise.resolve("/Users/me/data/picked.sqlite");
    await expect(pickDatabaseFile()).resolves.toBe("/Users/me/data/picked.sqlite");
    impl = () => Promise.resolve(null);
    await expect(pickDatabaseFile()).resolves.toBeNull();
    expect(calls).toEqual([["pick_database_file"], ["pick_database_file"]]);
  });

  it("errors are rethrown as AppError", async () => {
    const err: AppError = { kind: "filter", message: "x", position: 2 };
    impl = () => Promise.reject(err);
    const caught = await fetchPage(req).then(
      () => null,
      (e: unknown) => e,
    );
    expect(caught).toEqual(err);
  });
});
