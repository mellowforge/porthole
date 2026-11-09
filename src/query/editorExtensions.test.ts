import { MariaSQL, MySQL, PostgreSQL, SQLite } from "@codemirror/lang-sql";
import { describe, expect, it } from "vitest";
import { sqlDialectFor } from "./editorExtensions";

describe("sqlDialectFor", () => {
  it("is SQLite for a SQLite connection and PostgreSQL otherwise", () => {
    expect(sqlDialectFor("sqlite")).toBe(SQLite);
    expect(sqlDialectFor("postgres")).toBe(PostgreSQL);
    expect(sqlDialectFor(undefined)).toBe(PostgreSQL);
  });
});

describe("sqlDialectFor by server version", () => {
  it("MariaDB servers get MariaSQL, MySQL servers MySQL", () => {
    expect(sqlDialectFor("mysql", "MariaDB 11.4.13")).toBe(MariaSQL);
    expect(sqlDialectFor("mysql", "MySQL 8.4.13")).toBe(MySQL);
    expect(sqlDialectFor("mysql")).toBe(MySQL);
    expect(sqlDialectFor("postgres", "MariaDB 11.4.13")).toBe(PostgreSQL);
  });
});
