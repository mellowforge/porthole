import type { RowCount, SortSpec } from "../api/types";

export const PAGE_SIZE = 100;
const MAX_DISPLAY = 500;

export function formatCount(c: RowCount): string {
  if (c.kind === "unknown") return "many";
  if (c.kind === "exact") return c.value.toLocaleString("en-US");
  const n = c.value;
  if (n >= 1_000_000) return `~${(Math.round(n / 100_000) / 10).toString()}M`;
  if (n >= 1_000) {
    const k = Math.round(n / 1_000);
    return k >= 1000 ? "~1M" : `~${k}K`;
  }
  return `~${n}`;
}

export interface GridCellModel {
  kind: "text";
  data: string;
  displayData: string;
  isNull: boolean;
}

export function toGridCell(v: string | null): GridCellModel {
  if (v === null) return { kind: "text", data: "", displayData: "NULL", isNull: true };
  return {
    kind: "text",
    data: v,
    displayData: v.length > MAX_DISPLAY ? v.slice(0, MAX_DISPLAY) : v,
    isNull: false,
  };
}

/**
 * Cell theme for NULL: muted and italic. Glide builds the canvas
 * font as `${baseFontStyle} ${fontFamily}`, so the style must repeat Glide's
 * default 13px size; a bare "italic" would be an invalid font.
 */
export function nullCellTheme(muted: string): { textDark: string; baseFontStyle: string } {
  return { textDark: muted, baseFontStyle: "italic 13px" };
}

export function nextSort(current: SortSpec | null, column: string): SortSpec | null {
  if (!current || current.column !== column) return { column, descending: false };
  if (!current.descending) return { column, descending: true };
  return null;
}
