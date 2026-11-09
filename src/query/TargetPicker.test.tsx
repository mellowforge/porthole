import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../api/tauri", () => ({
  analyzeSql: vi.fn(),
  listDatabases: vi.fn(),
  targetSchemas: vi.fn(),
  targetColumns: vi.fn(),
}));

import * as api from "../api/tauri";
import { useApp } from "../state/store";
import type { QueryTab } from "../state/store";
import { QueryView } from "./QueryView";

const tab = (over: Partial<QueryTab> = {}): QueryTab => ({
  kind: "query", id: "q1", title: "Query 1", sql: "", savedQueryId: null, savedSql: null, params: {}, schema: null, database: null,
  run: { status: "idle", results: [], selected: 0, startedAt: null, sqlAtRun: null, pending: null, error: null, statementCount: null },
  txStatus: "idle", note: null, ...over,
});

function Host() {
  const t = useApp((s) => s.tabs.find((x) => x.id === "q1")) as QueryTab;
  return <QueryView tab={t} />;
}

function mount(t: QueryTab) {
  useApp.setState({ tabs: [t], activeTabId: t.id });
  return render(<Host />);
}

beforeEach(() => {
  vi.resetAllMocks();
  vi.mocked(api.analyzeSql).mockResolvedValue({ statements: [], params: [] });
  vi.mocked(api.listDatabases).mockResolvedValue(["analytics", "app_dev"]);
  vi.mocked(api.targetSchemas).mockImplementation(async (db) => (db === "analytics" ? ["public", "rollups"] : ["audit", "public"]));
  vi.mocked(api.targetColumns).mockImplementation(async (db, schema) => ({ [`t_${db ?? "own"}_${schema}`]: ["id"] }));
  useApp.setState(useApp.getInitialState(), true);
  useApp.setState({
    active: { profile: { name: "Local Postgres", database: "app_dev", kind: "postgres", transport: "network" } as never, serverVersion: "17", schemas: ["public", "audit"], schema: "public" },
  });
});

describe("TargetPicker", () => {
  it("lists the databases with the current one checked, and the schemas", async () => {
    mount(tab());
    await userEvent.click(screen.getByRole("button", { name: /PG · app_dev/ }));
    const dbs = await screen.findAllByRole("option", { name: /analytics|app_dev/ });
    expect(dbs.map((o) => o.textContent)).toEqual(["analytics", "app_dev"]);
    expect(screen.getByRole("option", { name: "app_dev" }).getAttribute("aria-selected")).toBe("true");
    expect(screen.getByRole("option", { name: "analytics" }).getAttribute("aria-selected")).toBe("false");
    const schemas = await screen.findAllByRole("option", { name: /Default schema|audit|public/ });
    expect(schemas[0].textContent).toBe("Default schema");
    expect(screen.getByText("Database")).not.toBeNull();
    expect(screen.getByText("Schema")).not.toBeNull();
  });

  it("picking analytics on an idle tab retargets it and the chip reads PG · analytics", async () => {
    mount(tab({ schema: "audit" }));
    await userEvent.click(screen.getByRole("button", { name: /PG · app_dev · audit/ }));
    await userEvent.click(await screen.findByRole("option", { name: "analytics" }));
    const t = useApp.getState().tabs[0] as QueryTab;
    expect(t).toMatchObject({ database: "analytics", schema: null });
    expect(screen.getByRole("button", { name: /PG · analytics ▾/ })).not.toBeNull();
    expect(api.targetSchemas).toHaveBeenCalledWith("analytics");
  });

  it("picking the current database changes nothing", async () => {
    mount(tab({ schema: "audit" }));
    await userEvent.click(screen.getByRole("button", { name: /PG · app_dev/ }));
    await userEvent.click(await screen.findByRole("option", { name: "app_dev" }));
    expect(useApp.getState().tabs[0]).toMatchObject({ database: null, schema: "audit" });
  });

  it("picking a schema sets it, and completion follows the chip's schema", async () => {
    mount(tab());
    await waitFor(() => expect(api.targetColumns).toHaveBeenCalledWith(null, "public"));
    await userEvent.click(screen.getByRole("button", { name: /PG · app_dev/ }));
    await userEvent.click(await screen.findByRole("option", { name: "audit" }));
    expect((useApp.getState().tabs[0] as QueryTab).schema).toBe("audit");
    await waitFor(() => expect(api.targetColumns).toHaveBeenCalledWith(null, "audit"));
    expect(useApp.getState().completionCache["|audit"]).toEqual({ t_own_audit: ["id"] });
  });

  it("a listDatabases rejection shows the error", async () => {
    vi.mocked(api.listDatabases).mockRejectedValue({ kind: "connection", message: "boom" });
    mount(tab());
    await userEvent.click(screen.getByRole("button", { name: /PG · app_dev/ }));
    expect(await screen.findByText("Couldn't list databases: boom")).not.toBeNull();
  });

  it("a targetSchemas rejection shows the error", async () => {
    vi.mocked(api.targetSchemas).mockRejectedValue({ kind: "connection", message: "gone" });
    mount(tab());
    await userEvent.click(screen.getByRole("button", { name: /PG · app_dev/ }));
    expect(await screen.findByText("Couldn't list schemas: gone")).not.toBeNull();
  });

  it("the Database list is disabled while the tab's query runs; schemas still pick", async () => {
    const t = tab();
    mount({ ...t, run: { ...t.run, status: "running" } });
    await userEvent.click(screen.getByRole("button", { name: /PG · app_dev/ }));
    await userEvent.click(await screen.findByRole("option", { name: "analytics" }));
    expect(useApp.getState().tabs[0]).toMatchObject({ database: null });
    expect(screen.getByRole("listbox", { name: "Database" }).getAttribute("aria-disabled")).toBe("true");
    await userEvent.click(await screen.findByRole("option", { name: "audit" }));
    expect((useApp.getState().tabs[0] as QueryTab).schema).toBe("audit");
  });

  it("the Database list is disabled while a Production confirmation waits", async () => {
    const t = tab();
    mount({ ...t, run: { ...t.run, status: "confirm" } });
    await userEvent.click(screen.getByRole("button", { name: /PG · app_dev/ }));
    await userEvent.click(await screen.findByRole("option", { name: "analytics" }));
    expect(useApp.getState().tabs[0]).toMatchObject({ database: null });
    expect(screen.getByRole("listbox", { name: "Database" }).getAttribute("aria-disabled")).toBe("true");
  });

  it("Esc closes the popover", async () => {
    mount(tab());
    await userEvent.click(screen.getByRole("button", { name: /PG · app_dev/ }));
    await screen.findByText("Database");
    await userEvent.keyboard("{Escape}");
    expect(screen.queryByText("Database")).toBeNull();
  });

  it("two tabs on different schemas get different completion maps, one call per key", async () => {
    const a = tab({ id: "q1", schema: "audit" });
    const b = tab({ id: "q2", schema: "public" });
    useApp.setState({ tabs: [a, b], activeTabId: "q1" });
    function Two() {
      const tabs = useApp((s) => s.tabs) as QueryTab[];
      return <>{tabs.map((t) => <QueryView key={t.id} tab={t} />)}</>;
    }
    render(<Two />);
    await waitFor(() => expect(Object.keys(useApp.getState().completionCache).sort()).toEqual(["|audit", "|public"]));
    expect(useApp.getState().completionCache["|audit"]).not.toEqual(useApp.getState().completionCache["|public"]);
    expect(api.targetColumns).toHaveBeenCalledTimes(2);
  });
});

describe("TargetPicker on a SQLite connection", () => {
  beforeEach(() => {
    vi.mocked(api.targetSchemas).mockResolvedValue(["main", "aux"]);
    useApp.setState({
      active: {
        profile: { name: "Local SQLite", kind: "sqlite", transport: "file", filePath: "/Users/me/data/app.sqlite" } as never,
        serverVersion: "SQLite 3.50.4",
        schemas: ["main"],
        schema: "main",
      },
    });
  });

  it("the chip reads SQ · {file name}; there is no Database section and listDatabases isn't called", async () => {
    mount(tab());
    await userEvent.click(screen.getByRole("button", { name: "SQ · app.sqlite ▾" }));
    expect(await screen.findByRole("option", { name: "aux" })).not.toBeNull();
    expect(screen.queryByText("Database")).toBeNull();
    expect(screen.queryByRole("listbox", { name: "Database" })).toBeNull();
    expect(screen.getByText("Schema")).not.toBeNull();
    expect(api.listDatabases).not.toHaveBeenCalled();
    expect(api.targetSchemas).toHaveBeenCalledWith(null);
  });

  it("picking a schema sets it, and completion uses main by default", async () => {
    mount(tab());
    await waitFor(() => expect(api.targetColumns).toHaveBeenCalledWith(null, "main"));
    await userEvent.click(screen.getByRole("button", { name: "SQ · app.sqlite ▾" }));
    await userEvent.click(await screen.findByRole("option", { name: "aux" }));
    expect(useApp.getState().tabs[0]).toMatchObject({ database: null, schema: "aux" });
    expect(screen.getByRole("button", { name: "SQ · app.sqlite · aux ▾" })).not.toBeNull();
  });
});
