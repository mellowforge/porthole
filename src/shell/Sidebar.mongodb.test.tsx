import { render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../api/tauri", () => ({}));

import { useApp } from "../state/store";
import { Sidebar } from "./Sidebar";
import { windowTitle } from "./useWindowEvents";

beforeEach(() => {
  useApp.setState(useApp.getInitialState(), true);
  useApp.setState({
    active: {
      profile: { id: "c1", name: "Local MongoDB", database: "fx", kind: "mongodb", transport: "network", host: "localhost", port: 27017 } as never,
      serverVersion: "MongoDB 8.0.34",
      schemas: ["fx", "fx_other"],
      schema: "fx",
      oneByOneSaves: true,
    },
    tree: { tables: ["users", "typed"], views: ["active_users"], functions: [] },
  });
});

describe("Sidebar with MongoDB", () => {
  it("labels the picker Database and the section COLLECTIONS", () => {
    render(<Sidebar />);
    expect(screen.getByLabelText("Database")).toBeTruthy();
    expect(screen.queryByLabelText("Schema")).toBeNull();
    expect(screen.getByRole("heading", { name: /^COLLECTIONS/ })).toBeTruthy();
    expect(screen.queryByRole("heading", { name: /^TABLES/ })).toBeNull();
    expect(screen.getByRole("heading", { name: /^VIEWS/ })).toBeTruthy();
    expect(screen.getByText("MG")).toBeTruthy();
  });

  it("the search names collections", () => {
    render(<Sidebar />);
    const search = screen.getByLabelText("Search collections, views") as HTMLInputElement;
    expect(search.placeholder).toBe("Search collections, views…");
  });

  it("the window title shows the current database", () => {
    expect(windowTitle(useApp.getState().active)).toBe("Local MongoDB · fx");
  });
});
