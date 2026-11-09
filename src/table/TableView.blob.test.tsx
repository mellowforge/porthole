import { act, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ColumnInfo, ConnectedInfo, TablePage } from "../api/types";
import type { DataGridProps } from "./DataGrid";
import { rowTypes } from "./cells";
import { emptyPending, keyId } from "./pending";

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
import { BLOB_READ_ONLY } from "./useTableEditing";
import { TableView } from "./TableView";

const col = (name: string, over: Partial<ColumnInfo> = {}): ColumnInfo => ({
  name, dataType: "blob", fullType: "blob", castType: "", nullable: true, default: null, isPrimaryKey: false, readOnly: false, ...over,
});
/** `typed`-like: an integer key and a blob column; row 2's blob is NULL (its cell is not a blob). */
const page: TablePage = {
  columns: [col("id", { dataType: "integer", isPrimaryKey: true, nullable: false }), col("b")],
  primaryKey: ["id"],
  editing: { kind: "allowed" },
  rows: [["1", "\\xdead"], ["2", null]],
  cellTypes: ["ib", "in"],
  total: { kind: "exact", value: 2 },
  matches: null,
  elapsedMs: 1,
};
const tabId = "main.typed";
const tab: TableTab = { kind: "table", id: tabId, table: { schema: "main", name: "typed" }, view: { mode: "data", filter: null, sort: null }, pending: emptyPending };

function Live() {
  const t = useApp((s) => s.tabs.find((x) => x.id === tabId)) as TableTab;
  return <TableView tab={t} />;
}
const pendingNow = () => (useApp.getState().tabs.find((t) => t.id === tabId) as TableTab).pending;

beforeEach(() => {
  vi.resetAllMocks();
  useApp.setState({ tabs: [tab], activeTabId: tabId, leave: null, leaveError: null });
  useApp.setState({ active: { profile: { readOnly: false }, schema: "main" } as unknown as ConnectedInfo });
  vi.mocked(api.fetchPage).mockResolvedValue(page);
});

describe("blob cells", () => {
  it("are read-only with the blob note; other cells are not", async () => {
    render(<Live />);
    await screen.findByTestId("grid");
    expect(BLOB_READ_ONLY).toBe("Blob values can't be edited yet.");
    expect(grid.editable!.readOnlyCell?.(0, 1)).toBe(BLOB_READ_ONLY);
    expect(grid.editable!.readOnlyCell?.(0, 0)).toBeNull();
    // A NULL in a blob column is not a blob value.
    expect(grid.editable!.readOnlyCell?.(1, 1)).toBeNull();
  });

  it("an edit of a blob cell records nothing", async () => {
    render(<Live />);
    await screen.findByTestId("grid");
    act(() => grid.editable!.onEdit(0, 1, "\\xbeef"));
    expect(pendingNow().edits[keyId(["1"], rowTypes(page, 0))]).toBeUndefined();
    act(() => grid.editable!.onEdit(1, 1, "\\xbeef"));
    expect(pendingNow().edits[keyId(["2"], rowTypes(page, 1))].values).toEqual({ b: "\\xbeef" });
  });

  it("Set NULL is disabled on a blob cell; Delete row still works", async () => {
    render(<Live />);
    await screen.findByTestId("grid");
    act(() => grid.editable!.onContextMenu(0, 1, { x: 10, y: 10 }));
    const setNull = screen.getByRole("menuitem", { name: "Set NULL" }) as HTMLButtonElement;
    expect(setNull.disabled).toBe(true);
    await userEvent.click(setNull);
    expect(pendingNow().edits[keyId(["1"], rowTypes(page, 0))]).toBeUndefined();
    await userEvent.click(screen.getByRole("menuitem", { name: "Delete row" }));
    expect(pendingNow().deletes[keyId(["1"], rowTypes(page, 0))]).toEqual(["1"]);
  });

  it("an x cell (another BSON type) is read-only with its own note and records no edit", async () => {
    vi.mocked(api.fetchPage).mockResolvedValue({ ...page, cellTypes: ["ix", "in"] });
    render(<Live />);
    await screen.findByTestId("grid");
    expect(grid.editable!.readOnlyCell?.(0, 1)).toBe("Values of this type can't be edited yet.");
    act(() => grid.editable!.onEdit(0, 1, "z"));
    expect(Object.keys(pendingNow().edits)).toEqual([]);
  });

  it("a page without cell types has no read-only cells", async () => {
    const { cellTypes: _, ...untyped } = page;
    vi.mocked(api.fetchPage).mockResolvedValue(untyped);
    render(<Live />);
    await screen.findByTestId("grid");
    expect(grid.editable!.readOnlyCell?.(0, 1)).toBeNull();
  });
});
