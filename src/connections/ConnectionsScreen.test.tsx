import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ConnectionProfile, NetworkProfile, ProfileBase } from "../api/types";

vi.mock("../api/tauri", () => ({
  listConnections: vi.fn(),
  saveConnection: vi.fn(),
  deleteConnection: vi.fn(),
  testConnection: vi.fn(),
  connect: vi.fn(),
  schemaTree: vi.fn(),
}));

import * as api from "../api/tauri";
import { useApp } from "../state/store";
import { ConnectionsScreen } from "./ConnectionsScreen";
import { setHomeDir } from "./profile";

const profile = (over: Partial<ProfileBase & NetworkProfile>): ConnectionProfile => ({
  id: "c1",
  name: "Local dev",
  kind: "postgres",
  transport: "network",
  host: "localhost",
  port: 5432,
  user: "me",
  database: "app",
  defaultSchema: null,
  environment: "local",
  sslMode: "prefer",
  sslRootCert: null,
  connectTimeoutSecs: 10,
  readOnly: false,
  startupSql: null,
  ...over,
});

const saved = [
  profile({ id: "p", name: "Prod DB", environment: "production", host: "db.prod" }),
  profile({ id: "l", name: "Local dev" }),
  profile({ id: "s", name: "Stage DB", environment: "staging", host: "db.stage" }),
];

beforeEach(() => {
  vi.resetAllMocks();
  useApp.setState(useApp.getInitialState(), true);
  vi.mocked(api.listConnections).mockResolvedValue(saved);
  vi.mocked(api.saveConnection).mockImplementation(async (p) => ({ ...p, id: p.id || "new-id" }));
  vi.mocked(api.connect).mockResolvedValue({} as never);
  useApp.setState({ connections: saved });
});

// The home folder is module state in `profile.ts`: reset it even when a test fails half-way.
afterEach(() => setHomeDir(null));

describe("ConnectionsScreen", () => {
  it("groups connections by environment in order", () => {
    render(<ConnectionsScreen />);
    const headings = screen.getAllByRole("heading", { level: 3 }).map((h) => h.textContent);
    expect(headings).toEqual(["LOCAL", "STAGING", "PRODUCTION"]);
  });

  it("hides empty groups and filters by search", async () => {
    render(<ConnectionsScreen />);
    await userEvent.type(screen.getByPlaceholderText("Search connections…"), "db.stage");
    expect(screen.getAllByRole("heading", { level: 3 }).map((h) => h.textContent)).toEqual(["STAGING"]);
  });

  it("search copes with a file profile and matches it by path", async () => {
    const file: ConnectionProfile = {
      id: "f", name: "Notes", kind: "sqlite", transport: "file", filePath: "/tmp/x.sqlite",
      defaultSchema: null, environment: "local", connectTimeoutSecs: 10, readOnly: false, startupSql: null,
    };
    useApp.setState({ connections: [...saved, file] });
    render(<ConnectionsScreen />);
    await userEvent.type(screen.getByPlaceholderText("Search connections…"), "x.sqlite");
    expect(screen.getByText("Notes")).toBeTruthy();
    expect(screen.queryByText("Prod DB")).toBeNull();
  });

  it("shows ~ in a file subtitle once the home folder arrives, with no other trigger", () => {
    const file: ConnectionProfile = {
      id: "f", name: "Notes", kind: "sqlite", transport: "file", filePath: "/Users/me/x.sqlite",
      defaultSchema: null, environment: "local", connectTimeoutSecs: 10, readOnly: false, startupSql: null,
    };
    useApp.setState({ connections: [...saved, file] });
    setHomeDir(null);
    render(<ConnectionsScreen />);
    expect(screen.getByText("/Users/me/x.sqlite")).toBeTruthy();
    act(() => setHomeDir("/Users/me"));
    expect(screen.getByText("~/x.sqlite")).toBeTruthy();
  });

  it("saves a new connection as a network profile", async () => {
    render(<ConnectionsScreen />);
    await userEvent.type(screen.getByLabelText("Name"), "Fresh");
    await userEvent.click(screen.getByRole("button", { name: /^Save/ }));
    await waitFor(() => expect(api.saveConnection).toHaveBeenCalledTimes(1));
    expect(vi.mocked(api.saveConnection).mock.calls[0][0]).toMatchObject({ transport: "network", host: "localhost", port: 5432 });
  });

  it("new connection defaults", () => {
    render(<ConnectionsScreen />);
    expect(screen.getByText("New connection")).toBeTruthy();
    expect((screen.getByLabelText("Port") as HTMLInputElement).value).toBe("5432");
    expect((screen.getByLabelText("SSL mode") as HTMLSelectElement).value).toBe("prefer");
    expect((screen.getByLabelText("Timeout (seconds)") as HTMLInputElement).value).toBe("10");
    expect(screen.getByRole("radio", { name: "Local" }).getAttribute("aria-checked")).toBe("true");
    expect(screen.getByRole("radio", { name: /PostgreSQL/ }).getAttribute("aria-checked")).toBe("true");
    expect((screen.getByRole("radio", { name: /MySQL/ }) as HTMLButtonElement).disabled).toBe(false);
    expect(screen.queryByText("Delete")).toBeNull();
  });

  it("test shows latency inline", async () => {
    vi.mocked(api.testConnection).mockResolvedValue({ latencyMs: 23, serverVersion: "PostgreSQL 17" });
    render(<ConnectionsScreen />);
    await userEvent.click(screen.getByText("Test connection"));
    expect(await screen.findByText(/Test passed · 23 ms/)).toBeTruthy();
  });

  it("test failure shows message inline", async () => {
    vi.mocked(api.testConnection).mockImplementation(() =>
      Promise.reject({ kind: "connection", message: "password authentication failed" }),
    );
    render(<ConnectionsScreen />);
    await userEvent.click(screen.getByText("Test connection"));
    expect(await screen.findByText(/password authentication failed/)).toBeTruthy();
  });

  it("connect saves then connects", async () => {
    const order: string[] = [];
    vi.mocked(api.saveConnection).mockImplementation(async (p) => {
      order.push("save");
      return { ...p, id: "new-id" };
    });
    vi.mocked(api.connect).mockImplementation(async () => {
      order.push("connect");
      return {} as never;
    });
    vi.mocked(api.schemaTree).mockResolvedValue({ tables: [], views: [], functions: [] });
    render(<ConnectionsScreen />);
    await userEvent.type(screen.getByLabelText("Name"), "X");
    await userEvent.click(screen.getByText("Connect"));
    await waitFor(() => expect(order).toEqual(["save", "connect"]));
    expect(api.connect).toHaveBeenCalledWith("new-id");
  });

  it("untouched password is not sent; cleared is empty string", async () => {
    useApp.setState({ selectedConnectionId: "l" });
    render(<ConnectionsScreen />);
    expect(screen.getByText("Edit connection")).toBeTruthy();
    await userEvent.type(screen.getByLabelText("Name"), "!");
    await userEvent.click(screen.getByText("Save"));
    await waitFor(() => expect(api.saveConnection).toHaveBeenCalledTimes(1));
    expect(vi.mocked(api.saveConnection).mock.calls[0][1]).toBeUndefined();
    expect(vi.mocked(api.saveConnection).mock.calls[0][0].name).toBe("Local dev!");

    const pw = screen.getByLabelText("Password");
    await userEvent.type(pw, "x");
    await userEvent.clear(pw);
    await userEvent.click(screen.getByText("Save"));
    await waitFor(() => expect(api.saveConnection).toHaveBeenCalledTimes(2));
    expect(vi.mocked(api.saveConnection).mock.calls[1][1]).toBe("");
  });

  it("test of saved profile with untouched password sends undefined", async () => {
    useApp.setState({ selectedConnectionId: "l" });
    vi.mocked(api.testConnection).mockResolvedValue({ latencyMs: 1, serverVersion: "17" });
    render(<ConnectionsScreen />);
    await userEvent.click(screen.getByText("Test connection"));
    await waitFor(() => expect(api.testConnection).toHaveBeenCalled());
    expect(vi.mocked(api.testConnection).mock.calls[0][1]).toBeUndefined();
  });

  it("save reloads connections and selects the saved profile", async () => {
    render(<ConnectionsScreen />);
    await userEvent.type(screen.getByLabelText("Name"), "X");
    await userEvent.click(screen.getByText("Save"));
    await waitFor(() => expect(useApp.getState().selectedConnectionId).toBe("new-id"));
    expect(api.listConnections).toHaveBeenCalled();
  });

  it("delete asks inline, then deletes", async () => {
    useApp.setState({ selectedConnectionId: "l" });
    const confirm = vi.spyOn(window, "confirm");
    vi.mocked(api.deleteConnection).mockResolvedValue(undefined);
    render(<ConnectionsScreen />);
    await userEvent.click(screen.getByRole("button", { name: "Delete" }));
    expect(screen.getByText("Delete “Local dev”?")).toBeTruthy();
    expect(api.deleteConnection).not.toHaveBeenCalled();
    const confirmButton = screen.getByRole("button", { name: "Delete" });
    expect(confirmButton.getAttribute("data-danger")).toBe("true");
    await userEvent.click(confirmButton);
    await waitFor(() => expect(api.deleteConnection).toHaveBeenCalledWith("l"));
    await waitFor(() => expect(screen.getByText("New connection")).toBeTruthy());
    expect(confirm).not.toHaveBeenCalled();
    confirm.mockRestore();
  });

  it("delete can be cancelled", async () => {
    useApp.setState({ selectedConnectionId: "l" });
    render(<ConnectionsScreen />);
    await userEvent.click(screen.getByRole("button", { name: "Delete" }));
    await userEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(screen.queryByText("Delete “Local dev”?")).toBeNull();
    expect(screen.getByRole("button", { name: "Save" })).toBeTruthy();
    expect(api.deleteConnection).not.toHaveBeenCalled();
  });

  it("⌘↵ does nothing while the delete confirm shows", async () => {
    useApp.setState({ selectedConnectionId: "l" });
    render(<ConnectionsScreen />);
    await userEvent.click(screen.getByRole("button", { name: "Delete" }));
    expect(screen.getByText("Delete “Local dev”?")).toBeTruthy();
    fireEvent.keyDown(window, { key: "Enter", metaKey: true });
    await act(async () => {});
    expect(api.saveConnection).not.toHaveBeenCalled();
    expect(api.connect).not.toHaveBeenCalled();
    await userEvent.click(screen.getByRole("button", { name: "Cancel" }));
    fireEvent.keyDown(window, { key: "Enter", metaKey: true });
    await waitFor(() => expect(api.connect).toHaveBeenCalled());
  });

  it("delete failure shows inline", async () => {
    useApp.setState({ selectedConnectionId: "l" });
    vi.mocked(api.deleteConnection).mockImplementation(() =>
      Promise.reject({ kind: "secret", message: "keychain locked" }),
    );
    render(<ConnectionsScreen />);
    await userEvent.click(screen.getByRole("button", { name: "Delete" }));
    await userEvent.click(screen.getByRole("button", { name: "Delete" }));
    expect(await screen.findByText(/keychain locked/)).toBeTruthy();
    expect(screen.getByText("Edit connection")).toBeTruthy();
  });

  it("title bar is a window drag region", () => {
    render(<ConnectionsScreen />);
    expect(screen.getByText("Connections").hasAttribute("data-tauri-drag-region")).toBe(true);
  });

  it("shows load error", () => {
    useApp.setState({ loadError: "storage broke" });
    render(<ConnectionsScreen />);
    expect(screen.getByText(/storage broke/)).toBeTruthy();
  });

  it("cmd+n starts a new connection", () => {
    useApp.setState({ selectedConnectionId: "l" });
    render(<ConnectionsScreen />);
    expect(screen.getByText("Edit connection")).toBeTruthy();
    act(() => {
      fireEvent.keyDown(window, { key: "n", metaKey: true });
    });
    expect(screen.getByText("New connection")).toBeTruthy();
  });

  it("connect failure on a new profile shows inline and keeps the form", async () => {
    vi.mocked(api.connect).mockImplementation(() =>
      Promise.reject({ kind: "connection", message: "connection refused" }),
    );
    vi.mocked(api.listConnections).mockResolvedValue([...saved, profile({ id: "new-id", name: "Fresh" })]);
    render(<ConnectionsScreen />);
    await userEvent.type(screen.getByLabelText("Name"), "Fresh");
    await userEvent.click(screen.getByText("Connect"));
    expect(await screen.findByText(/connection refused/)).toBeTruthy();
    expect((screen.getByLabelText("Name") as HTMLInputElement).value).toBe("Fresh");
  });

  it("blocks actions when port is cleared", async () => {
    render(<ConnectionsScreen />);
    await userEvent.clear(screen.getByLabelText("Port"));
    expect(screen.getByText(/Port must be a whole number from 1 to 65535/)).toBeTruthy();
    expect((screen.getByText("Save").closest("button") as HTMLButtonElement).disabled).toBe(true);
  });

  it.each(["0", "65536", "54.5"])("rejects port %s", async (port) => {
    render(<ConnectionsScreen />);
    const input = screen.getByLabelText("Port");
    await userEvent.clear(input);
    await userEvent.type(input, port);
    expect(screen.getByText(/Port must be a whole number from 1 to 65535/)).toBeTruthy();
    expect((screen.getByText("Connect").closest("button") as HTMLButtonElement).disabled).toBe(true);
  });

  it("accepts port 65535", async () => {
    render(<ConnectionsScreen />);
    const input = screen.getByLabelText("Port");
    await userEvent.clear(input);
    await userEvent.type(input, "65535");
    expect(screen.queryByText(/Port must be/)).toBeNull();
    expect((screen.getByText("Save").closest("button") as HTMLButtonElement).disabled).toBe(false);
  });

  it.each(["0", "2.5"])("rejects timeout %s", async (timeout) => {
    render(<ConnectionsScreen />);
    const input = screen.getByLabelText("Timeout (seconds)");
    await userEvent.clear(input);
    await userEvent.type(input, timeout);
    expect(screen.getByText(/Timeout must be a whole number of seconds, at least 1/)).toBeTruthy();
    expect((screen.getByText("Save").closest("button") as HTMLButtonElement).disabled).toBe(true);
  });
});
