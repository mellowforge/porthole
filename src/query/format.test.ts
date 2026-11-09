import { describe, expect, it } from "vitest";
import type { StatementSpan } from "../api/types";
import type { QueryTab } from "../state/store";
import type { StatementResult } from "../api/types";
import { editorStateLabel, formatSql, historyGroupLabel, lineCol, runSummary, statementAtCursor, statementDetail } from "./format";

const span = (from: number, to: number): StatementSpan => ({ from, to, kind: "read", verb: "SELECT" });

describe("statementAtCursor", () => {
  const spans = [span(0, 10), span(12, 20), span(25, 30)];
  it.each([
    [0, 0],
    [5, 0],
    [10, 0], // on the `;` after the first statement
    [11, 0], // between statements: the last one that started
    [12, 1],
    [19, 1],
    [22, 1],
    [25, 2],
    [100, 2],
  ])("cursor %i -> statement %i", (cursor, index) => {
    expect(statementAtCursor(spans, cursor)).toBe(spans[index]);
  });
  it("before the first statement picks the first", () => {
    expect(statementAtCursor([span(5, 9), span(12, 20)], 2)).toEqual(span(5, 9));
  });
  it("no statements -> null", () => {
    expect(statementAtCursor([], 3)).toBeNull();
  });
});

describe("formatSql", () => {
  it("keeps :params and ::casts", () => {
    const out = formatSql("select a::int from t where b = :b");
    expect(out).toContain("::int");
    expect(out).toContain(":b");
  });
});

describe("editorStateLabel", () => {
  const tab = (p: Partial<QueryTab>) => ({ sql: "select 1", savedQueryId: null, savedSql: null, ...p }) as QueryTab;
  it("never saved", () => {
    expect(editorStateLabel(tab({}), "Local")).toBe("Unsaved");
  });
  it("saved and clean", () => {
    expect(editorStateLabel(tab({ savedQueryId: "q", savedSql: "select 1" }), "Local")).toBe("Saved query · Local");
  });
  it("saved and changed", () => {
    expect(editorStateLabel(tab({ savedQueryId: "q", savedSql: "select 2" }), "Local")).toBe("Unsaved changes");
  });
});

const res = (outcome: StatementResult["outcome"], elapsedMs = 4): StatementResult => ({
  index: 0, from: 0, to: 1, verb: "X", kind: "read", sql: "x", params: {}, outcome, elapsedMs, notices: [],
});
const rows = (n: number, truncated = false): StatementResult["outcome"] => ({
  kind: "rows", columns: [{ name: "a", dataType: "int4" }], rows: Array.from({ length: n }, (_, i) => [String(i)]), truncated,
});

describe("statementDetail", () => {
  it.each([
    [res(rows(20), 18), "20 rows · 18 ms"],
    [res(rows(1), 3), "1 row · 3 ms"],
    [res(rows(0), 3), "0 rows · 3 ms"],
    [res(rows(1000, true), 18), "1,000+ rows · 18 ms"],
    [res({ kind: "affected", rows: 37 }, 4), "37 rows affected · 4 ms"],
    [res({ kind: "affected", rows: 1 }, 4), "1 row affected · 4 ms"],
    [res({ kind: "done" }, 3), "Done · 3 ms"],
    [res({ kind: "error", message: "m", code: null, position: null }, 4), "Error · 4 ms"],
    [res({ kind: "canceled" }), "Canceled"],
    [res({ kind: "skipped" }), "Not run"],
  ])("%#", (r, text) => expect(statementDetail(r)).toBe(text));
});

describe("runSummary", () => {
  const at = new Date(2026, 9, 5, 14, 32, 5).getTime();
  it("singular and plural", () => {
    expect(runSummary(1, at, {})).toBe("Ran at 14:32:05");
    expect(runSummary(2, at, {})).toBe("Ran 2 statements at 14:32:05");
  });
  it("with params", () => {
    const t = new Date(2026, 9, 5, 14, 40, 12).getTime();
    expect(runSummary(1, t, { status: "active", since: "2026-10-01", limit: "20" })).toBe(
      "Ran at 14:40:12 with status = active, since = 2026-10-01, limit = 20",
    );
  });
  it("lists params with NULL and empty string", () => {
    expect(runSummary(1, at, { status: "active", since: "2026-10-01", a: null, b: "" })).toBe(
      "Ran at 14:32:05 with status = active, since = 2026-10-01, a = NULL, b = ''",
    );
  });
});

describe("lineCol", () => {
  it("is 1-based", () => expect(lineCol("select", 0)).toEqual({ line: 1, column: 1 }));
  it("crosses a newline", () => expect(lineCol("select 1\nselect nope", 16)).toEqual({ line: 2, column: 8 }));
  it("counts an emoji as one column", () => expect(lineCol("a😀b", 3)).toEqual({ line: 1, column: 3 }));
});

describe("historyGroupLabel", () => {
  const now = new Date(2026, 10, 5, 15, 0, 0).getTime();
  const at = (y: number, m: number, d: number, h = 9) => new Date(y, m, d, h).getTime();
  it("today", () => expect(historyGroupLabel(at(2026, 10, 5, 0), now)).toBe("Today"));
  it("yesterday", () => expect(historyGroupLabel(at(2026, 10, 4, 23), now)).toBe("Yesterday"));
  it("older date in the same year", () => expect(historyGroupLabel(at(2026, 10, 3), now)).toBe("Nov 3"));
  it("previous year adds the year", () => expect(historyGroupLabel(at(2025, 11, 30), now)).toBe("Dec 30, 2025"));
});
