import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../api/tauri", () => ({
  applyChanges: vi.fn(),
  previewChanges: vi.fn(),
  endTransaction: vi.fn(),
  closeQuerySession: vi.fn(),
  quitContinue: vi.fn(),
  reportQuitPrompt: vi.fn(async () => undefined),
  quitCancel: vi.fn(),
  loadConnections: vi.fn(),
  saveTabState: vi.fn(async () => undefined),
}));
let quitHandler: (() => void) | null = null;
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@tauri-apps/api/webviewWindow", () => ({
  getCurrentWebviewWindow: () => ({
    listen: vi.fn(async (_n: string, h: () => void) => {
      quitHandler = h;
      return () => {};
    }),
  }),
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
    setTitle: vi.fn(async () => undefined),
  }),
}));
import * as api from "../api/tauri";
import { type QueryTab, type TableTab, useApp } from "../state/store";
import { emptyPending, setCell } from "../table/pending";
import { LeaveGuard } from "./LeaveGuard";
import { useWindowEvents } from "./useWindowEvents";

const withEdits = (n: number) => {
  let p = emptyPending;
  for (let i = 1; i <= n; i++) p = setCell(p, [String(i)], "name", "new", "old");
  return p;
};
const ttab = (name: string, edits = 1): TableTab => ({
  kind: "table",
  id: `public.${name}`,
  table: { schema: "public", name },
  view: { mode: "data", filter: null, sort: null },
  pending: withEdits(edits),
});
const qtab = (txStatus: QueryTab["txStatus"]): QueryTab => ({
  kind: "query", id: "q1", title: "Query 1", sql: "", savedQueryId: null, savedSql: null, params: {}, schema: null, database: null,
  run: { status: "idle", results: [], selected: 0, startedAt: null, sqlAtRun: null, pending: null, error: null, statementCount: null },
  txStatus, note: null,
});

function Harness() {
  useWindowEvents();
  return <LeaveGuard />;
}
const tabIds = () => useApp.getState().tabs.map((t) => t.id);

/** applyChanges calls that resolve (or reject) when the test says so, in call order. */
function deferApply() {
  const calls: { resolve: () => void; reject: (e: unknown) => void }[] = [];
  vi.mocked(api.applyChanges).mockImplementation(
    () => new Promise((resolve, reject) => calls.push({ resolve: () => resolve({ applied: 1 }), reject })),
  );
  return calls;
}

beforeEach(() => {
  vi.resetAllMocks();
  quitHandler = null;
  closeHandlers.length = 0;
  destroy.mockResolvedValue(undefined);
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
  vi.mocked(api.applyChanges).mockResolvedValue({ applied: 1 });
  vi.mocked(api.previewChanges).mockImplementation(async (set) => [`UPDATE ${set.table.name} SET name = 'new';`]);
  vi.mocked(api.endTransaction).mockResolvedValue("idle");
  vi.mocked(api.closeQuerySession).mockResolvedValue(undefined);
  vi.mocked(api.saveTabState).mockResolvedValue(undefined);
  vi.mocked(api.quitContinue).mockResolvedValue(undefined);
  vi.mocked(api.quitCancel).mockResolvedValue(undefined);
  useApp.setState({ screen: "workspace", tabs: [], activeTabId: null, leave: null, leaveError: null });
});

describe("LeaveGuard with unsaved grid edits", () => {
  it("closing a table tab with edits asks; Save saves then closes; Discard closes; Cancel keeps", async () => {
    useApp.setState({ tabs: [ttab("users", 2)], activeTabId: "public.users" });
    render(<LeaveGuard />);

    act(() => useApp.getState().closeTab("public.users"));
    expect(screen.getByText("“users” has 2 unsaved changes.")).toBeTruthy();
    await userEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(tabIds()).toEqual(["public.users"]);
    expect(screen.queryByRole("alertdialog")).toBeNull();

    act(() => useApp.getState().closeTab("public.users"));
    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(tabIds()).toEqual([]));
    expect(api.applyChanges).toHaveBeenCalledTimes(1);

    useApp.setState({ tabs: [ttab("users", 1)], activeTabId: "public.users" });
    act(() => useApp.getState().closeTab("public.users"));
    expect(screen.getByText("“users” has 1 unsaved change.")).toBeTruthy();
    await userEvent.click(screen.getByRole("button", { name: "Discard" }));
    await waitFor(() => expect(tabIds()).toEqual([]));
    expect(api.applyChanges).toHaveBeenCalledTimes(1);
  });

  it("switch connection with edits in two tabs → 2 tabs have unsaved changes.", async () => {
    useApp.setState({ tabs: [ttab("users"), ttab("orders"), qtab("idle")], activeTabId: "public.users" });
    render(<LeaveGuard />);
    const then = vi.fn();
    act(() => useApp.getState().requestLeave("switch", useApp.getState().tabs.map((t) => t.id), then));
    expect(screen.getByText("2 tabs have unsaved changes.")).toBeTruthy();
    expect(then).not.toHaveBeenCalled();
    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(then).toHaveBeenCalledTimes(1));
    expect(api.applyChanges).toHaveBeenCalledTimes(2);
  });

  it("edits then open transaction → edits prompt first, then transaction prompt", async () => {
    useApp.setState({ tabs: [ttab("users"), qtab("open")], activeTabId: "public.users" });
    render(<LeaveGuard />);
    const then = vi.fn();
    act(() => useApp.getState().requestLeave("edit", tabIds(), then));
    expect(screen.getByText("“users” has 1 unsaved change.")).toBeTruthy();
    expect(screen.queryByText(/open transaction/)).toBeNull();
    await userEvent.click(screen.getByRole("button", { name: "Discard" }));
    expect(await screen.findByText("“Query 1” has an open transaction.")).toBeTruthy();
    expect(screen.queryByText(/unsaved change/)).toBeNull();
    expect(then).not.toHaveBeenCalled();
    await userEvent.click(screen.getByRole("button", { name: "Roll back" }));
    await waitFor(() => expect(then).toHaveBeenCalledTimes(1));
  });

  it("save failure in prompt keeps it with the error", async () => {
    vi.mocked(api.applyChanges).mockRejectedValue({ kind: "query", message: "boom" });
    useApp.setState({ tabs: [ttab("users")], activeTabId: "public.users" });
    render(<LeaveGuard />);
    const then = vi.fn();
    act(() => useApp.getState().requestLeave("switch", tabIds(), then));
    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    expect((await screen.findByRole("alert")).textContent).toContain("Nothing was saved: boom");
    expect(then).not.toHaveBeenCalled();
    expect(screen.getByText("“users” has 1 unsaved change.")).toBeTruthy();
    expect((useApp.getState().tabs[0] as TableTab).pending.edits).not.toEqual({});
  });

  it("quit with edits asks", async () => {
    useApp.setState({ tabs: [ttab("users")], activeTabId: "public.users" });
    render(<Harness />);
    await waitFor(() => expect(quitHandler).not.toBeNull());
    act(() => quitHandler!());
    expect(screen.getByText("“users” has 1 unsaved change.")).toBeTruthy();
    expect(api.quitContinue).not.toHaveBeenCalled();
    await userEvent.click(screen.getByRole("button", { name: "Discard" }));
    await waitFor(() => expect(api.quitContinue).toHaveBeenCalled());
  });

  it("Discard on quit writes the tabs without the discarded changes", async () => {
    useApp.setState({ tabs: [{ ...ttab("invoices", 2), uncertainSave: true }, ttab("payments", 0)], activeTabId: "public.invoices" });
    render(<Harness />);
    await waitFor(() => expect(quitHandler).not.toBeNull());
    act(() => quitHandler!());
    expect(screen.getByText("“invoices” has 2 unsaved changes.")).toBeTruthy();
    await userEvent.click(screen.getByRole("button", { name: "Discard" }));
    await waitFor(() => expect(api.quitContinue).toHaveBeenCalled());
    expect(api.saveTabState).toHaveBeenCalled();
    const saves = vi.mocked(api.saveTabState).mock;
    const last = saves.calls[saves.calls.length - 1][0];
    const stored = JSON.parse(last) as { tabs: Record<string, unknown>[] };
    expect(stored.tabs.map((t) => t.id)).toEqual(["public.invoices", "public.payments"]);
    expect(stored.tabs.filter((t) => "pending" in t || "uncertainSave" in t)).toEqual([]);
    expect(saves.invocationCallOrder[saves.invocationCallOrder.length - 1]).toBeLessThan(vi.mocked(api.quitContinue).mock.invocationCallOrder[0]);
  });

  it("⌘S then closeTab while the save is pending: waiting sheet, closes after the save, never discards mid-write", async () => {
    let finish!: () => void;
    vi.mocked(api.applyChanges).mockReturnValue(new Promise((r) => (finish = () => r({ applied: 1 }))));
    useApp.setState({ tabs: [ttab("users")], activeTabId: "public.users" });
    render(<LeaveGuard />);
    let saving!: Promise<boolean>;
    act(() => {
      saving = useApp.getState().saveTableChanges("public.users");
    });
    act(() => useApp.getState().closeTab("public.users"));
    const sheet = screen.getByRole("alertdialog");
    expect(sheet.textContent).toContain("Waiting for the save to finish…");
    expect(within(sheet).getAllByRole("button").map((b) => b.textContent)).toEqual(["Cancel"]);
    expect(tabIds()).toEqual(["public.users"]);
    await act(async () => {
      finish();
      await saving;
    });
    await waitFor(() => expect(tabIds()).toEqual([]));
    expect(screen.queryByRole("alertdialog")).toBeNull();
  });

  it("⌘W during a pending save shows the waiting sheet; Cancel keeps the tab and the save goes on", async () => {
    let finish!: () => void;
    vi.mocked(api.applyChanges).mockReturnValue(new Promise((r) => (finish = () => r({ applied: 1 }))));
    useApp.setState({ tabs: [ttab("users")], activeTabId: "public.users" });
    render(<LeaveGuard />);
    let saving!: Promise<boolean>;
    act(() => {
      saving = useApp.getState().saveTableChanges("public.users");
    });
    act(() => useApp.getState().closeTab("public.users"));
    expect(screen.getByText("Waiting for the save to finish…")).toBeTruthy();
    await userEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(screen.queryByRole("alertdialog")).toBeNull();
    expect(useApp.getState().leave).toBeNull();
    let saved = false;
    await act(async () => {
      finish();
      saved = await saving;
    });
    expect(saved).toBe(true);
    expect(tabIds()).toEqual(["public.users"]);
    expect(screen.queryByRole("alertdialog")).toBeNull();
  });

  it("reverting the only edit with the sheet up closes the tab", async () => {
    useApp.setState({ tabs: [ttab("users")], activeTabId: "public.users" });
    render(<LeaveGuard />);
    const then = vi.fn();
    act(() => useApp.getState().requestLeave("closeTab", ["public.users"], then));
    expect(screen.getByText("“users” has 1 unsaved change.")).toBeTruthy();
    act(() => useApp.getState().revertCell("public.users", ["1"], "name"));
    expect(then).toHaveBeenCalledTimes(1);
    expect(useApp.getState().leave).toBeNull();
    expect(screen.queryByRole("alertdialog")).toBeNull();
  });

  it("a save refused because that tab's save is already running says so", async () => {
    const calls = deferApply();
    useApp.setState({ tabs: [ttab("users"), ttab("orders")], activeTabId: "public.users" });
    render(<LeaveGuard />);
    act(() => useApp.getState().requestLeave("switch", tabIds(), vi.fn()));
    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(calls).toHaveLength(1));
    // ⌘S in the second tab while the prompt is saving the first.
    let other!: Promise<boolean>;
    act(() => {
      other = useApp.getState().saveTableChanges("public.orders");
    });
    expect(calls).toHaveLength(2);
    await act(async () => calls[0].resolve());
    expect((await screen.findByRole("alert")).textContent).toBe("A save for “orders” is still running. Try again.");
    await act(async () => {
      calls[1].resolve();
      await other;
    });
  });

  it("with several tabs a save error names the table", async () => {
    vi.mocked(api.applyChanges).mockRejectedValue({ kind: "query", message: "boom" });
    useApp.setState({ tabs: [ttab("users"), ttab("orders")], activeTabId: "public.users" });
    render(<LeaveGuard />);
    act(() => useApp.getState().requestLeave("switch", tabIds(), vi.fn()));
    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    expect((await screen.findByRole("alert")).textContent).toBe("“users”: Nothing was saved: boom");
  });

  it("⌘W while a quit prompt is up is ignored; quit while a closeTab prompt is up replaces it", async () => {
    useApp.setState({ tabs: [ttab("users"), ttab("orders")], activeTabId: "public.users" });
    render(<Harness />);
    await waitFor(() => expect(quitHandler).not.toBeNull());

    act(() => quitHandler!());
    expect(screen.getByText("2 tabs have unsaved changes.")).toBeTruthy();
    act(() => useApp.getState().closeTab("public.users"));
    expect(useApp.getState().leave?.reason).toBe("quit");
    expect(screen.getByText("2 tabs have unsaved changes.")).toBeTruthy();
    await userEvent.click(screen.getByRole("button", { name: "Cancel" }));

    const closed = vi.fn();
    act(() => useApp.getState().requestLeave("closeTab", ["public.users"], closed));
    expect(screen.getByText("“users” has 1 unsaved change.")).toBeTruthy();
    act(() => quitHandler!());
    expect(useApp.getState().leave?.reason).toBe("quit");
    expect(screen.getByText("2 tabs have unsaved changes.")).toBeTruthy();
    await userEvent.click(screen.getByRole("button", { name: "Discard" }));
    await waitFor(() => expect(api.quitContinue).toHaveBeenCalled());
    expect(closed).not.toHaveBeenCalled();
  });

  it("quit replaced by window close cancels the quit in Rust before the window is destroyed", async () => {
    let cancelled!: () => void;
    vi.mocked(api.quitCancel).mockReturnValue(new Promise<void>((r) => (cancelled = r)));
    useApp.setState({ tabs: [ttab("users")], activeTabId: "public.users" });
    render(<Harness />);
    await waitFor(() => expect(quitHandler).not.toBeNull());
    await waitFor(() => expect(closeHandlers).toHaveLength(1));
    act(() => quitHandler!());
    expect(useApp.getState().leave?.reason).toBe("quit");
    act(() => closeHandlers[0]({ preventDefault: vi.fn() }));
    // Synchronously, at replacement time.
    expect(api.quitCancel).toHaveBeenCalledTimes(1);
    expect(useApp.getState().leave?.reason).toBe("window");
    await userEvent.click(screen.getByRole("button", { name: "Discard" }));
    await waitFor(() => expect(api.saveTabState).toHaveBeenCalled());
    await new Promise((r) => setTimeout(r, 20));
    expect(destroy).not.toHaveBeenCalled();
    await act(async () => cancelled());
    await waitFor(() => expect(destroy).toHaveBeenCalledTimes(1));
    expect(vi.mocked(api.quitCancel).mock.invocationCallOrder[0]).toBeLessThan(destroy.mock.invocationCallOrder[0]);
    expect(api.quitContinue).not.toHaveBeenCalled();
  });

  it("closing the window with edits asks, then destroys it after Save", async () => {
    useApp.setState({ tabs: [ttab("users")], activeTabId: "public.users" });
    render(<LeaveGuard />);
    await waitFor(() => expect(closeHandlers).toHaveLength(1));
    const preventDefault = vi.fn();
    act(() => closeHandlers[0]({ preventDefault }));
    expect(preventDefault).toHaveBeenCalled();
    expect(screen.getByText("“users” has 1 unsaved change.")).toBeTruthy();
    expect(destroy).not.toHaveBeenCalled();
    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(destroy).toHaveBeenCalledTimes(1));
    expect(api.applyChanges).toHaveBeenCalledTimes(1);
  });

  it("Save in the prompt while another save is in flight waits and does not error", async () => {
    let finish!: () => void;
    vi.mocked(api.applyChanges).mockReturnValue(new Promise((r) => (finish = () => r({ applied: 1 }))));
    useApp.setState({ tabs: [ttab("users")], activeTabId: "public.users" });
    render(<LeaveGuard />);
    const then = vi.fn();
    // The prompt is up, then a save (⌘S) starts under it.
    act(() => useApp.getState().requestLeave("switch", tabIds(), then));
    let saving!: Promise<boolean>;
    act(() => {
      saving = useApp.getState().saveTableChanges("public.users");
    });
    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(screen.queryByRole("alert")).toBeNull();
    expect(then).not.toHaveBeenCalled();
    await act(async () => {
      finish();
      await saving;
    });
    await waitFor(() => expect(then).toHaveBeenCalledTimes(1));
    expect(api.applyChanges).toHaveBeenCalledTimes(1);
    expect(screen.queryByText(/0 tabs/)).toBeNull();
  });

  it("Production: the edits prompt shows the SQL, and Save to Production waits for it", async () => {
    let respond!: (s: string[]) => void;
    vi.mocked(api.previewChanges).mockImplementation(() => new Promise((r) => (respond = r)));
    useApp.setState({
      active: { profile: { environment: "production" } } as never,
      tabs: [ttab("users")],
      activeTabId: "public.users",
    });
    render(<LeaveGuard />);
    act(() => useApp.getState().requestLeave("switch", tabIds(), vi.fn()));
    expect(screen.getByText("This writes to Production.")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Save" })).toBeNull();
    const button = () => screen.getByRole("button", { name: "Save to Production" }) as HTMLButtonElement;
    expect(screen.getByText("Preparing SQL…")).toBeTruthy();
    expect(button().disabled).toBe(true);
    await act(async () => respond(["UPDATE users SET name = 'new';"]));
    expect(within(screen.getByRole("region", { name: "SQL preview" })).getByText(/UPDATE/)).toBeTruthy();
    expect(button().disabled).toBe(false);
    await userEvent.click(button());
    await waitFor(() => expect(api.applyChanges).toHaveBeenCalledTimes(1));
    useApp.setState({ active: null });
  });

  it("Production: a failed preview keeps Save to Production disabled; Discard still works", async () => {
    vi.mocked(api.previewChanges).mockRejectedValue({ kind: "query", message: "no such column" });
    useApp.setState({ active: { profile: { environment: "production" } } as never, tabs: [ttab("users")], activeTabId: "public.users" });
    render(<LeaveGuard />);
    const then = vi.fn();
    act(() => useApp.getState().requestLeave("switch", tabIds(), then));
    expect((await screen.findByRole("alert")).textContent).toBe("no such column");
    expect((screen.getByRole("button", { name: "Save to Production" }) as HTMLButtonElement).disabled).toBe(true);
    await userEvent.click(screen.getByRole("button", { name: "Discard" }));
    expect(then).toHaveBeenCalledTimes(1);
    expect(api.applyChanges).not.toHaveBeenCalled();
    useApp.setState({ active: null });
  });

  it("Production with two tabs: one titled preview each; Save waits for both", async () => {
    const respond: Record<string, (s: string[]) => void> = {};
    vi.mocked(api.previewChanges).mockImplementation((set) => new Promise((r) => (respond[set.table.name] = r)));
    useApp.setState({ active: { profile: { environment: "production" } } as never, tabs: [ttab("users"), ttab("orders")], activeTabId: "public.users" });
    render(<LeaveGuard />);
    act(() => useApp.getState().requestLeave("switch", tabIds(), vi.fn()));
    const region = screen.getByRole("region", { name: "SQL preview" });
    expect(within(region).getByText("users")).toBeTruthy();
    expect(within(region).getByText("orders")).toBeTruthy();
    const button = () => screen.getByRole("button", { name: "Save to Production" }) as HTMLButtonElement;
    await act(async () => respond.users(["UPDATE users SET name = 'new';"]));
    expect(button().disabled).toBe(true);
    await act(async () => respond.orders(["UPDATE orders SET name = 'new';"]));
    expect(button().disabled).toBe(false);
    useApp.setState({ active: null });
  });

  it("a preview loaded by an earlier prompt does not enable Save in the next one", async () => {
    useApp.setState({ active: { profile: { environment: "production" } } as never, tabs: [ttab("users")], activeTabId: "public.users" });
    render(<LeaveGuard />);
    act(() => useApp.getState().requestLeave("switch", tabIds(), vi.fn()));
    await waitFor(() => expect((screen.getByRole("button", { name: "Save to Production" }) as HTMLButtonElement).disabled).toBe(false));
    await userEvent.click(screen.getByRole("button", { name: "Cancel" }));
    vi.mocked(api.previewChanges).mockImplementation(() => new Promise(() => {}));
    act(() => useApp.getState().requestLeave("switch", tabIds(), vi.fn()));
    expect((screen.getByRole("button", { name: "Save to Production" }) as HTMLButtonElement).disabled).toBe(true);
    useApp.setState({ active: null });
  });

  it("changes made after the preview loaded disable Save until their own preview loads", async () => {
    useApp.setState({ active: { profile: { environment: "production" } } as never, tabs: [ttab("users")], activeTabId: "public.users" });
    render(<LeaveGuard />);
    act(() => useApp.getState().requestLeave("switch", tabIds(), vi.fn()));
    const button = () => screen.getByRole("button", { name: "Save to Production" }) as HTMLButtonElement;
    await waitFor(() => expect(button().disabled).toBe(false));
    vi.mocked(api.previewChanges).mockImplementation(() => new Promise(() => {}));
    // E.g. an open cell editor committing as the prompt opens.
    act(() => useApp.getState().editCell("public.users", ["9"], "name", "late", "old"));
    expect(button().disabled).toBe(true);
    useApp.setState({ active: null });
  });

  it("after an uncertain save, the prompt warns, shows the SQL and reads Save anyway", async () => {
    useApp.setState({ tabs: [{ ...ttab("users"), uncertainSave: true }, ttab("orders")], activeTabId: "public.users" });
    render(<LeaveGuard />);
    const then = vi.fn();
    act(() => useApp.getState().requestLeave("switch", tabIds(), then));
    expect(
      screen.getByText("“users”: The last save may already have been applied. Check the rows before saving again."),
    ).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Save" })).toBeNull();
    const button = screen.getByRole("button", { name: "Save anyway" }) as HTMLButtonElement;
    await waitFor(() => expect(button.disabled).toBe(false));
    await userEvent.click(button);
    await waitFor(() => expect(then).toHaveBeenCalledTimes(1));
    expect(api.applyChanges).toHaveBeenCalledTimes(2);
  });


  it("a save made under the open prompt advances it: then runs once", async () => {
    useApp.setState({ tabs: [ttab("users")], activeTabId: "public.users" });
    render(<LeaveGuard />);
    act(() => useApp.getState().closeTab("public.users"));
    expect(screen.getByRole("alertdialog")).toBeTruthy();
    await act(async () => {
      await useApp.getState().saveTableChanges("public.users");
    });
    await waitFor(() => expect(tabIds()).toEqual([]));
    expect(useApp.getState().leave).toBeNull();
  });

  it("the same with an open transaction listed → the transaction prompt shows", async () => {
    useApp.setState({ tabs: [ttab("users"), qtab("open")], activeTabId: "public.users" });
    render(<LeaveGuard />);
    const then = vi.fn();
    act(() => useApp.getState().requestLeave("switch", tabIds(), then));
    await act(async () => {
      await useApp.getState().saveTableChanges("public.users");
    });
    expect(await screen.findByText("“Query 1” has an open transaction.")).toBeTruthy();
    expect(then).not.toHaveBeenCalled();
  });

  it("clicking Save racing a ⌘S runs then exactly once and saves once", async () => {
    useApp.setState({ tabs: [ttab("users")], activeTabId: "public.users" });
    render(<LeaveGuard />);
    const then = vi.fn();
    act(() => useApp.getState().requestLeave("switch", tabIds(), then));
    const save = screen.getByRole("button", { name: "Save" });
    let shortcut!: Promise<boolean>;
    await act(async () => {
      // ⌘S starts first, the click lands while it is in flight; both end by advancing the prompt.
      shortcut = useApp.getState().saveTableChanges("public.users");
      fireEvent.click(save);
      await shortcut;
    });
    await waitFor(() => expect(useApp.getState().leave).toBeNull());
    expect(then).toHaveBeenCalledTimes(1);
    expect(api.applyChanges).toHaveBeenCalledTimes(1);
  });
});
