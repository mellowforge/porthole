import { describe, expect, it } from "vitest";
import type { ConnectionProfile } from "../api/types";
import { capabilities, hasOtherDatabases, locationLabel, searchText, targetName } from "./profile";

const base = { id: "c", name: "Dev", kind: "postgres", defaultSchema: null, environment: "local", connectTimeoutSecs: 10, readOnly: false, startupSql: null } as const;
const network: ConnectionProfile = { ...base, transport: "network", host: "db.local", port: 5432, user: "u", database: "app", sslMode: "prefer" };
const file: ConnectionProfile = { ...base, kind: "sqlite", transport: "file", filePath: "/Users/me/x.sqlite" };
const uri: ConnectionProfile = { ...base, transport: "uri", uri: "mongodb://alice:secret@host:27017/shop" };

describe("locationLabel", () => {
  it("network: host:port · database, or host · database without the port", () => {
    expect(locationLabel(network, { port: true })).toBe("db.local:5432 · app");
    expect(locationLabel(network, { port: false })).toBe("db.local · app");
  });
  it("file: the path, with the home folder as ~", () => {
    expect(locationLabel(file, { port: true, home: "/Users/me" })).toBe("~/x.sqlite");
    expect(locationLabel(file, { port: true, home: "/Users/other" })).toBe("/Users/me/x.sqlite");
    expect(locationLabel(file, { port: true, home: null })).toBe("/Users/me/x.sqlite");
  });
  it("a MySQL profile with no database: host:port only", () => {
    const my: ConnectionProfile = { ...network, kind: "mysql", port: 3306, database: "" };
    expect(locationLabel(my, { port: true })).toBe("db.local:3306");
    expect(locationLabel({ ...my, database: "fx" }, { port: true })).toBe("db.local:3306 · fx");
  });
  it("uri: without credentials", () => {
    expect(locationLabel(uri, { port: true })).toBe("mongodb://host:27017/shop");
  });
});

describe("searchText and targetName", () => {
  it("search text is the lowercased name and location", () => {
    expect(searchText(network)).toBe("dev db.local:5432 · app");
    expect(searchText(file)).toContain("x.sqlite");
  });
  it("target name is the database, the file name, or empty", () => {
    expect(targetName(network)).toBe("app");
    expect(targetName(file)).toBe("x.sqlite");
    expect(targetName(uri)).toBe("");
  });
});

describe("capabilities", () => {
  it("postgres: network, port 5432, other databases, a schema field", () => {
    expect(capabilities("postgres")).toEqual({ transport: "network", defaultPort: 5432, otherDatabases: true, schemaField: true, schemaLabel: "Schema", queryTabs: true, tablesLabel: "TABLES", searchLabel: "Search tables, views" });
  });
  it("sqlite: a file, no port, one database, no schema field", () => {
    expect(capabilities("sqlite")).toEqual({ transport: "file", defaultPort: null, otherDatabases: false, schemaField: false, schemaLabel: "Schema", queryTabs: true, tablesLabel: "TABLES", searchLabel: "Search tables, views" });
  });
  it("hasOtherDatabases follows the kind", () => {
    expect(hasOtherDatabases(network)).toBe(true);
    expect(hasOtherDatabases(file)).toBe(false);
  });
});

describe("capabilities for MySQL", () => {
  it("a network database on 3306 with databases as schemas", () => {
    expect(capabilities("mysql")).toEqual({ transport: "network", defaultPort: 3306, otherDatabases: false, schemaField: false, schemaLabel: "Database", queryTabs: true, tablesLabel: "TABLES", searchLabel: "Search tables, views" });
  });
});

describe("capabilities for MongoDB", () => {
  it("a network server on 27017 with databases as schemas, collections and query tabs", () => {
    expect(capabilities("mongodb")).toEqual({ transport: "network", defaultPort: 27017, otherDatabases: false, schemaField: false, schemaLabel: "Database", queryTabs: true, tablesLabel: "COLLECTIONS", searchLabel: "Search collections, views" });
  });
});
