import type { ChangeSet, OriginalTypes, RowChange, TableRef } from "../api/types";
import { isCellTypeCode } from "./cells";

/** Original primary-key values, in primaryKey order. */
export type RowKey = string[];

/** A fetched row's original cell types (see `rowTypes` in `cells.ts`): its key columns, and every cell. */
export interface RowTypes {
  key: OriginalTypes;
  cells: OriginalTypes;
}

export interface PendingChanges {
  /** By `keyId(key, types)`. */
  edits: Record<string, { key: RowKey; values: Record<string, string | null> }>;
  /** By `keyId(key, types)`. */
  deletes: Record<string, RowKey>;
  inserts: { tempId: string; values: Record<string, string | null> }[];
  /** By `keyId(key, types)`: the original cell types of rows with an edit or a deletion, recorded when the
   *  page had `cellTypes`. Absent otherwise (always for Postgres). */
  types?: Record<string, RowTypes>;
}

export const emptyPending: PendingChanges = { edits: {}, deletes: {}, inserts: [] };

/**
 * A fetched row's id among the pending changes. Without cell types (Postgres) it is the key's text.
 * With them it is the key's text plus its key cells' types, so the integer `5` and the text `'5'`
 * (SQLite, a key column with no declared type) are two rows.
 */
export const keyId = (key: RowKey, types?: RowTypes) => (types ? JSON.stringify([key, types.key]) : JSON.stringify(key));

const isRecord = (x: unknown): x is Record<string, unknown> => typeof x === "object" && x !== null && !Array.isArray(x);
const isKey = (x: unknown): x is RowKey => Array.isArray(x) && x.every((v) => typeof v === "string");
const isValues = (x: unknown): x is Record<string, string | null> =>
  isRecord(x) && Object.values(x).every((v) => typeof v === "string" || v === null);
const isTypeCodes = (x: unknown) => isRecord(x) && Object.values(x).every(isCellTypeCode);
const isRowTypes = (x: unknown): x is RowTypes => isRecord(x) && isTypeCodes(x.key) && isTypeCodes(x.cells);

/**
 * Whether `x` (read back from storage) is well-formed pending changes: the right field types, keys
 * that are string arrays, values that are strings or null, each edit and deletion filed under its
 * own `keyId`, no edit without values, and no repeated new-row `tempId`. A row may have both an edit
 * and a deletion: a deleted row keeps its edits.
 */
export function validPending(x: unknown): x is PendingChanges {
  if (!isRecord(x) || !isRecord(x.edits) || !isRecord(x.deletes) || !Array.isArray(x.inserts)) return false;
  if (x.types !== undefined && !(isRecord(x.types) && Object.values(x.types).every(isRowTypes))) return false;
  const types = x.types as Record<string, RowTypes> | undefined;
  const filed = (id: string, key: RowKey) => id === keyId(key, types && Object.hasOwn(types, id) ? types[id] : undefined);
  return (
    Object.entries(x.edits).every(
      ([id, e]) => isRecord(e) && isKey(e.key) && isValues(e.values) && Object.keys(e.values).length > 0 && filed(id, e.key),
    ) &&
    Object.entries(x.deletes).every(([id, key]) => isKey(key) && filed(id, key)) &&
    x.inserts.every((r) => isRecord(r) && typeof r.tempId === "string" && isValues(r.values)) &&
    new Set(x.inserts.map((r) => (r as { tempId: string }).tempId)).size === x.inserts.length
  );
}

/** Records `types` for row `id` (when given) and drops the types of rows with nothing pending; with none
 * left the `types` field goes too. */
function withTypes(p: PendingChanges, id?: string, types?: RowTypes): PendingChanges {
  const all = id !== undefined && types ? { ...p.types, [id]: types } : p.types;
  const { types: _old, ...rest } = p;
  if (!all) return Object.hasOwn(p, "types") ? rest : p;
  const kept = Object.fromEntries(Object.entries(all).filter(([k]) => Object.hasOwn(p.edits, k) || Object.hasOwn(p.deletes, k)));
  return Object.keys(kept).length ? { ...rest, types: kept } : rest;
}

export function rowKey(row: (string | null)[], columns: { name: string }[], primaryKey: string[]): RowKey {
  return primaryKey.map((pk) => {
    const i = columns.findIndex((c) => c.name === pk);
    return i < 0 ? "" : (row[i] ?? "");
  });
}

/**
 * Records `value` for a cell, or removes the edit when it equals `original`. When `original` is NULL
 * and `value` is "" (the editor opened empty on a NULL cell and was confirmed unchanged) nothing is
 * recorded: an empty string can only be set by clearing a non-NULL cell. Set NULL is `value === null`.
 * Edits to a row marked for deletion are ignored. `types`: the row's original cell types, when the page
 * has them.
 */
export function setCell(
  p: PendingChanges,
  key: RowKey,
  column: string,
  value: string | null,
  original: string | null,
  types?: RowTypes,
): PendingChanges {
  const id = keyId(key, types);
  if (Object.hasOwn(p.deletes, id)) return p;
  if (value === original || (original === null && value === "")) return revertCell(p, key, column, types);
  const current = p.edits[id];
  return withTypes(
    {
      ...p,
      edits: { ...p.edits, [id]: { key, values: { ...current?.values, [column]: value } } },
    },
    id,
    types,
  );
}

/** `types`: the row's original cell types, when the page has them (they are part of the row's id). */
export function revertCell(p: PendingChanges, key: RowKey, column: string, types?: RowTypes): PendingChanges {
  const id = keyId(key, types);
  const current = p.edits[id];
  if (!current || !Object.hasOwn(current.values, column)) return p;
  const { [column]: _removed, ...rest } = current.values;
  const { [id]: _entry, ...edits } = p.edits;
  return withTypes({
    ...p,
    edits: Object.keys(rest).length ? { ...edits, [id]: { key: current.key, values: rest } } : edits,
  });
}

/**
 * Marks the row for deletion or, if already marked, restores it. The row's edits are kept while it
 * is marked (shown struck through, not saved or counted) and come back with Restore row. `types`: the
 * row's original cell types, when the page has them.
 */
export function toggleDelete(p: PendingChanges, key: RowKey, types?: RowTypes): PendingChanges {
  const id = keyId(key, types);
  if (Object.hasOwn(p.deletes, id)) {
    const { [id]: _removed, ...deletes } = p.deletes;
    return withTypes({ ...p, deletes });
  }
  return withTypes({ ...p, deletes: { ...p.deletes, [id]: key } }, id, types);
}

/** Edits that would be saved, with their row ids: those of rows marked for deletion are left out. */
const liveEdits = (p: PendingChanges) => Object.entries(p.edits).filter(([id]) => !Object.hasOwn(p.deletes, id));

export function addInsert(p: PendingChanges): { pending: PendingChanges; tempId: string } {
  const tempId = crypto.randomUUID();
  return { pending: { ...p, inserts: [...p.inserts, { tempId, values: {} }] }, tempId };
}

export function setInsertCell(p: PendingChanges, tempId: string, column: string, value: string | null): PendingChanges {
  return {
    ...p,
    inserts: p.inserts.map((r) => (r.tempId === tempId ? { ...r, values: { ...r.values, [column]: value } } : r)),
  };
}

export function removeInsert(p: PendingChanges, tempId: string): PendingChanges {
  return { ...p, inserts: p.inserts.filter((r) => r.tempId !== tempId) };
}

export function counts(p: PendingChanges): { edits: number; deletions: number; inserts: number; total: number } {
  const edits = liveEdits(p).reduce((n, [, e]) => n + Object.keys(e.values).length, 0);
  const deletions = Object.keys(p.deletes).length;
  const inserts = p.inserts.length;
  return { edits, deletions, inserts, total: edits + deletions + inserts };
}

/**
 * Order: deletes, updates, inserts, so a new row may reuse the key of a row deleted in the same save.
 * A row with recorded types carries `originalTypes`: its key columns, plus the edited columns of an
 * update. Rows without them get no `originalTypes` key at all.
 */
export function toChangeSet(p: PendingChanges, table: TableRef): ChangeSet {
  const typesOf = (id: string) => (p.types && Object.hasOwn(p.types, id) ? p.types[id] : undefined);
  const deletes = Object.entries(p.deletes).map(([id, key]): RowChange => {
    const t = typesOf(id);
    return { kind: "delete", key, ...(t ? { originalTypes: t.key } : {}) };
  });
  const updates = liveEdits(p).map(([id, e]): RowChange => {
    const t = typesOf(id);
    const edited = t ? Object.keys(e.values).filter((c) => Object.hasOwn(t.cells, c)).map((c) => [c, t.cells[c]] as const) : [];
    return {
      kind: "update",
      key: e.key,
      values: e.values,
      ...(t ? { originalTypes: { ...t.key, ...Object.fromEntries(edited) } } : {}),
    };
  });
  return {
    table,
    changes: [...deletes, ...updates, ...p.inserts.map((r): RowChange => ({ kind: "insert", values: r.values }))],
  };
}

/**
 * The first `n` changes of `p` in `toChangeSet` order (deletes, updates, inserts), as pending changes
 * for `subtractSent`: what a save that stopped part-way kept. An update is one change however many of
 * its cells were edited. A delete takes its row's hidden edits along, so they don't come back as an
 * update of a row that is gone.
 */
export function firstChanges(p: PendingChanges, n: number): PendingChanges {
  const edits: PendingChanges["edits"] = {};
  const deletes: PendingChanges["deletes"] = {};
  let left = Math.max(0, n);
  for (const [id, key] of Object.entries(p.deletes).slice(0, left)) {
    deletes[id] = key;
    if (Object.hasOwn(p.edits, id)) edits[id] = p.edits[id];
    left--;
  }
  for (const [id, e] of liveEdits(p).slice(0, left)) {
    edits[id] = e;
    left--;
  }
  return withTypes({ edits, deletes, inserts: p.inserts.slice(0, left), types: p.types });
}

/** What is still pending after `sent` was saved: edits made since (a cell whose value changed, new cells, rows, deletions) stay. */
export function subtractSent(current: PendingChanges, sent: PendingChanges): PendingChanges {
  const edits: PendingChanges["edits"] = {};
  for (const [id, e] of Object.entries(current.edits)) {
    const was = sent.edits[id]?.values ?? {};
    const values = Object.fromEntries(Object.entries(e.values).filter(([c, v]) => !Object.hasOwn(was, c) || was[c] !== v));
    if (Object.keys(values).length) edits[id] = { key: e.key, values };
  }
  const deletes = Object.fromEntries(Object.entries(current.deletes).filter(([id]) => !Object.hasOwn(sent.deletes, id)));
  const sentIds = new Set(sent.inserts.map((r) => r.tempId));
  return withTypes({ edits, deletes, inserts: current.inserts.filter((r) => !sentIds.has(r.tempId)), types: current.types });
}

const plural = (n: number, one: string, many: string) => `${n} ${n === 1 ? one : many}`;

export function summaryText(c: { edits: number; deletions: number; inserts: number; total: number }): {
  title: string;
  detail: string;
} {
  const parts = [
    c.edits > 0 ? plural(c.edits, "edit", "edits") : null,
    c.deletions > 0 ? plural(c.deletions, "deletion", "deletions") : null,
    c.inserts > 0 ? plural(c.inserts, "new row", "new rows") : null,
    "nothing is written until you save",
  ].filter((x): x is string => x !== null);
  return { title: plural(c.total, "pending change", "pending changes"), detail: parts.join(" · ") };
}
