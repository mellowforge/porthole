import { format } from "sql-formatter";
import type { DbKind, HistoryEntry, StatementResult, StatementSpan } from "../api/types";
import { schemaIsDatabase } from "../connections/profile";
import type { QueryTab } from "../state/store";

/**
 * The statement a cursor belongs to: the last one whose start is at or before
 * the cursor, or the first one when the cursor is before all of them.
 */
export function statementAtCursor(spans: StatementSpan[], cursor: number): StatementSpan | null {
  let found: StatementSpan | null = null;
  for (const s of spans) if (s.from <= cursor) found = s;
  return found ?? spans[0] ?? null;
}

// mongodb is never formatted (`formatSql` returns its text unchanged); its entry only satisfies the type.
const FORMATTER_LANGUAGES = { postgres: "postgresql", mysql: "mysql", sqlite: "sqlite", sqlserver: "postgresql", mongodb: "postgresql" } as const satisfies Record<DbKind, string>;

/** Whether a server version string (`MariaDB 11.4.13`, `MySQL 8.4.13`) names a MariaDB server. */
export const isMariaDb = (serverVersion: string | undefined): boolean => !!serverVersion?.startsWith("MariaDB ");

/** Formats SQL in the connection's dialect (a MongoDB statement is returned as written): SQLite's, MySQL's or MariaDB's (by the server's version) for those, PostgreSQL's otherwise. */
export function formatSql(sql: string, kind?: DbKind, serverVersion?: string): string {
  if (kind === "mongodb") return sql;
  const language = kind === "mysql" && isMariaDb(serverVersion) ? "mariadb" : kind ? FORMATTER_LANGUAGES[kind] : "postgresql";
  return format(sql, { language, paramTypes: { named: [":"] }, keywordCase: "upper" });
}

export function editorStateLabel(tab: QueryTab, connectionName: string): string {
  if (tab.savedQueryId === null) return "Unsaved";
  return tab.savedSql === tab.sql ? `Saved query · ${connectionName}` : "Unsaved changes";
}

const count = (n: number, noun: string) => `${n.toLocaleString("en-US")} ${noun}${n === 1 ? "" : "s"}`;

/** The detail shown after the verb in the statement bar, e.g. `20 rows · 18 ms`. */
export function statementDetail(r: StatementResult): string {
  const o = r.outcome;
  const ms = `${r.elapsedMs} ms`;
  switch (o.kind) {
    case "rows":
      return o.truncated ? `${o.rows.length.toLocaleString("en-US")}+ rows · ${ms}` : `${count(o.rows.length, "row")} · ${ms}`;
    case "affected":
      return `${count(o.rows, "row")} affected · ${ms}`;
    case "done":
      return `Done · ${ms}`;
    case "error":
      return `Error · ${ms}`;
    case "canceled":
      return "Canceled";
    case "skipped":
      return "Not run";
  }
}

const pad = (n: number) => String(n).padStart(2, "0");

/** Local 24-hour HH:MM:SS. */
export function formatTime(ms: number): string {
  const d = new Date(ms);
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}

/** `status = active, since = 2026-10-01`; NULL as `NULL`, an empty string as `''`. */
export function formatParams(params: Record<string, string | null>): string {
  return Object.entries(params)
    .map(([k, v]) => `${k} = ${v === null ? "NULL" : v === "" ? "''" : v}`)
    .join(", ");
}

export function runSummary(count: number, startedAt: number, params: Record<string, string | null>): string {
  const base = `Ran ${count > 1 ? `${count} statements ` : ""}at ${formatTime(startedAt)}`;
  const p = formatParams(params);
  return p ? `${base} with ${p}` : base;
}

/** 1-based line and column (in characters) of a 0-based UTF-16 offset. */
export function lineCol(text: string, utf16: number): { line: number; column: number } {
  const before = text.slice(0, Math.max(0, utf16));
  const nl = before.lastIndexOf("\n");
  return { line: before.split("\n").length, column: Array.from(before.slice(nl + 1)).length + 1 };
}

const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const dayStart = (ts: number) => {
  const d = new Date(ts);
  return new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
};

/** Date header for a history group: `Today`, `Yesterday`, `Nov 3`, or `Dec 30, 2025` for another year. */
export function historyGroupLabel(ts: number, now: number): string {
  const today = new Date(dayStart(now));
  const yesterday = new Date(today.getFullYear(), today.getMonth(), today.getDate() - 1).getTime();
  const day = dayStart(ts);
  if (day === today.getTime()) return "Today";
  if (day === yesterday) return "Yesterday";
  const d = new Date(ts);
  const label = `${MONTHS[d.getMonth()]} ${d.getDate()}`;
  return d.getFullYear() === today.getFullYear() ? label : `${label}, ${d.getFullYear()}`;
}

/** `HH:MM`, local 24-hour. */
export function clockTime(ts: number): string {
  const d = new Date(ts);
  return `${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/** Where a history entry ran: the database, or `database · schema`; `null` for the connection's own. */
export function historyTarget(e: HistoryEntry, kind: DbKind): string | null {
  const parts = schemaIsDatabase(kind) ? [e.targetSchema] : [e.targetDatabase, e.targetSchema];
  return parts.filter(Boolean).join(" · ") || null;
}
