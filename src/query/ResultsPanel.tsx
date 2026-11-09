import { useState } from "react";
import * as api from "../api/tauri";
import type { DbKind, ExportOutcome, StatementResult } from "../api/types";
import { ExportMenu } from "../export/ExportMenu";
import { ExportStatus } from "../export/ExportStatus";
import { useExport } from "../export/useExport";
import { DataGrid } from "../table/DataGrid";
import type { QueryRunState } from "../state/store";
import { MessagesLog, messageCount } from "./MessagesLog";
import { StatementBar } from "./StatementBar";

export interface ResultsPanelProps {
  /** The tab title; the default file name of an export. */
  title: string;
  run: QueryRunState;
  /** Where a truncated read result runs again to export every row; `null` when it can't
   * (the tab's transaction has failed), so the loaded rows are exported instead. */
  exportTarget: api.QueryExportTarget | null;
  /** Wraps an export that runs again on the tab's session (see `trackQueryExport`). */
  trackQueryExport(work: () => Promise<ExportOutcome>): Promise<ExportOutcome>;
  onSelect(index: number): void;
  onFetchAll(result: StatementResult): void;
  onGoToError(position: number): void;
  /** The connection's kind. A stopped MongoDB statement may have changed documents (its write
   * isn't rolled back), so its result says `Stopped.` with its warning instead of "did not run". */
  kind?: DbKind;
}

export function ResultsPanel({ title, run, exportTarget, trackQueryExport, onSelect, onFetchAll, onGoToError, kind }: ResultsPanelProps) {
  const exp = useExport();
  const [view, setView] = useState<"results" | "messages">("results");
  const { results } = run;
  const select = (i: number) => {
    setView("results");
    onSelect(i);
  };
  const current = results[run.selected] as StatementResult | undefined;
  const rowsOutcome = current?.outcome.kind === "rows" ? current.outcome : null;
  const fetched = (run.fetchedAll ?? []).includes(run.selected);
  const stoppedMongo = kind === "mongodb" && current?.outcome.kind === "canceled";
  const stopWarning = stoppedMongo ? current?.notices.find((n) => n.severity === "warning") : undefined;
  const canFetchAll = !!current && rowsOutcome !== null && rowsOutcome.truncated && current.kind === "read" && !fetched;

  // A truncated read result runs again on the server, so the file has every row; anything
  // else is exported from the rows the grid holds (running a write again would repeat it).
  // While the tab runs, or when its transaction has failed, the statement can't run again.
  const exportsAgain =
    !!current && rowsOutcome !== null && rowsOutcome.truncated && current.kind === "read" &&
    exportTarget !== null && run.status !== "running";
  const runExport = (format: "csv" | "json") => {
    if (!rowsOutcome || !current) return;
    const name = title.replace(/[/\\:]/g, "-");
    // Cell types go along only when the result has them, so an untyped (Postgres) export is called as before.
    const types: [string[]] | [] = rowsOutcome.cellTypes ? [rowsOutcome.cellTypes] : [];
    void exp.start((id, onProgress) =>
      exportsAgain && exportTarget
        ? trackQueryExport(() =>
            api.exportQuery(id, name, exportTarget, current.sql, current.params, format, (p) => onProgress(p.rows)),
          )
        : api.exportRows(
            id,
            name,
            rowsOutcome.columns,
            rowsOutcome.rows,
            format,
            (p) => onProgress(p.rows),
            ...types,
          ),
    );
  };

  return (
    <div className="query-results">
      <div className="results-head">
        <div className="pill-toggle" role="tablist" aria-label="Output">
          <button type="button" role="tab" aria-selected={view === "results"} onClick={() => setView("results")}>
            Results {rowsOutcome && <span className="pill-count">{rowsOutcome.rows.length.toLocaleString("en-US")}</span>}
          </button>
          <button type="button" role="tab" aria-selected={view === "messages"} onClick={() => setView("messages")}>
            Messages <span className="pill-count">{messageCount(results)}</span>
          </button>
        </div>
        <ExportStatus state={exp.state} onCancel={exp.cancel} onReveal={exp.reveal} />
        <span className="query-spacer" />
        {rowsOutcome && (
          <ExportMenu
            variant="ghost"
            disabled={exp.state.kind === "running"}
            note={
              exportsAgain
                ? "Runs the query again to export every row."
                : rowsOutcome.truncated
                  ? `Only the first ${rowsOutcome.rows.length.toLocaleString("en-US")} rows are loaded, and only those are exported.`
                  : null
            }
            onPick={runExport}
          />
        )}
        {results.length > 1 && current && (
          <label className="results-picker">
            <span aria-hidden="true">{`Showing: statement ${run.selected + 1} of ${results.length} (${current.verb}) ▾`}</span>
            <select aria-label="Statement" value={run.selected} onChange={(e) => select(Number(e.target.value))}>
              {results.map((r, i) => (
                <option key={r.index} value={i}>{`${i + 1} · ${r.verb}`}</option>
              ))}
            </select>
          </label>
        )}
      </div>
      {run.error && <div className="results-error" role="alert">{run.error}</div>}
      <div className="results-body">
        {view === "messages" ? (
          <MessagesLog results={results} startedAt={run.startedAt} sqlAtRun={run.sqlAtRun} onGoToError={onGoToError} />
        ) : rowsOutcome ? (
          <>
            <div className="results-grid">
              <DataGrid columns={rowsOutcome.columns} rows={rowsOutcome.rows} rowOffset={0} />
            </div>
            {rowsOutcome.truncated && (
              <div className="results-truncated">
                <span>{fetched ? "Stopped at 100,000 rows." : "Showing the first 1,000 rows."}</span>
                {canFetchAll && (
                  <button type="button" className="btn" data-variant="secondary" disabled={run.status === "running"} onClick={() => onFetchAll(current!)}>
                    <span>Fetch all</span>
                  </button>
                )}
              </div>
            )}
          </>
        ) : (
          <div className="results-empty">
            {!current
              ? run.status === "running" ? "Running…" : "Run a query to see results."
              : current.outcome.kind === "error"
                ? "This statement failed. See Messages for details."
                : current.outcome.kind === "affected" || current.outcome.kind === "done"
                  ? "No rows returned."
                  : stoppedMongo
                    ? <>Stopped.{stopWarning && <div>{stopWarning.message}</div>}</>
                    : "This statement did not run."}
          </div>
        )}
      </div>
      {(results.length > 0) && (
        <StatementBar
          results={results}
          selected={run.selected}
          onSelect={select}
          summary={run.startedAt !== null && run.statementCount !== null ? { count: run.statementCount, startedAt: run.startedAt } : null}
        />
      )}
    </div>
  );
}
