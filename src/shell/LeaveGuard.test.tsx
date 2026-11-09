import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../api/tauri", () => ({
  endTransaction: vi.fn(),
  closeQuerySession: vi.fn(),
  saveTabState: vi.fn(async () => undefined),
}));
const closeHandlers: Array<(e: { preventDefault: () => void }) => void> = [];
const destroy = vi.fn();
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({
    onCloseRequested: vi.fn(async (h: (e: { preventDefault: () => void }) => void) => {
      closeHandlers.push(h);
      return () => {};
    }),
    destroy,
  }),
}));
import * as api from "../api/tauri";
import { useApp } from "../state/store";
import type { QueryTab } from "../state/store";
import { LeaveGuard } from "./LeaveGuard";

const qtab = (id: string, title: string, txStatus: QueryTab["txStatus"] = "idle"): QueryTab => ({
  kind: "query",
  id,
  title,
  sql: "",
  savedQueryId: null,
  savedSql: null,
  params: {},
  schema: null,
  database: null,
  run: { status: "idle", results: [], selected: 0, startedAt: null, sqlAtRun: null, pending: null, error: null, statementCount: null },
  txStatus,
  note: null,
});

beforeEach(() => {
  vi.resetAllMocks();
  closeHandlers.length = 0;
  vi.mocked(api.endTransaction).mockResolvedValue("idle");
  vi.mocked(api.closeQuerySession).mockResolvedValue(undefined);
  useApp.setState({ screen: "workspace", tabs: [], activeTabId: null, leave: null, leaveError: null });
});

const setTabs = (tabs: QueryTab[]) => useApp.setState({ tabs, activeTabId: tabs[0]?.id ?? null });

describe("LeaveGuard", () => {
  it("closing a tab with open transaction asks", async () => {
    setTabs([qtab("q1", "Query 1", "open")]);
    render(<LeaveGuard />);
    act(() => useApp.getState().closeTab("q1"));
    expect(screen.getByText("“Query 1” has an open transaction.")).not.toBeNull();
    expect(useApp.getState().tabs).toHaveLength(1);
    await userEvent.click(screen.getByRole("button", { name: "Commit" }));
    await waitFor(() => expect(useApp.getState().tabs).toHaveLength(0));
    expect(api.endTransaction).toHaveBeenCalledWith("q1", true);
    expect(api.closeQuerySession).toHaveBeenCalledWith("q1");
    expect(vi.mocked(api.endTransaction).mock.invocationCallOrder[0]).toBeLessThan(
      vi.mocked(api.closeQuerySession).mock.invocationCallOrder[0],
    );
    expect(screen.queryByText(/open transaction/)).toBeNull();
  });

  it("Cancel keeps the tab and session", async () => {
    setTabs([qtab("q1", "Query 1", "open")]);
    render(<LeaveGuard />);
    act(() => useApp.getState().closeTab("q1"));
    await userEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(useApp.getState().tabs).toHaveLength(1);
    expect(api.endTransaction).not.toHaveBeenCalled();
    expect(api.closeQuerySession).not.toHaveBeenCalled();
    expect(screen.queryByText(/open transaction/)).toBeNull();
  });

  it("Esc cancels", async () => {
    setTabs([qtab("q1", "Query 1", "open")]);
    render(<LeaveGuard />);
    act(() => useApp.getState().closeTab("q1"));
    await userEvent.keyboard("{Escape}");
    expect(useApp.getState().leave).toBeNull();
    expect(useApp.getState().tabs).toHaveLength(1);
  });

  it("idle tab closes at once and closes its session", () => {
    setTabs([qtab("q1", "Query 1")]);
    render(<LeaveGuard />);
    act(() => useApp.getState().closeTab("q1"));
    expect(useApp.getState().tabs).toHaveLength(0);
    expect(api.closeQuerySession).toHaveBeenCalledWith("q1");
    expect(screen.queryByText(/open transaction/)).toBeNull();
  });

  it("a closeQuerySession failure never blocks closing", () => {
    vi.mocked(api.closeQuerySession).mockRejectedValue(new Error("gone"));
    vi.spyOn(console, "error").mockImplementation(() => {});
    setTabs([qtab("q1", "Query 1")]);
    act(() => useApp.getState().closeTab("q1"));
    expect(useApp.getState().tabs).toHaveLength(0);
  });

  it("switch connection with two open tabs", async () => {
    setTabs([qtab("q1", "Query 1", "open"), qtab("q2", "Query 2", "open"), qtab("q3", "Query 3")]);
    render(<LeaveGuard />);
    act(() => useApp.getState().requestLeave("switch", ["q1", "q2", "q3"], () => useApp.getState().openConnections()));
    expect(screen.getByText("2 tabs have open transactions.")).not.toBeNull();
    await userEvent.click(screen.getByRole("button", { name: "Roll back" }));
    await waitFor(() => expect(useApp.getState().screen).toBe("connections"));
    expect(api.endTransaction).toHaveBeenCalledTimes(2);
    expect(api.endTransaction).toHaveBeenCalledWith("q1", false);
    expect(api.endTransaction).toHaveBeenCalledWith("q2", false);
  });

  it("failed tabs are rolled back even when the user picks Commit", async () => {
    setTabs([qtab("q1", "Query 1", "open"), qtab("q2", "Query 2", "failed")]);
    render(<LeaveGuard />);
    act(() => useApp.getState().requestLeave("edit", ["q1", "q2"], () => {}));
    await userEvent.click(screen.getByRole("button", { name: "Commit" }));
    await waitFor(() => expect(useApp.getState().leave).toBeNull());
    expect(api.endTransaction).toHaveBeenCalledWith("q1", true);
    expect(api.endTransaction).toHaveBeenCalledWith("q2", false);
  });

  it("failed rollback does not trap the user", async () => {
    vi.mocked(api.endTransaction).mockRejectedValue({ message: "connection lost" });
    vi.spyOn(console, "error").mockImplementation(() => {});
    setTabs([qtab("q1", "Query 1", "open")]);
    render(<LeaveGuard />);
    act(() => useApp.getState().closeTab("q1"));
    await userEvent.click(screen.getByRole("button", { name: "Roll back" }));
    await waitFor(() => expect(useApp.getState().tabs).toHaveLength(0));
    expect(api.closeQuerySession).toHaveBeenCalledWith("q1");
    expect(useApp.getState().leave).toBeNull();
  });

  it("onCancel runs when a closeTab request is cancelled, not after then", async () => {
    setTabs([qtab("q1", "Query 1", "open")]);
    render(<LeaveGuard />);
    const onCancel = vi.fn();
    act(() => useApp.getState().requestLeave("closeTab", ["q1"], vi.fn(), onCancel));
    await userEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(onCancel).toHaveBeenCalledTimes(1);

    const then = vi.fn();
    act(() => useApp.getState().requestLeave("closeTab", ["q1"], then, onCancel));
    await userEvent.click(screen.getByRole("button", { name: "Roll back" }));
    await waitFor(() => expect(then).toHaveBeenCalledTimes(1));
    expect(onCancel).toHaveBeenCalledTimes(1);
  });

  it("onCancel runs when a quit or window close replaces the request", async () => {
    setTabs([qtab("q1", "Query 1", "open")]);
    render(<LeaveGuard />);
    const onCancel = vi.fn();
    act(() => useApp.getState().requestLeave("closeTab", ["q1"], vi.fn(), onCancel));
    act(() => useApp.getState().requestLeave("window", ["q1"], vi.fn()));
    expect(onCancel).toHaveBeenCalledTimes(1);
  });

  it("failed commit keeps prompt with error", async () => {
    vi.mocked(api.endTransaction).mockRejectedValue({ message: "could not serialize access" });
    setTabs([qtab("q1", "Query 1", "open")]);
    render(<LeaveGuard />);
    act(() => useApp.getState().closeTab("q1"));
    await userEvent.click(screen.getByRole("button", { name: "Commit" }));
    expect((await screen.findByRole("alert")).textContent).toContain("could not serialize access");
    expect(screen.getByText("“Query 1” has an open transaction.")).not.toBeNull();
    expect(useApp.getState().tabs).toHaveLength(1);
    expect(api.closeQuerySession).not.toHaveBeenCalled();
  });

  it("window close with an open transaction is prevented and asks, then destroys", async () => {
    (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
    setTabs([qtab("q1", "Query 1", "open")]);
    render(<LeaveGuard />);
    await waitFor(() => expect(closeHandlers).toHaveLength(1));
    const preventDefault = vi.fn();
    act(() => closeHandlers[0]({ preventDefault }));
    expect(preventDefault).toHaveBeenCalled();
    await userEvent.click(screen.getByRole("button", { name: "Roll back" }));
    await waitFor(() => expect(destroy).toHaveBeenCalled());
  });

  it("window close with nothing open saves the tab state, then destroys", async () => {
    (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
    setTabs([{ ...qtab("q1", "Query 1"), sql: "select 42" }]);
    render(<LeaveGuard />);
    await waitFor(() => expect(closeHandlers).toHaveLength(1));
    const preventDefault = vi.fn();
    act(() => closeHandlers[0]({ preventDefault }));
    expect(preventDefault).toHaveBeenCalled();
    await waitFor(() => expect(destroy).toHaveBeenCalled());
    expect(api.saveTabState).toHaveBeenCalled();
  });

  it("an unknown transaction state asks with hedged copy", async () => {
    setTabs([qtab("q1", "Query 1", "unknown")]);
    render(<LeaveGuard />);
    act(() => useApp.getState().closeTab("q1"));
    expect(screen.getByText("“Query 1” may have an open transaction.")).not.toBeNull();
    expect(useApp.getState().tabs).toHaveLength(1);
    await userEvent.click(screen.getByRole("button", { name: "Commit" }));
    await waitFor(() => expect(useApp.getState().tabs).toHaveLength(0));
    expect(api.endTransaction).toHaveBeenCalledWith("q1", true);
  });

  it("several unknown tabs, and unknown mixed with open", () => {
    setTabs([qtab("q1", "Query 1", "unknown"), qtab("q2", "Query 2", "unknown"), qtab("q3", "Query 3", "open")]);
    render(<LeaveGuard />);
    act(() => useApp.getState().requestLeave("switch", ["q1", "q2"], () => {}));
    expect(screen.getByText("2 tabs may have open transactions.")).not.toBeNull();
    // A second request is ignored while one is open, so resolve this one first.
    act(() => void useApp.getState().resolveLeave("cancel"));
    act(() => useApp.getState().requestLeave("switch", ["q1", "q2", "q3"], () => {}));
    expect(screen.getByText("3 tabs have open transactions.")).not.toBeNull();
  });

  it("quit replacing a prompt mid-Commit waits for it; once committed, quit goes ahead and the superseded then never runs", async () => {
    let done!: () => void;
    vi.mocked(api.endTransaction).mockReturnValue(new Promise((r) => (done = () => r("idle"))));
    setTabs([qtab("q1", "Query 1", "open")]);
    render(<LeaveGuard />);
    const closed = vi.fn();
    const quit = vi.fn();
    act(() => useApp.getState().requestLeave("closeTab", ["q1"], closed));
    let resolving!: Promise<void>;
    act(() => {
      resolving = useApp.getState().resolveLeave("commit");
    });
    act(() => useApp.getState().requestLeave("quit", ["q1"], quit));
    expect(useApp.getState().leave?.reason).toBe("quit");
    expect(screen.getByText("Waiting for the transaction to end…")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Commit" })).toBeNull();
    await act(async () => {
      done();
      await resolving;
    });
    expect(closed).not.toHaveBeenCalled();
    expect(quit).toHaveBeenCalledTimes(1);
    expect(useApp.getState().leave).toBeNull();
    expect(screen.queryByText(/0 tabs/)).toBeNull();
  });

  it("a Commit that fails under a prompt quit replaced is shown by the quit prompt", async () => {
    let fail!: () => void;
    vi.mocked(api.endTransaction).mockReturnValue(new Promise((_, reject) => (fail = () => reject({ kind: "query", message: "deferred check failed" }))));
    setTabs([qtab("q1", "Query 1", "open")]);
    render(<LeaveGuard />);
    const closed = vi.fn();
    const quit = vi.fn();
    act(() => useApp.getState().requestLeave("closeTab", ["q1"], closed));
    let resolving!: Promise<void>;
    act(() => {
      resolving = useApp.getState().resolveLeave("commit");
    });
    act(() => useApp.getState().requestLeave("quit", ["q1"], quit));
    await act(async () => {
      fail();
      await resolving;
    });
    expect(useApp.getState().leave).toMatchObject({ reason: "quit", stage: "tx" });
    expect((await screen.findByRole("alert")).textContent).toBe("deferred check failed");
    expect(screen.getByText("“Query 1” has an open transaction.")).toBeTruthy();
    expect(closed).not.toHaveBeenCalled();
    expect(quit).not.toHaveBeenCalled();
  });

  it("the transaction prompt goes ahead by itself once no listed transaction is open", () => {
    setTabs([qtab("q1", "Query 1", "open"), qtab("q2", "Query 2", "open")]);
    render(<LeaveGuard />);
    const then = vi.fn();
    act(() => useApp.getState().requestLeave("switch", ["q1", "q2"], then));
    expect(screen.getByText("2 tabs have open transactions.")).toBeTruthy();
    // E.g. a query that was still running ends the transaction.
    act(() => useApp.getState().updateQueryTab("q1", { txStatus: "idle" }));
    expect(screen.getByText("“Query 2” has an open transaction.")).toBeTruthy();
    act(() => useApp.getState().updateQueryTab("q2", { txStatus: "idle" }));
    expect(then).toHaveBeenCalledTimes(1);
    expect(useApp.getState().leave).toBeNull();
    expect(screen.queryByText(/0 tabs/)).toBeNull();
  });

  it("closing a tab while its Commit (from the toolbar) is running waits for it", async () => {
    let done!: () => void;
    vi.mocked(api.endTransaction).mockReturnValue(new Promise((r) => (done = () => r("idle"))));
    setTabs([qtab("q1", "Query 1", "open")]);
    render(<LeaveGuard />);
    let ending!: Promise<void>;
    act(() => {
      ending = useApp.getState().endTransaction("q1", true);
    });
    const closed = vi.fn();
    act(() => useApp.getState().requestLeave("closeTab", ["q1"], closed));
    expect(screen.getByText("Waiting for the transaction to end…")).toBeTruthy();
    await act(async () => {
      done();
      await ending;
    });
    expect(closed).toHaveBeenCalledTimes(1);
    expect(useApp.getState().leave).toBeNull();
  });

  it("a window destroy failure is logged, not thrown", async () => {
    (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
    const log = vi.spyOn(console, "error").mockImplementation(() => {});
    destroy.mockRejectedValue(new Error("no window"));
    setTabs([qtab("q1", "Query 1")]);
    render(<LeaveGuard />);
    await waitFor(() => expect(closeHandlers).toHaveLength(1));
    act(() => closeHandlers[0]({ preventDefault: vi.fn() }));
    await waitFor(() => expect(log).toHaveBeenCalledWith("destroy failed", expect.any(Error)));
  });
});
