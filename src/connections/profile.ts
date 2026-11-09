import { create } from "zustand";
import type { ConnectionProfile, DbKind } from "../api/types";

/** The user's home folder, shown as `~` in file paths. Set once at startup; reactive so that
 *  components showing a path re-render when it arrives. */
export const useHomeDir = create<{ home: string | null }>(() => ({ home: null }));

export const setHomeDir = (home: string | null) =>
  useHomeDir.setState({ home: home ? home.replace(/[\\/]+$/, "") : null });

const withTilde = (path: string, home: string | null) => {
  if (!home) return path;
  if (path === home) return "~";
  return path.startsWith(`${home}/`) || path.startsWith(`${home}\\`) ? `~${path.slice(home.length)}` : path;
};

const withoutCredentials = (uri: string) => uri.replace(/^([a-z][a-z0-9+.-]*:\/\/)[^/@?#]*@/i, "$1");

/** Where the profile points: `host:port · database`, a file path, or a URI without credentials. */
export function locationLabel(p: ConnectionProfile, opts: { port: boolean; home?: string | null }): string {
  switch (p.transport) {
    case "network":
    {
      const host = opts.port ? `${p.host}:${p.port}` : p.host;
      return p.database === "" ? host : `${host} · ${p.database}`;
    }
    case "file":
      return withTilde(p.filePath, opts.home === undefined ? useHomeDir.getState().home : opts.home);
    case "uri":
      return withoutCredentials(p.uri);
  }
}

/** Lowercased name and location, for the connection search box. */
export const searchText = (p: ConnectionProfile): string =>
  `${p.name} ${locationLabel(p, { port: true, home: null })}`.toLowerCase();

/** The database (network), the file name (file), or "" (uri): what a window title and chip show. */
export function targetName(p: ConnectionProfile): string {
  switch (p.transport) {
    case "network":
      return p.database;
    case "file":
      return p.filePath.split(/[\\/]/).pop() ?? "";
    case "uri":
      return "";
  }
}

/** What a database type needs from the form, the target picker and the editor. */
export interface Capabilities {
  transport: "network" | "file";
  defaultPort: number | null;
  /** The chip's Database section and `listDatabases`. */
  otherDatabases: boolean;
  /** The form's "Schema (optional)" field. */
  schemaField: boolean;
  schemaLabel: "Schema" | "Database";
  /** Whether the kind has query tabs (New query, ⌘T). */
  queryTabs: boolean;
  /** The sidebar's section title for tables. */
  tablesLabel: "TABLES" | "COLLECTIONS";
  /** The sidebar search's label (its placeholder adds `…`). */
  searchLabel: "Search tables, views" | "Search collections, views";
}

const SERVER: Capabilities = { transport: "network", defaultPort: 5432, otherDatabases: true, schemaField: true, schemaLabel: "Schema", queryTabs: true, tablesLabel: "TABLES", searchLabel: "Search tables, views" };
const CAPABILITIES: Record<DbKind, Capabilities> = {
  postgres: SERVER,
  // A MySQL "schema" is a database; the server's own databases are the picker's single list.
  mysql: { transport: "network", defaultPort: 3306, otherDatabases: false, schemaField: false, schemaLabel: "Database", queryTabs: true, tablesLabel: "TABLES", searchLabel: "Search tables, views" },
  // A SQLite file is one database (its attached ones are schemas), so its tabs always run on the file.
  sqlite: { transport: "file", defaultPort: null, otherDatabases: false, schemaField: false, schemaLabel: "Schema", queryTabs: true, tablesLabel: "TABLES", searchLabel: "Search tables, views" },
  sqlserver: SERVER,
  // Like MySQL: a database is the picker's single list.
  mongodb: { transport: "network", defaultPort: 27017, otherDatabases: false, schemaField: false, schemaLabel: "Database", queryTabs: true, tablesLabel: "COLLECTIONS", searchLabel: "Search collections, views" },
};

export const capabilities = (kind: DbKind): Capabilities => CAPABILITIES[kind];

/** Whether a slow table page can be stopped (its query cancelled on the server). */
export const canStopPage = (kind: DbKind): boolean => kind === "postgres";

/** Whether the connection's kind has query tabs; true while no kind is known. */
export const hasQueryTabs = (kind: DbKind | undefined): boolean => !kind || CAPABILITIES[kind]?.queryTabs !== false;

/** The two-letter badge of each database type: connection lists, the sidebar card, a query tab's chip. */
export const KIND_BADGES: Record<DbKind, string> = { postgres: "PG", mysql: "MY", sqlite: "SQ", sqlserver: "MS", mongodb: "MG" };

/** Whether a query tab can run on another database than the profile's own. */
export const hasOtherDatabases = (p: ConnectionProfile): boolean => capabilities(p.kind).otherDatabases;

/** Whether the kind's schemas are databases (MySQL): the chip and window title name the current one. */
export const schemaIsDatabase = (kind: DbKind): boolean => capabilities(kind).schemaLabel === "Database";

/** The server's name and version: Postgres's `version()` also names the build ("… on aarch64-…, compiled by …"). */
export const shortVersion = (version: string) => /^PostgreSQL \S+/.exec(version)?.[0] ?? version;
