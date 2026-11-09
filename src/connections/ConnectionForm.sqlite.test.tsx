import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ConnectionProfile } from "../api/types";

vi.mock("../api/tauri", () => ({
  saveConnection: vi.fn(),
  deleteConnection: vi.fn(),
  testConnection: vi.fn(),
  connect: vi.fn(),
  pickDatabaseFile: vi.fn(),
}));

import * as api from "../api/tauri";
import { useApp } from "../state/store";
import { ConnectionForm, newProfile } from "./ConnectionForm";

const sqlite: ConnectionProfile = {
  id: "s1",
  name: "Local SQLite",
  kind: "sqlite",
  transport: "file",
  filePath: "/Users/me/data/app.sqlite",
  defaultSchema: null,
  environment: "local",
  connectTimeoutSecs: 10,
  readOnly: false,
  startupSql: null,
};

function mount(initial: ConnectionProfile = newProfile()) {
  const onSaved = vi.fn(async () => {});
  render(<ConnectionForm initial={initial} onSaved={onSaved} onDeleted={vi.fn(async () => {})} />);
  return { onSaved };
}

const pickSqlite = () => userEvent.click(screen.getByRole("radio", { name: /SQLite/ }));
const fileInput = () => screen.getByLabelText("Database file") as HTMLInputElement;

beforeEach(() => {
  vi.resetAllMocks();
  useApp.setState(useApp.getInitialState(), true);
  vi.mocked(api.saveConnection).mockImplementation(async (p) => ({ ...p, id: p.id || "new-id" }));
  vi.mocked(api.testConnection).mockResolvedValue({ latencyMs: 4, serverVersion: "SQLite 3.50.4" });
});

describe("ConnectionForm with SQLite", () => {
  it("the SQLite card is enabled and shows File", () => {
    mount();
    const card = screen.getByRole("radio", { name: "SQLite File" });
    expect((card as HTMLButtonElement).disabled).toBe(false);
    expect(card.getAttribute("aria-checked")).toBe("false");
    expect(card.textContent).toContain("SQ");
  });

  it("picking SQLite shows Database file and Choose…, and hides the server fields", async () => {
    mount();
    await pickSqlite();
    expect(screen.getByRole("radio", { name: /SQLite/ }).getAttribute("aria-checked")).toBe("true");
    expect(screen.getByRole("radio", { name: /PostgreSQL/ }).getAttribute("aria-checked")).toBe("false");
    expect(fileInput().placeholder).toBe("/path/to/database.sqlite");
    expect(screen.getByRole("button", { name: "Choose…" })).toBeTruthy();
    for (const label of ["Host", "Port", "User", "Password", "Database", "Schema (optional)", "Timeout (seconds)"]) {
      expect(screen.queryByLabelText(label)).toBeNull();
    }
    expect(screen.queryByText(/SSL \/ TLS/)).toBeNull();
    expect(screen.queryByText(/SSH tunnel/)).toBeNull();
    // Name, Environment and the remaining Advanced options stay.
    expect(screen.getByLabelText("Name")).toBeTruthy();
    expect(screen.getByRole("radiogroup", { name: "Environment" })).toBeTruthy();
    expect(screen.getByLabelText("Read-only")).toBeTruthy();
    expect(screen.getByLabelText("Startup SQL")).toBeTruthy();
  });

  it("Choose… fills the path from pickDatabaseFile; a dismissed dialog keeps it", async () => {
    vi.mocked(api.pickDatabaseFile).mockResolvedValueOnce("/Users/me/data/picked.sqlite").mockResolvedValueOnce(null);
    mount();
    await pickSqlite();
    await userEvent.click(screen.getByRole("button", { name: "Choose…" }));
    await waitFor(() => expect(fileInput().value).toBe("/Users/me/data/picked.sqlite"));
    await userEvent.click(screen.getByRole("button", { name: "Choose…" }));
    await waitFor(() => expect(api.pickDatabaseFile).toHaveBeenCalledTimes(2));
    expect(fileInput().value).toBe("/Users/me/data/picked.sqlite");
  });

  it("Test, Save and Connect with an empty path show Choose a database file. and call nothing", async () => {
    mount();
    await pickSqlite();
    // Not shown before the user asks for something.
    expect(screen.queryByText(/Choose a database file\./)).toBeNull();
    for (const name of ["Test connection", "Save", /Connect/]) {
      await userEvent.click(screen.getByRole("button", { name }));
      expect(screen.getByText(/Choose a database file\./)).toBeTruthy();
    }
    expect(api.saveConnection).not.toHaveBeenCalled();
    expect(api.testConnection).not.toHaveBeenCalled();
    expect(api.connect).not.toHaveBeenCalled();
  });

  it("Save with a path sends a file profile", async () => {
    const { onSaved } = mount();
    await pickSqlite();
    await userEvent.type(screen.getByLabelText("Name"), "Notes");
    await userEvent.type(fileInput(), "/tmp/notes.sqlite");
    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(onSaved).toHaveBeenCalled());
    const [sent, password] = vi.mocked(api.saveConnection).mock.calls[0];
    expect(sent).toMatchObject({ kind: "sqlite", transport: "file", filePath: "/tmp/notes.sqlite", name: "Notes" });
    // No server fields travel with a file profile, and no password.
    for (const key of ["host", "port", "user", "database", "sslMode", "sslRootCert"]) expect(sent).not.toHaveProperty(key);
    expect(password).toBeUndefined();
  });

  it("Test connection on a file profile reports the latency", async () => {
    mount(sqlite);
    await userEvent.click(screen.getByRole("button", { name: "Test connection" }));
    expect(await screen.findByText("✓ Test passed · 4 ms")).toBeTruthy();
    expect(vi.mocked(api.testConnection).mock.calls[0]).toEqual([sqlite, undefined]);
  });

  it("a saved SQLite profile opens in the form with its path, and can be deleted", async () => {
    mount(sqlite);
    expect(screen.getByText("Edit connection")).toBeTruthy();
    expect(fileInput().value).toBe("/Users/me/data/app.sqlite");
    expect(screen.getByRole("radio", { name: /SQLite/ }).getAttribute("aria-checked")).toBe("true");
    expect(screen.getByRole("button", { name: "Delete" })).toBeTruthy();
    expect(screen.queryByText(/can't be edited yet/)).toBeNull();
  });

  it("picking PostgreSQL again gives today's network defaults; picking the current card changes nothing", async () => {
    mount();
    await userEvent.type(screen.getByLabelText("Host"), "x");
    await userEvent.click(screen.getByRole("radio", { name: /PostgreSQL/ }));
    expect((screen.getByLabelText("Host") as HTMLInputElement).value).toBe("localhostx");
    await pickSqlite();
    await userEvent.click(screen.getByRole("radio", { name: /PostgreSQL/ }));
    expect((screen.getByLabelText("Host") as HTMLInputElement).value).toBe("localhost");
    expect((screen.getByLabelText("Port") as HTMLInputElement).value).toBe("5432");
    expect(screen.queryByLabelText("Database file")).toBeNull();
    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(api.saveConnection).toHaveBeenCalled());
    const sent = vi.mocked(api.saveConnection).mock.calls[0][0];
    expect(sent).toEqual({ ...newProfile() });
    expect(sent).not.toHaveProperty("filePath");
  });

  it("a saved connection keeps its type: every type card is disabled", async () => {
    for (const saved of [sqlite, { ...newProfile(), id: "p1", name: "Local Postgres" }]) {
      const { unmount } = render(<ConnectionForm initial={saved} onSaved={vi.fn(async () => {})} onDeleted={vi.fn(async () => {})} />);
      for (const card of screen.getAllByRole("radio", { name: /PostgreSQL|MySQL|SQLite|SQL Server/ })) {
        expect((card as HTMLButtonElement).disabled).toBe(true);
      }
      expect(screen.getByRole("radio", { name: saved.kind === "sqlite" ? /SQLite/ : /PostgreSQL/ }).getAttribute("aria-checked")).toBe("true");
      unmount();
    }
    // A new connection picks its type.
    mount();
    expect((screen.getByRole("radio", { name: /PostgreSQL/ }) as HTMLButtonElement).disabled).toBe(false);
    expect((screen.getByRole("radio", { name: /SQLite/ }) as HTMLButtonElement).disabled).toBe(false);
  });

  it("switching type on a new form clears the status line", async () => {
    mount();
    await pickSqlite();
    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(screen.getByText(/Choose a database file\./)).toBeTruthy();
    await userEvent.click(screen.getByRole("radio", { name: /PostgreSQL/ }));
    expect(screen.queryByText(/Choose a database file\./)).toBeNull();
  });
});
