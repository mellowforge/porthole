import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ColumnInfo, ConnectedInfo, Editing, TablePage } from "../api/types";
import type { DataGridProps } from "./DataGrid";
import { emptyPending, keyId } from "./pending";

vi.mock("../api/tauri", () => ({ fetchPage: vi.fn(), tableStructure: vi.fn(), applyChanges: vi.fn() }));
let grid: DataGridProps;
vi.mock("./DataGrid", () => ({
  DataGrid: (p: DataGridProps) => {
    grid = p;
    return <div data-testid="grid" data-editable={p.editable ? "true" : undefined} data-extra={p.extraRows ?? 0} />;
  },
}));

import * as api from "../api/tauri";
import { type TableTab, useApp } from "../state/store";
import { LeaveGuard } from "../shell/LeaveGuard";
import { TableView } from "./TableView";

const col = (name: string, over: Partial<ColumnInfo> = {}): ColumnInfo => ({
  name,
  dataType: "text",
  fullType: "text",
  castType: "text",
  nullable: true,
  default: null,
  isPrimaryKey: false,
  readOnly: false,
  ...over,
});
const cols = [col("id", { isPrimaryKey: true, nullable: false }), col("status"), col("name", { nullable: false })];
const page = (editing: Editing["kind"] = "allowed"): TablePage => ({
  columns: cols,
  primaryKey: ["id"],
  editing: { kind: editing },
  rows: [
    ["1", "a", "Ann"],
    ["2", null, "Bo"],
  ],
  total: { kind: "exact", value: 2 },
  matches: null,
  elapsedMs: 3,
});
const table = { schema: "public", name: "users" };
const tabId = "public.users";
const tab: TableTab = { kind: "table", id: tabId, table, view: { mode: "data", filter: null, sort: null }, pending: emptyPending };
const fetchPage = vi.mocked(api.fetchPage);

function connect(readOnly: boolean) {
  useApp.setState({ active: { profile: { readOnly }, schema: "public" } as unknown as ConnectedInfo });
}

/** Renders TableView with its tab read from the store, as Workspace does. */
function Live() {
  const t = useApp((s) => s.tabs.find((x) => x.id === tabId)) as TableTab;
  return <TableView tab={t} />;
}

const pendingNow = () => (useApp.getState().tabs.find((t) => t.id === tabId) as TableTab).pending;

beforeEach(() => {
  vi.resetAllMocks();
  useApp.setState({ tabs: [tab], activeTabId: tabId, leave: null, leaveError: null });
  connect(false);
});

describe("TableView editing", () => {
  it("Add row only when editing allowed", async () => {
    fetchPage.mockResolvedValue(page());
    render(<Live />);
    const g = await screen.findByTestId("grid");
    expect(g.dataset.editable).toBe("true");
    expect(screen.queryByText(/read-only/)).toBeNull();
    await userEvent.click(screen.getByRole("button", { name: "+ Add row" }));
    expect(pendingNow().inserts).toHaveLength(1);
    expect(screen.getByTestId("grid").dataset.extra).toBe("1");
    expect(grid.editable?.rowState(2)).toBe("new");

    // The button sits right of the Data/Structure switch.
    const header = document.querySelector(".table-header")!;
    const kids = Array.from(header.children);
    const seg = kids.findIndex((k) => k.classList.contains("segmented"));
    const add = kids.findIndex((k) => k.textContent?.includes("Add row"));
    expect(seg).toBeGreaterThanOrEqual(0);
    expect(add).toBeGreaterThan(seg);
  });

  it("read-only notes", async () => {
    const notes: [Editing["kind"], string][] = [
      ["noPrimaryKey", "This table has no primary key, so it opens read-only."],
      ["view", "Views are read-only."],
      ["floatKey", "Rows can't be matched exactly by a floating-point primary key, so this table opens read-only."],
    ];
    for (const [kind, text] of notes) {
      fetchPage.mockResolvedValue(page(kind));
      const { unmount } = render(<Live />);
      expect(await screen.findByText(text)).not.toBeNull();
      expect(screen.getByTestId("grid").dataset.editable).toBeUndefined();
      expect(screen.queryByRole("button", { name: "+ Add row" })).toBeNull();
      unmount();
    }
    connect(true);
    fetchPage.mockResolvedValue(page());
    render(<Live />);
    expect(await screen.findByText("This connection is read-only. Turn read-only off in the connection settings to edit.")).not.toBeNull();
  });

  it("read-only connection disables editing", async () => {
    connect(true);
    fetchPage.mockResolvedValue(page());
    render(<Live />);
    const g = await screen.findByTestId("grid");
    expect(g.dataset.editable).toBeUndefined();
    expect(screen.queryByRole("button", { name: "+ Add row" })).toBeNull();
  });

  it("edits, Set NULL, revert and delete flow into the tab's pending changes", async () => {
    fetchPage.mockResolvedValue(page());
    render(<Live />);
    await screen.findByTestId("grid");
    const key1 = keyId(["1"]);

    act(() => grid.editable!.onEdit(0, 1, "b"));
    expect(pendingNow().edits[key1].values).toEqual({ status: "b" });
    expect(grid.editable!.cellValue(0, 1)).toEqual({ value: "b", edited: true });
    expect(grid.editable!.cellValue(0, 2)).toEqual({ value: "Ann", edited: false });

    // Context menu on the edited cell: Revert cell is offered.
    act(() => grid.editable!.onContextMenu(0, 1, { x: 10, y: 10 }));
    await userEvent.click(screen.getByRole("menuitem", { name: "Revert cell" }));
    expect(pendingNow().edits[key1]).toBeUndefined();

    // Set NULL is disabled on a NOT NULL column.
    act(() => grid.editable!.onContextMenu(0, 2, { x: 10, y: 10 }));
    expect((screen.getByRole("menuitem", { name: "Set NULL" }) as HTMLButtonElement).disabled).toBe(true);
    await userEvent.click(screen.getByRole("menuitem", { name: "Delete row" }));
    expect(pendingNow().deletes[key1]).toEqual(["1"]);
    expect(grid.editable!.rowState(0)).toBe("deleted");

    act(() => grid.editable!.onContextMenu(1, 1, { x: 10, y: 10 }));
    await userEvent.click(screen.getByRole("menuitem", { name: "Set NULL" }));
    // status was already NULL in row 2, so nothing is recorded.
    expect(pendingNow().edits[keyId(["2"])]).toBeUndefined();

    // ⌫ on selected rows: an already-deleted row stays deleted, a new row is removed.
    await userEvent.click(screen.getByRole("button", { name: "+ Add row" }));
    act(() => grid.editable!.onEdit(2, 2, "Cy"));
    expect(pendingNow().inserts[0].values).toEqual({ name: "Cy" });
    expect(grid.editable!.cellValue(2, 2)).toEqual({ value: "Cy", edited: true });
    expect(grid.editable!.cellValue(2, 1)).toEqual({ value: null, edited: false });
    act(() => grid.editable!.onDeleteRows([0, 1, 2]));
    expect(Object.keys(pendingNow().deletes).sort()).toEqual([key1, keyId(["2"])].sort());
    expect(pendingNow().inserts).toHaveLength(0);
  });

  it("a typed page's save sends the original cell types; an untyped page's sends none", async () => {
    vi.mocked(api.applyChanges).mockResolvedValue({ applied: 2 });
    fetchPage.mockResolvedValue({ ...page(), cellTypes: ["itt", "int"] });
    const { unmount } = render(<Live />);
    await screen.findByTestId("grid");
    act(() => grid.editable!.onEdit(0, 2, "Zed"));
    act(() => grid.editable!.onDeleteRows([1]));
    await act(async () => {
      await useApp.getState().saveTableChanges(tabId);
    });
    expect(vi.mocked(api.applyChanges).mock.calls[0][0].changes).toEqual([
      { kind: "delete", key: ["2"], originalTypes: { id: "i" } },
      { kind: "update", key: ["1"], values: { name: "Zed" }, originalTypes: { id: "i", name: "t" } },
    ]);
    unmount();

    useApp.setState({ tabs: [tab] });
    fetchPage.mockResolvedValue(page());
    render(<Live />);
    await screen.findByTestId("grid");
    act(() => grid.editable!.onEdit(0, 2, "Zed"));
    act(() => grid.editable!.onDeleteRows([1]));
    expect("types" in pendingNow()).toBe(false);
    await act(async () => {
      await useApp.getState().saveTableChanges(tabId);
    });
    expect(JSON.stringify(vi.mocked(api.applyChanges).mock.calls[1][0].changes)).toBe(
      '[{"kind":"delete","key":["2"]},{"kind":"update","key":["1"],"values":{"name":"Zed"}}]',
    );
  });

  it("the grid is read-only while a save is in flight", async () => {
    fetchPage.mockResolvedValue(page());
    let finish!: () => void;
    vi.mocked(api.applyChanges).mockReturnValue(new Promise((r) => (finish = () => r({ applied: 1 }))));
    render(<Live />);
    await screen.findByTestId("grid");
    act(() => grid.editable!.onEdit(0, 1, "b"));
    expect(screen.getByRole("button", { name: "+ Add row" })).toBeTruthy();

    let saving!: Promise<boolean>;
    act(() => {
      saving = useApp.getState().saveTableChanges(tabId);
    });
    await waitFor(() => expect(screen.queryByRole("button", { name: "+ Add row" })).toBeNull());
    // Locked, not hidden: the pending state is still rendered...
    expect(grid.editable).toBeDefined();
    expect(grid.editable!.cellValue(0, 1)).toEqual({ value: "b", edited: true });
    // ...but nothing can be changed.
    expect(grid.editable!.readOnlyColumn(1)).toBe(true);
    act(() => grid.editable!.onEdit(0, 2, "typed during save"));
    act(() => grid.editable!.onDeleteRows([1]));
    act(() => grid.editable!.onContextMenu(0, 1, { x: 1, y: 1 }));
    expect(screen.queryByRole("menuitem")).toBeNull();
    expect(Object.keys(pendingNow().edits)).toHaveLength(1);
    expect(pendingNow().edits[keyId(["1"])].values).toEqual({ status: "b" });
    expect(pendingNow().deletes).toEqual({});

    await act(async () => {
      finish();
      await saving;
    });
    await waitFor(() => expect(screen.getByRole("button", { name: "+ Add row" })).toBeTruthy());
    expect(grid.editable!.readOnlyColumn(1)).toBe(false);
  });

  it("a save starting closes an open context menu", async () => {
    fetchPage.mockResolvedValue(page());
    let finish!: () => void;
    vi.mocked(api.applyChanges).mockReturnValue(new Promise((r) => (finish = () => r({ applied: 1 }))));
    render(<Live />);
    await screen.findByTestId("grid");
    act(() => grid.editable!.onEdit(0, 1, "b"));
    act(() => grid.editable!.onContextMenu(1, 1, { x: 10, y: 10 }));
    expect(screen.getByRole("menuitem", { name: "Delete row" })).toBeTruthy();
    let saving!: Promise<boolean>;
    act(() => {
      saving = useApp.getState().saveTableChanges(tabId);
    });
    await waitFor(() => expect(screen.queryByRole("menuitem")).toBeNull());
    expect(pendingNow().deletes).toEqual({});
    await act(async () => {
      finish();
      await saving;
    });
  });

  it("a deleted row keeps its edits, struck through, and Restore row brings them back", async () => {
    fetchPage.mockResolvedValue(page());
    render(<Live />);
    await screen.findByTestId("grid");
    act(() => grid.editable!.onEdit(0, 1, "b"));
    act(() => grid.editable!.onContextMenu(0, 2, { x: 10, y: 10 }));
    await userEvent.click(screen.getByRole("menuitem", { name: "Delete row" }));
    expect(grid.editable!.rowState(0)).toBe("deleted");
    expect(grid.editable!.cellValue(0, 1)).toEqual({ value: "b", edited: true });
    expect(screen.getByText("1 pending change")).toBeTruthy();
    act(() => grid.editable!.onContextMenu(0, 2, { x: 10, y: 10 }));
    await userEvent.click(screen.getByRole("menuitem", { name: "Restore row" }));
    expect(grid.editable!.rowState(0)).toBeNull();
    expect(grid.editable!.cellValue(0, 1)).toEqual({ value: "b", edited: true });
    expect(pendingNow().edits[keyId(["1"])].values).toEqual({ status: "b" });
  });

  it("next page with edits asks; Discard proceeds to page 2; Cancel stays", async () => {
    const full = { ...page(), total: { kind: "exact", value: 200 } } as TablePage;
    fetchPage.mockResolvedValue(full);
    render(<><Live /><LeaveGuard /></>);
    await screen.findByTestId("grid");
    act(() => grid.editable!.onEdit(0, 1, "b"));
    await userEvent.click(screen.getByRole("button", { name: "Next page" }));
    expect(screen.getByText("“users” has 1 unsaved change.")).toBeTruthy();
    expect(fetchPage).toHaveBeenCalledTimes(1);
    await userEvent.click(within(screen.getByRole("alertdialog")).getByRole("button", { name: "Cancel" }));
    expect(screen.getByText("Page 1 of 2")).toBeTruthy();
    expect(Object.keys(pendingNow().edits)).toHaveLength(1);

    await userEvent.click(screen.getByRole("button", { name: "Next page" }));
    await userEvent.click(within(screen.getByRole("alertdialog")).getByRole("button", { name: "Discard" }));
    await screen.findByText("Page 2 of 2");
    expect(fetchPage).toHaveBeenCalledTimes(2);
    expect(fetchPage.mock.calls[1][0].page).toBe(1);
    expect(pendingNow().edits).toEqual({});
  });

  it("Structure switch keeps edits without asking", async () => {
    fetchPage.mockResolvedValue(page());
    vi.mocked(api.tableStructure).mockResolvedValue({ columns: [], indexes: [], constraints: [], foreignKeys: [] } as never);
    render(<Live />);
    await screen.findByTestId("grid");
    act(() => grid.editable!.onEdit(0, 1, "b"));
    await userEvent.click(screen.getByRole("button", { name: "Structure" }));
    expect(screen.queryByText(/unsaved change/)).toBeNull();
    expect(useApp.getState().leave).toBeNull();
    expect(Object.keys(pendingNow().edits)).toHaveLength(1);
  });

  it("Filter Clear then Cancel keeps the applied filter text", async () => {
    fetchPage.mockResolvedValue(page());
    render(<><Live /><LeaveGuard /></>);
    await screen.findByTestId("grid");
    const input = screen.getByPlaceholderText(/id = 1 and/) as HTMLInputElement;
    await userEvent.type(input, "id = 1{Enter}");
    await waitFor(() => expect(fetchPage).toHaveBeenCalledTimes(2));
    act(() => grid.editable!.onEdit(0, 1, "b"));
    await userEvent.click(screen.getByRole("button", { name: "Clear" }));
    await userEvent.click(within(screen.getByRole("alertdialog")).getByRole("button", { name: "Cancel" }));
    expect(input.value).toBe("id = 1");
    expect(fetchPage).toHaveBeenCalledTimes(2);
  });

  it("Save in a reload prompt keeps the Saved note through the requested fetch", async () => {
    fetchPage.mockResolvedValue({ ...page(), total: { kind: "exact", value: 200 } } as TablePage);
    vi.mocked(api.applyChanges).mockResolvedValue({ applied: 1 });
    render(<><Live /><LeaveGuard /></>);
    await screen.findByTestId("grid");
    act(() => grid.editable!.onEdit(0, 1, "b"));
    await userEvent.click(screen.getByRole("button", { name: "Next page" }));
    await userEvent.click(within(screen.getByRole("alertdialog")).getByRole("button", { name: "Save" }));
    await screen.findByText("Page 2 of 2");
    expect(screen.getByText("Saved 1 change")).toBeTruthy();
  });
});
