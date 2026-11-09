import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../api/tauri", () => ({ exportRows: vi.fn(), exportQuery: vi.fn(), cancelExport: vi.fn(), revealExport: vi.fn() }));
vi.mock("../table/DataGrid", () => ({ DataGrid: () => <div data-testid="grid" /> }));

import * as api from "../api/tauri";
import type { StatementResult } from "../api/types";
import type { QueryRunState } from "../state/store";
import { ResultsPanel } from "./ResultsPanel";

const columns = [{ name: "id", dataType: "int4" }] as never;
const rows = Array.from({ length: 1000 }, (_, i) => [String(i)]);
const result = (outcome: StatementResult["outcome"], kind: StatementResult["kind"] = "read"): StatementResult =>
  ({ index: 0, verb: "SELECT", kind, notices: [], sql: "select 1", params: {}, outcome }) as unknown as StatementResult;
const target = { tabId: "t1", database: "analytics", schema: "fx" };
const runWith = (r: StatementResult): QueryRunState =>
  ({ status: "done", results: [r], selected: 0, error: null, startedAt: null, statementCount: 1 }) as unknown as QueryRunState;
const track = vi.fn((work: () => Promise<unknown>) => work());
const panel = (r: StatementResult, opts: { target?: typeof target | null; status?: "running" | "done" } = {}) => (
  <ResultsPanel
    title="a/b: report"
    run={{ ...runWith(r), status: opts.status ?? "done" } as QueryRunState}
    exportTarget={opts.target === undefined ? target : opts.target}
    trackQueryExport={track as never}
    onSelect={vi.fn()}
    onFetchAll={vi.fn()}
    onGoToError={vi.fn()}
  />
);

beforeEach(() => vi.clearAllMocks());

describe("results export", () => {
  it("a truncated read result runs again on the tab's session to export every row", async () => {
    vi.mocked(api.exportQuery).mockResolvedValue({ kind: "dismissed" });
    const r = { ...result({ kind: "rows", columns, rows, truncated: true }), sql: "select * from big where id > :min", params: { min: "5" } };
    render(panel(r));
    await userEvent.click(screen.getByRole("button", { name: "Export" }));
    expect(screen.getByText("Runs the query again to export every row.")).toBeTruthy();
    await userEvent.click(screen.getByRole("menuitem", { name: "Export as JSON" }));
    expect(api.exportQuery).toHaveBeenCalledWith(
      expect.any(String), "a-b- report", target, "select * from big where id > :min", { min: "5" }, "json", expect.any(Function),
    );
    expect(track).toHaveBeenCalledTimes(1);
    expect(api.exportRows).not.toHaveBeenCalled();
  });

  it.each([
    ["the tab is still running", { status: "running" as const }],
    ["the tab's transaction has failed", { target: null }],
  ])("a truncated read result exports the loaded rows when %s", async (_, opts) => {
    vi.mocked(api.exportRows).mockResolvedValue({ kind: "dismissed" });
    render(panel(result({ kind: "rows", columns, rows, truncated: true }), opts));
    await userEvent.click(screen.getByRole("button", { name: "Export" }));
    expect(screen.getByText("Only the first 1,000 rows are loaded, and only those are exported.")).toBeTruthy();
    await userEvent.click(screen.getByRole("menuitem", { name: "Export as CSV" }));
    expect(api.exportRows).toHaveBeenCalledTimes(1);
    expect(api.exportQuery).not.toHaveBeenCalled();
  });

  it("a truncated result of a write exports the held rows under a safe name and says so", async () => {
    vi.mocked(api.exportRows).mockResolvedValue({ kind: "dismissed" });
    render(panel(result({ kind: "rows", columns, rows, truncated: true }, "write")));
    await userEvent.click(screen.getByRole("button", { name: "Export" }));
    expect(screen.getByText("Only the first 1,000 rows are loaded, and only those are exported.")).toBeTruthy();
    await userEvent.click(screen.getByRole("menuitem", { name: "Export as CSV" }));
    expect(api.exportRows).toHaveBeenCalledWith(expect.any(String), "a-b- report", columns, rows, "csv", expect.any(Function));
    expect(api.exportQuery).not.toHaveBeenCalled();
  });

  it("a complete result exports the held rows without running again", async () => {
    vi.mocked(api.exportRows).mockResolvedValue({ kind: "dismissed" });
    render(panel(result({ kind: "rows", columns, rows: rows.slice(0, 3), truncated: false })));
    await userEvent.click(screen.getByRole("button", { name: "Export" }));
    await userEvent.click(screen.getByRole("menuitem", { name: "Export as CSV" }));
    expect(api.exportRows).toHaveBeenCalledWith(expect.any(String), "a-b- report", columns, rows.slice(0, 3), "csv", expect.any(Function));
    expect(api.exportQuery).not.toHaveBeenCalled();
  });

  it("a typed result exports its cell types with the rows", async () => {
    vi.mocked(api.exportRows).mockResolvedValue({ kind: "dismissed" });
    const held = rows.slice(0, 2);
    render(panel(result({ kind: "rows", columns, rows: held, truncated: false, cellTypes: ["i", "n"] })));
    await userEvent.click(screen.getByRole("button", { name: "Export" }));
    await userEvent.click(screen.getByRole("menuitem", { name: "Export as JSON" }));
    expect(api.exportRows).toHaveBeenCalledWith(
      expect.any(String), "a-b- report", columns, held, "json", expect.any(Function), ["i", "n"],
    );
  });

  it("has no Export for an affected-rows outcome", () => {
    render(panel(result({ kind: "affected", rows: 3 })));
    expect(screen.queryByRole("button", { name: "Export" })).toBeNull();
  });

  it("a failed Show in Finder is shown inline with its own message", async () => {
    vi.mocked(api.exportRows).mockResolvedValue({ kind: "done", rows: 3, path: "/tmp/r.csv", fileName: "r.csv" });
    vi.mocked(api.revealExport).mockRejectedValue({ kind: "query", message: "Couldn't show the file: it was moved" });
    render(panel(result({ kind: "rows", columns, rows: rows.slice(0, 3), truncated: false })));
    await userEvent.click(screen.getByRole("button", { name: "Export" }));
    await userEvent.click(screen.getByRole("menuitem", { name: "Export as CSV" }));
    await userEvent.click(await screen.findByRole("button", { name: "Show in Finder" }));
    expect((await screen.findByRole("alert")).textContent).toBe("Couldn't show the file: it was moved");
  });
});
