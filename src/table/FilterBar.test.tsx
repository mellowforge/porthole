import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { ColumnInfo } from "../api/types";
import { columnToken, FilterBar } from "./FilterBar";

const col = (name: string): ColumnInfo => ({
  name,
  dataType: "text",
  fullType: "text",
  castType: "text",
  readOnly: false,
  nullable: true,
  default: null,
  isPrimaryKey: false,
});
const columns = [col("id"), col("status"), col("created_at")];

function setup(over: Partial<Parameters<typeof FilterBar>[0]> = {}) {
  const onApply = vi.fn();
  const onClear = vi.fn();
  render(
    <FilterBar columns={columns} value="" matches={null} error={null} onApply={onApply} onClear={onClear} {...over} />,
  );
  return { onApply, onClear };
}

describe("FilterBar", () => {
  it("enter applies", async () => {
    const { onApply } = setup();
    await userEvent.type(screen.getByRole("textbox"), "id = 1{Enter}");
    expect(onApply).toHaveBeenCalledWith("id = 1");
  });

  it("clear calls onClear", async () => {
    const { onClear } = setup();
    await userEvent.click(screen.getByRole("button", { name: "Clear" }));
    expect(onClear).toHaveBeenCalled();
  });

  it("shows match count", () => {
    setup({ matches: { kind: "exact", value: 17 } });
    expect(screen.getByText("17 matches")).not.toBeNull();
  });

  it("shows inline error with message", () => {
    setup({ error: { kind: "filter", message: 'Unknown column "nope"', position: 0 } });
    expect(screen.getByText('Unknown column "nope"')).not.toBeNull();
    expect(screen.getByTestId("filter-caret")).not.toBeNull();
  });

  it("keeps the hint visible while an error is shown", () => {
    setup({ error: { kind: "filter", message: "bad", position: 2 } });
    expect(screen.getByText(/^Type a column, an operator/)).not.toBeNull();
  });

  it("suggests columns", async () => {
    const { onApply } = setup();
    const input = screen.getByRole("textbox") as HTMLInputElement;
    await userEvent.type(input, "sta");
    expect(screen.getByText("status")).not.toBeNull();
    await userEvent.keyboard("{Tab}");
    expect(input.value).toBe("status");
    expect(onApply).not.toHaveBeenCalled();
  });

  it("suggests after and, and Enter inserts a suggestion instead of applying", async () => {
    const { onApply } = setup();
    const input = screen.getByRole("textbox") as HTMLInputElement;
    await userEvent.type(input, "id = 1 AND cre");
    await userEvent.keyboard("{Enter}");
    expect(input.value).toBe("id = 1 AND created_at");
    expect(onApply).not.toHaveBeenCalled();
  });

  it("inserts awkward column names backtick-quoted", async () => {
    setup({ columns: [col("Order Id"), col("notes"), col("not"), col("total$usd")] });
    const input = screen.getByRole("textbox") as HTMLInputElement;
    await userEvent.type(input, "Ord");
    await userEvent.keyboard("{Tab}");
    expect(input.value).toBe("`Order Id`");
    await userEvent.type(input, " = 1 and no");
    expect(screen.getAllByRole("option").map((o) => o.textContent)).toEqual(["notes", "not"]);
    await userEvent.keyboard("{ArrowDown}{Enter}");
    expect(input.value).toBe("`Order Id` = 1 and `not`");
    await userEvent.type(input, " = 1 or total$");
    await userEvent.keyboard("{Tab}");
    expect(input.value).toBe("`Order Id` = 1 and `not` = 1 or total$usd");
  });

  it("columnToken quotes only when needed", () => {
    expect(columnToken("status")).toBe("status");
    expect(columnToken("_x1$")).toBe("_x1$");
    expect(columnToken("Order Id")).toBe("`Order Id`");
    expect(columnToken("1st")).toBe("`1st`");
    expect(columnToken("LIKE")).toBe("`LIKE`");
    expect(columnToken("null")).toBe("`null`");
  });

  it("does not suggest in a value position", async () => {
    setup();
    await userEvent.type(screen.getByRole("textbox"), "id = sta");
    expect(screen.queryByRole("listbox")).toBeNull();
  });

  it("hint copy is exact", () => {
    setup();
    expect(
      screen.getByText(
        "Type a column, an operator (=, !=, >, <, like, in, is null) and a value. Combine with and / or.",
      ),
    ).not.toBeNull();
  });
});
