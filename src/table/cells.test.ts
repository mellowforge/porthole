import { describe, expect, it } from "vitest";
import { cellType, rowTypes } from "./cells";

describe("cell types", () => {
  it("CODES accepts all twelve codes", () => {
    const all = "nirtblmyodjx";
    for (let i = 0; i < all.length; i++) expect(cellType([all], 0, i)).toBe(all[i]);
  });

  it("cellType reads one code per column per row", () => {
    const types = ["itnrb", "tt"];
    expect(cellType(types, 0, 0)).toBe("i");
    expect(cellType(types, 0, 4)).toBe("b");
    expect(cellType(types, 1, 1)).toBe("t");
    expect(cellType(types, 1, 2)).toBeNull();
    expect(cellType(types, 2, 0)).toBeNull();
    expect(cellType(undefined, 0, 0)).toBeNull();
    expect(cellType(["z"], 0, 0)).toBeNull();
  });

  it("rowTypes keeps the key columns and every cell of a typed row", () => {
    const page = { columns: [{ name: "id" }, { name: "name" }, { name: "pic" }], primaryKey: ["id"], cellTypes: ["itb", "in?"] };
    expect(rowTypes(page, 0)).toEqual({ key: { id: "i" }, cells: { id: "i", name: "t", pic: "b" } });
    // An unknown code is left out rather than guessed.
    expect(rowTypes(page, 1)).toEqual({ key: { id: "i" }, cells: { id: "i", name: "n" } });
    expect(rowTypes({ ...page, cellTypes: undefined }, 0)).toBeUndefined();
  });

  it("rowTypes keeps only own columns: names like __proto__ and constructor are ordinary", () => {
    const page = {
      columns: [{ name: "__proto__" }, { name: "constructor" }, { name: "id" }],
      primaryKey: ["__proto__", "constructor", "id"],
      cellTypes: ["it"],
    };
    const t = rowTypes(page, 0)!;
    expect(Object.entries(t.key)).toEqual([["__proto__", "i"], ["constructor", "t"]]);
    expect(Object.entries(t.cells)).toEqual([["__proto__", "i"], ["constructor", "t"]]);
    // A key column without a code is left out, whatever its name.
    const untyped = rowTypes({ columns: [{ name: "id" }, { name: "constructor" }], primaryKey: ["id", "constructor"], cellTypes: ["i"] }, 0)!;
    expect(Object.entries(untyped.key)).toEqual([["id", "i"]]);
  });
});
