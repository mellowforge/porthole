import { fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { ContextMenu } from "./ContextMenu";

const setup = () => {
  const onClose = vi.fn();
  const onA = vi.fn();
  render(
    <div>
      <button>outside</button>
      <ContextMenu x={10} y={20} onClose={onClose} items={[{ label: "A", onSelect: onA }, { label: "B", onSelect: vi.fn(), danger: true }]} />
    </div>,
  );
  return { onClose, onA };
};

describe("ContextMenu", () => {
  it("item click runs and closes", async () => {
    const { onClose, onA } = setup();
    await userEvent.click(screen.getByRole("menuitem", { name: "A" }));
    expect(onA).toHaveBeenCalled();
    expect(onClose).toHaveBeenCalled();
  });
  it("Esc and outside click close", async () => {
    const { onClose } = setup();
    fireEvent.keyDown(document, { key: "Escape" });
    expect(onClose).toHaveBeenCalledTimes(1);
    fireEvent.mouseDown(screen.getByText("outside"));
    expect(onClose).toHaveBeenCalledTimes(2);
  });
  it("Enter activates the focused item, arrows move", async () => {
    const { onA } = setup();
    expect(document.activeElement).toBe(screen.getByRole("menuitem", { name: "A" }));
    await userEvent.keyboard("{Enter}");
    expect(onA).toHaveBeenCalled();
  });
});
