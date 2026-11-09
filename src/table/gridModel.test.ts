import { describe, expect, it } from "vitest";
import { getDefaultTheme } from "@glideapps/glide-data-grid";
import { formatCount, nextSort, nullCellTheme, toGridCell } from "./gridModel";

describe("formatCount", () => {
  it("formats exact with separators", () => {
    expect(formatCount({ kind: "exact", value: 1284 })).toBe("1,284");
  });
  it("formats estimated millions", () => {
    expect(formatCount({ kind: "estimated", value: 1200000 })).toBe("~1.2M");
    expect(formatCount({ kind: "estimated", value: 1000000 })).toBe("~1M");
  });
  it("formats estimated thousands", () => {
    expect(formatCount({ kind: "estimated", value: 48000 })).toBe("~48K");
    expect(formatCount({ kind: "estimated", value: 999_600 })).toBe("~1M");
  });
  it("formats small estimates", () => {
    expect(formatCount({ kind: "estimated", value: 420 })).toBe("~420");
  });
  it("formats unknown", () => {
    expect(formatCount({ kind: "unknown" })).toBe("many");
  });
});

describe("toGridCell", () => {
  it("renders null as NULL", () => {
    const c = toGridCell(null);
    expect(c.displayData).toBe("NULL");
    expect(c.isNull).toBe(true);
  });
  it("keeps empty string distinct from null", () => {
    const c = toGridCell("");
    expect(c.displayData).toBe("");
    expect(c.isNull).toBe(false);
  });
  it("truncates long values for display only", () => {
    const c = toGridCell("x".repeat(600));
    expect(c.displayData.length).toBe(500);
    expect(c.data.length).toBe(600);
  });
});

describe("nextSort", () => {
  it("cycles asc, desc, none", () => {
    const a = nextSort(null, "id");
    expect(a).toEqual({ column: "id", descending: false });
    const d = nextSort(a, "id");
    expect(d).toEqual({ column: "id", descending: true });
    expect(nextSort(d, "id")).toBeNull();
  });
  it("starts ascending on a different column", () => {
    expect(nextSort({ column: "id", descending: true }, "name")).toEqual({
      column: "name",
      descending: false,
    });
  });
});

describe("nullCellTheme", () => {
  it("is muted and italic at the grid's base font size", () => {
    const t = nullCellTheme("#999");
    expect(t.textDark).toBe("#999");
    expect(t.baseFontStyle).toBe(`italic ${getDefaultTheme().baseFontStyle}`);
  });
});
