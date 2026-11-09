import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../api/tauri", () => ({
  saveTabState: vi.fn(),
  releaseTabState: vi.fn(),
}));

import * as api from "../api/tauri";
import { type QueryTab, useApp } from "../state/store";
import { switchConnection } from "./connectionFlow";

const tab: QueryTab = {
  kind: "query", id: "q1", title: "Query 1", sql: "select 1", savedQueryId: null, savedSql: null, params: {}, schema: null, database: null,
  run: { status: "idle", results: [], selected: 0, startedAt: null, sqlAtRun: null, pending: null, error: null, statementCount: null },
  txStatus: "idle", note: null,
};

beforeEach(() => {
  vi.resetAllMocks();
  vi.mocked(api.saveTabState).mockResolvedValue(undefined);
  vi.mocked(api.releaseTabState).mockResolvedValue(undefined);
  useApp.setState(useApp.getInitialState(), true);
  useApp.setState({
    screen: "workspace",
    active: { profile: { id: "c1", name: "Local Postgres", database: "app_dev" } as never, serverVersion: "17", schemas: ["public"], schema: "public" },
    tabs: [tab],
    activeTabId: "q1",
  });
});

describe("switchConnection", () => {
  it("saves the tabs, then releases the tab state, then shows the Connections screen", async () => {
    let screenAtRelease: string | null = null;
    vi.mocked(api.releaseTabState).mockImplementation(async () => {
      screenAtRelease = useApp.getState().screen;
    });
    switchConnection("c2");
    await vi.waitFor(() => expect(useApp.getState().screen).toBe("connections"));
    expect(api.saveTabState).toHaveBeenCalledTimes(1);
    expect(api.releaseTabState).toHaveBeenCalledTimes(1);
    expect(vi.mocked(api.saveTabState).mock.invocationCallOrder[0]).toBeLessThan(vi.mocked(api.releaseTabState).mock.invocationCallOrder[0]);
    expect(screenAtRelease).toBe("workspace");
    expect(useApp.getState().selectedConnectionId).toBe("c2");
  });

  it("a failed release still leaves the workspace", async () => {
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    vi.mocked(api.releaseTabState).mockRejectedValue(new Error("ipc down"));
    switchConnection();
    await vi.waitFor(() => expect(useApp.getState().screen).toBe("connections"));
    expect(error).toHaveBeenCalledWith("release_tab_state failed", expect.anything());
    error.mockRestore();
  });
});
