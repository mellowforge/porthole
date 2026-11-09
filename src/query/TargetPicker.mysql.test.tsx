import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../api/tauri", () => ({
  analyzeSql: vi.fn(),
  listDatabases: vi.fn(),
  targetSchemas: vi.fn(),
  targetColumns: vi.fn(),
}));

import * as api from "../api/tauri";
import { useApp } from "../state/store";
import type { QueryTab } from "../state/store";
import { QueryView } from "./QueryView";

const tab = (over: Partial<QueryTab> = {}): QueryTab => ({
  kind: "query", id: "q1", title: "Query 1", sql: "", savedQueryId: null, savedSql: null, params: {}, schema: null, database: null,
  run: { status: "idle", results: [], selected: 0, startedAt: null, sqlAtRun: null, pending: null, error: null, statementCount: null },
  txStatus: "idle", note: null, ...over,
});

function Host() {
  const t = useApp((s) => s.tabs.find((x) => x.id === "q1")) as QueryTab;
  return <QueryView tab={t} />;
}

beforeEach(() => {
  vi.resetAllMocks();
  vi.mocked(api.analyzeSql).mockResolvedValue({ statements: [], params: [] });
  vi.mocked(api.listDatabases).mockResolvedValue([]);
  vi.mocked(api.targetSchemas).mockResolvedValue(["fx", "fx_other"]);
  vi.mocked(api.targetColumns).mockResolvedValue({});
  useApp.setState(useApp.getInitialState(), true);
  useApp.setState({
    active: { profile: { name: "Local MySQL", database: "fx", kind: "mysql", transport: "network" } as never, serverVersion: "MySQL 8.4.13", schemas: ["fx", "fx_other"], schema: "fx" },
    tabs: [tab()],
    activeTabId: "q1",
  });
});

describe("TargetPicker with a MySQL profile", () => {
  it("the chip reads MY · current database; one section titled Database; listDatabases is not called", async () => {
    render(<Host />);
    await userEvent.click(screen.getByRole("button", { name: /MY · fx/ }));
    const options = await screen.findAllByRole("option", { name: /fx/ });
    expect(options.map((o) => o.textContent)).toEqual(["fx", "fx_other"]);
    expect(screen.getAllByText("Database")).toHaveLength(1);
    expect(screen.queryByText("Schema")).toBeNull();
    expect(api.listDatabases).not.toHaveBeenCalled();
  });

  it("picking a database moves the chip to it", async () => {
    render(<Host />);
    await userEvent.click(screen.getByRole("button", { name: /MY · fx/ }));
    await userEvent.click(await screen.findByRole("option", { name: "fx_other" }));
    expect(screen.getByRole("button", { name: /MY · fx_other ▾/ })).toBeTruthy();
  });
});
