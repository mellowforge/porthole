import { describe, expect, it } from "vitest";
import { cellView, menuItemsFor, rowThemeFor, shouldRecordEdit } from "./editModel";

const colors = { muted: "#a1a1a6", pendingBg: "#FEF3C7", deletedBg: "#FDECEC", danger: "#d92d20" };

describe("editModel", () => {
  it("edited cell shows pending value and amber override", () => {
    const v = cellView({ value: "pro", edited: true }, null, false, colors);
    expect(v.data).toBe("pro");
    expect(v.displayData).toBe("pro");
    expect(v.readonly).toBe(false);
    expect(v.themeOverride?.bgCell).toBe("#FEF3C7");
    expect(v.strike).toBe(false);

    // An unedited cell has no override; an edited NULL stays italic muted on amber.
    expect(cellView({ value: "free", edited: false }, null, false, colors).themeOverride).toBeUndefined();
    const nul = cellView({ value: null, edited: true }, null, false, colors);
    expect(nul.displayData).toBe("NULL");
    expect(nul.data).toBe("");
    expect(nul.themeOverride).toEqual({ textDark: "#a1a1a6", baseFontStyle: "italic 13px", bgCell: "#FEF3C7" });
  });

  it("deleted row override and strikethrough flag", () => {
    expect(rowThemeFor("deleted", colors)).toEqual({ bgCell: "#FDECEC", textDark: "#d92d20", textLight: "#d92d20" });
    expect(rowThemeFor(null, colors)).toBeUndefined();
    expect(rowThemeFor("new", colors)).toBeUndefined();

    const v = cellView({ value: "Johanna Berg", edited: false }, "deleted", false, colors);
    expect(v.strike).toBe(true);
    expect(v.readonly).toBe(true);
    // The row's red text must win, so the cell sets no colour of its own.
    expect(v.themeOverride).toBeUndefined();
    const nul = cellView({ value: null, edited: false }, "deleted", false, colors);
    expect(nul.themeOverride).toEqual({ baseFontStyle: "italic 13px" });
    // A deleted row's kept edit shows its value, struck through in the row's red, not amber.
    const kept = cellView({ value: "pro", edited: true }, "deleted", false, colors);
    expect(kept.displayData).toBe("pro");
    expect(kept.strike).toBe(true);
    expect(kept.themeOverride).toBeUndefined();
  });

  it("new row cells are amber; unset ones are blank, read-only columns don't edit", () => {
    const unset = cellView({ value: null, edited: false }, "new", false, colors);
    expect(unset.displayData).toBe("");
    expect(unset.data).toBe("");
    expect(unset.themeOverride).toEqual({ bgCell: "#FEF3C7" });
    expect(unset.readonly).toBe(false);
    const set = cellView({ value: "Ann", edited: true }, "new", false, colors);
    expect(set.displayData).toBe("Ann");
    expect(cellView({ value: null, edited: false }, "new", true, colors).readonly).toBe(true);
    expect(cellView({ value: "1", edited: false }, null, true, colors).readonly).toBe(true);
  });

  it("menu items per state", () => {
    const labels = (items: ReturnType<typeof menuItemsFor>) => items.map((i) => (i.disabled ? `${i.label} (off)` : i.label));
    const col = { nullable: true, readOnly: false };
    expect(labels(menuItemsFor({ ...col, edited: false, rowState: null }))).toEqual(["Set NULL", "Delete row"]);
    expect(labels(menuItemsFor({ ...col, edited: true, rowState: null }))).toEqual(["Set NULL", "Revert cell", "Delete row"]);
    expect(labels(menuItemsFor({ nullable: false, readOnly: false, edited: false, rowState: null }))).toEqual([
      "Set NULL (off)",
      "Delete row",
    ]);
    expect(labels(menuItemsFor({ nullable: true, readOnly: true, edited: false, rowState: null }))).toEqual([
      "Set NULL (off)",
      "Delete row",
    ]);
    expect(labels(menuItemsFor({ ...col, edited: false, rowState: "deleted" }))).toEqual(["Set NULL (off)", "Restore row"]);
    expect(labels(menuItemsFor({ ...col, edited: true, rowState: "new" }))).toEqual(["Set NULL", "Delete row"]);
    expect(menuItemsFor({ ...col, edited: false, rowState: null }).find((i) => i.id === "delete")?.danger).toBe(true);
  });

  it("confirming an empty editor on a NULL cell records no edit", () => {
    expect(shouldRecordEdit(null, "")).toBe(false);
    expect(shouldRecordEdit(null, "x")).toBe(true);
    expect(shouldRecordEdit("abc", "")).toBe(true);
    expect(shouldRecordEdit("", "")).toBe(true);
  });
});
