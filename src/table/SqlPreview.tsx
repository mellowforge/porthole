import { type ReactNode, useEffect, useState } from "react";
import * as api from "../api/tauri";
import type { ChangeSet } from "../api/types";
import { errorMessage, useApp } from "../state/store";
import { Button } from "../components/Button";

const KEYWORDS = new Set([
  "UPDATE", "SET", "WHERE", "AND", "INSERT", "INTO", "VALUES", "DELETE", "FROM", "DEFAULT", "NULL", "RETURNING", "TRUE", "FALSE",
]);
const TOKEN = /('(?:[^']|'')*')|(-?\b\d+(?:\.\d+)?\b)|([A-Za-z_][A-Za-z0-9_]*)/g;

/** Colours keywords, strings and numbers with the editor's syntax tokens. Display only. */
export function highlightSql(sql: string): ReactNode[] {
  const out: ReactNode[] = [];
  let last = 0;
  for (const m of sql.matchAll(TOKEN)) {
    const at = m.index ?? 0;
    const kind = m[1] ? "string" : m[2] ? "number" : KEYWORDS.has(m[3].toUpperCase()) ? "keyword" : null;
    if (!kind) continue;
    if (at > last) out.push(sql.slice(last, at));
    out.push(
      <span key={at} style={{ color: `var(--syntax-${kind})` }} data-token={kind}>
        {m[0]}
      </span>,
    );
    last = at + m[0].length;
  }
  if (last < sql.length) out.push(sql.slice(last));
  return out;
}

/** The copy of a confirmation asked before a save; see `saveConfirmCopy`. */
export interface ConfirmCopy {
  message: string;
  saveLabel: string;
}

export const UNCERTAIN_CONFIRM = "The last save may already have been applied. Check the rows before saving again.";

/**
 * Whether a save must first show its SQL and be confirmed, and with what copy: on Production, and
 * after a save whose commit got no answer (saving again could repeat it, adding new rows twice).
 * `null` when the save may go ahead directly.
 */
export function saveConfirmCopy(production: boolean, uncertain: boolean): ConfirmCopy | null {
  if (uncertain) return { message: UNCERTAIN_CONFIRM, saveLabel: production ? "Save to Production" : "Save anyway" };
  if (production) return { message: "This writes to Production.", saveLabel: "Save to Production" };
  return null;
}

export interface SqlPreviewState {
  statements: string[] | null;
  error: string | null;
  /** The statements of exactly this set have loaded. */
  ready: boolean;
}

/** The statements a save of `set` would run, fetched from the core whenever the set changes. */
export function useSqlPreview(set: ChangeSet): SqlPreviewState {
  // Keyed by the set it belongs to, so statements of an earlier set are never shown for this one.
  const [result, setResult] = useState<{ set: ChangeSet; statements: string[] | null; error: string | null } | null>(null);
  const current = result?.set === set ? result : null;

  useEffect(() => {
    let cancelled = false;
    api
      .previewChanges(set)
      .then((statements) => !cancelled && setResult({ set, statements, error: null }))
      .catch((e) => !cancelled && setResult({ set, statements: null, error: errorMessage(e) }));
    return () => {
      cancelled = true;
    };
  }, [set]);

  return { statements: current?.statements ?? null, error: current?.error ?? null, ready: current?.statements != null };
}

/** The statements, their error, or `Preparing SQL…` while they load. MongoDB's are mongosh
 *  commands, not SQL, so they are shown without SQL highlighting. */
export function SqlStatements({ preview }: { preview: SqlPreviewState }) {
  const highlight = useApp((s) => s.active?.profile?.kind !== "mongodb");
  if (preview.error) return <p className="sql-preview-error" role="alert">{preview.error}</p>;
  if (!preview.statements) return <p className="sql-preview-loading">Preparing SQL…</p>;
  return (
    <pre className="sql-preview-code">
      {preview.statements.map((s, i) => (
        <div key={i}>{highlight ? highlightSql(s) : s}</div>
      ))}
    </pre>
  );
}

export interface SqlPreviewProps {
  set: ChangeSet;
  connectionName: string;
  /** Confirmation row (Production, or after an uncertain save); omitted when not asking. */
  confirm?: ConfirmCopy & { onSave: () => void; onCancel: () => void };
}

/**
 * The statements a save would run. Until the preview of the current set has loaded, it shows
 * `Preparing SQL…` and the confirming Save stays disabled, so a save is only ever confirmed against
 * SQL the user has seen.
 */
export function SqlPreview({ set, connectionName, confirm }: SqlPreviewProps) {
  const preview = useSqlPreview(set);
  // A server without transactions (a standalone MongoDB) saves one change at a time.
  const oneByOne = useApp((s) => s.active?.oneByOneSaves === true);

  const cancel = confirm?.onCancel;
  useEffect(() => {
    if (!cancel) return;
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && cancel();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [cancel]);

  return (
    <div className="sql-preview" role="region" aria-label="SQL preview">
      <div className="sql-preview-title">
        Will run on {connectionName} · {oneByOne ? "one change at a time (this server has no transactions)" : "in one transaction"}
      </div>
      <SqlStatements preview={preview} />
      {confirm && (
        <div className="sql-preview-confirm">
          <span>{confirm.message}</span>
          <Button variant="secondary" danger label={confirm.saveLabel} onClick={confirm.onSave} disabled={!preview.ready} />
          <Button variant="ghost" label="Cancel" onClick={confirm.onCancel} />
        </div>
      )}
    </div>
  );
}
