import type { CellTypeCode, TablePage } from "../api/types";
import type { RowTypes } from "./pending";

const CODES: readonly string[] = ["n", "i", "r", "t", "b", "l", "m", "y", "o", "d", "j", "x"];

export const isCellTypeCode = (x: unknown): x is CellTypeCode => typeof x === "string" && CODES.includes(x);

/** The type code of one cell, from a page's or result's `cellTypes`; `null` when there is none. */
export function cellType(types: string[] | undefined, row: number, col: number): CellTypeCode | null {
  const code = types?.[row]?.[col];
  return code !== undefined && CODES.includes(code) ? (code as CellTypeCode) : null;
}

/** A fetched row's original cell types, kept with its pending changes; `undefined` when the page has none. */
export function rowTypes(
  page: Pick<TablePage, "primaryKey" | "cellTypes"> & { columns: { name: string }[] },
  row: number,
): RowTypes | undefined {
  if (!page.cellTypes) return undefined;
  // Built with fromEntries so that a column named `__proto__` becomes an own entry like any other.
  const cells: Record<string, CellTypeCode> = Object.fromEntries(
    page.columns.flatMap((c, col) => {
      const code = cellType(page.cellTypes, row, col);
      return code ? [[c.name, code] as const] : [];
    }),
  );
  const key = Object.fromEntries(page.primaryKey.filter((k) => Object.hasOwn(cells, k)).map((k) => [k, cells[k]]));
  return { key, cells };
}
