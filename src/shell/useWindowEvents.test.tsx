import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../api/tauri", () => ({
  endTransaction: vi.fn(async () => "idle"),
  closeQuerySession: vi.fn(async () => undefined),
  saveTabState: vi.fn(async () => undefined),
  quitContinue: vi.fn(async () => undefined),
  reportQuitPrompt: vi.fn(async () => undefined),
  quitCancel: vi.fn(async () => undefined),
}));
const globalHandlers: Record<string, (e: { payload: unknown }) => void> = {};
const webviewHandlers: Record<string, () => void> = {};
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (n: string, h: (e: { payload: unknown }) => void) => {
    globalHandlers[n] = h;
    return () => {};
  }),
}));
vi.mock("@tauri-apps/api/webviewWindow", () => ({
  getCurrentWebviewWindow: () => ({
    listen: vi.fn(async (n: string, h: () => void) => {
      webviewHandlers[n] = h;
      return () => {};
    }),
  }),
}));
const closeHandlers: Array<(e: { preventDefault: () => void }) => void> = [];
const destroy = vi.fn();
const setTitle = vi.fn();
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({
    onCloseRequested: vi.fn(async (h: (e: { preventDefault: () => void }) => void) => {
      closeHandlers.push(h);
      return () => {};
    }),
    destroy,
    setTitle,
  }),
}));

import * as api from "../api/tauri";
import { type QueryTab, useApp } from "../state/store";
import { LeaveGuard } from "./LeaveGuard";
import { useWindowEvents } from "./useWindowEvents";

function Harness() {
  useWindowEvents();
  return <LeaveGuard />;
}
const qtab = (txStatus: QueryTab["txStatus"]): QueryTab => ({
  kind: "query", id: "q1", title: "Query 1", sql: "", savedQueryId: null, savedSql: null, params: {}, schema: null, database: null,
  run: { status: "idle", results: [], selected: 0, startedAt: null, sqlAtRun: null, pending: null, error: null, statementCount: null },
  txStatus, note: null,
});
const conn = { id: "c1", name: "Local Postgres", kind: "postgres", transport: "network", host: "localhost", port: 5432, user: "u", database: "app_dev" };
const info = { profile: conn as never, serverVersion: "17", schemas: ["public"], schema: "public" };

beforeEach(() => {
  vi.clearAllMocks();
  for (const k of Object.keys(globalHandlers)) delete globalHandlers[k];
  for (const k of Object.keys(webviewHandlers)) delete webviewHandlers[k];
  closeHandlers.length = 0;
  destroy.mockResolvedValue(undefined);
  setTitle.mockResolvedValue(undefined);
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
  useApp.setState({ screen: "connections", active: null, tabs: [], activeTabId: null, leave: null, leaveError: null });
});

const ready = async () => {
  render(<Harness />);
  await waitFor(() => expect(webviewHandlers["quit-requested"]).toBeDefined());
  await waitFor(() => expect(globalHandlers["saved-queries-changed"]).toBeDefined());
  await waitFor(() => expect(closeHandlers).toHaveLength(1));
};
const withOpenTx = () => useApp.setState({ screen: "workspace", tabs: [qtab("open")], activeTabId: "q1" });

describe("quit-requested", () => {
  it("quit asks, then continues", async () => {
    withOpenTx();
    await ready();
    act(() => webviewHandlers["quit-requested"]());
    expect(useApp.getState().leave?.reason).toBe("quit");
    expect(api.quitContinue).not.toHaveBeenCalled();
    await userEvent.click(screen.getByRole("button", { name: "Roll back" }));
    await waitFor(() => expect(api.quitContinue).toHaveBeenCalledTimes(1));
    expect(api.quitCancel).not.toHaveBeenCalled();
    expect(vi.mocked(api.saveTabState).mock.invocationCallOrder[0]).toBeLessThan(vi.mocked(api.quitContinue).mock.invocationCallOrder[0]);
  });

  it("quit cancel tells Rust", async () => {
    withOpenTx();
    await ready();
    act(() => webviewHandlers["quit-requested"]());
    await userEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(api.quitCancel).toHaveBeenCalledTimes(1);
    expect(api.quitContinue).not.toHaveBeenCalled();
  });

  it("quit replaced by window close cancels the quit, then the window is destroyed without continuing", async () => {
    withOpenTx();
    await ready();
    act(() => webviewHandlers["quit-requested"]());
    act(() => closeHandlers[0]({ preventDefault: vi.fn() }));
    expect(api.quitCancel).toHaveBeenCalledTimes(1);
    await userEvent.click(screen.getByRole("button", { name: "Roll back" }));
    await waitFor(() => expect(destroy).toHaveBeenCalledTimes(1));
    expect(api.quitContinue).not.toHaveBeenCalled();
  });

  it("a second quit-requested while the quit prompt is up changes nothing", async () => {
    withOpenTx();
    await ready();
    act(() => webviewHandlers["quit-requested"]());
    const first = useApp.getState().leave;
    act(() => webviewHandlers["quit-requested"]());
    expect(useApp.getState().leave).toBe(first);
    expect(api.quitCancel).not.toHaveBeenCalled();
    await userEvent.click(screen.getByRole("button", { name: "Roll back" }));
    await waitFor(() => expect(api.quitContinue).toHaveBeenCalledTimes(1));
  });

  it("a second quit-requested after a failed quit_continue asks again", async () => {
    vi.mocked(api.quitContinue).mockRejectedValueOnce(new Error("ipc down"));
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    await ready();
    act(() => webviewHandlers["quit-requested"]());
    await waitFor(() => expect(api.quitContinue).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(error).toHaveBeenCalled());
    act(() => webviewHandlers["quit-requested"]());
    await waitFor(() => expect(api.quitContinue).toHaveBeenCalledTimes(2));
    error.mockRestore();
  });

  it("a second quit-requested after another prompt was answered starts the quit prompt again", async () => {
    withOpenTx();
    await ready();
    act(() => webviewHandlers["quit-requested"]());
    await userEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(useApp.getState().leave).toBeNull();
    act(() => webviewHandlers["quit-requested"]());
    expect(useApp.getState().leave?.reason).toBe("quit");
  });

  it("quit off the workspace continues at once", async () => {
    await ready();
    act(() => webviewHandlers["quit-requested"]());
    await waitFor(() => expect(api.quitContinue).toHaveBeenCalledTimes(1));
  });
});

describe("open-window-failed", () => {
  it("shows the menu's New Window failure as the workspace notice", async () => {
    await ready();
    const handler = webviewHandlers["open-window-failed"] as unknown as (e: { payload: string }) => void;
    act(() => handler({ payload: "the window system said no" }));
    expect(useApp.getState().notice).toBe("Couldn't open a new window: the window system said no");
  });
});

describe("cross-window refresh", () => {
  it("connections-changed reloads connections", async () => {
    const loadConnections = vi.fn(async () => {});
    useApp.setState({ loadConnections });
    await ready();
    act(() => globalHandlers["connections-changed"]({ payload: null }));
    expect(loadConnections).toHaveBeenCalledTimes(1);
  });

  it("saved-queries-changed for another connection is ignored; for this one it reloads", async () => {
    const loadSavedQueries = vi.fn(async () => {});
    useApp.setState({ loadSavedQueries, active: info });
    await ready();
    act(() => globalHandlers["saved-queries-changed"]({ payload: { connectionId: "other" } }));
    expect(loadSavedQueries).not.toHaveBeenCalled();
    act(() => globalHandlers["saved-queries-changed"]({ payload: { connectionId: "c1" } }));
    expect(loadSavedQueries).toHaveBeenCalledTimes(1);
  });
});

describe("window title", () => {
  it("follows the connection", async () => {
    await ready();
    expect(setTitle).not.toHaveBeenCalled();
    act(() => useApp.setState({ active: info }));
    expect(setTitle).toHaveBeenLastCalledWith("Local Postgres · app_dev");
    act(() => useApp.setState({ tabs: [], activeTabId: null }));
    expect(setTitle).toHaveBeenCalledTimes(1);
    act(() => useApp.setState({ active: null, screen: "connections" }));
    expect(setTitle).toHaveBeenLastCalledWith("Porthole");
  });
});

describe("window title on a SQLite connection", () => {
  it("is {connection} · {file name}", async () => {
    await ready();
    const profile = { id: "s1", name: "Local SQLite", kind: "sqlite", transport: "file", filePath: "/Users/me/data/app.sqlite" };
    act(() => useApp.setState({ active: { profile: profile as never, serverVersion: "SQLite 3.50.4", schemas: ["main"], schema: "main" } }));
    expect(setTitle).toHaveBeenLastCalledWith("Local SQLite · app.sqlite");
  });
});

describe("quit prompt report", () => {
  it("tells Rust whether quitting would ask, only when it changes", async () => {
    await ready();
    // Connections screen: nothing to ask.
    await waitFor(() => expect(api.reportQuitPrompt).toHaveBeenLastCalledWith(false));
    const calls = vi.mocked(api.reportQuitPrompt).mock.calls.length;
    act(() => withOpenTx());
    await waitFor(() => expect(api.reportQuitPrompt).toHaveBeenLastCalledWith(true));
    act(() => useApp.setState({ tabs: [qtab("idle")] }));
    await waitFor(() => expect(api.reportQuitPrompt).toHaveBeenLastCalledWith(false));
    // An unrelated change sends nothing.
    const before = vi.mocked(api.reportQuitPrompt).mock.calls.length;
    act(() => useApp.setState({ activeTabId: "q1" }));
    expect(vi.mocked(api.reportQuitPrompt).mock.calls.length).toBe(before);
    expect(before).toBeGreaterThan(calls);
  });

  it("sends the value again after a failed report", async () => {
    await ready();
    await waitFor(() => expect(api.reportQuitPrompt).toHaveBeenLastCalledWith(false));
    vi.mocked(api.reportQuitPrompt).mockRejectedValueOnce(new Error("ipc"));
    act(() => withOpenTx());
    await waitFor(() => expect(api.reportQuitPrompt).toHaveBeenLastCalledWith(true));
    const failed = vi.mocked(api.reportQuitPrompt).mock.calls.length;
    // An unrelated change resends the value Rust may not have.
    await act(async () => useApp.setState({ activeTabId: "q1" }));
    expect(vi.mocked(api.reportQuitPrompt).mock.calls.length).toBe(failed + 1);
    expect(api.reportQuitPrompt).toHaveBeenLastCalledWith(true);
  });
});
