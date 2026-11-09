import type { Theme } from "@glideapps/glide-data-grid";
import { nullCellTheme, toGridCell } from "./gridModel";

export type RowState = "deleted" | "new" | null;

/** A cell's current value: the pending value if edited, else the original. */
export interface CellValue {
  value: string | null;
  edited: boolean;
}

export interface EditColors {
  muted: string;
  pendingBg: string;
  deletedBg: string;
  danger: string;
}

export interface EditCellView {
  data: string;
  displayData: string;
  readonly: boolean;
  themeOverride: Partial<Theme> | undefined;
  /** Draw a line through the text (rows marked for deletion). */
  strike: boolean;
}

/**
 * How an editable cell renders. Edited cells and new-row cells are amber; deleted rows take their
 * red from `rowThemeFor`, so their cells set no colour. A new row's cell the user hasn't set is blank
 * (the database fills in its default). Read-only columns and deleted rows can't be edited.
 */
export function cellView(v: CellValue, rowState: RowState, readOnlyColumn: boolean, colors: EditColors): EditCellView {
  const deleted = rowState === "deleted";
  const blank = rowState === "new" && !v.edited;
  const cell = blank ? { data: "", displayData: "", isNull: false } : toGridCell(v.value);
  let theme: Partial<Theme> = {};
  if (cell.isNull) theme = deleted ? { baseFontStyle: nullCellTheme(colors.muted).baseFontStyle } : nullCellTheme(colors.muted);
  if (!deleted && (v.edited || rowState === "new")) theme = { ...theme, bgCell: colors.pendingBg };
  return {
    data: cell.data,
    displayData: cell.displayData,
    readonly: deleted || readOnlyColumn,
    themeOverride: Object.keys(theme).length ? theme : undefined,
    strike: deleted,
  };
}

/** Row colours for a row marked for deletion (`textLight` colours the row number too). */
export function rowThemeFor(rowState: RowState, colors: EditColors): Partial<Theme> | undefined {
  if (rowState !== "deleted") return undefined;
  return { bgCell: colors.deletedBg, textDark: colors.danger, textLight: colors.danger };
}

export type MenuAction = "setNull" | "revert" | "delete" | "restore";

export interface EditMenuItem {
  id: MenuAction;
  label: string;
  disabled?: boolean;
  danger?: boolean;
}

/** Context-menu items for a cell. New rows have no Revert: their cells have no original to go back to. */
export function menuItemsFor(c: { nullable: boolean; readOnly: boolean; edited: boolean; rowState: RowState }): EditMenuItem[] {
  const items: EditMenuItem[] = [
    { id: "setNull", label: "Set NULL", disabled: !c.nullable || c.readOnly || c.rowState === "deleted" },
  ];
  if (c.edited && c.rowState === null) items.push({ id: "revert", label: "Revert cell" });
  items.push(
    c.rowState === "deleted"
      ? { id: "restore", label: "Restore row" }
      : { id: "delete", label: "Delete row", danger: true },
  );
  return items;
}

/**
 * The editor opens empty on a NULL cell, so confirming "" there keeps NULL (no edit).
 * `current` is the pending value if the cell is edited, else the original.
 */
export function shouldRecordEdit(current: string | null, typed: string): boolean {
  return !(current === null && typed === "");
}
