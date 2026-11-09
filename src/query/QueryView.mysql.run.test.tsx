import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../api/tauri", () => ({
  analyzeSql: vi.fn(),
  targetColumns: vi.fn(),
  targetSchemas: vi.fn(),
  listDatabases: vi.fn(),
  schemaTree: vi.fn(),
  runQuery: vi.fn(),
  cancelQuery: vi.fn(),
  endTransaction: vi.fn(),
  exportQuery: vi.fn(),
}));
vi.mock("../table/DataGrid", () => ({
  DataGrid: () => <table data-testid="grid" />,
}));

import * as api from "../api/tauri";
import type { RunOutcome, StatementResult } from "../api/types";
import { useApp } from "../state/store";
import type { QueryTab } from "../state/store";
import { QueryView } from "./QueryView";

const sel: StatementResult = {
  index: 0, from: 0, to: 17, verb: "SELECT", kind: "read", sql: "select database()", params: {},
  outcome: { kind: "rows", columns: [{ name: "database()", dataType: "varchar" }], rows: [["fx_other"]], truncated: false },
  elapsedMs: 1, notices: [],
};
const completed: RunOutcome = { kind: "completed", statementCount: 1, startedAt: 0, txStatus: "idle" };
const runMock = vi.mocked(api.runQuery);

function Host({ id }: { id: string }) {
  const t = useApp((s) => s.tabs.find((x) => x.id === id)) as QueryTab;
  return <QueryView tab={t} />;
}

beforeEach(() => {
  vi.mocked(api.analyzeSql).mockResolvedValue({ statements: [], params: [] });
  vi.mocked(api.targetColumns).mockResolvedValue({});
  vi.mocked(api.targetSchemas).mockResolvedValue(["fx", "fx_other"]);
  vi.mocked(api.listDatabases).mockResolvedValue([]);
  vi.mocked(api.schemaTree).mockResolvedValue({ tables: [], views: [], functions: [] } as never);
  runMock.mockImplementation(async (_id, _req, onEvent) => (onEvent({ type: "finished", result: sel }), completed));
  useApp.setState(useApp.getInitialState(), true);
  useApp.setState({
    active: {
      profile: { name: "Local MySQL", database: "fx", kind: "mysql", transport: "network", environment: "local" } as never,
      serverVersion: "MySQL 8.4.13",
      schemas: ["fx", "fx_other"],
      schema: "fx",
    },
  });
});
afterEach(() => {
  vi.resetAllMocks();
});

describe("a MySQL query tab runs on the database its chip names", () => {
  it("after the picker switched to fx_other, a new tab runs with schema fx_other", async () => {
    await act(async () => useApp.getState().setSchema("fx_other"));
    const id = useApp.getState().newQueryTab({ sql: "select database()" });
    render(<Host id={id} />);
    expect(screen.getByRole("button", { name: /MY · fx_other/ })).toBeTruthy();
    await act(async () => fireEvent.click(screen.getByRole("button", { name: /^Run/ })));
    expect(runMock.mock.calls[0][1]).toMatchObject({ sql: "select database()", schema: "fx_other", database: null });
    // The run is pinned to it: an export of its result names the same database.
    const tab = useApp.getState().tabs.find((t) => t.id === id) as QueryTab;
    expect(tab.run.schemaAtRun).toBe("fx_other");
  });

  it("an export of a result names the database the run used, even after the picker moved", async () => {
    vi.mocked(api.exportQuery).mockResolvedValue({ kind: "dismissed" } as never);
    // A truncated read result is exported by running it again on the tab's session.
    const truncated = { ...sel, outcome: { ...sel.outcome, truncated: true } as StatementResult["outcome"] };
    runMock.mockImplementation(async (_id, _req, onEvent) => (onEvent({ type: "finished", result: truncated }), completed));
    const id = useApp.getState().newQueryTab({ sql: "select database()" });
    render(<Host id={id} />);
    await act(async () => fireEvent.click(screen.getByRole("button", { name: /^Run/ })));
    expect(runMock.mock.calls[0][1]).toMatchObject({ schema: "fx" });
    await act(async () => useApp.getState().setSchema("fx_other"));
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Export" })));
    await act(async () => fireEvent.click(screen.getByRole("menuitem", { name: "Export as CSV" })));
    expect(vi.mocked(api.exportQuery).mock.calls[0][2]).toMatchObject({ schema: "fx", database: null });
  });

  it("a Postgres tab still sends schema null", async () => {
    useApp.setState({
      active: {
        profile: { name: "Local Postgres", database: "app_dev", kind: "postgres", transport: "network", environment: "local" } as never,
        serverVersion: "17",
        schemas: ["public", "audit"],
        schema: "audit",
      },
    });
    const id = useApp.getState().newQueryTab({ sql: "select 1" });
    render(<Host id={id} />);
    await act(async () => fireEvent.click(screen.getByRole("button", { name: /^Run/ })));
    expect(runMock.mock.calls[0][1]).toMatchObject({ schema: null, database: null });
  });
});
