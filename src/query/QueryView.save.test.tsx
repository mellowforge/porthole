import { act, fireEvent, render, screen } from "@testing-library/react";
import { EditorView } from "@codemirror/view";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../api/tauri", () => ({
  analyzeSql: vi.fn(),
  targetColumns: vi.fn(),
  saveQuery: vi.fn(),
  listSavedQueries: vi.fn(),
}));

import * as api from "../api/tauri";
import { useApp } from "../state/store";
import type { QueryTab } from "../state/store";
import { QueryView } from "./QueryView";

const tab = (over: Partial<QueryTab> = {}): QueryTab => ({
  kind: "query",
  id: "q1",
  title: "Query 1",
  sql: "select 1",
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
  vi.mocked(api.analyzeSql).mockResolvedValue({ statements: [], params: [] });
  vi.mocked(api.listSavedQueries).mockResolvedValue([]);
  vi.mocked(api.saveQuery).mockImplementation(async (q) => ({ ...q, id: q.id || "sq9", updatedAt: "" }));
  useApp.setState({
    active: { profile: { id: "c1", name: "Local Postgres", database: "app_dev", kind: "postgres" } as never, serverVersion: "17", schemas: ["public"], schema: "public" },
    completionCache: {},
    savedQueries: [],
  });
});
afterEach(() => vi.resetAllMocks());

function Host() {
  const t = useApp((s) => s.tabs[0]) as QueryTab;
  return <QueryView tab={t} />;
}
function mount(t: QueryTab) {
  useApp.setState({ tabs: [t], activeTabId: t.id });
  const utils = render(<Host />);
  const view = EditorView.findFromDOM(utils.container.querySelector(".cm-editor") as HTMLElement)!;
  return { ...utils, view };
}
const cmdS = (view: EditorView) =>
  fireEvent.keyDown(view.contentDOM, { key: "s", code: "KeyS", metaKey: true, keyCode: 83 });

describe("QueryView save", () => {
  it("⌘S on unsaved asks for name; Enter saves and title changes", async () => {
    const { view } = mount(tab());
    cmdS(view);
    const input = (await screen.findByPlaceholderText("Query name")) as HTMLInputElement;
    expect(input.value).toBe("Query 1");
    fireEvent.change(input, { target: { value: "My report" } });
    await act(async () => {
      fireEvent.keyDown(input, { key: "Enter" });
    });
    expect(api.saveQuery).toHaveBeenCalledWith(expect.objectContaining({ id: "", connectionId: "c1", name: "My report", sql: "select 1" }));
    expect(useApp.getState().tabs[0]).toMatchObject({ title: "My report", savedQueryId: "sq9", savedSql: "select 1" });
    expect(screen.getByRole("heading", { name: "My report" })).toBeTruthy();
    expect(screen.queryByPlaceholderText("Query name")).toBeNull();
  });

  it("Esc cancels and an empty name is not saved", async () => {
    const { view } = mount(tab());
    cmdS(view);
    const input = await screen.findByPlaceholderText("Query name");
    fireEvent.change(input, { target: { value: "  " } });
    await act(async () => {
      fireEvent.keyDown(input, { key: "Enter" });
    });
    expect(api.saveQuery).not.toHaveBeenCalled();
    fireEvent.keyDown(input, { key: "Escape" });
    expect(screen.queryByPlaceholderText("Query name")).toBeNull();
  });

  it("⌘S on saved query upserts without asking", async () => {
    const { view } = mount(tab({ title: "Mine", savedQueryId: "sq1", savedSql: "select 0" }));
    await act(async () => {
      cmdS(view);
    });
    expect(screen.queryByPlaceholderText("Query name")).toBeNull();
    expect(api.saveQuery).toHaveBeenCalledWith(expect.objectContaining({ id: "sq1", name: "Mine", sql: "select 1" }));
    expect(useApp.getState().tabs[0].kind === "query" && (useApp.getState().tabs[0] as QueryTab).savedSql).toBe("select 1");
  });

  it("state label switches to Unsaved changes after edit", async () => {
    const { view } = mount(tab({ savedQueryId: "sq1", savedSql: "select 1" }));
    expect(screen.getByText("Saved query · Local Postgres")).toBeTruthy();
    act(() => {
      view.dispatch({ changes: { from: 0, to: 0, insert: "-- x\n" } });
    });
    expect(screen.getByText("Unsaved changes")).toBeTruthy();
  });
});
