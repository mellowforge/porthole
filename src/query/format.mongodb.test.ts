import { describe, expect, it } from "vitest";
import { formatSql } from "./format";

describe("formatSql for mongodb", () => {
  it("returns the text unchanged", () => {
    const text = 'db.users.find({a:1}).sort({name:1})\n  .limit( 5 )';
    expect(formatSql(text, "mongodb")).toBe(text);
  });
});
