import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

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

function mount() {
  render(<ConnectionForm initial={newProfile()} onSaved={vi.fn(async () => {})} onDeleted={vi.fn(async () => {})} />);
}
const pickMysql = () => userEvent.click(screen.getByRole("radio", { name: /MySQL/ }));
const port = () => screen.getByLabelText("Port") as HTMLInputElement;

beforeEach(() => {
  vi.resetAllMocks();
  useApp.setState(useApp.getInitialState(), true);
  vi.mocked(api.saveConnection).mockImplementation(async (p) => ({ ...p, id: p.id || "new-id" }));
});

describe("ConnectionForm with MySQL", () => {
  it("the MySQL / MariaDB card is enabled and shows Server", () => {
    mount();
    const card = screen.getByRole("radio", { name: "MySQL / MariaDB Server" }) as HTMLButtonElement;
    expect(card.disabled).toBe(false);
    expect(card.textContent).toContain("MY");
  });

  it("picking MySQL shows the server fields with Database (optional), port 3306, and no Schema", async () => {
    mount();
    await pickMysql();
    for (const label of ["Host", "Port", "User", "Password", "Database (optional)"]) expect(screen.getByLabelText(label)).toBeTruthy();
    expect(port().value).toBe("3306");
    expect(screen.queryByLabelText("Schema (optional)")).toBeNull();
    expect(screen.queryByLabelText("Database")).toBeNull();
    expect(screen.getByText(/SSL \/ TLS/)).toBeTruthy();
    expect(screen.getByLabelText("CA file path")).toBeTruthy();
    expect(screen.getByLabelText("Timeout (seconds)")).toBeTruthy();
    expect(screen.getByLabelText("Read-only")).toBeTruthy();
    expect(screen.getByLabelText("Startup SQL")).toBeTruthy();
  });

  it("Save sends kind mysql, transport network and port 3306", async () => {
    mount();
    await pickMysql();
    await userEvent.type(screen.getByLabelText("Name"), "My");
    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(api.saveConnection).toHaveBeenCalledTimes(1);
    expect(vi.mocked(api.saveConnection).mock.calls[0][0]).toMatchObject({ kind: "mysql", transport: "network", port: 3306 });
  });

  it("switching back to PostgreSQL gives 5432 and the Schema field again", async () => {
    mount();
    await pickMysql();
    await userEvent.click(screen.getByRole("radio", { name: /PostgreSQL/ }));
    expect(port().value).toBe("5432");
    expect(screen.getByLabelText("Schema (optional)")).toBeTruthy();
    expect(screen.getByLabelText("Database")).toBeTruthy();
  });
});
