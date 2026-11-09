import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { TableStructure } from "../api/types";
import { StructureView } from "./StructureView";

const structure = (indexes: TableStructure["indexes"]): TableStructure => ({
  columns: [],
  primaryKey: [],
  relation: "table",
  editing: { kind: "allowed" },
  indexes,
  foreignKeys: [],
});

describe("StructureView indexes", () => {
  it("an index with included columns shows them after INCLUDE", () => {
    const { container } = render(
      <StructureView
        structure={structure([
          { name: "t_id_incl", columns: ["id"], included: ["total", "status"], unique: false, primary: false },
        ])}
      />,
    );
    const cell = screen.getByText("t_id_incl").nextElementSibling as HTMLElement;
    expect(cell.textContent).toBe("id INCLUDE (total, status)");
    expect(container.querySelector(".index-include")?.textContent).toBe("INCLUDE (total, status)");
  });

  it("an index without included columns reads as before", () => {
    const { container } = render(
      <StructureView structure={structure([{ name: "t_pkey", columns: ["a", "b"], unique: true, primary: true }])} />,
    );
    expect(screen.getByText("t_pkey").nextElementSibling?.textContent).toBe("a, b");
    expect(container.querySelector(".index-include")).toBeNull();
  });
});
