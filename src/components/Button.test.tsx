import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { Button } from "./Button";

describe("Button", () => {
  it("shows shortcut in kbd", () => {
    render(<Button variant="secondary" label="Run" shortcut="⌘↵" />);
    expect(screen.getByText("⌘↵").tagName).toBe("KBD");
  });

  it("primary has data-variant=primary", () => {
    render(<Button variant="primary" label="Save" />);
    expect(screen.getByRole("button").getAttribute("data-variant")).toBe("primary");
  });

  it("disabled does not fire onClick", () => {
    const onClick = vi.fn();
    render(<Button variant="ghost" label="Go" disabled onClick={onClick} />);
    fireEvent.click(screen.getByRole("button"));
    expect(onClick).not.toHaveBeenCalled();
  });
});
