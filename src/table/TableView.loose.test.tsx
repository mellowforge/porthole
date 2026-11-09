import { act, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ColumnInfo, ConnectedInfo, TablePage } from "../api/types";
import type { DataGridProps } from "./DataGrid";
import { emptyPending } from "./pending";

vi.mock("../api/tauri", () => ({ fetchPage: vi.fn(), tableStructure: vi.fn(), applyChanges: vi.fn() }));
let grid: DataGridProps;
vi.mock("./DataGrid", () => ({
  DataGrid: (p: DataGridProps) => {
    grid = p;
    return <div data-testid="grid" />;
  },
}));

import * as api from "../api/tauri";
import { type TableTab, useApp } from "../state/store";
import { EMPTY_KEY_READ_ONLY } from "./useTableEditing";
import { TableView } from "./TableView";

const col = (name: string, over: Partial<ColumnInfo> = {}): ColumnInfo => ({
  name, dataType: "text", fullType: "text", castType: "text", nullable: true, default: null, isPrimaryKey: false, readOnly: false, ...over,
});
/** `loose`: a key column with no declared type holding the integer 5 and the text '5', plus a row whose key is NULL. */
const page: TablePage = {
  columns: [col("k", { dataType: "", fullType: "", castType: "blob", isPrimaryKey: true }), col("v")],
  primaryKey: ["k"],
  editing: { kind: "allowed" },
  rows: [["5", "int five"], ["5", "text five"], [null, "null key"]],
  cellTypes: ["it", "tt", "nt"],
  total: { kind: "exact", value: 3 },
  matches: null,
  elapsedMs: 1,
};
const tabId = "main.loose";
const table = { schema: "main", name: "loose" };
const tab: TableTab = { kind: "table", id: tabId, table, view: { mode: "data", filter: null, sort: null }, pending: emptyPending };

function Live() {
  const t = useApp((s) => s.tabs.find((x) => x.id === tabId)) as TableTab;
  return <TableView tab={t} />;
}

async function mount() {
  render(<Live />);
  await screen.findByTestId("grid");
}

beforeEach(() => {
  vi.resetAllMocks();
  useApp.setState({ tabs: [tab], activeTabId: tabId, leave: null, leaveError: null });
  useApp.setState({ active: { profile: { readOnly: false, environment: "local" }, schema: "main" } as unknown as ConnectedInfo });
  vi.mocked(api.fetchPage).mockResolvedValue(page);
  vi.mocked(api.applyChanges).mockResolvedValue({ applied: 2 });
});

describe("rows whose keys have the same text", () => {
  it("are two rows: editing both saves two updates with their own key types", async () => {
    await mount();
    act(() => grid.editable!.onEdit(0, 1, "int edited"));
    act(() => grid.editable!.onEdit(1, 1, "text edited"));
    expect(grid.editable!.cellValue(0, 1)).toEqual({ value: "int edited", edited: true });
    expect(grid.editable!.cellValue(1, 1)).toEqual({ value: "text edited", edited: true });

    await act(() => useApp.getState().saveTableChanges(tabId));
    expect(api.applyChanges).toHaveBeenCalledWith({
      table,
      changes: [
        { kind: "update", key: ["5"], values: { v: "int edited" }, originalTypes: { k: "i", v: "t" } },
        { kind: "update", key: ["5"], values: { v: "text edited" }, originalTypes: { k: "t", v: "t" } },
      ],
    });
  });

  it("deleting one leaves the other editable; Restore row and Revert cell act on their own row", async () => {
    await mount();
    act(() => grid.editable!.onDeleteRows([0]));
    expect(grid.editable!.rowState(0)).toBe("deleted");
    expect(grid.editable!.rowState(1)).toBeNull();
    act(() => grid.editable!.onEdit(1, 1, "text edited"));
    expect(grid.editable!.cellValue(1, 1)).toEqual({ value: "text edited", edited: true });
    expect(grid.editable!.cellValue(0, 1)).toEqual({ value: "int five", edited: false });

    act(() => grid.editable!.onContextMenu(1, 1, { x: 10, y: 10 }));
    await userEvent.click(screen.getByRole("menuitem", { name: "Revert cell" }));
    expect(grid.editable!.cellValue(1, 1)).toEqual({ value: "text five", edited: false });
    expect(grid.editable!.rowState(0)).toBe("deleted");

    act(() => grid.editable!.onContextMenu(0, 1, { x: 10, y: 10 }));
    await userEvent.click(screen.getByRole("menuitem", { name: "Restore row" }));
    expect(grid.editable!.rowState(0)).toBeNull();
    expect(grid.editable!.rowState(1)).toBeNull();

    act(() => grid.editable!.onDeleteRows([1]));
    await act(() => useApp.getState().saveTableChanges(tabId));
    expect(api.applyChanges).toHaveBeenCalledWith({
      table,
      changes: [{ kind: "delete", key: ["5"], originalTypes: { k: "t" } }],
    });
  });
});

describe("a row whose key is NULL", () => {
  it("is read-only with the empty-key note: no edit, no delete", async () => {
    await mount();
    expect(EMPTY_KEY_READ_ONLY).toBe("Rows with an empty key can't be edited.");
    expect(grid.editable!.readOnlyCell?.(2, 0)).toBe(EMPTY_KEY_READ_ONLY);
    expect(grid.editable!.readOnlyCell?.(2, 1)).toBe(EMPTY_KEY_READ_ONLY);
    expect(grid.editable!.readOnlyCell?.(1, 1)).toBeNull();

    act(() => grid.editable!.onEdit(2, 1, "changed"));
    act(() => grid.editable!.onDeleteRows([2]));
    expect(grid.editable!.cellValue(2, 1)).toEqual({ value: "null key", edited: false });
    expect(grid.editable!.rowState(2)).toBeNull();

    act(() => grid.editable!.onContextMenu(2, 1, { x: 10, y: 10 }));
    const setNull = screen.getByRole("menuitem", { name: "Set NULL" }) as HTMLButtonElement;
    const del = screen.getByRole("menuitem", { name: "Delete row" }) as HTMLButtonElement;
    expect(setNull.disabled).toBe(true);
    expect(del.disabled).toBe(true);
    await userEvent.click(del);
    expect(grid.editable!.rowState(2)).toBeNull();
    const pendingNow = (useApp.getState().tabs.find((t) => t.id === tabId) as TableTab).pending;
    expect(pendingNow).toEqual(emptyPending);
  });
});

describe("a column named like an Object.prototype member", () => {
  it("shows its fetched value, not an edit, while another cell of the row is edited", async () => {
    const odd: TablePage = {
      columns: [col("id", { isPrimaryKey: true }), col("constructor"), col("toString")],
      primaryKey: ["id"],
      editing: { kind: "allowed" },
      rows: [["1", "c", "t"]],
      total: { kind: "exact", value: 1 },
      matches: null,
      elapsedMs: 1,
    };
    vi.mocked(api.fetchPage).mockResolvedValue(odd);
    await mount();
    act(() => grid.editable!.onEdit(0, 0, "2"));
    expect(grid.editable!.cellValue(0, 0)).toEqual({ value: "2", edited: true });
    expect(grid.editable!.cellValue(0, 1)).toEqual({ value: "c", edited: false });
    expect(grid.editable!.cellValue(0, 2)).toEqual({ value: "t", edited: false });
  });
});
