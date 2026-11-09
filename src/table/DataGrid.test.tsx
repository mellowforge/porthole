import { act, render, screen } from "@testing-library/react";
import { CompactSelection, GridCellKind } from "@glideapps/glide-data-grid";
import type { DataEditorProps, GridCell, GridSelection, TextCell } from "@glideapps/glide-data-grid";
import { beforeEach, describe, expect, it, vi } from "vitest";

// jsdom can't drive Glide's canvas: capture the props DataGrid hands to DataEditor instead.
const h = vi.hoisted(() => ({ props: undefined as unknown as DataEditorProps, scrollTo: (() => {}) as (...a: unknown[]) => void }));
vi.mock("@glideapps/glide-data-grid", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@glideapps/glide-data-grid")>();
  const { forwardRef } = await import("react");
  return {
    ...actual,
    DataEditor: forwardRef((p: DataEditorProps, ref) => {
      h.props = p;
      const api = { scrollTo: (...a: unknown[]) => h.scrollTo(...a) };
      if (typeof ref === "function") ref(api as never);
      else if (ref) (ref as { current: unknown }).current = api;
      return <div data-testid="editor" />;
    }),
  };
});

import { DataGrid, type DataGridProps } from "./DataGrid";

const scrollTo = vi.fn();
h.scrollTo = scrollTo;

type Editable = NonNullable<DataGridProps["editable"]>;
const columns = [
  { name: "id", dataType: "int4" },
  { name: "plan", dataType: "varchar" },
];
const rows: (string | null)[][] = [
  ["1003", "pro"],
  ["1040", null],
];

function editable(over: Partial<Editable> = {}): Editable {
  return {
    cellValue: (row, col) => ({ value: rows[row]?.[col] ?? null, edited: false }),
    rowState: () => null,
    readOnlyColumn: () => false,
    onEdit: vi.fn(),
    onContextMenu: vi.fn(),
    onDeleteRows: vi.fn(),
    ...over,
  };
}

const cell = (c: number, r: number) => h.props.getCellContent([c, r]) as GridCell & TextCell;
const selection = (rowsSel: CompactSelection): GridSelection => ({ rows: rowsSel, columns: CompactSelection.empty() });

beforeEach(() => {
  scrollTo.mockReset();
});

describe("DataGrid editing", () => {
  it("read-only grid keeps the row-number column and readonly cells", () => {
    render(<DataGrid columns={columns} rows={rows} rowOffset={0} />);
    expect(h.props.columns).toHaveLength(3);
    expect(cell(1, 0).data).toBe("1003");
    expect(cell(1, 0).readonly).toBe(true);
    expect(h.props.onCellEdited).toBeUndefined();
    expect(h.props.onDelete).toBeUndefined();
    expect(h.props.rowMarkers).toBe("none");
  });

  it("edited cell shows pending value and amber override", () => {
    const ed = editable({
      cellValue: (row, col) => (row === 0 && col === 1 ? { value: "team", edited: true } : { value: rows[row][col], edited: false }),
    });
    render(<DataGrid columns={columns} rows={rows} rowOffset={0} editable={ed} />);
    // Editable grids use Glide's row markers, so column indices map straight to `columns`.
    expect(h.props.columns).toHaveLength(2);
    expect(cell(1, 0).data).toBe("team");
    expect(cell(1, 0).readonly).toBe(false);
    expect(cell(1, 0).themeOverride?.bgCell).toBe("#FEF3C7");
    expect(cell(0, 0).themeOverride).toBeUndefined();
  });

  it("deleted row override and strikethrough flag", () => {
    const ed = editable({ rowState: (row) => (row === 1 ? "deleted" : null) });
    render(<DataGrid columns={columns} rows={rows} rowOffset={0} editable={ed} />);
    expect(h.props.getRowThemeOverride?.(1)).toMatchObject({ bgCell: "#FDECEC", textDark: "#d92d20" });
    expect(h.props.getRowThemeOverride?.(0)).toBeUndefined();
    expect(cell(0, 1).readonly).toBe(true);

    // drawCell draws the content, then a line through it for deleted rows only.
    const ctx = { measureText: () => ({ width: 40 }), beginPath: vi.fn(), moveTo: vi.fn(), lineTo: vi.fn(), stroke: vi.fn() };
    const theme = { cellHorizontalPadding: 10, baseFontStyle: "13px", fontFamily: "x", textDark: "#d92d20" };
    const rect = { x: 0, y: 0, width: 160, height: 30 };
    const drawContent = vi.fn();
    const args = (row: number) => ({ ctx, cell: cell(0, row), theme, rect, col: 0, row }) as never;
    h.props.drawCell?.(args(1), drawContent);
    expect(drawContent).toHaveBeenCalledTimes(1);
    expect(ctx.stroke).toHaveBeenCalledTimes(1);
    expect(ctx.moveTo).toHaveBeenCalledWith(10, 15);
    expect(ctx.lineTo).toHaveBeenCalledWith(50, 15);
    h.props.drawCell?.(args(0), drawContent);
    expect(drawContent).toHaveBeenCalledTimes(2);
    expect(ctx.stroke).toHaveBeenCalledTimes(1);
  });

  it("new rows render after fetched rows", () => {
    const ed = editable({
      rowState: (row) => (row >= 2 ? "new" : null),
      cellValue: (row, col) => (row >= 2 ? { value: col === 1 ? "free" : null, edited: col === 1 } : { value: rows[row][col], edited: false }),
    });
    const { rerender } = render(<DataGrid columns={columns} rows={rows} rowOffset={0} editable={ed} extraRows={1} />);
    expect(h.props.rows).toBe(3);
    expect(cell(1, 2).data).toBe("free");
    expect(cell(0, 2).displayData).toBe("");
    expect(cell(0, 2).themeOverride?.bgCell).toBe("#FEF3C7");
    rerender(<DataGrid columns={columns} rows={rows} rowOffset={0} editable={ed} extraRows={2} />);
    expect(h.props.rows).toBe(4);
    // Adding a row scrolls to it.
    expect(scrollTo).toHaveBeenLastCalledWith(0, 3, "vertical");
  });

  it("onDelete with row selection marks rows and returns false", () => {
    const ed = editable();
    render(<DataGrid columns={columns} rows={rows} rowOffset={0} editable={ed} />);
    expect(h.props.onDelete?.(selection(CompactSelection.fromSingleSelection([0, 2])))).toBe(false);
    expect(ed.onDeleteRows).toHaveBeenCalledWith([0, 1]);

    // A cell range never clears cells.
    vi.mocked(ed.onDeleteRows).mockClear();
    const range: GridSelection = {
      ...selection(CompactSelection.empty()),
      current: { cell: [0, 0], range: { x: 0, y: 0, width: 2, height: 2 }, rangeStack: [] },
    };
    expect(h.props.onDelete?.(range)).toBe(false);
    expect(ed.onDeleteRows).not.toHaveBeenCalled();
  });

  it("typing on a selected cell does not open the editor", () => {
    render(<DataGrid columns={columns} rows={rows} rowOffset={0} editable={editable()} />);
    expect(h.props.editOnType).toBe(false);
    expect(h.props.cellActivationBehavior).toBe("double-click");
  });

  it("onCellEdited passes text through and skips an empty confirm on NULL", () => {
    const ed = editable();
    render(<DataGrid columns={columns} rows={rows} rowOffset={0} editable={ed} />);
    const text = (data: string) => ({ kind: GridCellKind.Text, data, displayData: data, allowOverlay: true }) as const;
    h.props.onCellEdited?.([1, 0], text("team"));
    expect(ed.onEdit).toHaveBeenCalledWith(0, 1, "team");
    vi.mocked(ed.onEdit).mockClear();
    h.props.onCellEdited?.([1, 1], text("")); // plan is NULL in row 1
    expect(ed.onEdit).not.toHaveBeenCalled();
    h.props.onCellEdited?.([1, 0], text(""));
    expect(ed.onEdit).toHaveBeenCalledWith(0, 1, "");
  });

  it("right-click on a cell opens the menu at the pointer", () => {
    const ed = editable();
    render(<DataGrid columns={columns} rows={rows} rowOffset={0} editable={ed} />);
    const preventDefault = vi.fn();
    h.props.onCellContextMenu?.([1, 0], {
      bounds: { x: 100, y: 200, width: 160, height: 30 },
      localEventX: 12,
      localEventY: 8,
      preventDefault,
    } as never);
    expect(preventDefault).toHaveBeenCalled();
    expect(ed.onContextMenu).toHaveBeenCalledWith(0, 1, { x: 112, y: 208 });
  });
});

describe("DataGrid read-only cells (blob values)", () => {
  const blob = "Blob values can't be edited yet.";
  const ed = () => editable({ readOnlyCell: (row, col) => (row === 0 && col === 1 ? blob : null) });
  const hover = (col: number, row: number) =>
    act(() => h.props.onItemHovered?.({ kind: "cell", location: [col, row], bounds: { x: 100, y: 200, width: 160, height: 30 } } as never));

  it("a cell the grid calls read-only doesn't open the editor on double-click, nor record an edit", () => {
    const e = ed();
    render(<DataGrid columns={columns} rows={[["1003", "\\xdead"], ...rows.slice(1)]} rowOffset={0} editable={e} />);
    expect(cell(1, 0).readonly).toBe(true);
    expect(cell(1, 0).allowOverlay).toBe(false);
    expect(cell(0, 0).readonly).toBe(false);
    expect(cell(0, 0).allowOverlay).toBe(true);
    expect(cell(1, 1).readonly).toBe(false);
    h.props.onCellEdited?.([1, 0], { kind: GridCellKind.Text, data: "x", displayData: "x", allowOverlay: true });
    expect(e.onEdit).not.toHaveBeenCalled();
  });

  it("hovering it shows its note under the cell; leaving it hides the note", () => {
    render(<DataGrid columns={columns} rows={rows} rowOffset={0} editable={ed()} />);
    expect(screen.queryByRole("tooltip")).toBeNull();
    hover(1, 0);
    const tip = screen.getByRole("tooltip");
    expect(tip.textContent).toBe(blob);
    expect(tip.style.left).toBe("100px");
    expect(tip.style.top).toBe("234px");
    hover(0, 0);
    expect(screen.queryByRole("tooltip")).toBeNull();
    hover(1, 0);
    act(() => h.props.onItemHovered?.({ kind: "out-of-bounds", location: [0, 0] } as never));
    expect(screen.queryByRole("tooltip")).toBeNull();
  });

  it("without the callback (Postgres grids) nothing is read-only per cell", () => {
    render(<DataGrid columns={columns} rows={rows} rowOffset={0} editable={editable()} />);
    expect(cell(1, 0).readonly).toBe(false);
    hover(1, 0);
    expect(screen.queryByRole("tooltip")).toBeNull();
  });
});
