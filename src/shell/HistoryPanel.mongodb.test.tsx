import { fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

const listHistory = vi.fn();
vi.mock("../api/tauri", () => ({ listHistory: (n: number) => listHistory(n) }));

import { useApp } from "../state/store";
import { Toolbar } from "./Toolbar";

const mongo = { id: "c1", name: "Local MongoDB", kind: "mongodb", transport: "network", host: "localhost", port: 27017, database: "fx" };

beforeEach(() => {
  listHistory.mockReset();
  listHistory.mockResolvedValue([
    { id: 1, sql: "db.users.find({})", params: {}, verb: "find", ok: true, summary: "3 rows", startedAt: Date.now(), elapsedMs: 2 },
  ]);
  useApp.setState(useApp.getInitialState(), true);
  useApp.setState({
    screen: "workspace",
    active: { profile: mongo as never, serverVersion: "MongoDB 8.0.34", schemas: ["fx"], schema: "fx", oneByOneSaves: true },
    tree: { tables: ["users"], views: [], functions: [] },
    connections: [mongo as never],
  });
});

describe("HistoryPanel on MongoDB", () => {
  it("clicking a MongoDB entry opens a query tab with its text", async () => {
    render(<Toolbar />);
    fireEvent.keyDown(window, { key: "y", metaKey: true });
    await userEvent.click(await screen.findByText("db.users.find({})"));
    const tab = useApp.getState().tabs[0];
    expect(tab.kind === "query" && tab.sql).toBe("db.users.find({})");
  });
});
