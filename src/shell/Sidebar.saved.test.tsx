import { act, fireEvent, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../api/tauri", () => ({
  listSavedQueries: vi.fn(),
  saveQuery: vi.fn(),
  deleteSavedQuery: vi.fn(),
}));

import * as api from "../api/tauri";
import type { SavedQuery } from "../api/types";
import { useApp } from "../state/store";
import type { QueryTab } from "../state/store";
import { Sidebar } from "./Sidebar";

const sq = (id: string, name: string, over: Partial<SavedQuery> = {}): SavedQuery => ({
  id,
  connectionId: "c1",
  name,
  sql: `select '${name}'`,
  lastParams: {},
  updatedAt: "",
  ...over,
});
const A = sq("a", "Orders per user since", { lastParams: { status: "active", limit: "20" } });
const B = sq("b", "Failed payments today");
const C = sq("c", "Slow sessions");

let db: SavedQuery[];
const tabs = () => useApp.getState().tabs as QueryTab[];

beforeEach(() => {
  db = [A, B, C];
  vi.mocked(api.listSavedQueries).mockImplementation(async () => [...db]);
  vi.mocked(api.saveQuery).mockImplementation(async (q) => {
    const saved = { ...q, id: q.id || "new1" };
    db = [...db.filter((x) => x.id !== saved.id), saved];
    return saved;
  });
  vi.mocked(api.deleteSavedQuery).mockImplementation(async (id) => {
    db = db.filter((x) => x.id !== id);
  });
  useApp.setState(useApp.getInitialState(), true);
  useApp.setState({
    active: { profile: { id: "c1", name: "Local Postgres", database: "app_dev", kind: "postgres", transport: "network", host: "localhost" } as never, serverVersion: "17", schemas: ["public"], schema: "public" },
    tree: { tables: ["users"], views: [], functions: [] },
    savedQueries: [A, B, C],
  });
});
afterEach(() => vi.resetAllMocks());

const item = (name: string) => screen.getByRole("button", { name });
const openMenu = async (name: string, entry: string) => {
  fireEvent.contextMenu(item(name));
  await userEvent.click(screen.getByRole("menuitem", { name: entry }));
};

describe("Sidebar saved queries", () => {
  it("lists saved queries with count", () => {
    render(<Sidebar />);
    expect(screen.getByRole("heading", { name: /SAVED QUERIES\s*3/ })).toBeTruthy();
    for (const q of [A, B, C]) expect(item(q.name)).toBeTruthy();
  });

  it("click opens tab with last params", async () => {
    render(<Sidebar />);
    await userEvent.click(item(A.name));
    expect(tabs()).toHaveLength(1);
    expect(tabs()[0]).toMatchObject({ title: A.name, sql: A.sql, savedSql: A.sql, savedQueryId: "a", params: A.lastParams });
    expect(item(A.name).getAttribute("aria-current")).toBe("true");
  });

  it("click again focuses existing tab", async () => {
    render(<Sidebar />);
    await userEvent.click(item(A.name));
    await userEvent.click(item(B.name));
    expect(useApp.getState().activeTabId).toBe(tabs()[1].id);
    await userEvent.click(item(A.name));
    expect(tabs()).toHaveLength(2);
    expect(useApp.getState().activeTabId).toBe(tabs()[0].id);
  });

  it("rename inline", async () => {
    render(<Sidebar />);
    await userEvent.click(item(B.name));
    await openMenu(B.name, "Rename");
    const input = screen.getByRole("textbox", { name: "Rename saved query" }) as HTMLInputElement;
    expect(input.value).toBe(B.name);
    fireEvent.change(input, { target: { value: "Late payments" } });
    await act(async () => {
      fireEvent.keyDown(input, { key: "Enter" });
    });
    expect(api.saveQuery).toHaveBeenCalledWith(expect.objectContaining({ id: "b", name: "Late payments", sql: B.sql }));
    expect(item("Late payments")).toBeTruthy();
    expect(tabs()[0].title).toBe("Late payments");
  });

  it("rename Esc cancels", async () => {
    render(<Sidebar />);
    await openMenu(B.name, "Rename");
    fireEvent.keyDown(screen.getByRole("textbox", { name: "Rename saved query" }), { key: "Escape" });
    expect(screen.queryByRole("textbox", { name: "Rename saved query" })).toBeNull();
    expect(api.saveQuery).not.toHaveBeenCalled();
  });

  it('duplicate creates "X copy"', async () => {
    render(<Sidebar />);
    await openMenu(A.name, "Duplicate");
    expect(api.saveQuery).toHaveBeenCalledWith(
      expect.objectContaining({ id: "", connectionId: "c1", name: `${A.name} copy`, sql: A.sql, lastParams: A.lastParams }),
    );
    expect(await screen.findByRole("button", { name: `${A.name} copy` })).toBeTruthy();
  });

  it("delete asks inline then removes and unsaves open tab", async () => {
    render(<Sidebar />);
    await userEvent.click(item(C.name));
    await openMenu(C.name, "Delete");
    expect(screen.getByText("Delete “Slow sessions”?")).toBeTruthy();
    expect(api.deleteSavedQuery).not.toHaveBeenCalled();
    const del = screen.getByRole("button", { name: "Delete" });
    expect(del.getAttribute("data-danger")).toBe("true");
    await userEvent.click(del);
    expect(api.deleteSavedQuery).toHaveBeenCalledWith("c");
    expect(screen.queryByRole("button", { name: C.name })).toBeNull();
    expect(tabs()[0]).toMatchObject({ savedQueryId: null, savedSql: null });
    expect(screen.getByRole("heading", { name: /SAVED QUERIES\s*2/ })).toBeTruthy();
  });

  it("delete Cancel keeps it", async () => {
    render(<Sidebar />);
    await openMenu(C.name, "Delete");
    await userEvent.click(within(screen.getByText("Delete “Slow sessions”?").parentElement!).getByRole("button", { name: "Cancel" }));
    expect(api.deleteSavedQuery).not.toHaveBeenCalled();
    expect(item(C.name)).toBeTruthy();
  });

  it("search filters saved queries", async () => {
    render(<Sidebar />);
    await userEvent.type(screen.getByRole("searchbox"), "slow");
    expect(screen.queryByRole("button", { name: A.name })).toBeNull();
    expect(item(C.name)).toBeTruthy();
    expect(screen.getByRole("heading", { name: /SAVED QUERIES\s*1/ })).toBeTruthy();
  });
});
