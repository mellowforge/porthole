import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../api/tauri", () => ({}));

import { type AppState, useApp } from "../state/store";
import { filterItems, paletteItems } from "./paletteItems";

const deps = { openHistory: vi.fn(), newWindow: vi.fn(), exportActive: vi.fn() };
const tableTab = { kind: "table", id: "public.orders", table: { schema: "public", name: "orders" }, view: { mode: "data", filter: null, sort: null }, pending: { edits: {}, deletes: [], inserts: [] } };
const queryTab = { kind: "query", id: "q:1", title: "Query 1", sql: "", savedQueryId: null, savedSql: null, params: {}, schema: null };

const conn = { id: "c1", name: "Local", kind: "postgres", transport: "network", host: "localhost", port: 5432, database: "app" };

function setup(patch: Record<string, unknown> = {}) {
  useApp.setState(useApp.getInitialState(), true);
  useApp.setState({
    screen: "workspace",
    active: { profile: conn as never, serverVersion: "17", schemas: ["public", "Billing"], schema: "public" },
    tree: { tables: ["orders"], views: [], functions: [] },
    connections: [conn as never],
    ...patch,
  });
  return useApp.getState() as AppState;
}
const actions = (s: AppState) => paletteItems(s, deps).filter((i) => i.kind === "action").map((i) => i.label);

beforeEach(() => vi.clearAllMocks());

describe("paletteItems", () => {
  it("offers both table exports while a table tab is active", () => {
    const a = actions(setup({ tabs: [tableTab], activeTabId: "public.orders" }));
    expect(a).toEqual(expect.arrayContaining(["Export table as CSV", "Export table as JSON", "Close tab"]));
  });
  it("omits table exports with a query tab active", () => {
    const a = actions(setup({ tabs: [queryTab], activeTabId: "q:1" }));
    expect(a.some((l) => l.startsWith("Export"))).toBe(false);
    expect(a).toContain("New query");
  });
  it("on the Connections screen offers only New connection and New window, plus connections", () => {
    const s = setup({ screen: "connections", active: null, tree: null });
    expect(actions(s)).toEqual(["New connection", "New window"]);
    const kinds = paletteItems(s, deps).map((i) => i.kind);
    expect(kinds).toContain("connection");
    expect(kinds).not.toContain("table");
  });
  it("opens a function as a SELECT with the signature as a comment", () => {
    const s = setup({ tree: { tables: [], views: [], functions: [{ name: "calc_tax", signature: "amount numeric" }] } });
    paletteItems(s, deps).find((i) => i.kind === "function")!.run();
    expect(useApp.getState().tabs[0]).toMatchObject({ sql: "SELECT public.calc_tax(/* amount numeric */);" });
  });
  it("quotes a schema that needs it and drops an empty signature comment", () => {
    const s = setup({
      active: { profile: conn as never, serverVersion: "17", schemas: ["Billing"], schema: "Billing" },
      tree: { tables: [], views: [], functions: [{ name: "calc_tax", signature: "amount numeric" }, { name: "now2", signature: "" }] },
    });
    const fns = paletteItems(s, deps).filter((i) => i.kind === "function");
    fns[0].run();
    fns[1].run();
    const [a, b] = useApp.getState().tabs;
    expect(a).toMatchObject({ sql: 'SELECT "Billing".calc_tax(/* amount numeric */);' });
    expect(b).toMatchObject({ sql: 'SELECT "Billing".now2();' });
  });
});

describe("filterItems", () => {
  it("empty query: actions, saved queries, tables", () => {
    const s = setup({ savedQueries: [{ id: "s1", name: "Report", sql: "select 1", lastParams: {} }] });
    const kinds = filterItems(paletteItems(s, deps), "").map((i) => i.kind);
    expect(kinds.indexOf("savedQuery")).toBeGreaterThan(kinds.lastIndexOf("action"));
    expect(kinds.indexOf("table")).toBeGreaterThan(kinds.indexOf("savedQuery"));
  });
  it("ranks, then kind, then label, and caps at 50", () => {
    const tables = Array.from({ length: 80 }, (_, i) => `t_${String(i).padStart(2, "0")}`);
    const s = setup({ tree: { tables, views: [], functions: [] } });
    expect(filterItems(paletteItems(s, deps), "t_")).toHaveLength(50);
    expect(filterItems(paletteItems(s, deps), "t_").map((i) => i.label).slice(0, 2)).toEqual(["t_00", "t_01"]);
  });
  describe("with a MySQL connection", () => {
    const my = { ...conn, kind: "mysql", database: "fx" };
    const mysql = (functions: unknown[]) =>
      setup({ active: { profile: my as never, serverVersion: "MySQL 8.4.13", schemas: ["fx"], schema: "fx" }, connections: [my as never], tree: { tables: [], views: [], functions } as never });
    it("opens a procedure as CALL with the signature as a comment, detailed as procedure", () => {
      const s = mysql([{ name: "two_sets", signature: "IN n int, OUT total int", kind: "procedure" }]);
      const item = paletteItems(s, deps).find((i) => i.kind === "function")!;
      expect(item.detail).toBe("procedure \u00b7 IN n int, OUT total int");
      item.run();
      expect(useApp.getState().tabs[0]).toMatchObject({ sql: "CALL fx.two_sets(/* IN n int, OUT total int */);" });
    });
    it("quotes a name that needs it with backticks and drops an empty signature comment", () => {
      const s = mysql([{ name: "Calc Tax", signature: "", kind: "function" }, { name: "a`b", signature: "x int", kind: "function" }]);
      for (const i of paletteItems(s, deps).filter((i) => i.kind === "function")) i.run();
      const [a, b] = useApp.getState().tabs;
      expect(a).toMatchObject({ sql: "SELECT fx.`Calc Tax`();" });
      expect(b).toMatchObject({ sql: "SELECT fx.`a``b`(/* x int */);" });
    });
  });
});
