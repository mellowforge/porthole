import type { StatementResult } from "../api/types";
import { runSummary, statementDetail } from "./format";

export interface StatementBarProps {
  results: StatementResult[];
  selected: number;
  onSelect(index: number): void;
  /** Total statements and start time, known once the run completed. */
  summary: { count: number; startedAt: number } | null;
}

const mark = (r: StatementResult) =>
  r.outcome.kind === "error" || r.outcome.kind === "canceled" ? "err" : r.outcome.kind === "skipped" ? null : "ok";

export function StatementBar({ results, selected, onSelect, summary }: StatementBarProps) {
  const params = Object.assign({}, ...results.map((r) => r.params));
  return (
    <div className="statement-bar" data-testid="statement-bar">
      <div className="statement-chips">
        {results.map((r, i) => {
          const m = mark(r);
          return (
            <button key={r.index} type="button" className="statement-chip" aria-pressed={i === selected} onClick={() => onSelect(i)}>
              {m && (
                <span className="statement-mark" data-kind={m} aria-hidden="true">
                  {m === "ok" ? "✓" : "✗"}
                </span>
              )}
              <strong>{r.verb}</strong>
              <span className="statement-detail">{statementDetail(r)}</span>
            </button>
          );
        })}
      </div>
      {summary && <span className="statement-summary">{runSummary(summary.count, summary.startedAt, params)}</span>}
    </div>
  );
}
