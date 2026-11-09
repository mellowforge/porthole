import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../api/tauri", () => ({
  listSavedQueries: vi.fn(),
  saveQuery: vi.fn(),
  deleteSavedQuery: vi.fn(),
  applyChanges: vi.fn(),
  saveTabState: vi.fn(),
}));

import * as api from "../api/tauri";
import { type TableTab, useApp } from "../state/store";
import { emptyPending, setCell } from "../table/pending";
import { LeaveGuard } from "./LeaveGuard";
import { Sidebar } from "./Sidebar";

const dirty: TableTab = {
  kind: "table",
  id: "public.users",
  table: { schema: "public", name: "users" },
  view: { mode: "data", filter: null, sort: null },
  pending: setCell(emptyPending, ["1"], "name", "new", "old"),
};

beforeEach(() => {
  vi.resetAllMocks();
  vi.mocked(api.saveTabState).mockResolvedValue(undefined);
  vi.mocked(api.applyChanges).mockResolvedValue({ applied: 1 });
  useApp.setState(useApp.getInitialState(), true);
  useApp.setState({
    screen: "workspace",
    active: { profile: { id: "c1", name: "Local Postgres", database: "app_dev", kind: "postgres", transport: "network", host: "localhost" } as never, serverVersion: "17", schemas: ["public"], schema: "public" },
    tree: { tables: ["users"], views: [], functions: [] },
    tabs: [dirty],
    activeTabId: dirty.id,
  });
});

describe("Sidebar with unsaved grid edits", () => {
  it.each([
    ["Edit", "edit"],
    ["Switch connection ▾", "switch"],
  ])("%s asks first; Cancel stays in the workspace", async (name, reason) => {
    render(
      <>
        <Sidebar />
        <LeaveGuard />
      </>,
    );
    await userEvent.click(screen.getByRole("button", { name }));
    const sheet = screen.getByRole("alertdialog");
    expect(sheet.textContent).toContain("“users” has 1 unsaved change.");
    expect(useApp.getState().leave?.reason).toBe(reason);
    await userEvent.click(within(sheet).getByRole("button", { name: "Cancel" }));
    expect(useApp.getState().screen).toBe("workspace");
    expect(useApp.getState().tabs).toHaveLength(1);
  });

  it("Save in the prompt saves, then opens the connections screen", async () => {
    render(
      <>
        <Sidebar />
        <LeaveGuard />
      </>,
    );
    await userEvent.click(screen.getByRole("button", { name: "Switch connection ▾" }));
    await userEvent.click(within(screen.getByRole("alertdialog")).getByRole("button", { name: "Save" }));
    await waitFor(() => expect(useApp.getState().screen).toBe("connections"));
    expect(api.applyChanges).toHaveBeenCalledTimes(1);
  });
});
