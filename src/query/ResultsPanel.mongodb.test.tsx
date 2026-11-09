import { render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

vi.mock("../api/tauri", () => ({ exportRows: vi.fn(), exportQuery: vi.fn(), cancelExport: vi.fn(), revealExport: vi.fn() }));
vi.mock("../table/DataGrid", () => ({ DataGrid: () => <div data-testid="grid" /> }));

import type { DbKind, StatementResult } from "../api/types";
import type { QueryRunState } from "../state/store";
import { ResultsPanel } from "./ResultsPanel";

const STOPPED_WRITE = "Stopped. Some documents may already have been changed.";
const canceled = (verb: string, notices: StatementResult["notices"]): StatementResult =>
  ({ index: 0, from: 0, to: 1, verb, kind: "write", sql: "db.big.updateMany({}, { $set: { s: 1 } })", params: {}, outcome: { kind: "canceled" }, elapsedMs: 3, notices }) as StatementResult;
const panel = (r: StatementResult, kind?: DbKind) => (
  <ResultsPanel
    title="Query 1"
    run={{ status: "done", results: [r], selected: 0, error: null, startedAt: null, statementCount: 1 } as unknown as QueryRunState}
    exportTarget={null}
    trackQueryExport={vi.fn() as never}
    onSelect={vi.fn()}
    onFetchAll={vi.fn()}
    onGoToError={vi.fn()}
    kind={kind}
  />
);
const empty = () => document.querySelector(".results-empty")!;

describe("a stopped statement's result", () => {
  it("on MongoDB says Stopped. and shows the write's warning under it", () => {
    render(panel(canceled("updateMany", [{ severity: "warning", message: STOPPED_WRITE }]), "mongodb"));
    expect(empty().textContent).toBe(`Stopped.${STOPPED_WRITE}`);
    expect(screen.getByText(STOPPED_WRITE).tagName).toBe("DIV");
    expect(screen.queryByText("This statement did not run.")).toBeNull();
  });

  it("on MongoDB with no warning (a stopped read) says only Stopped.", () => {
    render(panel(canceled("find", [{ severity: "info", message: "ignored" }]), "mongodb"));
    expect(empty().textContent).toBe("Stopped.");
  });

  it.each([["postgres" as const], ["mysql" as const], ["sqlite" as const], [undefined]])(
    "on %s keeps saying the statement did not run",
    (kind) => {
      render(panel(canceled("UPDATE", [{ severity: "warning", message: "x" }]), kind));
      expect(empty().textContent).toBe("This statement did not run.");
    },
  );

  it("a skipped MongoDB statement still did not run", () => {
    const skipped = { ...canceled("find", []), outcome: { kind: "skipped" } } as StatementResult;
    render(panel(skipped, "mongodb"));
    expect(empty().textContent).toBe("This statement did not run.");
  });
});
