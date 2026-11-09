import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../api/tauri", () => ({
  analyzeSql: vi.fn(),
  targetColumns: vi.fn(),
  runQuery: vi.fn(),
  cancelQuery: vi.fn(),
}));

import * as api from "../api/tauri";
import { useApp } from "../state/store";
import type { QueryTab } from "../state/store";
import { QueryView } from "./QueryView";

const SQL = "select 1 where a = :status and b > :since limit :limit and c = :status";
const tab = (over: Partial<QueryTab> = {}): QueryTab => ({
  kind: "query",
  id: "q1",
  title: "Query 1",
  sql: SQL,
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

beforeEach(() => {
  vi.mocked(api.analyzeSql).mockResolvedValue({ statements: [], params: ["status", "since", "limit"] });
  vi.mocked(api.runQuery).mockResolvedValue({ kind: "completed", statementCount: 0, startedAt: 0, txStatus: "idle" });
  useApp.setState({
    active: { profile: { name: "Local Postgres", database: "app_dev", kind: "postgres" } as never, serverVersion: "17", schemas: ["public"], schema: "public" },
    completionCache: {},
  });
});
afterEach(() => vi.resetAllMocks());

function Host() {
  const t = useApp((s) => s.tabs[0]) as QueryTab;
  return <QueryView tab={t} />;
}
const mount = (t: QueryTab) => {
  useApp.setState({ tabs: [t], activeTabId: t.id });
  return render(<Host />);
};
const clickRun = () => fireEvent.click(screen.getByRole("button", { name: /^Run/ }));
const sent = () => vi.mocked(api.runQuery).mock.calls[0][1].params;

describe("ParamsBar", () => {
  it("renders one field per unique param", async () => {
    mount(tab());
    expect(await screen.findByText("Parameters")).toBeTruthy();
    for (const n of ["status", "since", "limit"]) expect(screen.getByLabelText(`:${n}`)).toBeTruthy();
    expect(screen.getAllByRole("textbox", { name: /^:/ })).toHaveLength(3);
  });

  it("shows nothing without params", async () => {
    vi.mocked(api.analyzeSql).mockResolvedValue({ statements: [], params: [] });
    mount(tab({ sql: "select 1" }));
    await act(async () => {});
    expect(screen.queryByText("Parameters")).toBeNull();
  });

  it("NULL toggle sends null", async () => {
    mount(tab({ params: { status: "active", since: "2026-10-01", limit: "20" } }));
    await screen.findByText("Parameters");
    fireEvent.click(screen.getByRole("button", { name: "Set :since to NULL" }));
    expect((screen.getByLabelText(":since") as HTMLInputElement).disabled).toBe(true);
    await act(async () => clickRun());
    expect(sent()).toEqual({ status: "active", since: null, limit: "20" });
  });

  it("empty field sends empty string", async () => {
    mount(tab({ params: { status: "active" } }));
    await screen.findByText("Parameters");
    fireEvent.change(screen.getByLabelText(":limit"), { target: { value: "5" } });
    expect(useApp.getState().tabs[0]).toMatchObject({ params: { status: "active", limit: "5" } });
    await act(async () => clickRun());
    expect(sent()).toEqual({ status: "active", since: "", limit: "5" });
  });

  it("hint only for saved queries", async () => {
    const { unmount } = mount(tab());
    await screen.findByText("Parameters");
    expect(screen.queryByText("Last used values are remembered for this query")).toBeNull();
    unmount();
    mount(tab({ savedQueryId: "sq1", savedSql: SQL }));
    expect(await screen.findByText("Last used values are remembered for this query")).toBeTruthy();
  });
});
