import { fireEvent, render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../api/tauri", () => ({}));

import { useApp } from "../state/store";
import { Sidebar } from "./Sidebar";

const refreshCatalog = vi.fn(async () => {});

beforeEach(() => {
  refreshCatalog.mockClear();
  useApp.setState(useApp.getInitialState(), true);
  useApp.setState({
    active: { profile: { id: "c1", name: "Local", database: "app", kind: "postgres", transport: "network", host: "localhost", port: 5432 } as never, serverVersion: "17", schemas: ["public"], schema: "public" },
    tree: { tables: ["users"], views: [], functions: [] },
    refreshCatalog,
  });
});

describe("Sidebar refresh", () => {
  it("the Refresh button reloads the sidebar", () => {
    render(<Sidebar />);
    const button = screen.getByRole("button", { name: "Refresh" });
    expect(button.getAttribute("title")).toBe("Refresh (⌘R)");
    fireEvent.click(button);
    expect(refreshCatalog).toHaveBeenCalledTimes(1);
  });

  it("⌘R refreshes, also while focus is in a text field", () => {
    render(<Sidebar />);
    fireEvent.keyDown(window, { key: "r", metaKey: true });
    expect(refreshCatalog).toHaveBeenCalledTimes(1);
    const input = screen.getByLabelText("Search tables, views");
    fireEvent.keyDown(input, { key: "r", metaKey: true });
    expect(refreshCatalog).toHaveBeenCalledTimes(2);
  });

  it("is disabled while a refresh runs, and ⌘R does nothing then", () => {
    useApp.setState({ catalogRefreshing: true });
    render(<Sidebar />);
    expect((screen.getByRole("button", { name: "Refresh" }) as HTMLButtonElement).disabled).toBe(true);
    fireEvent.keyDown(window, { key: "r", metaKey: true });
    expect(refreshCatalog).not.toHaveBeenCalled();
  });

  it("shows a refresh failure", () => {
    useApp.setState({ schemaError: "connection lost" });
    render(<Sidebar />);
    expect(screen.getByRole("alert").textContent).toBe("connection lost");
  });
});
