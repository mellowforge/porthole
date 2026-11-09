import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { ExportMenu } from "./ExportMenu";

describe("ExportMenu", () => {
  it("renders the note first and not as a menu item", async () => {
    render(<ExportMenu note="Unsaved changes are not included." disabled={false} onPick={vi.fn()} variant="secondary" />);
    await userEvent.click(screen.getByRole("button", { name: "Export" }));
    const menu = screen.getByRole("menu");
    expect(menu.firstElementChild?.textContent).toBe("Unsaved changes are not included.");
    expect(screen.getAllByRole("menuitem").map((e) => e.textContent)).toEqual(["Export as CSV", "Export as JSON"]);
  });

  it("calls onPick with the format", async () => {
    const onPick = vi.fn();
    render(<ExportMenu note={null} disabled={false} onPick={onPick} variant="ghost" />);
    await userEvent.click(screen.getByRole("button", { name: "Export" }));
    await userEvent.click(screen.getByRole("menuitem", { name: "Export as JSON" }));
    expect(onPick).toHaveBeenCalledWith("json");
    expect(screen.queryByRole("menu")).toBeNull();
  });

  it("toggles shut on a second click", async () => {
    render(<ExportMenu note={null} disabled={false} onPick={vi.fn()} variant="ghost" />);
    const b = screen.getByRole("button", { name: "Export" });
    await userEvent.click(b);
    await userEvent.click(b);
    expect(screen.queryByRole("menu")).toBeNull();
  });

  it("cannot be opened when disabled", async () => {
    render(<ExportMenu note={null} disabled onPick={vi.fn()} variant="secondary" />);
    await userEvent.click(screen.getByRole("button", { name: "Export" }));
    expect(screen.queryByRole("menu")).toBeNull();
  });
});
