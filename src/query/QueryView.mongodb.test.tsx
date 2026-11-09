import { act, fireEvent, render, screen } from "@testing-library/react";
import { currentCompletions, startCompletion } from "@codemirror/autocomplete";
import { undo } from "@codemirror/commands";
import { EditorView } from "@codemirror/view";
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
  formatMongo: vi.fn(),
  collectionFields: vi.fn(),
}));
vi.mock("../table/DataGrid", () => ({
  DataGrid: ({ columns }: { columns: { name: string }[] }) => (
    <table data-testid="grid"><thead><tr>{columns.map((c) => <th key={c.name}>{c.name}</th>)}</tr></thead></table>
  ),
}));

import * as api from "../api/tauri";
import type { RunOutcome, StatementResult } from "../api/types";
import { Toolbar } from "../shell/Toolbar";
import { useApp } from "../state/store";
import type { QueryTab } from "../state/store";
import { QueryView } from "./QueryView";

const completed: RunOutcome = { kind: "completed", statementCount: 1, startedAt: 0, txStatus: "idle" };
const SQL = 'db.users.insertOne({ name: "Ann" })';
const inserted: StatementResult = {
  index: 0, from: 0, to: SQL.length, verb: "insertOne", kind: "write", sql: SQL, params: {},
  outcome: { kind: "affected", rows: 1 }, elapsedMs: 3,
  notices: [{ severity: "info", message: "Inserted 1 document (_id: ObjectId(\"65a1b2c3d4e5f60718293a4b\"))." }],
};
const found: StatementResult = {
  index: 0, from: 0, to: 14, verb: "find", kind: "read", sql: "db.users.find()", params: {},
  outcome: { kind: "rows", columns: [{ name: "_id", dataType: "objectId" }, { name: "name", dataType: "string" }], rows: [["65a1", "Ann"]], truncated: false, cellTypes: ["ot"] } as never,
  elapsedMs: 5, notices: [],
};
const runMock = vi.mocked(api.runQuery);

function Host({ id }: { id: string }) {
  const t = useApp((s) => s.tabs.find((x) => x.id === id)) as QueryTab;
  return <QueryView tab={t} />;
}
const setup = (environment: string, sql = SQL) => {
  useApp.setState({
    active: {
      profile: { id: "g1", name: "Local MongoDB", database: "fx", kind: "mongodb", transport: "network", environment } as never,
      serverVersion: "MongoDB 8.0.34",
      schemas: ["fx", "fx_other"],
      schema: "fx",
    },
  });
  return useApp.getState().newQueryTab({ sql });
};

beforeEach(() => {
  vi.mocked(api.analyzeSql).mockResolvedValue({ statements: [], params: [] });
  vi.mocked(api.targetColumns).mockResolvedValue({});
  vi.mocked(api.targetSchemas).mockResolvedValue(["fx", "fx_other"]);
  vi.mocked(api.listDatabases).mockResolvedValue([]);
  vi.mocked(api.schemaTree).mockResolvedValue({ tables: ["users"], views: [], functions: [] });
  useApp.setState(useApp.getInitialState(), true);
});
afterEach(() => {
  vi.resetAllMocks();
});

describe("a MongoDB query tab", () => {
  it("New query is enabled and ⌘T opens a tab", () => {
    setup("local");
    useApp.setState({ tabs: [], activeTabId: null });
    render(<Toolbar />);
    const btn = screen.getByRole("button", { name: /New query/ }) as HTMLButtonElement;
    expect(btn.disabled).toBe(false);
    fireEvent.keyDown(window, { key: "t", metaKey: true });
    expect(useApp.getState().tabs).toHaveLength(1);
    expect(useApp.getState().tabs[0].kind).toBe("query");
  });

  const editorView = () => EditorView.findFromDOM(document.querySelector(".cm-editor") as HTMLElement)!;
  const pressFormat = (view: EditorView) => {
    // The binding lives in the editor's keymap: press it on the editor itself.
    const e = new KeyboardEvent("keydown", { key: "F", code: "KeyF", keyCode: 70, metaKey: true, shiftKey: true, bubbles: true, cancelable: true });
    view.contentDOM.dispatchEvent(e);
    return e;
  };

  it("⇧⌘F formats the whole text through formatMongo, as one undoable change", async () => {
    vi.mocked(api.formatMongo).mockResolvedValue("db.users.find({ a: 1 })");
    const id = setup("local", "db.users.find({a:1})");
    render(<Host id={id} />);
    expect(screen.getByRole("button", { name: /Format/ }).textContent).toContain("⇧⌘F");
    const view = editorView();
    await act(async () => expect(pressFormat(view).defaultPrevented).toBe(true));
    expect(api.formatMongo).toHaveBeenCalledWith("db.users.find({a:1})");
    expect(view.state.doc.toString()).toBe("db.users.find({ a: 1 })");
    expect((useApp.getState().tabs[0] as QueryTab).sql).toBe("db.users.find({ a: 1 })");
    act(() => void undo(view));
    expect(view.state.doc.toString()).toBe("db.users.find({a:1})");
  });

  it("the Format button formats only the selection", async () => {
    vi.mocked(api.formatMongo).mockResolvedValue("db.b.find({ b: 2 })");
    const id = setup("local", "db.a.find({a:1})\ndb.b.find({b:2})");
    render(<Host id={id} />);
    const view = editorView();
    act(() => view.dispatch({ selection: { anchor: 17, head: 33 } }));
    await act(async () => fireEvent.click(screen.getByRole("button", { name: /Format/ })));
    expect(api.formatMongo).toHaveBeenCalledWith("db.b.find({b:2})");
    expect(view.state.doc.toString()).toBe("db.a.find({a:1})\ndb.b.find({ b: 2 })");
  });

  it("an edit made while formatMongo runs drops its result", async () => {
    let resolve: (text: string) => void = () => {};
    vi.mocked(api.formatMongo).mockReturnValue(new Promise((r) => (resolve = r)));
    const id = setup("local", "db.users.find({a:1})");
    render(<Host id={id} />);
    const view = editorView();
    act(() => void pressFormat(view));
    act(() => view.dispatch({ changes: { from: 0, insert: " " }, userEvent: "input.type" }));
    await act(async () => resolve("db.users.find({ a: 1 })"));
    expect(view.state.doc.toString()).toBe(" db.users.find({a:1})");
  });

  it("closing the tab while formatMongo runs drops its result", async () => {
    let resolve: (text: string) => void = () => {};
    vi.mocked(api.formatMongo).mockReturnValue(new Promise((r) => (resolve = r)));
    const id = setup("local", "db.users.find({a:1})");
    const { unmount } = render(<Host id={id} />);
    const view = editorView();
    act(() => void pressFormat(view));
    const dispatch = vi.spyOn(view, "dispatch");
    unmount();
    await act(async () => resolve("db.users.find({ a: 1 })"));
    expect(dispatch).not.toHaveBeenCalled();
    expect((useApp.getState().tabs[0] as QueryTab).sql).toBe("db.users.find({a:1})");
  });

  it("a formatMongo error shows as Couldn't format", async () => {
    vi.mocked(api.formatMongo).mockRejectedValue({ kind: "query", message: "Unclosed string." });
    const id = setup("local", 'db.users.find({ name: "x');
    render(<Host id={id} />);
    await act(async () => fireEvent.click(screen.getByRole("button", { name: /Format/ })));
    expect(screen.getByText("Couldn't format: Unclosed string.")).toBeTruthy();
    expect(editorView().state.doc.toString()).toBe('db.users.find({ name: "x');
  });

  it("a Postgres tab still formats through formatSql and never calls formatMongo", async () => {
    useApp.setState({
      active: {
        profile: { id: "p1", name: "Local Postgres", database: "app_dev", kind: "postgres", transport: "network", environment: "local" } as never,
        serverVersion: "17",
        schemas: ["public"],
        schema: "public",
      },
    });
    const id = useApp.getState().newQueryTab({ sql: "select a,b from t" });
    render(<Host id={id} />);
    await act(async () => fireEvent.click(screen.getByRole("button", { name: /Format/ })));
    expect(editorView().state.doc.toString()).toBe("SELECT\n  a,\n  b\nFROM\n  t");
    await act(async () => void pressFormat(editorView()));
    expect(api.formatMongo).not.toHaveBeenCalled();
  });

  it("completes a key with the fields of the statement's collection, in the tab's database", async () => {
    vi.mocked(api.collectionFields).mockResolvedValue(["name", "address.city"]);
    const text = "db.people.find({ na";
    const id = setup("local", text);
    act(() => useApp.getState().updateQueryTab(id, { schema: "fx_other" }));
    render(<Host id={id} />);
    const view = editorView();
    act(() => view.dispatch({ selection: { anchor: text.length } }));
    act(() => void startCompletion(view));
    // Filtered by what was typed: "address.city" doesn't match "na".
    await vi.waitFor(() => expect(currentCompletions(view.state).map((c) => c.label)).toEqual(["name"]));
    expect(api.collectionFields).toHaveBeenCalledWith("fx_other", "people");
  });

  it("a run shows the rows, and the write's notice in Messages", async () => {
    runMock.mockImplementation(async (_id, _req, onEvent) => (onEvent({ type: "finished", result: found }), completed));
    const id = setup("local", "db.users.find()");
    render(<Host id={id} />);
    await act(async () => fireEvent.click(screen.getByRole("button", { name: /^Run/ })));
    expect(screen.getByTestId("grid").textContent).toContain("name");

    runMock.mockImplementation(async (_id, _req, onEvent) => (onEvent({ type: "finished", result: inserted }), completed));
    await act(async () => fireEvent.click(screen.getByRole("button", { name: /^Run/ })));
    expect(runMock.mock.calls[1][1]).toMatchObject({ schema: "fx" });
    fireEvent.click(screen.getByRole("tab", { name: /Messages/ }));
    expect(screen.getByText(/info: Inserted 1 document/)).toBeTruthy();
  });

  it("Stop calls cancelQuery", async () => {
    runMock.mockImplementation(() => new Promise(() => {}));
    const id = setup("local");
    render(<Host id={id} />);
    await act(async () => fireEvent.click(screen.getByRole("button", { name: /^Run/ })));
    fireEvent.click(screen.getByRole("button", { name: "Stop" }));
    expect(api.cancelQuery).toHaveBeenCalledWith(id);
  });

  it("the Production confirmation lists insertOne", async () => {
    runMock.mockResolvedValueOnce({ kind: "needsConfirmation", statements: [{ verb: "insertOne", preview: SQL }] });
    const id = setup("production");
    render(<Host id={id} />);
    await act(async () => fireEvent.click(screen.getByRole("button", { name: /^Run/ })));
    expect(screen.getByText("This looks like it changes data on Production:")).toBeTruthy();
    expect(screen.getByText("insertOne")).toBeTruthy();
  });

  it("the TargetPicker is labelled Database", async () => {
    const id = setup("local");
    render(<Host id={id} />);
    await act(async () => fireEvent.click(screen.getByRole("button", { name: /MG · fx/ })));
    expect(screen.getByText("Database")).toBeTruthy();
    expect(screen.getByRole("listbox", { name: "Database" })).toBeTruthy();
    expect(screen.queryByText("Schema")).toBeNull();
  });
});
