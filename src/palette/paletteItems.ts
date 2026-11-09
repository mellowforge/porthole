import type { DbKind, FunctionInfo } from "../api/types";
import type { AppState } from "../state/store";
import { switchConnection } from "../shell/connectionFlow";
import { hasQueryTabs, locationLabel } from "../connections/profile";
import { rank } from "./rank";

export type PaletteKind = "table" | "view" | "function" | "savedQuery" | "connection" | "action";

export type PaletteItem = {
  id: string;
  kind: PaletteKind;
  label: string;
  detail?: string;
  shortcut?: string;
  run: () => void;
};

export const KIND_LABELS: Record<PaletteKind, string> = {
  table: "Table",
  view: "View",
  function: "Function",
  savedQuery: "Saved query",
  connection: "Connection",
  action: "Action",
};

const KIND_ORDER: PaletteKind[] = ["action", "savedQuery", "table", "view", "function", "connection"];
const MAX_ITEMS = 50;

const quoteIdent = (name: string) => (/^[a-z_][a-z0-9_]*$/.test(name) ? name : `"${name.replace(/"/g, '""')}"`);
const quoteMysqlIdent = (name: string) => (/^[a-z_][a-z0-9_$]*$/.test(name) ? name : `\`${name.replace(/`/g, "``")}\``);

/** The statement a palette function opens in a query tab: `SELECT` (`CALL` for a MySQL procedure) with the signature as a comment. */
export function functionSql(schema: string, f: FunctionInfo, kind?: DbKind): string {
  const comment = f.signature ? `/* ${f.signature} */` : "";
  if (kind === "mysql") {
    const target = `${quoteMysqlIdent(schema)}.${quoteMysqlIdent(f.name)}(${comment})`;
    return `${f.kind === "procedure" ? "CALL" : "SELECT"} ${target};`;
  }
  return `SELECT ${quoteIdent(schema)}.${quoteIdent(f.name)}(${comment});`;
}

export interface PaletteDeps {
  openHistory: () => void;
  newWindow: () => void;
  exportActive: (format: "csv" | "json") => void;
}

export function paletteItems(state: AppState, deps: PaletteDeps): PaletteItem[] {
  const items: PaletteItem[] = [];
  const action = (id: string, label: string, run: () => void, shortcut?: string) =>
    items.push({ id: `action:${id}`, kind: "action", label, shortcut, run });

  const workspace = state.screen === "workspace" && !!state.active;
  if (workspace) {
    const activeTab = state.tabs.find((t) => t.id === state.activeTabId);
    if (hasQueryTabs(state.active?.profile?.kind)) action("new-query", "New query", () => state.newQueryTab(), "⌘T");
    action("history", "Show history", deps.openHistory, "⌘Y");
    action("edit-connection", "Edit connection", () => switchConnection(state.active?.profile.id, "edit"));
    action("switch-connection", "Switch connection", () => switchConnection());
    action("new-window", "New window", deps.newWindow, "⇧⌘N");
    if (activeTab) action("close-tab", "Close tab", () => state.closeTab(activeTab.id), "⌘W");
    if (activeTab?.kind === "table") {
      action("export-csv", "Export table as CSV", () => deps.exportActive("csv"));
      action("export-json", "Export table as JSON", () => deps.exportActive("json"));
    }

    for (const q of state.savedQueries) {
      items.push({ id: `saved:${q.id}`, kind: "savedQuery", label: q.name, run: () => state.openSavedQuery(q) });
    }
    const schema = state.active?.schema ?? "public";
    for (const name of state.tree?.tables ?? []) {
      items.push({ id: `table:${name}`, kind: "table", label: name, detail: schema, run: () => state.openTable({ schema, name }) });
    }
    for (const name of state.tree?.views ?? []) {
      items.push({ id: `view:${name}`, kind: "view", label: name, detail: schema, run: () => state.openTable({ schema, name }) });
    }
    for (const f of state.tree?.functions ?? []) {
      items.push({
        id: `function:${f.name}(${f.signature})`,
        kind: "function",
        label: f.name,
        detail: f.kind === "procedure" ? `procedure · ${f.signature}` : f.signature || undefined,
        run: () => state.newQueryTab({ sql: functionSql(schema, f, state.active?.profile.kind) }),
      });
    }
  } else {
    action("new-connection", "New connection", state.requestNewConnection, "⌘N");
    action("new-window", "New window", deps.newWindow, "⇧⌘N");
  }

  for (const c of state.connections) {
    items.push({
      id: `connection:${c.id}`,
      kind: "connection",
      label: c.name,
      detail: locationLabel(c, { port: false }),
      run: () => (workspace ? switchConnection(c.id) : state.openConnections(c.id)),
    });
  }
  return items;
}

/** Empty query: actions, saved queries, tables, … in that order. Otherwise by rank, kind, label. At most 50. */
export function filterItems(items: PaletteItem[], query: string): PaletteItem[] {
  const q = query.trim();
  const kindIndex = (i: PaletteItem) => KIND_ORDER.indexOf(i.kind);
  if (!q) return [...items].sort((a, b) => kindIndex(a) - kindIndex(b)).slice(0, MAX_ITEMS);
  return items
    .flatMap((item) => {
      const r = rank(q, item.label);
      return r === null ? [] : [{ item, r }];
    })
    .sort((a, b) => b.r - a.r || kindIndex(a.item) - kindIndex(b.item) || a.item.label.localeCompare(b.item.label))
    .slice(0, MAX_ITEMS)
    .map((x) => x.item);
}
