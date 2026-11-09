import { DataEditor, GridCellKind } from "@glideapps/glide-data-grid";
import type {
  CellClickedEventArgs,
  DataEditorRef,
  DrawCellCallback,
  DrawHeaderCallback,
  EditableGridCell,
  GridCell,
  GridColumn,
  GridMouseEventArgs,
  GridSelection,
  Item,
  Theme,
} from "@glideapps/glide-data-grid";
import "@glideapps/glide-data-grid/dist/index.css";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { SortSpec } from "../api/types";
import { cellView, type EditColors, type RowState, rowThemeFor, shouldRecordEdit } from "./editModel";
import { nullCellTheme, toGridCell } from "./gridModel";

/** The part of a column the grid needs; `ColumnInfo` and `ResultColumn` both satisfy it. */
export interface GridColumnInfo {
  name: string;
  dataType: string;
}

export interface DataGridProps {
  columns: GridColumnInfo[];
  rows: (string | null)[][];
  /** Number of rows on earlier pages, used for the row-number column. */
  rowOffset: number;
  sort?: SortSpec | null;
  /** Without a handler, header clicks do nothing. */
  onHeaderClick?: (columnName: string) => void;
  /**
   * Makes the grid editable. Rows and columns here are grid indices: `col` indexes `columns` and
   * rows at or past `rows.length` are the `extraRows`. Query results never pass this.
   */
  editable?: {
    /** The pending value if edited, else the original. */
    cellValue(row: number, col: number): { value: string | null; edited: boolean };
    rowState(row: number): RowState;
    readOnlyColumn(col: number): boolean;
    /** Why one cell can't be edited (a blob value), shown when hovering it; `null` when it can. */
    readOnlyCell?(row: number, col: number): string | null;
    onEdit(row: number, col: number, value: string): void;
    onContextMenu(row: number, col: number, at: { x: number; y: number }): void;
    onDeleteRows(rows: number[]): void;
  };
  /** New rows appended after the fetched rows. */
  extraRows?: number;
}

const ROW_NUM_WIDTH = 48;
const DEFAULT_WIDTH = 160;

function cssVar(name: string, fallback: string): string {
  const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return v || fallback;
}
const mutedColor = () => cssVar("--muted", "#a1a1a6");
const editColors = (): EditColors => ({
  muted: mutedColor(),
  pendingBg: cssVar("--pending-bg", "#FEF3C7"),
  deletedBg: cssVar("--deleted-bg", "#FDECEC"),
  danger: cssVar("--danger", "#d92d20"),
});

/** Strikes through the text of cells in rows marked for deletion. */
function strikeDrawer(isDeleted: (row: number) => boolean): DrawCellCallback {
  return ({ ctx, cell, theme, rect, row }, drawContent) => {
    drawContent();
    if (!isDeleted(row) || cell.kind !== GridCellKind.Text) return;
    const pad = theme.cellHorizontalPadding;
    ctx.font = `${theme.baseFontStyle} ${theme.fontFamily}`;
    const width = Math.min(ctx.measureText(cell.displayData).width, rect.width - 2 * pad);
    if (width <= 0) return;
    const y = Math.round(rect.y + rect.height / 2);
    ctx.beginPath();
    ctx.moveTo(rect.x + pad, y);
    ctx.lineTo(rect.x + pad + width, y);
    ctx.strokeStyle = theme.textDark;
    ctx.lineWidth = 1;
    ctx.stroke();
  };
}

/** Grid colours from the design tokens. Neutral selection keeps `--accent` for primary buttons only. */
function gridTheme(): Partial<Theme> {
  return {
    textDark: cssVar("--text", "#1c1c1e"),
    textMedium: cssVar("--text-2", "#6e6e73"),
    textLight: cssVar("--muted", "#a1a1a6"),
    textHeader: cssVar("--text", "#1c1c1e"),
    textHeaderSelected: "#ffffff",
    accentColor: cssVar("--text-2", "#6e6e73"),
    accentLight: "rgba(110, 110, 115, 0.12)",
    bgHeader: cssVar("--grid-header", "#fafafc"),
    bgHeaderHasFocus: cssVar("--hover", "#f2f2f7"),
    bgHeaderHovered: cssVar("--hover", "#f2f2f7"),
    borderColor: cssVar("--grid-line", "#f0f0f3"),
    headerBottomBorderColor: cssVar("--border", "#e3e3e8"),
    cellHorizontalPadding: 10,
    headerFontStyle: "600 12px",
    // Canvas cannot resolve CSS variables, so the --font-ui stack is repeated here.
    fontFamily: '-apple-system, BlinkMacSystemFont, "Inter", sans-serif',
  };
}

/** Header as in the sketch: column name in semibold, its type muted beside it. */
function headerDrawer(columns: GridColumnInfo[], sort: SortSpec | null): DrawHeaderCallback {
  const byName = new Map(columns.map((c) => [c.name, c]));
  return ({ ctx, column, theme, rect, isSelected }, drawContent) => {
    const info = column.id ? byName.get(column.id) : undefined;
    if (!info) return drawContent();
    const arrow = sort?.column === info.name ? (sort.descending ? " ↓" : " ↑") : "";
    const x = rect.x + theme.cellHorizontalPadding;
    const y = rect.y + rect.height / 2;
    ctx.textBaseline = "middle";
    ctx.textAlign = "left";
    ctx.font = `${theme.headerFontStyle} ${theme.fontFamily}`;
    ctx.fillStyle = isSelected ? theme.textHeaderSelected : theme.textHeader;
    const label = info.name + arrow;
    ctx.fillText(label, x, y);
    const typeX = x + ctx.measureText(label).width + 6;
    ctx.font = `11px ${theme.fontFamily}`;
    ctx.fillStyle = isSelected ? theme.textHeaderSelected : theme.textLight;
    ctx.fillText(info.dataType, typeX, y);
  };
}

export function DataGrid({
  columns,
  rows,
  rowOffset,
  sort = null,
  onHeaderClick,
  editable,
  extraRows = 0,
}: DataGridProps) {
  const [widths, setWidths] = useState<Record<string, number>>({});
  const [note, setNote] = useState<{ text: string; x: number; y: number } | null>(null);
  const muted = useMemo(mutedColor, []);
  const colors = useMemo(editColors, []);
  const theme = useMemo(gridTheme, []);
  const drawHeader = useMemo(() => headerDrawer(columns, sort), [columns, sort]);
  const ref = useRef<DataEditorRef>(null);
  // Editable grids use Glide's row markers (clickable, so whole rows can be selected and deleted);
  // read-only grids keep a plain row-number column at index 0.
  const first = editable ? 0 : 1;
  const totalRows = rows.length + (editable ? extraRows : 0);

  const gridColumns = useMemo<GridColumn[]>(
    () => [
      ...(editable ? [] : [{ id: "__row", title: "", width: ROW_NUM_WIDTH }]),
      ...columns.map((c) => {
        const arrow = sort?.column === c.name ? (sort.descending ? " ↓" : " ↑") : "";
        return {
          id: c.name,
          title: `${c.name}  ${c.dataType}${arrow}`,
          width: widths[c.name] ?? DEFAULT_WIDTH,
        };
      }),
    ],
    [columns, sort, widths, editable],
  );

  const getCellContent = useCallback(
    ([col, row]: Item): GridCell => {
      if (editable) {
        const v = cellView(editable.cellValue(row, col), editable.rowState(row), editable.readOnlyColumn(col), colors);
        // A cell with a read-only note (a blob value) doesn't open at all on double-click.
        const note = editable.readOnlyCell?.(row, col);
        return {
          kind: GridCellKind.Text,
          data: v.data,
          displayData: v.displayData,
          allowOverlay: !note,
          readonly: v.readonly || !!note,
          themeOverride: v.themeOverride,
        };
      }
      if (col === 0) {
        const n = String(rowOffset + row + 1);
        return {
          kind: GridCellKind.Text,
          data: n,
          displayData: n,
          allowOverlay: false,
          readonly: true,
          themeOverride: { textDark: muted, baseFontStyle: "11px" },
        };
      }
      const cell = toGridCell(rows[row]?.[col - 1] ?? null);
      return {
        kind: GridCellKind.Text,
        data: cell.data,
        displayData: cell.displayData,
        allowOverlay: true,
        readonly: true,
        themeOverride: cell.isNull ? nullCellTheme(muted) : undefined,
      };
    },
    [rows, rowOffset, muted, editable, colors],
  );

  const getRowThemeOverride = useCallback(
    (row: number) => (editable ? rowThemeFor(editable.rowState(row), colors) : undefined),
    [editable, colors],
  );
  const drawCell = useMemo(
    () => (editable ? strikeDrawer((row) => editable.rowState(row) === "deleted") : undefined),
    [editable],
  );

  const onCellEdited = useCallback(
    ([col, row]: Item, value: EditableGridCell) => {
      if (!editable || value.kind !== GridCellKind.Text) return;
      if (editable.readOnlyCell?.(row, col)) return;
      if (!shouldRecordEdit(editable.cellValue(row, col).value, value.data)) return;
      editable.onEdit(row, col, value.data);
    },
    [editable],
  );

  const onCellContextMenu = useCallback(
    ([col, row]: Item, e: CellClickedEventArgs) => {
      if (!editable || col < 0) return;
      e.preventDefault();
      // `bounds` is the cell in viewport coordinates and `localEvent*` the pointer inside it.
      editable.onContextMenu(row, col, { x: e.bounds.x + e.localEventX, y: e.bounds.y + e.localEventY });
    },
    [editable],
  );

  // A read-only cell's note, under the cell (Glide draws cells on a canvas, so they have no `title`).
  const onItemHovered = useCallback(
    (args: GridMouseEventArgs) => {
      const [col, row] = args.location;
      const text = args.kind === "cell" ? editable?.readOnlyCell?.(row, col) : null;
      setNote(text && args.kind === "cell" ? { text, x: args.bounds.x, y: args.bounds.y + args.bounds.height + 4 } : null);
    },
    [editable],
  );

  // ⌫ / Delete: whole selected rows are marked for deletion; a cell range never clears cells.
  const onDelete = useCallback(
    (sel: GridSelection) => {
      const selected = sel.rows.toArray();
      if (editable && selected.length > 0) editable.onDeleteRows(selected);
      return false;
    },
    [editable],
  );

  // Scroll to a row the user just added.
  const prevExtra = useRef(extraRows);
  useEffect(() => {
    if (editable && extraRows > prevExtra.current) ref.current?.scrollTo(0, totalRows - 1, "vertical");
    prevExtra.current = extraRows;
  }, [editable, extraRows, totalRows]);

  return (
    <>
      <DataEditor
        ref={ref}
        columns={gridColumns}
        rows={totalRows}
        getCellContent={getCellContent}
        width="100%"
        height="100%"
        theme={theme}
        rowHeight={30}
        headerHeight={32}
        drawHeader={drawHeader}
        smoothScrollX
        smoothScrollY
        onHeaderClicked={(col) => {
          if (col < first) return;
          const c = columns[col - first];
          if (c) onHeaderClick?.(c.name);
        }}
        onColumnResize={(col, newSize) => {
          if (col.id && col.id !== "__row") setWidths((w) => ({ ...w, [col.id as string]: newSize }));
        }}
        {...(editable
          ? {
              rowMarkers: {
                kind: "clickable-number" as const,
                startIndex: rowOffset + 1,
                width: ROW_NUM_WIDTH,
                theme: { markerFontStyle: "11px" },
              },
              editOnType: false,
              cellActivationBehavior: "double-click" as const,
              getRowThemeOverride,
              drawCell,
              onCellEdited,
              onCellContextMenu,
              onDelete,
              onItemHovered,
            }
          : { rowMarkers: "none" as const })}
      />
      {note && (
        <div className="grid-note" role="tooltip" style={{ left: note.x, top: note.y }}>
          {note.text}
        </div>
      )}
    </>
  );
}
