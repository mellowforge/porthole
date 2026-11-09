import { act, fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../api/tauri", () => ({ openWindow: vi.fn(async () => undefined) }));
import * as api from "../api/tauri";

import { Toolbar } from "../shell/Toolbar";
import { useApp } from "../state/store";
import { useShortcuts } from "../hooks/useShortcuts";
import { CommandPaletteHost } from "./CommandPalette";

const conn = { id: "c1", name: "Local", kind: "postgres", transport: "network", host: "localhost", port: 5432, database: "app" };

beforeEach(() => {
  useApp.setState(useApp.getInitialState(), true);
  useApp.setState({
    screen: "workspace",
    active: { profile: conn as never, serverVersion: "17", schemas: ["public"], schema: "public" },
    tree: { tables: ["orders", "customer_orders", "word_count"], views: [], functions: [] },
    connections: [conn as never],
  });
});

function PageShortcuts({ handlers }: { handlers: Record<string, () => void> }) {
  useShortcuts(handlers);
  return null;
}

const open = () => act(() => useApp.getState().setPaletteOpen(true));

describe("CommandPalette", () => {
  it("lists the best match first with its kind, and Enter opens it and closes", async () => {
    render(<CommandPaletteHost />);
    open();
    await userEvent.type(screen.getByRole("combobox"), "ord");
    const options = screen.getAllByRole("option");
    expect(options[0].textContent).toContain("orders");
    expect(options[0].textContent).toContain("Table");
    await userEvent.keyboard("{Enter}");
    expect(useApp.getState().activeTabId).toBe("public.orders");
    expect(useApp.getState().paletteOpen).toBe(false);
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("closes when a leave prompt opens", () => {
    render(<CommandPaletteHost />);
    open();
    expect(screen.getByRole("dialog")).toBeTruthy();
    act(() => useApp.setState({ leave: { reason: "quit", tabIds: [], then: vi.fn(), stage: "edits" } }));
    expect(useApp.getState().paletteOpen).toBe(false);
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("New window opens a window", async () => {
    render(<CommandPaletteHost />);
    open();
    await userEvent.type(screen.getByRole("combobox"), "new window");
    await userEvent.keyboard("{Enter}");
    expect(api.openWindow).toHaveBeenCalledTimes(1);
  });

  it("a failed New window becomes the workspace notice", async () => {
    vi.mocked(api.openWindow).mockRejectedValueOnce({ kind: "query", message: "the window system said no" });
    render(<CommandPaletteHost />);
    open();
    await userEvent.type(screen.getByRole("combobox"), "new window");
    await userEvent.keyboard("{Enter}");
    await vi.waitFor(() => expect(useApp.getState().notice).toBe("Couldn't open a new window: the window system said no"));
  });

  it("ArrowDown then Enter runs the second item", async () => {
    render(<CommandPaletteHost />);
    open();
    await userEvent.type(screen.getByRole("combobox"), "ord");
    await userEvent.keyboard("{ArrowDown}{Enter}");
    expect(useApp.getState().activeTabId).toBe("public.customer_orders");
  });

  it("Escape closes without running", async () => {
    render(<CommandPaletteHost />);
    open();
    await userEvent.type(screen.getByRole("combobox"), "ord");
    await userEvent.keyboard("{Escape}");
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(useApp.getState().tabs).toHaveLength(0);
  });

  it("hover selects a row and click runs it", async () => {
    render(<CommandPaletteHost />);
    open();
    await userEvent.type(screen.getByRole("combobox"), "ord");
    const second = screen.getAllByRole("option")[1];
    await userEvent.hover(second);
    expect(second.getAttribute("aria-selected")).toBe("true");
    await userEvent.click(second);
    expect(useApp.getState().activeTabId).toBe("public.customer_orders");
  });

  it("shows the empty state", async () => {
    render(<CommandPaletteHost />);
    open();
    await userEvent.type(screen.getByRole("combobox"), "zzz");
    expect(screen.getByText("Nothing matches “zzz”.")).toBeTruthy();
  });

  it("prints shortcuts and the placeholder", () => {
    render(<CommandPaletteHost />);
    open();
    expect(screen.getByPlaceholderText("Search tables, queries, connections and actions")).toBeTruthy();
    expect(screen.getByRole("dialog", { name: "Search everything" })).toBeTruthy();
    expect(screen.getByText("⇧⌘N")).toBeTruthy();
  });

  it("the toolbar button opens it and ⌘K closes it, even while typing in it", async () => {
    render(
      <>
        <Toolbar />
        <CommandPaletteHost />
      </>,
    );
    await userEvent.click(screen.getByRole("button", { name: /Search everything/ }));
    expect(screen.getByRole("dialog")).toBeTruthy();
    fireEvent.keyDown(screen.getByRole("combobox"), { key: "k", metaKey: true });
    expect(screen.queryByRole("dialog")).toBeNull();
    fireEvent.keyDown(window, { key: "k", metaKey: true });
    expect(screen.getByRole("dialog")).toBeTruthy();
  });

  it("⌘T, ⌘W, ⌘Y and ⌘S don't fire while the palette input has focus", () => {
    const handlers = { "mod+t": vi.fn(), "mod+w": vi.fn(), "mod+y": vi.fn(), "mod+s": vi.fn() };
    render(
      <>
        <PageShortcuts handlers={handlers} />
        <CommandPaletteHost />
      </>,
    );
    open();
    const input = screen.getByRole("combobox");
    input.focus();
    for (const key of ["t", "w", "y", "s"]) fireEvent.keyDown(input, { key, metaKey: true });
    for (const h of Object.values(handlers)) expect(h).not.toHaveBeenCalled();
    expect(screen.getByRole("dialog")).toBeTruthy();
  });

  it("⌘↵ and ⌘F registered on the page don't fire from the palette input", () => {
    // Both are allowed while typing, so only the palette keeps them from the page.
    const handlers = { "mod+enter": vi.fn(), "mod+f": vi.fn() };
    render(
      <>
        <PageShortcuts handlers={handlers} />
        <CommandPaletteHost />
      </>,
    );
    open();
    const input = screen.getByRole("combobox");
    input.focus();
    // Nothing matches, so ⌘↵ has no item to run either.
    fireEvent.change(input, { target: { value: "zzz-no-such-item" } });
    for (const key of ["Enter", "f"]) fireEvent.keyDown(input, { key, metaKey: true });
    for (const h of Object.values(handlers)) expect(h).not.toHaveBeenCalled();
    expect(screen.getByRole("dialog")).toBeTruthy();
  });

  it("⌘K still toggles the palette while the page registers mod+k", () => {
    // The palette host listens first (as in App): the palette must not swallow ⌘K as a page shortcut.
    const pageK = vi.fn();
    render(
      <>
        <CommandPaletteHost />
        <PageShortcuts handlers={{ "mod+k": pageK }} />
      </>,
    );
    open();
    fireEvent.keyDown(screen.getByRole("combobox"), { key: "k", metaKey: true });
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(pageK).not.toHaveBeenCalled();
  });

  it("editing keys still work in the palette input", () => {
    render(
      <>
        <PageShortcuts handlers={{ "mod+t": vi.fn() }} />
        <CommandPaletteHost />
      </>,
    );
    open();
    const input = screen.getByRole("combobox");
    for (const key of ["a", "c", "v", "x", "z"]) {
      const notPrevented = fireEvent.keyDown(input, { key, metaKey: true });
      expect(notPrevented).toBe(true);
    }
  });

  it("⌘K is not handled while the leave prompt is open", () => {
    render(<CommandPaletteHost />);
    useApp.setState({ leave: { reason: "quit", tabIds: [], then: () => {}, stage: "tx" } });
    fireEvent.keyDown(window, { key: "k", metaKey: true });
    expect(useApp.getState().paletteOpen).toBe(false);
  });

  it("Export table as CSV sets a one-shot export request for the active tab", async () => {
    useApp.setState({
      tabs: [{ kind: "table", id: "public.orders", table: { schema: "public", name: "orders" }, view: { mode: "data", filter: null, sort: null }, pending: { edits: {}, deletes: [], inserts: [] } } as never],
      activeTabId: "public.orders",
    });
    render(<CommandPaletteHost />);
    open();
    await userEvent.type(screen.getByRole("combobox"), "export table as csv");
    await userEvent.keyboard("{Enter}");
    expect(useApp.getState().exportRequest).toEqual({ tabId: "public.orders", format: "csv" });
  });
});
