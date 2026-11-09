import { act, fireEvent, render, screen } from "@testing-library/react";
import { EditorView } from "@codemirror/view";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../api/tauri", () => ({
  analyzeSql: vi.fn(),
  targetColumns: vi.fn(),
}));

import * as api from "../api/tauri";
import { useApp } from "../state/store";
import type { QueryTab } from "../state/store";
import { QueryView } from "./QueryView";

const tab = (over: Partial<QueryTab> = {}): QueryTab => ({
  kind: "query",
  id: "q1",
  title: "Query 1",
  sql: "",
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
  vi.useFakeTimers();
  vi.mocked(api.analyzeSql).mockResolvedValue({ statements: [], params: [] });
  useApp.setState({
    active: {
      profile: { name: "Local Postgres", database: "app_dev", kind: "postgres", transport: "network" } as never,
      serverVersion: "17",
      schemas: ["public", "audit"],
      schema: "public",
    },
    completionCache: {},
  });
});
afterEach(() => {
  vi.useRealTimers();
  vi.resetAllMocks();
});

function Host({ id }: { id: string }) {
  const t = useApp((s) => s.tabs.find((x) => x.id === id)) as QueryTab;
  return <QueryView tab={t} />;
}

function mount(t: QueryTab) {
  useApp.setState({ tabs: [t], activeTabId: t.id });
  const utils = render(<Host id={t.id} />);
  const view = EditorView.findFromDOM(utils.container.querySelector(".cm-editor") as HTMLElement)!;
  return { ...utils, view };
}

describe("QueryView", () => {
  it("debounces analyzeSql 150 ms", async () => {
    const { view } = mount(tab({ sql: "" }));
    await act(() => vi.advanceTimersByTimeAsync(500));
    expect(api.analyzeSql).not.toHaveBeenCalled();

    act(() => view.dispatch({ changes: { from: 0, insert: "s" }, userEvent: "input.type" }));
    await act(() => vi.advanceTimersByTimeAsync(100));
    act(() => view.dispatch({ changes: { from: 1, insert: "e" }, userEvent: "input.type" }));
    await act(() => vi.advanceTimersByTimeAsync(149));
    expect(api.analyzeSql).not.toHaveBeenCalled();
    await act(() => vi.advanceTimersByTimeAsync(1));
    expect(api.analyzeSql).toHaveBeenCalledTimes(1);
    expect(api.analyzeSql).toHaveBeenCalledWith("se");
    expect(useApp.getState().tabs[0]).toMatchObject({ sql: "se" });
  });

  it("Format button formats editor text", () => {
    const { view } = mount(tab({ sql: "select a,b from t" }));
    fireEvent.click(screen.getByRole("button", { name: /Format/ }));
    expect(view.state.doc.toString()).toBe("SELECT\n  a,\n  b\nFROM\n  t");
    expect(useApp.getState().tabs[0]).toMatchObject({ sql: "SELECT\n  a,\n  b\nFROM\n  t" });
  });

  it("the chip names the kind, the target database and the schema", () => {
    mount(tab({ schema: "audit" }));
    expect(screen.getByRole("button", { name: "PG · app_dev · audit ▾" })).toBeTruthy();
  });

  it("the chip shows the tab's own database when it has one", () => {
    mount(tab({ database: "analytics" }));
    expect(screen.getByRole("button", { name: "PG · analytics ▾" })).toBeTruthy();
  });

  it("shows the state label, header buttons and splitter", () => {
    mount(tab());
    expect(screen.getByText("Unsaved")).toBeTruthy();
    expect(screen.getByRole("button", { name: /Run/ })).toBeTruthy();
    expect(screen.getByRole("button", { name: /Save/ })).toBeTruthy();
    expect(screen.getByRole("separator").getAttribute("aria-orientation")).toBe("horizontal");
  });

  it("a formatter error shows until the next edit", () => {
    const { view } = mount(tab({ sql: "select 'unterminated" }));
    fireEvent.click(screen.getByRole("button", { name: /Format/ }));
    expect(screen.getByText(/^Couldn't format: /)).toBeTruthy();
    act(() => view.dispatch({ changes: { from: 0, insert: " " }, userEvent: "input.type" }));
    expect(screen.queryByText(/^Couldn't format: /)).toBeNull();
  });
});
