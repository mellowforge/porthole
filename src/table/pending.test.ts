import { describe, expect, it } from "vitest";
import {
  addInsert, counts, emptyPending, keyId, removeInsert, revertCell, rowKey, setCell, setInsertCell, summaryText,
  toChangeSet, toggleDelete, subtractSent,
} from "./pending";
import { rowTypes } from "./cells";

const T = { schema: "public", name: "users" };

describe("pending changes", () => {
  it("setCell back to original removes the edit", () => {
    let p = setCell(emptyPending, ["1"], "name", "b", "a");
    expect(p.edits[keyId(["1"])].values).toEqual({ name: "b" });
    p = setCell(p, ["1"], "age", "3", "2");
    p = setCell(p, ["1"], "name", "a", "a");
    expect(p.edits[keyId(["1"])].values).toEqual({ age: "3" });
    p = setCell(p, ["1"], "age", "2", "2");
    expect(p).toEqual(emptyPending);
  });

  it("confirming an empty editor on a NULL cell records no edit", () => {
    expect(setCell(emptyPending, ["1"], "name", "", null)).toEqual(emptyPending);
    expect(setCell(emptyPending, ["1"], "name", null, null)).toEqual(emptyPending);
    expect(setCell(emptyPending, ["1"], "name", "", "a").edits[keyId(["1"])].values).toEqual({ name: "" });
    expect(setCell(emptyPending, ["1"], "name", null, "a").edits[keyId(["1"])].values).toEqual({ name: null });
  });

  it("editing a deleted row's cell is ignored", () => {
    const p = toggleDelete(emptyPending, ["1"]);
    expect(setCell(p, ["1"], "name", "x", "a")).toBe(p);
  });

  it("delete then restore keeps edits", () => {
    let p = setCell(emptyPending, ["1"], "name", "x", "a");
    p = toggleDelete(p, ["1"]);
    expect(p.edits[keyId(["1"])].values).toEqual({ name: "x" });
    expect(p.deletes[keyId(["1"])]).toEqual(["1"]);
    p = toggleDelete(p, ["1"]);
    expect(p.deletes).toEqual({});
    expect(p.edits[keyId(["1"])].values).toEqual({ name: "x" });
    expect(counts(p)).toEqual({ edits: 1, deletions: 0, inserts: 0, total: 1 });
  });

  it("deleted row's edits not in change set or counts", () => {
    let p = setCell(emptyPending, ["1"], "name", "x", "a");
    p = setCell(p, ["1"], "age", "3", "2");
    p = setCell(p, ["2"], "name", "y", "b");
    p = toggleDelete(p, ["1"]);
    expect(counts(p)).toEqual({ edits: 1, deletions: 1, inserts: 0, total: 2 });
    expect(toChangeSet(p, T).changes).toEqual([
      { kind: "delete", key: ["1"] },
      { kind: "update", key: ["2"], values: { name: "y" } },
    ]);
    // Once the delete is saved, the row's suppressed edits go with it.
    expect(subtractSent(p, p)).toEqual(emptyPending);
  });

  it("subtractSent keeps what changed or was added after the snapshot", () => {
    let sent = setCell(emptyPending, ["1"], "a", "x", "o");
    sent = setCell(sent, ["1"], "b", "y", "o");
    sent = toggleDelete(sent, ["2"]);
    sent = addInsert(sent).pending;
    let now = setCell(sent, ["1"], "b", "z", "o"); // changed after send
    now = setCell(now, ["3"], "a", "n", "o"); // new edit
    now = toggleDelete(now, ["4"]);
    now = addInsert(now).pending;
    const left = subtractSent(now, sent);
    expect(left.edits[keyId(["1"])].values).toEqual({ b: "z" });
    expect(left.edits[keyId(["3"])].values).toEqual({ a: "n" });
    expect(Object.keys(left.deletes)).toEqual([keyId(["4"])]);
    expect(left.inserts).toHaveLength(1);
    expect(subtractSent(sent, sent)).toEqual(emptyPending);
  });

  it("revertCell removes one cell", () => {
    let p = setCell(emptyPending, ["1"], "a", "x", "o");
    p = setCell(p, ["1"], "b", "y", "o");
    expect(revertCell(p, ["1"], "a").edits[keyId(["1"])].values).toEqual({ b: "y" });
    expect(revertCell(p, ["2"], "a")).toBe(p);
  });

  it("counts and summaryText", () => {
    let p = setCell(emptyPending, ["1"], "a", "x", "o");
    p = setCell(p, ["1"], "b", "y", "o");
    p = toggleDelete(p, ["2"]);
    p = addInsert(p).pending;
    const c = counts(p);
    expect(c).toEqual({ edits: 2, deletions: 1, inserts: 1, total: 4 });
    expect(summaryText(c)).toEqual({
      title: "4 pending changes",
      detail: "2 edits · 1 deletion · 1 new row · nothing is written until you save",
    });
    const q = toggleDelete(setCell(emptyPending, ["1"], "a", "x", "o"), ["2"]);
    expect(summaryText(counts(q)).detail).toBe("1 edit · 1 deletion · nothing is written until you save");
    expect(summaryText(counts(q)).title).toBe("2 pending changes");
    expect(summaryText(counts(toggleDelete(emptyPending, ["2"])))).toEqual({
      title: "1 pending change",
      detail: "1 deletion · nothing is written until you save",
    });
    expect(summaryText(counts(addInsert(emptyPending).pending)).detail).toBe("1 new row · nothing is written until you save");
    expect(summaryText(counts(emptyPending))).toEqual({
      title: "0 pending changes",
      detail: "nothing is written until you save",
    });
  });

  it("toChangeSet order and shapes", () => {
    let p = toggleDelete(emptyPending, ["9"]);
    const a = addInsert(p);
    p = setInsertCell(a.pending, a.tempId, "name", "n");
    p = setCell(p, ["1"], "name", null, "a");
    expect(toChangeSet(p, T)).toEqual({
      table: T,
      changes: [
        { kind: "delete", key: ["9"] },
        { kind: "update", key: ["1"], values: { name: null } },
        { kind: "insert", values: { name: "n" } },
      ],
    });
    expect(removeInsert(p, a.tempId).inserts).toEqual([]);
  });

  it("a typed page's changes carry the original types of the key and edited columns", () => {
    const page = { columns: [{ name: "id" }, { name: "name" }], primaryKey: ["id"], cellTypes: ["it", "in"] };
    let p = setCell(emptyPending, ["1"], "name", "b", "a", rowTypes(page, 0));
    p = toggleDelete(p, ["2"], rowTypes(page, 1));
    expect(toChangeSet(p, T).changes).toEqual([
      { kind: "delete", key: ["2"], originalTypes: { id: "i" } },
      { kind: "update", key: ["1"], values: { name: "b" }, originalTypes: { id: "i", name: "t" } },
    ]);
    // Reverting the edit and restoring the row leaves nothing behind.
    p = toggleDelete(revertCell(p, ["1"], "name", rowTypes(page, 0)), ["2"], rowTypes(page, 1));
    expect(p).toEqual(emptyPending);
    expect("types" in p).toBe(false);
  });

  it("a row's id is its key text, plus its key types when the page has them", () => {
    expect(keyId(["5"])).toBe(JSON.stringify(["5"]));
    expect(keyId(["5"], undefined)).toBe('["5"]');
    const page = { columns: [{ name: "k" }, { name: "v" }], primaryKey: ["k"], cellTypes: ["it", "tt"] };
    const [int, text] = [rowTypes(page, 0), rowTypes(page, 1)];
    expect(keyId(["5"], int)).not.toBe(keyId(["5"], text));
    let p = setCell(emptyPending, ["5"], "v", "a", "int five", int);
    p = setCell(p, ["5"], "v", "b", "text five", text);
    expect(counts(p).edits).toBe(2);
    expect(toChangeSet(p, T).changes).toEqual([
      { kind: "update", key: ["5"], values: { v: "a" }, originalTypes: { k: "i", v: "t" } },
      { kind: "update", key: ["5"], values: { v: "b" }, originalTypes: { k: "t", v: "t" } },
    ]);
    // Deleting the integer row leaves the text row's edit live.
    p = toggleDelete(p, ["5"], int);
    expect(toChangeSet(p, T).changes).toEqual([
      { kind: "delete", key: ["5"], originalTypes: { k: "i" } },
      { kind: "update", key: ["5"], values: { v: "b" }, originalTypes: { k: "t", v: "t" } },
    ]);
  });

  it("an untyped page's changes have no originalTypes key", () => {
    let p = setCell(emptyPending, ["1"], "name", "b", "a", undefined);
    p = toggleDelete(p, ["2"], undefined);
    expect("types" in p).toBe(false);
    for (const c of toChangeSet(p, T).changes) expect("originalTypes" in c).toBe(false);
  });

  it("subtractSent keeps the types of rows still pending", () => {
    const page = { columns: [{ name: "id" }, { name: "name" }], primaryKey: ["id"], cellTypes: ["it", "it"] };
    const sent = setCell(emptyPending, ["1"], "name", "b", "a", rowTypes(page, 0));
    const now = setCell(sent, ["2"], "name", "c", "a", rowTypes(page, 1));
    expect(toChangeSet(subtractSent(now, sent), T).changes).toEqual([
      { kind: "update", key: ["2"], values: { name: "c" }, originalTypes: { id: "i", name: "t" } },
    ]);
    expect(subtractSent(sent, sent)).toEqual(emptyPending);
    expect("types" in subtractSent(sent, sent)).toBe(false);
  });

  it("rowKey picks PK columns in primaryKey order", () => {
    const cols = [{ name: "a" }, { name: "b" }, { name: "c" }];
    expect(rowKey(["1", "2", "3"], cols, ["c", "a"])).toEqual(["3", "1"]);
  });

  it("a typed page with columns named __proto__ and constructor builds a change set Rust accepts", () => {
    const page = {
      columns: [{ name: "id" }, { name: "__proto__" }, { name: "constructor" }],
      primaryKey: ["id"],
      cellTypes: ["itr"],
    };
    let p = setCell(emptyPending, ["1"], "__proto__", "x", "a", rowTypes(page, 0));
    p = setCell(p, ["1"], "constructor", "2", "1", rowTypes(page, 0));
    // What goes over IPC: every originalTypes value must be a cell type code (Rust's CellType).
    const [change] = JSON.parse(JSON.stringify(toChangeSet(p, T))).changes;
    expect(Object.entries(change.values)).toEqual([["__proto__", "x"], ["constructor", "2"]]);
    expect(Object.entries(change.originalTypes)).toEqual([["id", "i"], ["__proto__", "t"], ["constructor", "r"]]);
    // Reverting a column that was never edited leaves the pending changes as they are.
    const edited = setCell(emptyPending, ["1"], "id", "2", "1");
    expect(revertCell(edited, ["1"], "constructor")).toBe(edited);
  });
});
