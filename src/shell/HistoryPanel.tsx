import { useEffect, useMemo, useRef, useState } from "react";
import * as api from "../api/tauri";
import type { HistoryEntry } from "../api/types";
import historyIcon from "../assets/icons/history.svg";
import { clockTime, historyGroupLabel, historyTarget } from "../query/format";
import { errorMessage, useApp } from "../state/store";

const firstLine = (sql: string) => sql.trim().split("\n")[0];

/** Popover under the toolbar listing the newest 200 statements run on this connection. */
export function HistoryPanel({ onClose }: { onClose: () => void }) {
  const newQueryTab = useApp((s) => s.newQueryTab);
  const kind = useApp((s) => s.active?.profile?.kind);
  const [entries, setEntries] = useState<HistoryEntry[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let live = true;
    api
      .listHistory(200)
      .then((e) => live && setEntries(e))
      .catch((e) => live && setError(errorMessage(e)));
    return () => {
      live = false;
    };
  }, []);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    const onDown = (e: MouseEvent) => {
      const el = e.target instanceof Element ? e.target : null;
      // The toolbar button toggles the panel itself.
      if (!ref.current?.contains(el) && !el?.closest("[data-history-toggle]")) onClose();
    };
    document.addEventListener("keydown", onKey);
    document.addEventListener("mousedown", onDown);
    return () => {
      document.removeEventListener("keydown", onKey);
      document.removeEventListener("mousedown", onDown);
    };
  }, [onClose]);

  const groups = useMemo(() => {
    const now = Date.now();
    const out: { label: string; items: HistoryEntry[] }[] = [];
    for (const e of entries ?? []) {
      const label = historyGroupLabel(e.startedAt, now);
      const last = out[out.length - 1];
      if (last?.label === label) last.items.push(e);
      else out.push({ label, items: [e] });
    }
    return out;
  }, [entries]);

  const open = (e: HistoryEntry) => {
    newQueryTab({
      sql: e.sql,
      params: e.params,
      ...(e.targetSchema ? { schema: e.targetSchema } : {}),
      ...(e.targetDatabase ? { database: e.targetDatabase } : {}),
    });
    onClose();
  };

  return (
    <div className="history-panel" role="dialog" aria-label="History" ref={ref}>
      <div className="history-head">
        <img src={historyIcon} alt="" width={14} height={14} />
        <span>History</span>
      </div>
      {error && <p className="history-error">Couldn’t load history: {error}</p>}
      {!error && entries?.length === 0 && <p className="history-empty">No queries run on this connection yet.</p>}
      {groups.map((g) => (
        <section key={g.label}>
          <h3 className="history-group">{g.label}</h3>
          {g.items.map((e) => {
            const target = kind ? historyTarget(e, kind) : null;
            return (
            <button type="button" key={e.id} className="history-row" onClick={() => open(e)}>
              <span className="history-line">
                <span className="history-time">{clockTime(e.startedAt)}</span>
                {!e.ok && (
                  <span className="history-fail" role="img" aria-label="Failed">
                    ✗
                  </span>
                )}
                <b>{e.verb}</b>
                <span className="history-summary">{e.summary}</span>
                {target && <span className="history-target">{target}</span>}
              </span>
              <span className="history-sql">{firstLine(e.sql)}</span>
            </button>
            );
          })}
        </section>
      ))}
    </div>
  );
}
