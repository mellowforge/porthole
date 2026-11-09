import { type ReactNode, useCallback, useEffect, useMemo, useState } from "react";
import type { TablePage } from "../api/types";
import { ContextMenu, type ContextMenuItem } from "../components/ContextMenu";
import { useApp } from "../state/store";
import type { DataGridProps } from "./DataGrid";
import { cellType, rowTypes } from "./cells";
import { type MenuAction, menuItemsFor } from "./editModel";
import { keyId, type PendingChanges, rowKey, type RowKey } from "./pending";

/** A grid row: a fetched row (by its original key and its id among the pending changes) or a new row (by its temp id). */
type RowRef = { kind: "fetched"; key: RowKey; id: string } | { kind: "new"; tempId: string };

type Editable = NonNullable<DataGridProps["editable"]>;

/** The note on a blob cell, which the grid can't edit yet. */
export const BLOB_READ_ONLY = "Blob values can't be edited yet.";
/** The note on a cell of a BSON type the grid can't edit (MongoDB's binary, UUID, regex, …). */
export const OTHER_READ_ONLY = "Values of this type can't be edited yet.";
/** The note on every cell of a row whose key holds NULL (SQLite allows it): the key can't name the row. */
export const EMPTY_KEY_READ_ONLY = "Rows with an empty key can't be edited.";

/**
 * The grid's `editable` callbacks and the cell context menu, over a fetched page and the tab's pending
 * changes. Grid rows past the fetched rows are the pending inserts, in order. `enabled` false gives
 * a read-only grid (`editable` undefined, no extra rows). `locked` (a save is running) keeps showing
 * the pending state but refuses every edit, context-menu action and ⌫, and closes an open context
 * menu; TableView also hides + Add row meanwhile.
 */
export function useTableEditing(
  tabId: string,
  data: TablePage | null,
  pending: PendingChanges,
  enabled: boolean,
  locked = false,
): { editable: Editable | undefined; extraRows: number; menu: ReactNode } {
  const editCell = useApp((s) => s.editCell);
  const revertCell = useApp((s) => s.revertCell);
  const toggleDeleteRow = useApp((s) => s.toggleDeleteRow);
  const editNewRow = useApp((s) => s.editNewRow);
  const removeNewRow = useApp((s) => s.removeNewRow);
  const [menuAt, setMenuAt] = useState<{ row: number; col: number; x: number; y: number } | null>(null);
  // A menu opened before the save started must not act on rows that are being written.
  useEffect(() => {
    if (locked) setMenuAt(null);
  }, [locked]);

  // Each fetched row's key and its id among the pending changes (see `keyId`).
  const fetched = useMemo(
    () =>
      data
        ? data.rows.map((r, row) => {
            const key = rowKey(r, data.columns, data.primaryKey);
            return { key, id: keyId(key, rowTypes(data, row)) };
          })
        : [],
    [data],
  );
  const rowRef = useCallback(
    (row: number): RowRef | null => {
      if (row < fetched.length) return { kind: "fetched", ...fetched[row] };
      const ins = pending.inserts[row - fetched.length];
      return ins ? { kind: "new", tempId: ins.tempId } : null;
    },
    [fetched, pending.inserts],
  );

  const ops = useMemo(() => {
    if (!enabled || !data) return null;
    const name = (col: number) => data.columns[col]?.name ?? "";
    const isDeleted = (r: RowRef | null) => r?.kind === "fetched" && Object.hasOwn(pending.deletes, r.id);
    const cellValue: Editable["cellValue"] = (row, col) => {
      const r = rowRef(row);
      const values =
        r?.kind === "fetched"
          ? (Object.hasOwn(pending.edits, r.id) ? pending.edits[r.id].values : undefined)
          : pending.inserts.find((i) => r && i.tempId === r.tempId)?.values;
      const column = name(col);
      if (values && Object.hasOwn(values, column)) return { value: values[column], edited: true };
      return { value: r?.kind === "fetched" ? (data.rows[row][col] ?? null) : null, edited: false };
    };
    // Only fetched cells carry a type; a NULL in a blob column is not a blob value.
    const noEditType = (row: number, col: number) => {
      const code = row < data.rows.length ? cellType(data.cellTypes, row, col) : null;
      return code === "b" ? BLOB_READ_ONLY : code === "x" ? OTHER_READ_ONLY : null;
    };
    const notEditable = (row: number, col: number) => noEditType(row, col) !== null;
    // A fetched row whose key holds NULL: rowKey shows it as "", which would name no row (or another one).
    const emptyKey = (row: number) =>
      row < data.rows.length &&
      data.primaryKey.some((pk) => {
        const i = data.columns.findIndex((c) => c.name === pk);
        return i >= 0 && data.rows[row][i] === null;
      });
    const setValue = (row: number, col: number, value: string | null) => {
      if (notEditable(row, col) || emptyKey(row)) return;
      const r = rowRef(row);
      if (r?.kind === "fetched") editCell(tabId, r.key, name(col), value, data.rows[row][col] ?? null, rowTypes(data, row));
      else if (r) editNewRow(tabId, r.tempId, name(col), value);
    };
    const editable: Editable = {
      cellValue,
      rowState: (row) => {
        const r = rowRef(row);
        return r?.kind === "new" ? "new" : isDeleted(r) ? "deleted" : null;
      },
      readOnlyColumn: (col) => locked || (data.columns[col]?.readOnly ?? true),
      readOnlyCell: (row, col) => (emptyKey(row) ? EMPTY_KEY_READ_ONLY : noEditType(row, col)),
      onEdit: (row, col, value) => !locked && setValue(row, col, value),
      onContextMenu: (row, col, at) => !locked && setMenuAt({ row, col, ...at }),
      onDeleteRows: (rows) => {
        if (locked) return;
        // Resolve every row first: removing a new row shifts the ones after it. ⌫ only marks rows;
        // a row already marked stays marked.
        for (const [row, r] of rows.map((row) => [row, rowRef(row)] as const)) {
          if (r?.kind === "new") removeNewRow(tabId, r.tempId);
          else if (r && !isDeleted(r) && !emptyKey(row)) toggleDeleteRow(tabId, r.key, rowTypes(data, row));
        }
      },
    };
    const menuItems = (row: number, col: number): ContextMenuItem[] => {
      const r = rowRef(row);
      const column = data.columns[col];
      if (!r || !column) return [];
      const { value, edited } = cellValue(row, col);
      const noKey = emptyKey(row);
      const run: Record<MenuAction, () => void> = {
        setNull: () => !locked && value !== null && setValue(row, col, null),
        revert: () => !locked && r.kind === "fetched" && revertCell(tabId, r.key, column.name, rowTypes(data, row)),
        delete: () => {
          if (locked) return;
          if (r.kind === "new") removeNewRow(tabId, r.tempId);
          else if (!noKey) toggleDeleteRow(tabId, r.key, rowTypes(data, row));
        },
        restore: () => !locked && r.kind === "fetched" && toggleDeleteRow(tabId, r.key, rowTypes(data, row)),
      };
      const readOnly = column.readOnly || notEditable(row, col) || noKey;
      return menuItemsFor({ ...column, readOnly, edited, rowState: editable.rowState(row) }).map((it) => ({
        label: it.label,
        danger: it.danger,
        disabled: it.disabled || (noKey && it.id === "delete"),
        onSelect: run[it.id],
      }));
    };
    return { editable, menuItems };
  }, [enabled, locked, data, pending, rowRef, tabId, editCell, editNewRow, revertCell, removeNewRow, toggleDeleteRow]);

  const items = ops && menuAt ? ops.menuItems(menuAt.row, menuAt.col) : [];
  const menu =
    menuAt && items.length > 0 ? (
      <ContextMenu x={menuAt.x} y={menuAt.y} items={items} onClose={() => setMenuAt(null)} />
    ) : null;

  return { editable: ops?.editable, extraRows: ops ? pending.inserts.length : 0, menu };
}
