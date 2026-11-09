import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../api/tauri", () => ({}));

import queryIcon from "../assets/icons/query.svg";
import tableIcon from "../assets/icons/table.svg";
import { useApp } from "../state/store";
import { Tabs } from "./Tabs";

beforeEach(() => {
  useApp.setState(useApp.getInitialState(), true);
});

describe("Tabs", () => {
  it("plus button opens a query tab", async () => {
    render(<Tabs />);
    await userEvent.click(screen.getByRole("button", { name: "New query" }));
    const { tabs, activeTabId } = useApp.getState();
    expect(tabs).toHaveLength(1);
    expect(tabs[0].kind).toBe("query");
    expect(activeTabId).toBe(tabs[0].id);
    expect(screen.getByRole("tab", { name: /Query 1/ })).not.toBeNull();
  });

  it("query tab shows query icon", () => {
    useApp.getState().openTable({ schema: "public", name: "users" });
    useApp.getState().newQueryTab();
    render(<Tabs />);
    const icon = (name: RegExp) => screen.getByRole("tab", { name }).querySelector("img")?.getAttribute("src");
    expect(icon(/Query 1/)).toBe(queryIcon);
    expect(icon(/users/)).toBe(tableIcon);
  });
});
