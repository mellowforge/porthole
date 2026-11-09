import { fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../api/tauri", () => ({}));

import { useApp } from "../state/store";
import { Toolbar } from "./Toolbar";

beforeEach(() => {
  useApp.setState(useApp.getInitialState(), true);
});

describe("Toolbar", () => {
  it("New query enabled and ⌘T opens a query tab", async () => {
    render(<Toolbar />);
    const btn = screen.getByRole("button", { name: /New query/ }) as HTMLButtonElement;
    expect(btn.disabled).toBe(false);
    expect(btn.textContent).toContain("⌘T");
    await userEvent.click(btn);
    expect(useApp.getState().tabs).toHaveLength(1);
    fireEvent.keyDown(window, { key: "t", metaKey: true });
    expect(useApp.getState().tabs).toHaveLength(2);
  });
});
