import { act, render, screen, fireEvent, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ConnectionProfile } from "../api/types";

vi.mock("../api/tauri", () => ({
  listConnections: vi.fn(),
  connect: vi.fn(),
  schemaTree: vi.fn(),
  fetchPage: vi.fn(),
  tableStructure: vi.fn(),
}));

vi.mock("../table/DataGrid", () => ({
  DataGrid: (p: { rows: unknown[] }) => <div data-testid="grid">{`${p.rows.length} rows in grid`}</div>,
}));

vi.mock("../state/persist", async (orig) => ({ ...(await orig<typeof import("../state/persist")>()), discardStoredTabs: vi.fn() }));

import * as api from "../api/tauri";
import { discardStoredTabs } from "../state/persist";
import { useApp } from "../state/store";
import { Workspace } from "./Workspace";

const profile: ConnectionProfile = {
  id: "c1",
  name: "Prod DB",
  kind: "postgres",
  transport: "network",
  host: "db.prod",
  port: 5432,
  user: "me",
  database: "app",
  defaultSchema: null,
  environment: "production",
  sslMode: "prefer",
  sslRootCert: null,
  connectTimeoutSecs: 10,
  readOnly: false,
  startupSql: null,
};

beforeEach(() => {
  vi.resetAllMocks();
  vi.mocked(discardStoredTabs).mockClear();
  useApp.setState({
    screen: "workspace",
    selectedConnectionId: null,
    active: { profile, serverVersion: "17", schemas: ["public", "audit"], schema: "public" },
    tree: {
      tables: ["users", "orders", "order_items"],
      views: ["recent_orders"],
      functions: [
        { name: "fn_total", signature: "integer" },
        { name: "fn_total", signature: "numeric" },
        { name: "fn_other", signature: "integer" },
      ],
    },
    schemaError: null,
    tabs: [],
    activeTabId: null,
  });
});
afterEach(() => {
  vi.mocked(discardStoredTabs).mockClear();
  useApp.setState({ tabsNotice: null, tabsSaveBlocked: false });
});

describe("Workspace", () => {
  it("env strip uses production color", () => {
    render(<Workspace />);
    expect(screen.getByTestId("env-strip").style.background).toBe("var(--env-production)");
  });

  it("sidebar search filters tables", async () => {
    render(<Workspace />);
    await userEvent.type(screen.getByPlaceholderText("Search tables, views…"), "ord");
    expect(screen.getByText("orders")).not.toBeNull();
    expect(screen.getByText("order_items")).not.toBeNull();
    expect(screen.queryByText("users")).toBeNull();
  });

  it("hides sections that are empty after filtering", async () => {
    render(<Workspace />);
    await userEvent.type(screen.getByPlaceholderText("Search tables, views…"), "ORDERS");
    expect(screen.queryByText(/^FUNCTIONS/)).toBeNull();
    expect(screen.getByText(/^VIEWS/)).not.toBeNull();
  });

  it("clicking a table opens a tab, and cmd+w closes it", async () => {
    render(<Workspace />);
    expect(screen.getByText("Double-click or click a table to open it.")).not.toBeNull();
    await userEvent.click(screen.getByRole("button", { name: "orders" }));
    expect(useApp.getState().tabs).toHaveLength(1);
    expect(screen.queryByText("Double-click or click a table to open it.")).toBeNull();
    act(() => {
      fireEvent.keyDown(window, { key: "w", metaKey: true });
    });
    expect(useApp.getState().tabs).toHaveLength(0);
  });

  it("the empty workspace on MongoDB names collections", () => {
    useApp.setState({ active: { profile: { ...profile, kind: "mongodb", port: 27017 }, serverVersion: "MongoDB 8.0.4", schemas: ["fx"], schema: "fx" } });
    render(<Workspace />);
    expect(screen.getByText("Double-click or click a collection to open it.")).not.toBeNull();
    expect(screen.queryByText("Double-click or click a table to open it.")).toBeNull();
  });

  it("clicking a tab activates it", async () => {
    render(<Workspace />);
    await userEvent.click(screen.getByRole("button", { name: "orders" }));
    await userEvent.click(screen.getByRole("button", { name: "users" }));
    expect(useApp.getState().activeTabId).toBe("public.users");
    await userEvent.click(screen.getByRole("tab", { name: /orders/ }));
    expect(useApp.getState().activeTabId).toBe("public.orders");
    expect(screen.getByRole("tab", { name: /orders/ }).getAttribute("aria-selected")).toBe("true");
    useApp.getState().setActiveTab("public.users");
    screen.getByRole("tab", { name: /orders/ }).focus();
    await userEvent.keyboard("{Enter}");
    expect(useApp.getState().activeTabId).toBe("public.orders");
    expect(useApp.getState().tabs).toHaveLength(2);
  });

  it("edit button opens connections screen with the active connection selected", async () => {
    render(<Workspace />);
    await userEvent.click(screen.getByRole("button", { name: "Edit" }));
    expect(useApp.getState().screen).toBe("connections");
    expect(useApp.getState().selectedConnectionId).toBe("c1");
  });

  it("lists overloaded functions with unique keys", () => {
    const consoleError = vi.spyOn(console, "error").mockImplementation(() => {});
    render(<Workspace />);
    expect(screen.getAllByText("fn_total")).toHaveLength(2);
    expect(screen.getByText("fn_other")).not.toBeNull();
    expect(consoleError).not.toHaveBeenCalled();
    consoleError.mockRestore();
  });

  it("schema switch failure shows inline in the sidebar", async () => {
    vi.mocked(api.schemaTree).mockImplementation(() =>
      Promise.reject({ kind: "query", message: "permission denied for schema audit" }),
    );
    render(<Workspace />);
    await userEvent.selectOptions(screen.getByLabelText("Schema"), "audit");
    expect((await screen.findByRole("alert")).textContent).toBe("permission denied for schema audit");
    expect((screen.getByLabelText("Schema") as HTMLSelectElement).value).toBe("public");
    expect(screen.getByText("users")).not.toBeNull();
  });

  it("switching tabs keeps table filter without refetch", async () => {
    const page = {
      columns: [{ name: "id", dataType: "int4", fullType: "integer", castType: "integer", readOnly: false, nullable: false, default: null, isPrimaryKey: true }],
      primaryKey: ["id"],
      editing: { kind: "allowed" },
      rows: [["1"]],
      total: { kind: "exact", value: 1 },
      matches: null,
      elapsedMs: 1,
    };
    vi.mocked(api.fetchPage).mockResolvedValue(page as never);
    render(<Workspace />);
    await userEvent.click(screen.getByRole("button", { name: "orders" }));
    await screen.findByTestId("grid");
    await userEvent.type(screen.getByPlaceholderText(/^id = 1 and/), "id = 1{Enter}");
    await waitFor(() => expect(useApp.getState().tabs[0]).toMatchObject({ view: { filter: "id = 1" } }));
    await userEvent.click(screen.getByRole("button", { name: "users" }));
    await userEvent.click(screen.getByRole("tab", { name: /orders/ }));
    const calls = vi.mocked(api.fetchPage).mock.calls.map((c) => c[0]);
    expect(calls.filter((r) => r.table.name === "orders" && r.filter === "id = 1")).toHaveLength(1);
    expect(calls.filter((r) => r.table.name === "orders")).toHaveLength(2);
    expect((screen.getAllByPlaceholderText(/^id = 1 and/)[0] as HTMLInputElement).value).toBe("id = 1");
  });

  it("new query is enabled", () => {
    render(<Workspace />);
    expect((screen.getByRole("button", { name: /^\+ New query/ }) as HTMLButtonElement).disabled).toBe(false);
  });

  it("shows the tab-restore notice until dismissed", () => {
    useApp.setState({ tabsNotice: "Couldn't restore your open tabs. They won't be saved until you reconnect." });
    render(<Workspace />);
    const alert = screen.getByRole("alert");
    expect(alert.textContent).toContain("Couldn't restore your open tabs. They won't be saved until you reconnect.");
    fireEvent.click(screen.getByRole("button", { name: "Dismiss" }));
    expect(screen.queryByRole("alert")).toBeNull();
    expect(useApp.getState().tabsNotice).toBeNull();
  });

  it("the not-restored notice offers Discard saved tabs", async () => {
    useApp.setState({ tabsNotice: "Couldn't restore your open tabs. They won't be saved until you reconnect.", tabsSaveBlocked: true });
    const { unmount } = render(<Workspace />);
    await userEvent.click(screen.getByRole("button", { name: "Discard saved tabs" }));
    expect(discardStoredTabs).toHaveBeenCalledTimes(1);
    unmount();
    useApp.setState({ tabsNotice: "Couldn't discard the saved tabs: x", tabsSaveBlocked: false });
    render(<Workspace />);
    expect(screen.queryByRole("button", { name: "Discard saved tabs" })).toBeNull();
  });

  it("shows the window notice until dismissed", () => {
    useApp.setState({ notice: "Couldn't open a new window: the window system said no" });
    render(<Workspace />);
    expect(screen.getByRole("alert").textContent).toContain("Couldn't open a new window: the window system said no");
    fireEvent.click(screen.getByRole("button", { name: "Dismiss" }));
    expect(screen.queryByRole("alert")).toBeNull();
    expect(useApp.getState().notice).toBeNull();
  });
});
