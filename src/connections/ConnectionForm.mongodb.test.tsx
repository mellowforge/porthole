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
import type { ConnectionProfile } from "../api/types";
import { useApp } from "../state/store";
import { ConnectionForm, newProfile } from "./ConnectionForm";

const READ_ONLY_NOTE = "MongoDB has no read-only sessions: the app refuses saves. For a hard block, connect as a user with the read role.";

function mount(initial: ConnectionProfile = newProfile()) {
  render(<ConnectionForm initial={initial} onSaved={vi.fn(async () => {})} onDeleted={vi.fn(async () => {})} />);
}
const pickMongo = () => userEvent.click(screen.getByRole("radio", { name: "MongoDB Server" }));
const port = () => screen.getByLabelText("Port") as HTMLInputElement;
const sent = () => vi.mocked(api.saveConnection).mock.calls[0][0];
const save = async () => {
  await userEvent.type(screen.getByLabelText("Name"), "Mongo");
  await userEvent.click(screen.getByRole("button", { name: "Save" }));
};

beforeEach(() => {
  vi.resetAllMocks();
  useApp.setState(useApp.getInitialState(), true);
  vi.mocked(api.saveConnection).mockImplementation(async (p) => ({ ...p, id: p.id || "new-id" }));
});

describe("ConnectionForm with MongoDB", () => {
  it("the MongoDB card is enabled, shows MG and Server; SQL Server stays Later", () => {
    mount();
    const card = screen.getByRole("radio", { name: "MongoDB Server" }) as HTMLButtonElement;
    expect(card.disabled).toBe(false);
    expect(card.textContent).toContain("MG");
    expect((screen.getByRole("radio", { name: "SQL Server Later" }) as HTMLButtonElement).disabled).toBe(true);
  });

  it("picking MongoDB shows the server fields with port 27017, the Auth database, and no Schema or Startup SQL", async () => {
    mount();
    await pickMongo();
    for (const label of ["Host", "Port", "User", "Password", "Database (optional)", "Auth database (optional)"]) {
      expect(screen.getByLabelText(label)).toBeTruthy();
    }
    expect(port().value).toBe("27017");
    expect((screen.getByLabelText("Auth database (optional)") as HTMLInputElement).placeholder).toBe("admin");
    expect(screen.queryByLabelText("Schema (optional)")).toBeNull();
    expect(screen.queryByLabelText("Startup SQL")).toBeNull();
    expect(screen.queryByText(/startup SQL/i)).toBeNull();
    expect(screen.getByLabelText("Timeout (seconds)")).toBeTruthy();
    expect(screen.getByLabelText("Read-only")).toBeTruthy();
  });

  it("offers disable (the default), require, verify-ca and verify-full, but not prefer", async () => {
    mount();
    await pickMongo();
    const select = screen.getByLabelText("SSL mode") as HTMLSelectElement;
    expect([...select.options].map((o) => o.value)).toEqual(["disable", "require", "verify-ca", "verify-full"]);
    expect(select.value).toBe("disable");
  });

  it("shows the read-only note under Read-only", async () => {
    mount();
    await pickMongo();
    expect(screen.getByText(READ_ONLY_NOTE)).toBeTruthy();
  });

  it("Save sends kind mongodb, port 27017, sslMode disable and the Auth database as authSource", async () => {
    mount();
    await pickMongo();
    await userEvent.type(screen.getByLabelText("Auth database (optional)"), "admin");
    await save();
    expect(api.saveConnection).toHaveBeenCalledTimes(1);
    expect(sent()).toMatchObject({ kind: "mongodb", transport: "network", port: 27017, sslMode: "disable", authSource: "admin", startupSql: null });
  });

  it("an empty Auth database is left out of the profile", async () => {
    mount();
    await pickMongo();
    await userEvent.type(screen.getByLabelText("Auth database (optional)"), "x");
    await userEvent.clear(screen.getByLabelText("Auth database (optional)"));
    await save();
    expect(Object.hasOwn(sent(), "authSource")).toBe(false);
  });

  it("startup SQL typed for another type is not carried into a MongoDB profile", async () => {
    mount();
    await userEvent.click(screen.getByText(/Advanced/));
    await userEvent.type(screen.getByLabelText("Startup SQL"), "set x");
    await pickMongo();
    await save();
    expect(sent().startupSql).toBeNull();
  });

  it("switching back to PostgreSQL gives 5432, prefer, the Schema field, and no authSource", async () => {
    mount();
    await pickMongo();
    await userEvent.type(screen.getByLabelText("Auth database (optional)"), "admin");
    await userEvent.click(screen.getByRole("radio", { name: /PostgreSQL/ }));
    expect(port().value).toBe("5432");
    expect(screen.getByLabelText("Schema (optional)")).toBeTruthy();
    expect(screen.queryByLabelText("Auth database (optional)")).toBeNull();
    await save();
    expect(sent()).toMatchObject({ kind: "postgres", sslMode: "prefer" });
    expect(Object.hasOwn(sent(), "authSource")).toBe(false);
  });

  it("a saved MongoDB profile shows its Auth database", () => {
    mount({ ...newProfile(), id: "m1", name: "Mongo", kind: "mongodb", port: 27017, sslMode: "disable", authSource: "admin" } as ConnectionProfile);
    expect((screen.getByLabelText("Auth database (optional)") as HTMLInputElement).value).toBe("admin");
  });

  describe("the statement time limit", () => {
    const LIMIT = "Statement time limit (seconds)";
    const LIMIT_ERROR = "The statement time limit must be a whole number of seconds.";
    const limit = () => screen.getByLabelText(LIMIT) as HTMLInputElement;

    it("is under Advanced, with the placeholder None, and the Advanced summary keeps its text", async () => {
      mount();
      await pickMongo();
      expect(limit().placeholder).toBe("None");
      expect(limit().value).toBe("");
      expect(limit().closest("details")?.querySelector("summary")?.textContent).toBe("Advanced: timeout, read-only mode");
    });

    it("30 saves statementTimeoutSecs 30", async () => {
      mount();
      await pickMongo();
      await userEvent.type(limit(), "30");
      await save();
      expect(api.saveConnection).toHaveBeenCalledTimes(1);
      expect(sent()).toMatchObject({ kind: "mongodb", statementTimeoutSecs: 30 });
    });

    it("an empty limit is left out of the profile", async () => {
      mount();
      await pickMongo();
      await userEvent.type(limit(), "30");
      await userEvent.clear(limit());
      await save();
      expect(api.saveConnection).toHaveBeenCalledTimes(1);
      expect(Object.hasOwn(sent(), "statementTimeoutSecs")).toBe(false);
    });

    it.each(["0", "-1", "1.5"])("%s shows the whole-seconds error and doesn't save", async (typed) => {
      mount();
      await pickMongo();
      await userEvent.type(limit(), typed);
      expect(screen.getByText(new RegExp(LIMIT_ERROR.replace(/\./g, "\\.")))).toBeTruthy();
      await save();
      expect(api.saveConnection).not.toHaveBeenCalled();
    });

    it("a saved MongoDB profile shows its limit", () => {
      mount({ ...newProfile(), id: "m1", name: "Mongo", kind: "mongodb", port: 27017, sslMode: "disable", statementTimeoutSecs: 45 } as ConnectionProfile);
      expect(limit().value).toBe("45");
    });

    it("a PostgreSQL form has no limit field, and switching from MongoDB drops the limit", async () => {
      mount();
      expect(screen.queryByLabelText(LIMIT)).toBeNull();
      await pickMongo();
      await userEvent.type(limit(), "30");
      await userEvent.click(screen.getByRole("radio", { name: /PostgreSQL/ }));
      expect(screen.queryByLabelText(LIMIT)).toBeNull();
      await save();
      expect(sent()).toMatchObject({ kind: "postgres" });
      expect(Object.hasOwn(sent(), "statementTimeoutSecs")).toBe(false);
    });
  });
});
