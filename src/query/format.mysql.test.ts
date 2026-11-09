import { describe, expect, it, vi } from "vitest";

vi.mock("sql-formatter", async (importOriginal) => {
  const actual = await importOriginal<typeof import("sql-formatter")>();
  return { ...actual, format: vi.fn(actual.format) };
});

import { format } from "sql-formatter";
import { formatSql } from "./format";

describe("formatSql's language for MySQL", () => {
  it.each([
    ["MariaDB 11.4.13", "mariadb"],
    ["MySQL 8.4.13", "mysql"],
    [undefined, "mysql"],
  ])("%s formats as %s", (version, language) => {
    vi.mocked(format).mockClear();
    formatSql("select 1", "mysql", version);
    expect(vi.mocked(format).mock.calls[0][1]).toMatchObject({ language });
  });
  it("ignores the version for other kinds", () => {
    vi.mocked(format).mockClear();
    formatSql("select 1", "postgres", "MariaDB 11");
    expect(vi.mocked(format).mock.calls[0][1]).toMatchObject({ language: "postgresql" });
  });
});
