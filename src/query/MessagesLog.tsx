import type { StatementResult } from "../api/types";
import { formatParams, formatTime, lineCol, statementDetail } from "./format";

/** One entry per statement, plus one per notice. */
export const messageCount = (results: StatementResult[]) => results.reduce((n, r) => n + 1 + r.notices.length, 0);

export interface MessagesLogProps {
  results: StatementResult[];
  startedAt: number | null;
  sqlAtRun: string | null;
  onGoToError(position: number): void;
}

export function MessagesLog({ results, startedAt, sqlAtRun, onGoToError }: MessagesLogProps) {
  let clock = startedAt;
  return (
    <div className="messages-log">
      {results.map((r) => {
        const time = clock === null ? "" : formatTime(clock);
        if (clock !== null) clock += r.elapsedMs;
        const params = formatParams(r.params);
        const o = r.outcome;
        return (
          <div key={r.index} className="message-entry">
            <div className="message-head">
              {time && <span className="message-time">{time}</span>}
              <strong>{r.verb}</strong>
            </div>
            <pre className="message-sql">{r.sql}</pre>
            {params && <div className="message-params">{params}</div>}
            {r.notices.map((n, i) => (
              <div key={i} className="message-notice">{`${n.severity}: ${n.message}`}</div>
            ))}
            {o.kind === "error" ? (
              <div className="message-error">
                <span>{o.message}</span>
                {o.position !== null && sqlAtRun !== null && (() => {
                  const { line, column } = lineCol(sqlAtRun, o.position);
                  const label = `Line ${line}, column ${column}`;
                  return (
                    <button type="button" className="message-goto" onClick={() => onGoToError(o.position!)}>
                      {label}
                    </button>
                  );
                })()}
              </div>
            ) : (
              <div className="message-outcome">{statementDetail(r)}</div>
            )}
          </div>
        );
      })}
    </div>
  );
}
