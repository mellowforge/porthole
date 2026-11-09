import { fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../api/tauri", () => ({}));

import type { SavedQuery } from "../api/types";
import { type AppState, useApp } from "../state/store";
import { paletteItems } from "../palette/paletteItems";
import { restoreTabs } from "../state/persist";
import { Tabs } from "./Tabs";
import { Toolbar } from "./Toolbar";

const mongo = { id: "c1", name: "Local MongoDB", kind: "mongodb", transport: "network", host: "localhost", port: 27017, database: "fx" };

beforeEach(() => {
  useApp.setState(useApp.getInitialState(), true);
  useApp.setState({
    screen: "workspace",
    active: { profile: mongo as never, serverVersion: "MongoDB 8.0.34", schemas: ["fx"], schema: "fx", oneByOneSaves: true },
    tree: { tables: ["users"], views: [], functions: [] },
    connections: [mongo as never],
  });
});

describe("Query tabs on MongoDB", () => {
  it("New query is enabled, and ⌘T opens a query tab", async () => {
    render(<Toolbar />);
    const btn = screen.getByRole("button", { name: /New query/ }) as HTMLButtonElement;
    expect(btn.disabled).toBe(false);
    expect(btn.title).toBe("");
    expect(btn.textContent).toContain("⌘T");
    fireEvent.keyDown(window, { key: "t", metaKey: true });
    expect(useApp.getState().tabs).toHaveLength(1);
    await userEvent.click(btn);
    expect(useApp.getState().tabs).toHaveLength(2);
  });

  it("the tab strip has a + button", () => {
    render(<Tabs />);
    expect(screen.getByRole("button", { name: "New query" })).toBeTruthy();
  });

  it("the palette has New query", () => {
    const deps = { openHistory: vi.fn(), newWindow: vi.fn(), exportActive: vi.fn() };
    const labels = paletteItems(useApp.getState() as AppState, deps).map((i) => i.label);
    expect(labels).toContain("New query");
    expect(labels).toContain("Show history");
  });

  it("newQueryTab and openSavedQuery open tabs", () => {
    const q: SavedQuery = { id: "s1", connectionId: "c1", name: "q", sql: "x", lastParams: {}, updatedAt: "" };
    useApp.getState().newQueryTab({ sql: "x" });
    useApp.getState().openSavedQuery(q);
    expect(useApp.getState().tabs).toHaveLength(2);
  });

  it("restored query tabs are dropped for a kind without query tabs; table tabs stay", () => {
    const json = JSON.stringify({
      v: 1,
      activeTabId: "q:1",
      tabs: [
        { kind: "query", id: "q:1", title: "Q", sql: "x", savedQueryId: null, savedSql: null, params: {}, schema: null, txOpen: false },
        { kind: "table", id: "fx.users", table: { schema: "fx", name: "users" }, view: { mode: "data", filter: null, sort: null } },
      ],
    });
    const restored = restoreTabs(json, null, { queryTabs: false });
    expect(restored.tabs.map((t) => t.id)).toEqual(["fx.users"]);
    expect(restored.activeTabId).toBe("fx.users");
    expect(restoreTabs(json, null).tabs.map((t) => t.id)).toEqual(["q:1", "fx.users"]);
  });
});
