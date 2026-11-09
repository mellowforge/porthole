import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ConnectedInfo } from "../api/types";

vi.mock("../api/tauri", () => ({
  connect: vi.fn(),
  disconnect: vi.fn(),
  schemaTree: vi.fn(),
  targetColumns: vi.fn(),
  listSavedQueries: vi.fn(),
  loadTabState: vi.fn(),
  applyChanges: vi.fn(),
}));

import * as api from "../api/tauri";
import { type TableTab, useApp } from "./store";
import { firstChanges, keyId, toChangeSet } from "../table/pending";

const PARTIAL =
  "Saved 1 of 3 changes, then: E11000 duplicate key error This server has no transactions, so the saved changes stay saved; the rest are still pending.";
const table = { schema: "fx", name: "users" };
const tabId = "fx.users";
const tt = () => useApp.getState().tabs.find((t) => t.id === tabId) as TableTab;

beforeEach(() => {
  vi.resetAllMocks();
  useApp.setState(useApp.getInitialState(), true);
  useApp.getState().openTable(table);
});

/** Three distinct changes in `toChangeSet` order: a delete, a two-cell update of one row, an insert. */
function threeChanges() {
  const s = useApp.getState();
  s.toggleDeleteRow(tabId, ["3"]);
  s.editCell(tabId, ["1"], "name", "Zed", "Ann");
  s.editCell(tabId, ["1"], "age", "31", "30");
  const tempId = s.addRow(tabId);
  s.editNewRow(tabId, tempId, "email", "dup@example.com");
  return tempId;
}

describe("firstChanges", () => {
  it("is the first n changes in toChangeSet order, whole rows, for every n", () => {
    threeChanges();
    // A second edited row and a deleted row with hidden edits (never sent).
    useApp.getState().editCell(tabId, ["2"], "name", "Bo2", "Bo");
    useApp.getState().editCell(tabId, ["3"], "name", "Gone", "Cy");
    const p = tt().pending;
    const all = toChangeSet(p, table).changes;
    expect(all.map((c) => c.kind)).toEqual(["delete", "update", "update", "insert"]);
    for (let n = 0; n <= all.length + 1; n++) {
      expect(toChangeSet(firstChanges(p, n), table).changes).toEqual(all.slice(0, n));
    }
  });

  it("keeps the original types of typed rows (a MongoDB page)", () => {
    const s = useApp.getState();
    const types = (code: "o" | "t") => ({ key: { _id: code }, cells: { _id: code, name: "t" as const } });
    s.toggleDeleteRow(tabId, ["65a1b2c3d4e5f60718293a4b"], types("o"));
    s.editCell(tabId, ["65a1b2c3d4e5f60718293a4b"], "name", "x", "y", types("t"));
    s.editCell(tabId, ["k"], "name", "Zed", "Ann", types("t"));
    const p = tt().pending;
    const all = toChangeSet(p, table).changes;
    expect(all.every((c) => c.kind === "insert" || c.originalTypes)).toBe(true);
    for (let n = 0; n <= all.length; n++) {
      expect(toChangeSet(firstChanges(p, n), table).changes).toEqual(all.slice(0, n));
    }
  });

  it("an applied delete takes the row's hidden edits with it", () => {
    const s = useApp.getState();
    s.editCell(tabId, ["3"], "name", "Gone", "Cy");
    s.toggleDeleteRow(tabId, ["3"]);
    const first = firstChanges(tt().pending, 1);
    expect(Object.keys(first.deletes)).toEqual([keyId(["3"])]);
    expect(first.edits[keyId(["3"])]?.values).toEqual({ name: "Gone" });
  });
});

describe("a partly saved save", () => {
  it("applied 1 of 3: the saved change leaves the pending list, the other two stay, with the message", async () => {
    const tempId = threeChanges();
    const before = tt().reloadNonce ?? 0;
    vi.mocked(api.applyChanges).mockRejectedValue({ kind: "partlySaved", message: PARTIAL, applied: 1 });
    const saved = await useApp.getState().saveTableChanges(tabId);
    expect(saved).toBe(false);
    const t = tt();
    expect(t.saveError).toBe(PARTIAL);
    expect(t.saving).toBe(false);
    expect(t.reloadNonce).toBe(before + 1);
    const left = toChangeSet(t.pending, table).changes;
    expect(left).toHaveLength(2);
    expect(t.pending.deletes).toEqual({});
    expect(t.pending.edits[keyId(["1"])]?.values).toEqual({ name: "Zed", age: "31" });
    expect(t.pending.inserts.map((r) => r.tempId)).toEqual([tempId]);
  });

  it("applied 2 of 3 leaves only the insert", async () => {
    const tempId = threeChanges();
    vi.mocked(api.applyChanges).mockRejectedValue({ kind: "partlySaved", message: "Saved 2 of 3 changes, then: x", applied: 2 });
    await useApp.getState().saveTableChanges(tabId);
    const t = tt();
    expect(t.pending.deletes).toEqual({});
    expect(t.pending.edits).toEqual({});
    expect(t.pending.inserts.map((r) => r.tempId)).toEqual([tempId]);
    expect(t.saveError).toBe("Saved 2 of 3 changes, then: x");
  });

  it("an edit made while the save ran stays pending", async () => {
    threeChanges();
    let reject: (e: unknown) => void = () => {};
    vi.mocked(api.applyChanges).mockReturnValue(new Promise((_, r) => (reject = r)));
    const run = useApp.getState().saveTableChanges(tabId);
    useApp.getState().editCell(tabId, ["2"], "name", "Bo2", "Bo");
    reject({ kind: "partlySaved", message: PARTIAL, applied: 1 });
    await run;
    expect(tt().pending.edits[keyId(["2"])]?.values).toEqual({ name: "Bo2" });
    expect(tt().pending.deletes).toEqual({});
  });

  it("a next write with an unknown outcome makes the next save ask first", async () => {
    threeChanges();
    const message = "Saved 1 of 3 changes, then the connection failed, so the next one may or may not have been saved. Reload before saving again.";
    vi.mocked(api.applyChanges).mockRejectedValue({ kind: "partlySaved", message, applied: 1, uncertain: true });
    await useApp.getState().saveTableChanges(tabId);
    expect(tt().uncertainSave).toBe(true);
    expect(tt().saveError).toBe(message);
    expect(toChangeSet(tt().pending, table).changes).toHaveLength(2);
  });

  it("without uncertain, a partial save leaves uncertainSave as it was", async () => {
    threeChanges();
    vi.mocked(api.applyChanges).mockRejectedValue({ kind: "partlySaved", message: PARTIAL, applied: 1 });
    await useApp.getState().saveTableChanges(tabId);
    expect(tt().uncertainSave).toBeFalsy();
    useApp.setState((s) => ({ tabs: s.tabs.map((t) => (t.id === tabId ? { ...t, uncertainSave: true } : t)) }));
    vi.mocked(api.applyChanges).mockRejectedValue({ kind: "partlySaved", message: PARTIAL, applied: 1 });
    await useApp.getState().saveTableChanges(tabId);
    expect(tt().uncertainSave).toBe(true);
  });

  it("other errors keep their copy", async () => {
    threeChanges();
    vi.mocked(api.applyChanges).mockRejectedValue({ kind: "query", message: "boom" });
    await useApp.getState().saveTableChanges(tabId);
    expect(tt().saveError).toBe("Nothing was saved: boom");
    expect(toChangeSet(tt().pending, table).changes).toHaveLength(3);
  });
});

describe("connecting to MongoDB", () => {
  it("restores query tabs and table tabs (MongoDB has query tabs)", async () => {
    const info = {
      profile: { id: "c1", name: "Local MongoDB", kind: "mongodb", transport: "network", host: "localhost", port: 27017, database: "fx" },
      serverVersion: "MongoDB 8.0.34",
      schemas: ["fx"],
      schema: "fx",
      oneByOneSaves: true,
    } as unknown as ConnectedInfo;
    vi.mocked(api.connect).mockResolvedValue(info);
    vi.mocked(api.schemaTree).mockResolvedValue({ tables: ["users"], views: [], functions: [] });
    vi.mocked(api.targetColumns).mockResolvedValue({});
    vi.mocked(api.listSavedQueries).mockResolvedValue([]);
    vi.mocked(api.loadTabState).mockResolvedValue(
      JSON.stringify({
        v: 1,
        activeTabId: "q:1",
        tabs: [
          { kind: "query", id: "q:1", title: "Q", sql: "x", savedQueryId: null, savedSql: null, params: {}, schema: null, txOpen: false },
          { kind: "table", id: "fx.users", table: { schema: "fx", name: "users" }, view: { mode: "data", filter: null, sort: null } },
        ],
      }),
    );
    await useApp.getState().connectTo("c1");
    expect(useApp.getState().tabs.map((t) => t.id)).toEqual(["q:1", "fx.users"]);
    expect(useApp.getState().activeTabId).toBe("q:1");
  });
});
