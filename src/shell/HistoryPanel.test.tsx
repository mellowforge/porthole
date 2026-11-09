import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { HistoryEntry } from "../api/types";

const listHistory = vi.fn();
vi.mock("../api/tauri", () => ({ listHistory: (n: number) => listHistory(n) }));

import { useApp } from "../state/store";
import { Toolbar } from "./Toolbar";

const NOW = new Date(2026, 10, 5, 15, 0, 0);
const at = (d: number, h: number, m: number) => new Date(2026, 10, d, h, m).getTime();
const entry = (id: number, startedAt: number, extra: Partial<HistoryEntry> = {}): HistoryEntry => ({
  id, sql: `select ${id}`, params: {}, verb: "SELECT", ok: true, summary: "1 row", startedAt, elapsedMs: 3, ...extra,
});
const entries = [
  entry(4, at(5, 14, 32), { sql: "select * from users\nwhere id = :id", params: { id: "7" } }),
  entry(3, at(5, 9, 10), { verb: "UPDATE", ok: false, summary: "boom" }),
  entry(2, at(4, 18, 0)),
  entry(1, at(3, 8, 5)),
];

beforeEach(() => {
  vi.useFakeTimers({ toFake: ["Date"], now: NOW });
  listHistory.mockReset();
  listHistory.mockResolvedValue(entries);
  useApp.setState(useApp.getInitialState(), true);
});
afterEach(() => vi.useRealTimers());

async function open() {
  render(<Toolbar />);
  fireEvent.keyDown(window, { key: "y", metaKey: true });
  await screen.findByRole("dialog", { name: "History" });
}

describe("HistoryPanel", () => {
  it("⌘Y opens and lists newest first", async () => {
    await open();
    expect(listHistory).toHaveBeenCalledWith(200);
    const rows = screen.getAllByRole("button", { name: /select|UPDATE/i }).filter((b) => b.classList.contains("history-row"));
    expect(rows.map((r) => r.textContent)).toEqual([
      expect.stringContaining("14:32"),
      expect.stringContaining("09:10"),
      expect.stringContaining("18:00"),
      expect.stringContaining("08:05"),
    ]);
  });

  it("groups by Today/Yesterday/date", async () => {
    await open();
    const heads = screen.getAllByRole("heading", { level: 3 }).map((h) => h.textContent);
    expect(heads).toEqual(["Today", "Yesterday", "Nov 3"]);
  });

  it("shows a red cross for failures and the first SQL line", async () => {
    await open();
    expect(screen.getByLabelText("Failed")).toBeTruthy();
    expect(screen.getByText("select * from users")).toBeTruthy();
  });

  it("click opens a query tab with sql and params", async () => {
    await open();
    await userEvent.click(screen.getByText("select * from users"));
    const tab = useApp.getState().tabs[0];
    expect(tab.kind === "query" && tab.sql).toBe("select * from users\nwhere id = :id");
    expect(tab.kind === "query" && tab.params).toEqual({ id: "7" });
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("an entry shows where it ran", async () => {
    useApp.setState({ active: { profile: { kind: "postgres" } as never } as never });
    listHistory.mockResolvedValue([entry(1, at(5, 9, 0), { targetDatabase: "fx_other", targetSchema: "sales" })]);
    await open();
    expect(document.querySelector(".history-target")?.textContent).toBe("fx_other · sales");
  });

  it("an entry shows only the database on MySQL", async () => {
    useApp.setState({ active: { profile: { kind: "mysql" } as never } as never });
    listHistory.mockResolvedValue([entry(1, at(5, 9, 0), { targetSchema: "fx" })]);
    await open();
    expect(document.querySelector(".history-target")?.textContent).toBe("fx");
  });

  it("clicking an entry opens it where it ran", async () => {
    useApp.setState({ active: { profile: { kind: "postgres" } as never } as never });
    listHistory.mockResolvedValue([entry(1, at(5, 9, 0), { targetDatabase: "fx_other", targetSchema: "sales" })]);
    await open();
    await userEvent.click(screen.getByText("select 1"));
    const tab = useApp.getState().tabs[0];
    expect(tab.kind === "query" && [tab.database, tab.schema]).toEqual(["fx_other", "sales"]);
  });

  it("an entry from before has no label and opens as before", async () => {
    useApp.setState({ active: { profile: { kind: "postgres" } as never } as never });
    await open();
    expect(document.querySelector(".history-target")).toBeNull();
    await userEvent.click(screen.getByText("select * from users"));
    const tab = useApp.getState().tabs[0];
    expect(tab.kind === "query" && [tab.database, tab.schema]).toEqual([null, null]);
  });

  it("Esc closes", async () => {
    await open();
    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("clicking outside closes", async () => {
    await open();
    fireEvent.mouseDown(document.body);
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("empty state copy", async () => {
    listHistory.mockResolvedValue([]);
    await open();
    expect(await screen.findByText("No queries run on this connection yet.")).toBeTruthy();
  });

  it("load failure shows an inline error", async () => {
    listHistory.mockRejectedValue(new Error("db locked"));
    await open();
    await waitFor(() => expect(within(screen.getByRole("dialog")).getByText(/db locked/)).toBeTruthy());
  });

  it("an AppError from the backend shows its message", async () => {
    listHistory.mockRejectedValue({ kind: "storage", message: "database is locked" });
    await open();
    const dialog = screen.getByRole("dialog");
    await waitFor(() => expect(within(dialog).getByText("Couldn’t load history: database is locked")).toBeTruthy());
    expect(within(dialog).queryByText(/object Object/)).toBeNull();
  });
});
