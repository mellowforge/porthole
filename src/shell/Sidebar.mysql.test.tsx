import { render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../api/tauri", () => ({}));

import { useApp } from "../state/store";
import { Sidebar } from "./Sidebar";
import { windowTitle } from "./useWindowEvents";

beforeEach(() => {
  useApp.setState(useApp.getInitialState(), true);
  useApp.setState({
    active: { profile: { id: "c1", name: "Local MySQL", database: "fx", kind: "mysql", transport: "network", host: "localhost", port: 3306 } as never, serverVersion: "MySQL 8.4.13", schemas: ["fx", "fx_other"], schema: "fx_other" },
    tree: { tables: ["other_t"], views: [], functions: [{ name: "calc_tax", signature: "amount decimal(10,2)" }, { name: "two_sets", signature: "IN n int, OUT total int", kind: "procedure" }] },
  });
});

describe("Sidebar and window title with MySQL", () => {
  it("labels the picker Database and lists functions and procedures", () => {
    render(<Sidebar />);
    expect(screen.getByLabelText("Database")).toBeTruthy();
    expect(screen.queryByLabelText("Schema")).toBeNull();
    expect(screen.getByText("calc_tax")).toBeTruthy();
    expect(screen.getByText("two_sets").closest(".side-item")?.getAttribute("title")).toBe("procedure · IN n int, OUT total int");
  });

  it("the window title shows the current database", () => {
    expect(windowTitle(useApp.getState().active)).toBe("Local MySQL · fx_other");
  });
});
