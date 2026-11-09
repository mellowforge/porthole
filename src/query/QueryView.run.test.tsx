import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../api/tauri", () => ({
  analyzeSql: vi.fn(),
  targetColumns: vi.fn(),
  runQuery: vi.fn(),
  cancelQuery: vi.fn(),
  endTransaction: vi.fn(),
}));
vi.mock("../table/DataGrid", () => ({
  DataGrid: ({ columns, rows }: { columns: { name: string }[]; rows: (string | null)[][] }) => (
    <table data-testid="grid">
      <thead>
        <tr>{columns.map((c) => <th key={c.name}>{c.name}</th>)}</tr>
      </thead>
      <tbody>
        {rows.slice(0, 1001).map((r, i) => (
          <tr key={i}>{r.map((v, j) => <td key={j}>{v}</td>)}</tr>
        ))}
      </tbody>
    </table>
  ),
}));

import * as api from "../api/tauri";
import type { RunEvent, RunOutcome, RunRequest, StatementResult } from "../api/types";
import { useApp } from "../state/store";
import type { QueryTab } from "../state/store";
import { QueryView } from "./QueryView";

const tab = (over: Partial<QueryTab> = {}): QueryTab => ({
  kind: "query",
  id: "q1",
  title: "Query 1",
  sql: "select 1;\nupdate t set a = 1",
  savedQueryId: null,
  savedSql: null,
  params: {},
  schema: null,
  database: null,
  run: { status: "idle", results: [], selected: 0, startedAt: null, sqlAtRun: null, pending: null, error: null, statementCount: null },
  txStatus: "idle",
  note: null,
  ...over,
});

const sel: StatementResult = {
  index: 0, from: 0, to: 9, verb: "SELECT", kind: "read", sql: "select 1", params: {},
  outcome: { kind: "rows", columns: [{ name: "id", dataType: "int4" }], rows: [["1077"], ["1382"]], truncated: false },
  elapsedMs: 18, notices: [],
};
sel.outcome = { kind: "rows", columns: [{ name: "id", dataType: "int4" }], rows: Array.from({ length: 20 }, (_, i) => [String(1077 + i)]), truncated: false };
const upd: StatementResult = {
  index: 1, from: 11, to: 30, verb: "UPDATE", kind: "write", sql: "update t set a = 1", params: {},
  outcome: { kind: "affected", rows: 37 }, elapsedMs: 4, notices: [{ severity: "NOTICE", message: "hello 42" }],
};
const at = new Date(2026, 9, 5, 14, 32, 5).getTime();
const completed = (n: number): RunOutcome => ({ kind: "completed", statementCount: n, startedAt: at, txStatus: "idle" });
const fin = (result: StatementResult): RunEvent => ({ type: "finished", result });

const runMock = vi.mocked(api.runQuery);
const run = () => useApp.getState().tabs[0] as QueryTab;

beforeEach(() => {
  vi.mocked(api.analyzeSql).mockResolvedValue({ statements: [], params: [] });
  useApp.setState({
    active: {
      profile: { name: "Local Postgres", database: "app_dev", kind: "postgres", transport: "network", environment: "production" } as never,
      serverVersion: "17",
      schemas: ["public"],
      schema: "public",
    },
    completionCache: {},
  });
});
afterEach(() => {
  vi.resetAllMocks();
});

function Host() {
  const t = useApp((s) => s.tabs[0]) as QueryTab;
  return <QueryView tab={t} />;
}
function mount(t: QueryTab = tab()) {
  useApp.setState({ tabs: [t], activeTabId: t.id });
  return render(<Host />);
}
const clickRun = () => fireEvent.click(screen.getByRole("button", { name: /^Run/ }));

describe("QueryView running", () => {
  it("Run shows statement bar and grid", async () => {
    runMock.mockImplementation(async (_id, _req, onEvent) => {
      onEvent(fin(sel));
      onEvent(fin(upd));
      return completed(2);
    });
    mount();
    await act(async () => clickRun());
    const bar = screen.getByTestId("statement-bar");
    expect(within(bar).getByText("SELECT")).toBeTruthy();
    expect(within(bar).getByText("20 rows · 18 ms")).toBeTruthy();
    expect(within(bar).getByText("UPDATE")).toBeTruthy();
    expect(within(bar).getByText("37 rows affected · 4 ms")).toBeTruthy();
    expect(screen.getAllByRole("cell")[0].textContent).toBe("1077");
    expect(screen.getByText("Ran 2 statements at 14:32:05")).toBeTruthy();
    expect(run().run.status).toBe("idle");
    expect(runMock.mock.calls[0][1]).toMatchObject({ sql: tab().sql, confirmed: false, fetchAll: false, schema: null });
  });

  it("results stream in before completion", async () => {
    let emit!: (e: RunEvent) => void;
    let done!: (o: RunOutcome) => void;
    runMock.mockImplementation((_id, _req, onEvent) => {
      emit = onEvent;
      return new Promise((r) => (done = r));
    });
    mount();
    await act(async () => clickRun());
    expect(run().run.status).toBe("running");
    act(() => emit(fin(sel)));
    expect(within(screen.getByTestId("statement-bar")).getByText("20 rows · 18 ms")).toBeTruthy();
    expect(run().run.status).toBe("running");
    act(() => emit(fin(upd)));
    await act(async () => done(completed(2)));
    expect(run().run.status).toBe("idle");
  });

  it("late Finished event after Completed still lands", async () => {
    let emit!: (e: RunEvent) => void;
    runMock.mockImplementation(async (_id, _req, onEvent) => {
      emit = onEvent;
      onEvent(fin(sel));
      return completed(2);
    });
    mount();
    await act(async () => clickRun());
    expect(run().run.status).toBe("running");
    expect(run().run.statementCount).toBe(2);
    act(() => emit(fin(upd)));
    expect(within(screen.getByTestId("statement-bar")).getByText("37 rows affected · 4 ms")).toBeTruthy();
    expect(run().run.status).toBe("idle");
    expect(run().run.selected).toBe(0);
  });

  it("Stop calls cancelQuery while running", async () => {
    runMock.mockImplementation(() => new Promise(() => {}));
    mount();
    await act(async () => clickRun());
    expect(screen.queryByRole("button", { name: /^Run/ })).toBeNull();
    const stop = screen.getByRole("button", { name: "Stop" });
    expect(stop.querySelector("kbd")).toBeNull();
    fireEvent.click(stop);
    expect(api.cancelQuery).toHaveBeenCalledWith("q1");
  });

  it("error shows in Messages with line and column", async () => {
    const sql = "select 1;\nselect nope";
    const err: StatementResult = {
      index: 0, from: 10, to: 21, verb: "SELECT", kind: "read", sql: "select nope", params: {},
      outcome: { kind: "error", message: 'column "nope" does not exist', code: "42703", position: 17 },
      elapsedMs: 4, notices: [],
    };
    runMock.mockImplementation(async (_id, _req, onEvent) => {
      onEvent(fin(err));
      return completed(1);
    });
    mount(tab({ sql }));
    await act(async () => clickRun());
    fireEvent.click(screen.getByRole("tab", { name: /Messages/ }));
    expect(screen.getByText(/does not exist/)).toBeTruthy();
    expect(screen.getByRole("button", { name: "Line 2, column 8" })).toBeTruthy();
  });

  it("messages log lists sql, notices and outcome", async () => {
    runMock.mockImplementation(async (_id, _req, onEvent) => {
      onEvent(fin({ ...sel, params: { status: "active" } }));
      onEvent(fin(upd));
      return completed(2);
    });
    mount();
    await act(async () => clickRun());
    fireEvent.click(screen.getByRole("tab", { name: /Messages/ }));
    expect(screen.getByText("NOTICE: hello 42")).toBeTruthy();
    expect(screen.getByText("status = active")).toBeTruthy();
    expect(screen.getByText("update t set a = 1")).toBeTruthy();
    expect(screen.getAllByText("14:32:05").length).toBe(2);
  });

  it("Production confirmation", async () => {
    runMock.mockResolvedValueOnce({ kind: "needsConfirmation", statements: [{ verb: "UPDATE", preview: "update t" }] });
    mount();
    await act(async () => clickRun());
    expect(screen.getByText("This looks like it changes data on Production:")).toBeTruthy();
    expect(screen.getByText("UPDATE")).toBeTruthy();
    expect(run().run.status).toBe("confirm");

    runMock.mockResolvedValueOnce(completed(0));
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Run anyway" })));
    expect(runMock).toHaveBeenCalledTimes(2);
    expect(runMock.mock.calls[1][1]).toMatchObject({ confirmed: true, sql: tab().sql });

    runMock.mockResolvedValueOnce({ kind: "needsConfirmation", statements: [{ verb: "UPDATE", preview: "" }] });
    await act(async () => clickRun());
    fireEvent.click(screen.getByRole("button", { name: /^Cancel/ }));
    expect(run().run.status).toBe("idle");
    expect(screen.queryByText("This looks like it changes data on Production:")).toBeNull();

    runMock.mockResolvedValueOnce({ kind: "needsConfirmation", statements: [{ verb: "UPDATE", preview: "" }] });
    await act(async () => clickRun());
    fireEvent.keyDown(window, { key: "Escape" });
    expect(run().run.status).toBe("idle");
    expect(runMock).toHaveBeenCalledTimes(4);
  });

  it("Fetch all offered only for truncated reads", async () => {
    const trunc: StatementResult = {
      ...sel,
      outcome: { kind: "rows", columns: [{ name: "id", dataType: "int4" }], rows: Array.from({ length: 1000 }, (_, i) => [String(i)]), truncated: true },
    };
    runMock.mockImplementation(async (_id, _req, onEvent) => {
      onEvent(fin(trunc));
      onEvent(fin({ ...trunc, index: 1, kind: "write", verb: "UPDATE" }));
      return completed(2);
    });
    mount();
    await act(async () => clickRun());
    expect(within(screen.getByTestId("statement-bar")).getAllByText("1,000+ rows · 18 ms")).toHaveLength(2);
    // statement 2 is selected (last with rows)
    expect(run().run.selected).toBe(1);
    expect(screen.queryByRole("button", { name: "Fetch all" })).toBeNull();
    fireEvent.click(within(screen.getByTestId("statement-bar")).getByText("SELECT"));
    expect(screen.getByText("Showing the first 1,000 rows.")).toBeTruthy();

    const full = { ...trunc, outcome: { ...trunc.outcome, truncated: false, rows: Array.from({ length: 1500 }, (_, i) => [String(i)]) } } as StatementResult;
    runMock.mockImplementation(async (_id, _req: RunRequest, onEvent) => {
      onEvent(fin(full));
      return completed(1);
    });
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Fetch all" })));
    expect(runMock.mock.calls[runMock.mock.calls.length - 1][1]).toMatchObject({ target: { kind: "all" }, fetchAll: true, sql: "select 1" });
    const results = run().run.results;
    expect(results).toHaveLength(2);
    expect(results[0].outcome).toMatchObject({ truncated: false });
    expect(results[0].index).toBe(0);
    expect(results[1].verb).toBe("UPDATE");
  });

  it("Fetch all hitting the cap says Stopped at 100,000 rows and offers no Fetch all", async () => {
    const mk = (n: number): StatementResult => ({
      ...sel,
      outcome: { kind: "rows", columns: [{ name: "id", dataType: "int4" }], rows: Array.from({ length: n }, (_, i) => [String(i)]), truncated: true },
    });
    runMock.mockImplementation(async (_id, _req, onEvent) => (onEvent(fin(mk(1000))), completed(1)));
    mount(tab());
    await act(async () => clickRun());
    runMock.mockImplementation(async (_id, _req, onEvent) => (onEvent(fin(mk(100000))), completed(1)));
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Fetch all" })));
    expect(screen.getByText("Stopped at 100,000 rows.")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Fetch all" })).toBeNull();
  });

  it("canceling Fetch all keeps the shown rows", async () => {
    const trunc: StatementResult = {
      ...sel,
      outcome: { kind: "rows", columns: [{ name: "id", dataType: "int4" }], rows: Array.from({ length: 1000 }, (_, i) => [String(i)]), truncated: true },
    };
    runMock.mockImplementation(async (_id, _req, onEvent) => (onEvent(fin(trunc)), completed(1)));
    mount(tab());
    await act(async () => clickRun());
    let emit!: (e: RunEvent) => void;
    let done!: (o: RunOutcome) => void;
    runMock.mockImplementation((_id, _req, onEvent) => {
      emit = onEvent;
      return new Promise((r) => (done = r));
    });
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Fetch all" })));
    fireEvent.click(screen.getByRole("button", { name: "Stop" }));
    act(() => emit(fin({ ...trunc, outcome: { kind: "canceled" }, elapsedMs: 5 })));
    await act(async () => done(completed(1)));
    expect(screen.getByText("Fetch all was canceled.")).toBeTruthy();
    expect(screen.getAllByRole("row")).toHaveLength(1001);
    expect(within(screen.getByTestId("statement-bar")).getByText("1,000+ rows · 18 ms")).toBeTruthy();
    expect(screen.getByText("Showing the first 1,000 rows.")).toBeTruthy();
    expect(run().run.status).toBe("idle");
  });

  it("statement picker switches grid", async () => {
    runMock.mockImplementation(async (_id, _req, onEvent) => {
      onEvent(fin({ ...sel, outcome: { ...sel.outcome, rows: [["1"]] } as never }));
      onEvent(fin({ ...sel, index: 1, verb: "SELECT", outcome: { kind: "rows", columns: [{ name: "b", dataType: "text" }], rows: [["zzz"]], truncated: false } }));
      return completed(2);
    });
    mount();
    await act(async () => clickRun());
    expect(screen.getByText("Showing: statement 2 of 2 (SELECT) ▾")).toBeTruthy();
    expect(screen.getByText("zzz")).toBeTruthy();
    fireEvent.change(screen.getByRole("combobox", { name: "Statement" }), { target: { value: "0" } });
    expect(screen.queryByText("zzz")).toBeNull();
    expect(screen.getByText("Showing: statement 1 of 2 (SELECT) ▾")).toBeTruthy();
  });

  it("a stopped run with a failed transaction shows Not run, keeps the rows selected and offers Rollback", async () => {
    const sql = "begin;\nselect 1;\nselect nope;\nselect 3";
    const failed: StatementResult = {
      index: 1, from: 17, to: 28, verb: "SELECT", kind: "read", sql: "select nope", params: {},
      outcome: { kind: "error", message: 'column "nope" does not exist', code: "42703", position: 24 },
      elapsedMs: 4, notices: [],
    };
    const skipped: StatementResult = {
      index: 2, from: 30, to: 38, verb: "SELECT", kind: "read", sql: "select 3", params: {}, outcome: { kind: "skipped" }, elapsedMs: 0, notices: [],
    };
    runMock.mockImplementation(async (_id, _req, onEvent) => {
      onEvent(fin({ ...sel, from: 7, to: 15 }));
      onEvent(fin(failed));
      onEvent(fin(skipped));
      return { kind: "completed", statementCount: 3, startedAt: at, txStatus: "failed" };
    });
    mount(tab({ sql }));
    await act(async () => clickRun());
    const bar = screen.getByTestId("statement-bar");
    const chips = within(bar).getAllByRole("button");
    expect(chips.map((c) => c.getAttribute("aria-pressed"))).toEqual(["true", "false", "false"]);
    expect(within(chips[0]).getByText("20 rows · 18 ms")).toBeTruthy();
    expect(within(chips[1]).getByText("Error · 4 ms")).toBeTruthy();
    expect(within(chips[2]).getByText("Not run")).toBeTruthy();
    expect(screen.getAllByRole("cell")[0].textContent).toBe("1077");
    expect(screen.getByText("Transaction failed — roll back to continue")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Rollback" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Commit" })).toBeNull();
  });

  it("a run right after typing a new :param sends it, without waiting for the analysis", async () => {
    vi.mocked(api.analyzeSql).mockImplementation(async (s) => ({ statements: [], params: s.includes(":x") ? ["x", "y"] : [] }));
    runMock.mockResolvedValue(completed(0));
    mount(tab({ sql: "select 1", params: { y: null } }));
    act(() => useApp.getState().updateQueryTab("q1", { sql: "select :x, :y" }));
    await act(async () => clickRun());
    expect(runMock.mock.calls[0][1].params).toEqual({ x: "", y: null });
  });

  it("Fetch all after a retarget runs on the database the run used", async () => {
    const trunc: StatementResult = {
      ...sel,
      outcome: { kind: "rows", columns: [{ name: "id", dataType: "int4" }], rows: Array.from({ length: 1000 }, (_, i) => [String(i)]), truncated: true },
    };
    runMock.mockImplementation(async (_id, _req, onEvent) => (onEvent(fin(trunc)), completed(1)));
    mount(tab({ sql: "select * from t", database: "analytics", schema: "rollups" }));
    await act(async () => clickRun());
    act(() => useApp.getState().retargetQueryTab("q1", "billing"));
    expect(run().database).toBe("billing");
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Fetch all" })));
    const req = runMock.mock.calls[runMock.mock.calls.length - 1][1];
    expect(req).toMatchObject({ fetchAll: true, database: "analytics", schema: "rollups" });
  });

  it("Run anyway runs on the database and schema recorded at run", async () => {
    runMock.mockResolvedValueOnce({ kind: "needsConfirmation", statements: [{ verb: "UPDATE", preview: "update t" }] });
    mount(tab({ database: "analytics", schema: "rollups" }));
    await act(async () => clickRun());
    expect(run().run.status).toBe("confirm");
    // Changed behind the confirmation (retarget itself is refused meanwhile).
    act(() => useApp.getState().updateQueryTab("q1", { database: "billing", schema: "public" }));
    runMock.mockResolvedValueOnce(completed(0));
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Run anyway" })));
    expect(runMock.mock.calls[1][1]).toMatchObject({ confirmed: true, database: "analytics", schema: "rollups" });
  });

  it("Fetch all re-runs with the result's own params and the schema the run used", async () => {
    const trunc: StatementResult = {
      ...sel,
      sql: "select * from t where id > :id",
      params: { id: "7" },
      outcome: { kind: "rows", columns: [{ name: "id", dataType: "int4" }], rows: Array.from({ length: 1000 }, (_, i) => [String(i)]), truncated: true },
    };
    runMock.mockImplementation(async (_id, _req, onEvent) => (onEvent(fin(trunc)), completed(1)));
    mount(tab({ sql: "select * from t where id > :id", params: { id: "7" }, schema: "audit" }));
    await act(async () => clickRun());
    act(() => useApp.getState().updateQueryTab("q1", { params: { id: "99" }, schema: "public" }));
    vi.mocked(api.analyzeSql).mockClear();
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Fetch all" })));
    const req = runMock.mock.calls[runMock.mock.calls.length - 1][1];
    expect(req).toMatchObject({ fetchAll: true, sql: "select * from t where id > :id", params: { id: "7" }, schema: "audit" });
    expect(api.analyzeSql).not.toHaveBeenCalled();
  });

  describe("a target database that no longer exists", () => {
    const gone = { kind: "connection", message: 'database "gone" does not exist' };

    it("shows the run error inline, and pointing the tab back at the connection's database works", async () => {
      runMock.mockRejectedValueOnce(gone);
      mount(tab({ sql: "select 1", database: "gone" }));
      await act(async () => clickRun());
      expect(runMock.mock.calls[0][1]).toMatchObject({ database: "gone" });
      expect(screen.getByRole("alert").textContent).toBe('database "gone" does not exist');
      expect(run().run.status).toBe("idle");

      act(() => useApp.getState().retargetQueryTab("q1", null));
      expect(run()).toMatchObject({ database: null, schema: null });
      runMock.mockImplementationOnce(async (_id, _req, onEvent) => (onEvent(fin(sel)), completed(1)));
      await act(async () => clickRun());
      expect(runMock.mock.calls[1][1]).toMatchObject({ database: null });
      expect(run().run.error).toBeNull();
      expect(screen.getByTestId("grid")).toBeTruthy();
    });

    it("a restored tab on it is still usable", async () => {
      const { restoreTabs } = await import("../state/persist");
      const json = JSON.stringify({
        v: 1,
        activeTabId: "q1",
        tabs: [{ kind: "query", id: "q1", title: "Query 1", sql: "select 1", savedQueryId: null, savedSql: null, params: {}, schema: "rollups", database: "gone", txOpen: false }],
      });
      const restored = restoreTabs(json, new Set()).tabs[0] as QueryTab;
      expect(restored).toMatchObject({ database: "gone", schema: "rollups" });
      runMock.mockRejectedValueOnce(gone);
      mount(restored);
      await act(async () => clickRun());
      expect(screen.getByRole("alert").textContent).toBe('database "gone" does not exist');
      act(() => useApp.getState().retargetQueryTab("q1", "app_dev"));
      expect(run()).toMatchObject({ database: null, schema: null });
      runMock.mockResolvedValueOnce(completed(0));
      await act(async () => clickRun());
      expect(runMock.mock.calls[1][1]).toMatchObject({ database: null, schema: null });
      expect(screen.queryByRole("alert")).toBeNull();
    });
  });
});
