import { act, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ColumnInfo, ConnectedInfo, ExportOutcome, ExportProgress, TablePage } from "../api/types";
import { emptyPending } from "./pending";

vi.mock("../api/tauri", () => ({
  fetchPage: vi.fn(),
  tableStructure: vi.fn(),
  exportTable: vi.fn(),
  cancelExport: vi.fn().mockResolvedValue(undefined),
  revealExport: vi.fn().mockResolvedValue(undefined),
}));
vi.mock("./DataGrid", () => ({ DataGrid: () => <div data-testid="grid" /> }));

import * as api from "../api/tauri";
import { type TableTab, useApp } from "../state/store";
import { TableView } from "./TableView";

const col = (name: string): ColumnInfo => ({
  name, dataType: "text", fullType: "text", castType: "text", nullable: true, default: null,
  isPrimaryKey: name === "id", readOnly: false,
});
const page: TablePage = {
  columns: [col("id"), col("status")],
  primaryKey: ["id"],
  editing: { kind: "allowed" },
  rows: [["1", "active"]],
  total: { kind: "exact", value: 1 },
  matches: null,
  elapsedMs: 3,
};
const table = { schema: "public", name: "users" };
const sort = { column: "id", descending: false };
const tab: TableTab = {
  kind: "table", id: "public.users", table,
  view: { mode: "data", filter: "status = 'active'", sort },
  pending: emptyPending,
};
const exportTable = vi.mocked(api.exportTable);

function Live() {
  const t = useApp((s) => s.tabs.find((x) => x.id === tab.id)) as TableTab;
  return <TableView tab={t} />;
}

beforeEach(() => {
  vi.clearAllMocks();
  vi.mocked(api.fetchPage).mockResolvedValue(page);
  useApp.setState({
    tabs: [tab], activeTabId: tab.id,
    active: { profile: { name: "L", readOnly: false, environment: "local" }, schema: "public" } as unknown as ConnectedInfo,
  });
});

async function openMenu() {
  await screen.findByTestId("grid");
  await userEvent.click(screen.getByRole("button", { name: "Export" }));
}

describe("table export", () => {
  it("exports with the applied filter and sort, shows progress, done and Show in Finder", async () => {
    let progress!: (p: ExportProgress) => void;
    let finish!: (o: ExportOutcome) => void;
    exportTable.mockImplementation((_id, _q, _f, onProgress) => {
      progress = onProgress;
      return new Promise((r) => (finish = r));
    });
    render(<Live />);
    await screen.findByText("+ Add row");
    expect(screen.getByRole("button", { name: "Export" }).parentElement?.previousElementSibling?.textContent).toContain("+ Add row");
    await openMenu();
    expect(screen.queryByText("Unsaved changes are not included.")).toBeNull();
    await userEvent.click(screen.getByRole("menuitem", { name: "Export as CSV" }));
    expect(exportTable).toHaveBeenCalledWith(expect.any(String), { table, filter: "status = 'active'", sort }, "csv", expect.any(Function));
    expect(screen.getByRole("button", { name: "Export" }).hasAttribute("disabled")).toBe(true);
    act(() => progress({ rows: 1000 }));
    expect(screen.getByText("Exporting… 1,000 rows")).toBeTruthy();
    await act(async () => finish({ kind: "done", rows: 17, path: "/tmp/users.csv", fileName: "users.csv" }));
    expect(screen.getByText("Exported 17 rows to users.csv")).toBeTruthy();
    await userEvent.click(screen.getByRole("button", { name: "Show in Finder" }));
    expect(api.revealExport).toHaveBeenCalledWith("/tmp/users.csv");
  });

  it("notes unsaved changes", async () => {
    render(<Live />);
    await screen.findByTestId("grid");
    act(() => useApp.getState().editCell(tab.id, ["1"], "status", "x", "active"));
    await userEvent.click(screen.getByRole("button", { name: "Export" }));
    expect(screen.getByText("Unsaved changes are not included.")).toBeTruthy();
  });

  it("Cancel calls cancelExport with the running id", async () => {
    exportTable.mockReturnValue(new Promise(() => {}));
    render(<Live />);
    await openMenu();
    await userEvent.click(screen.getByRole("menuitem", { name: "Export as JSON" }));
    await userEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(api.cancelExport).toHaveBeenCalledWith(exportTable.mock.calls[0][0]);
  });

  it("shows a failure and nothing for a dismissed dialog", async () => {
    exportTable.mockRejectedValueOnce({ kind: "io", message: "disk full" });
    render(<Live />);
    await openMenu();
    await userEvent.click(screen.getByRole("menuitem", { name: "Export as CSV" }));
    expect((await screen.findByRole("alert")).textContent).toBe("Export failed: disk full");
    exportTable.mockResolvedValueOnce({ kind: "dismissed" });
    await userEvent.click(screen.getByRole("button", { name: "Export" }));
    await userEvent.click(screen.getByRole("menuitem", { name: "Export as CSV" }));
    expect(screen.queryByRole("alert")).toBeNull();
    expect(screen.queryByRole("status")).toBeNull();
  });

  it("a palette export asked before the page has loaded runs once it has", async () => {
    let loaded!: (p: TablePage) => void;
    vi.mocked(api.fetchPage).mockReturnValueOnce(new Promise((r) => (loaded = r)));
    exportTable.mockResolvedValue({ kind: "dismissed" });
    render(<Live />);
    act(() => useApp.getState().setExportRequest({ tabId: tab.id, format: "json" }));
    expect(exportTable).not.toHaveBeenCalled();
    expect(useApp.getState().exportRequest).not.toBeNull();
    await act(async () => loaded(page));
    expect(exportTable).toHaveBeenCalledTimes(1);
    expect(exportTable).toHaveBeenCalledWith(expect.any(String), { table, filter: "status = 'active'", sort }, "json", expect.any(Function));
    expect(useApp.getState().exportRequest).toBeNull();
  });

  it("a palette export while one is running is dropped; the footer already says Exporting…", async () => {
    exportTable.mockReturnValue(new Promise(() => {}));
    render(<Live />);
    await openMenu();
    await userEvent.click(screen.getByRole("menuitem", { name: "Export as CSV" }));
    act(() => useApp.getState().setExportRequest({ tabId: tab.id, format: "json" }));
    expect(useApp.getState().exportRequest).toBeNull();
    expect(exportTable).toHaveBeenCalledTimes(1);
    expect(screen.getByText("Exporting… 0 rows")).toBeTruthy();
  });

  it("a failed Show in Finder is shown inline with its own message", async () => {
    exportTable.mockResolvedValueOnce({ kind: "done", rows: 17, path: "/tmp/users.csv", fileName: "users.csv" });
    vi.mocked(api.revealExport).mockRejectedValueOnce({ kind: "query", message: "Couldn't show the file: it was moved" });
    render(<Live />);
    await openMenu();
    await userEvent.click(screen.getByRole("menuitem", { name: "Export as CSV" }));
    await userEvent.click(await screen.findByRole("button", { name: "Show in Finder" }));
    expect((await screen.findByRole("alert")).textContent).toBe("Couldn't show the file: it was moved");
  });

  it("a failed cancel keeps the export running", async () => {
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    exportTable.mockReturnValue(new Promise(() => {}));
    vi.mocked(api.cancelExport).mockRejectedValueOnce({ kind: "query", message: "ipc down" });
    render(<Live />);
    await openMenu();
    await userEvent.click(screen.getByRole("menuitem", { name: "Export as JSON" }));
    await userEvent.click(screen.getByRole("button", { name: "Cancel" }));
    await vi.waitFor(() => expect(error).toHaveBeenCalled());
    expect(screen.getByText("Exporting… 0 rows")).toBeTruthy();
    expect(screen.queryByRole("alert")).toBeNull();
    error.mockRestore();
  });

  it("a failed export is cleared by the next fetch, so the save note shows", async () => {
    exportTable.mockRejectedValueOnce({ kind: "io", message: "disk full" });
    render(<Live />);
    await openMenu();
    await userEvent.click(screen.getByRole("menuitem", { name: "Export as CSV" }));
    expect((await screen.findByRole("alert")).textContent).toBe("Export failed: disk full");
    // What a save does: a note, then a refetch of the page.
    act(() => useApp.setState((s) => ({
      tabs: s.tabs.map((t) => (t.id === tab.id && t.kind === "table" ? { ...t, saveNote: "Saved 1 change", reloadNonce: (t.reloadNonce ?? 0) + 1 } : t)),
    })));
    expect(await screen.findByText("Saved 1 change")).toBeTruthy();
    expect(screen.queryByRole("alert")).toBeNull();
  });
});
