import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("sql-formatter", async (importOriginal) => {
  const actual = await importOriginal<typeof import("sql-formatter")>();
  return { ...actual, format: vi.fn(actual.format) };
});

import { format } from "sql-formatter";
import { formatSql } from "./format";

beforeEach(() => {
  vi.mocked(format).mockClear();
});

describe("formatSql's language", () => {
  it("is sqlite for a SQLite connection", () => {
    expect(formatSql("select 1", "sqlite")).toBe("SELECT\n  1");
    expect(vi.mocked(format).mock.calls[0][1]).toMatchObject({ language: "sqlite" });
  });

  it("is postgresql for Postgres and by default", () => {
    formatSql("select 1", "postgres");
    formatSql("select 1");
    expect(vi.mocked(format).mock.calls.map((c) => c[1]?.language)).toEqual(["postgresql", "postgresql"]);
  });
});
