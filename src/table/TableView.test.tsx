import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ColumnInfo, TablePage } from "../api/types";
import { emptyPending } from "./pending";

vi.mock("../api/tauri", () => ({ fetchPage: vi.fn(), tableStructure: vi.fn() }));
vi.mock("./DataGrid", () => ({
  DataGrid: (p: { columns: unknown[]; rows: unknown[]; onHeaderClick: (n: string) => void }) => (
    <div data-testid="grid">
      {`${p.rows.length} rows x ${p.columns.length} cols`}
      <button type="button" onClick={() => p.onHeaderClick("status")}>
        header-status
      </button>
    </div>
  ),
}));

import * as api from "../api/tauri";
import type { TableTab } from "../state/store";
import { TableView } from "./TableView";

const col = (name: string, pk = false): ColumnInfo => ({
  name,
  dataType: "text",
  fullType: "text",
  castType: "text",
  nullable: !pk,
  default: null,
  isPrimaryKey: pk,
  readOnly: false,
});
const cols = [col("id", true), col("status")];
const page = (over: Partial<TablePage> = {}): TablePage => ({
  columns: cols,
  primaryKey: ["id"],
  editing: { kind: "allowed" },
  rows: [["1", "a"], ["2", null]],
  total: { kind: "exact", value: 1284 },
  matches: null,
  elapsedMs: 12,
  ...over,
});
const table = { schema: "public", name: "orders" };
const tab: TableTab = { kind: "table", id: "public.orders", table, view: { mode: "data", filter: null, sort: null }, pending: emptyPending };
const fetchPage = vi.mocked(api.fetchPage);
const tableStructure = vi.mocked(api.tableStructure);

beforeEach(() => {
  vi.resetAllMocks();
});

describe("TableView", () => {
  it("loads page 0 with page size 100", async () => {
    fetchPage.mockResolvedValue(page());
    render(<TableView tab={tab} />);
    await screen.findByText("2 rows x 2 cols");
    expect(fetchPage).toHaveBeenCalledWith({ table, filter: null, sort: null, page: 0, pageSize: 100 });
    expect(screen.getByText("public · 1,284 rows · 2 columns")).not.toBeNull();
    expect(screen.getByText("12 ms")).not.toBeNull();
    expect(screen.getByText("Page 1 of 13")).not.toBeNull();
  });

  it("applying a filter refetches page 0 with the filter and shows 17 matches", async () => {
    fetchPage.mockResolvedValue(page());
    render(<TableView tab={tab} />);
    await screen.findByTestId("grid");
    fetchPage.mockResolvedValue(page({ matches: { kind: "exact", value: 17 }, rows: [["1", "a"]] }));
    await userEvent.type(screen.getByPlaceholderText("id = 1 and status = 'paid'"), "status = 'a'{Enter}");
    await screen.findByText("17 matches");
    expect(fetchPage).toHaveBeenLastCalledWith({
      table,
      filter: "status = 'a'",
      sort: null,
      page: 0,
      pageSize: 100,
    });
    expect(screen.getByText("1 rows · filtered from 1,284")).not.toBeNull();
  });

  it("filter error does not clear rows", async () => {
    fetchPage.mockResolvedValue(page());
    render(<TableView tab={tab} />);
    await screen.findByTestId("grid");
    fetchPage.mockImplementation(() =>
      Promise.reject({ kind: "filter", message: 'Unknown column "nope"', position: 0 }),
    );
    await userEvent.type(screen.getByRole("textbox"), "nope = 1{Enter}");
    await screen.findByText('Unknown column "nope"');
    expect(screen.getByText("2 rows x 2 cols")).not.toBeNull();
    expect(screen.queryByRole("button", { name: "Retry" })).toBeNull();
  });

  it("non-filter error shows banner with retry", async () => {
    fetchPage.mockImplementation(() => Promise.reject({ kind: "query", message: "boom" }));
    render(<TableView tab={tab} />);
    await screen.findByText("boom");
    fetchPage.mockResolvedValue(page());
    await userEvent.click(screen.getByRole("button", { name: "Retry" }));
    await screen.findByText("2 rows x 2 cols");
    expect(screen.queryByText("boom")).toBeNull();
  });

  it("structure tab shows columns, indexes and foreign keys", async () => {
    fetchPage.mockResolvedValue(page());
    tableStructure.mockResolvedValue({
      columns: cols,
      primaryKey: ["id"],
      relation: "table" as const,
      editing: { kind: "allowed" as const },
      indexes: [{ name: "orders_pkey", columns: ["id"], unique: true, primary: true }],
      foreignKeys: [
        {
          name: "orders_user_fk",
          columns: ["user_id"],
          refTable: { schema: "public", name: "users" },
          refColumns: ["id"],
        },
      ],
    });
    render(<TableView tab={tab} />);
    await screen.findByTestId("grid");
    expect(tableStructure).not.toHaveBeenCalled();
    await userEvent.click(screen.getByRole("button", { name: "Structure" }));
    await screen.findByText("orders_pkey");
    expect(screen.getByText("orders_user_fk")).not.toBeNull();
    expect(screen.getByText("→ public.users(id)")).not.toBeNull();
    expect(tableStructure).toHaveBeenCalledWith(table);
  });

  it("header click sorts and refetches page 0", async () => {
    fetchPage.mockResolvedValue(page());
    render(<TableView tab={tab} />);
    await screen.findByTestId("grid");
    await userEvent.click(screen.getByRole("button", { name: "header-status" }));
    await waitFor(() =>
      expect(fetchPage).toHaveBeenLastCalledWith({
        table,
        filter: null,
        sort: { column: "status", descending: false },
        page: 0,
        pageSize: 100,
      }),
    );
  });

  it("structure error can be retried", async () => {
    fetchPage.mockResolvedValue(page());
    tableStructure.mockImplementationOnce(() => Promise.reject({ kind: "query", message: "no catalog" }));
    render(<TableView tab={tab} />);
    await screen.findByTestId("grid");
    await userEvent.click(screen.getByRole("button", { name: "Structure" }));
    await screen.findByText("no catalog");
    tableStructure.mockResolvedValue({
      columns: cols,
      primaryKey: ["id"],
      relation: "table" as const,
      editing: { kind: "allowed" as const },
      indexes: [{ name: "orders_pkey", columns: ["id"], unique: true, primary: true }],
      foreignKeys: [],
    });
    await userEvent.click(screen.getByRole("button", { name: "Retry" }));
    await screen.findByText("orders_pkey");
    expect(screen.queryByText("no catalog")).toBeNull();
  });

  it("estimated total renders ~1.2M rows", async () => {
    fetchPage.mockResolvedValue(page({ total: { kind: "estimated", value: 1200000 } }));
    render(<TableView tab={tab} />);
    await waitFor(() => expect(screen.getByText("public · ~1.2M rows · 2 columns")).not.toBeNull());
  });

  it("unknown count pager allows next on a full page", async () => {
    fetchPage.mockResolvedValue(
      page({ total: { kind: "unknown" }, rows: Array.from({ length: 100 }, () => ["1", "a"]) }),
    );
    render(<TableView tab={tab} />);
    await screen.findByText("Page 1");
    const next = screen.getByRole("button", { name: "Next page" }) as HTMLButtonElement;
    expect(next.disabled).toBe(false);
    fetchPage.mockResolvedValue(page({ total: { kind: "unknown" } }));
    await userEvent.click(next);
    await screen.findByText("Page 2");
    expect(fetchPage).toHaveBeenLastCalledWith(expect.objectContaining({ page: 1 }));
  });
});

const rows = (n: number) => Array.from({ length: n }, (_, i) => [String(i + 1), "a"]);
const nextButton = () => screen.getByRole("button", { name: "Next page" }) as HTMLButtonElement;
const prevButton = () => screen.getByRole("button", { name: "Previous page" }) as HTMLButtonElement;

describe("TableView pager", () => {
  it("exact 100 rows is one page with Next disabled", async () => {
    fetchPage.mockResolvedValue(page({ total: { kind: "exact", value: 100 }, rows: rows(100) }));
    render(<TableView tab={tab} />);
    await screen.findByText("Page 1 of 1");
    expect(nextButton().disabled).toBe(true);
  });

  it("exact 101 rows is two pages", async () => {
    fetchPage.mockResolvedValue(page({ total: { kind: "exact", value: 101 }, rows: rows(100) }));
    render(<TableView tab={tab} />);
    await screen.findByText("Page 1 of 2");
    expect(nextButton().disabled).toBe(false);
    fetchPage.mockResolvedValue(page({ total: { kind: "exact", value: 101 }, rows: rows(1) }));
    await userEvent.click(nextButton());
    await screen.findByText("Page 2 of 2");
    expect(nextButton().disabled).toBe(true);
  });

  it("estimated count allows Next on a full page even past the estimate", async () => {
    fetchPage.mockResolvedValue(page({ total: { kind: "estimated", value: 50 }, rows: rows(100) }));
    render(<TableView tab={tab} />);
    await screen.findByText("Page 1");
    expect(nextButton().disabled).toBe(false);
  });

  it("estimated count disables Next on a partial page", async () => {
    fetchPage.mockResolvedValue(page({ total: { kind: "estimated", value: 60000 }, rows: rows(40) }));
    render(<TableView tab={tab} />);
    await screen.findByText("Page 1");
    expect(nextButton().disabled).toBe(true);
  });

  it("unknown filter matches allow Next on a full page", async () => {
    fetchPage.mockResolvedValue(page());
    render(<TableView tab={tab} />);
    await screen.findByTestId("grid");
    fetchPage.mockResolvedValue(page({ matches: { kind: "unknown" }, rows: rows(100) }));
    await userEvent.type(screen.getByRole("textbox"), "status = 'a'{Enter}");
    await screen.findByText("many matches");
    expect(nextButton().disabled).toBe(false);
  });

  it("Next and Prev move between pages", async () => {
    fetchPage.mockResolvedValue(page({ rows: rows(100) }));
    render(<TableView tab={tab} />);
    await screen.findByText("Page 1 of 13");
    expect(prevButton().disabled).toBe(true);
    await userEvent.click(nextButton());
    await screen.findByText("Page 2 of 13");
    expect(fetchPage).toHaveBeenLastCalledWith({ table, filter: null, sort: null, page: 1, pageSize: 100 });
    await userEvent.click(nextButton());
    await screen.findByText("Page 3 of 13");
    await userEvent.click(prevButton());
    await screen.findByText("Page 2 of 13");
    expect(fetchPage).toHaveBeenLastCalledWith({ table, filter: null, sort: null, page: 1, pageSize: 100 });
    expect(prevButton().disabled).toBe(false);
  });

  it("the filter survives a sort click", async () => {
    fetchPage.mockResolvedValue(page());
    render(<TableView tab={tab} />);
    await screen.findByTestId("grid");
    fetchPage.mockResolvedValue(page({ matches: { kind: "exact", value: 17 } }));
    await userEvent.type(screen.getByRole("textbox"), "status = 'a'{Enter}");
    await screen.findByText("17 matches");
    await userEvent.click(screen.getByRole("button", { name: "header-status" }));
    await waitFor(() =>
      expect(fetchPage).toHaveBeenLastCalledWith({
        table,
        filter: "status = 'a'",
        sort: { column: "status", descending: false },
        page: 0,
        pageSize: 100,
      }),
    );
    expect((screen.getByRole("textbox") as HTMLInputElement).value).toBe("status = 'a'");
  });

  const ORDER_NOTE = "Rows here have no fixed order, so paging can repeat or skip a row.";
  const keyless = (over: Partial<TablePage> = {}) =>
    page({ primaryKey: [], editing: { kind: "noPrimaryKey" }, unstableOrder: true, ...over });

  it("a page without a fixed order says so when there is more than one page", async () => {
    fetchPage.mockResolvedValue(keyless({ total: { kind: "exact", value: 101 }, rows: rows(100) }));
    render(<TableView tab={tab} />);
    await screen.findByText("Page 1 of 2");
    expect(screen.getByRole("note").textContent).toBe(ORDER_NOTE);
    // The last page still says so: the rows before it were paged.
    fetchPage.mockResolvedValue(keyless({ total: { kind: "exact", value: 101 }, rows: rows(1) }));
    await userEvent.click(nextButton());
    await screen.findByText("Page 2 of 2");
    expect(screen.getByRole("note").textContent).toBe(ORDER_NOTE);
    // A page with a fixed order says nothing.
    fetchPage.mockResolvedValue(page({ total: { kind: "exact", value: 101 }, rows: rows(100) }));
    await userEvent.click(prevButton());
    await screen.findByText("Page 1 of 2");
    expect(screen.queryByText(ORDER_NOTE)).toBeNull();
  });

  it("a page without a fixed order says so, but not when everything fits on one page", async () => {
    fetchPage.mockResolvedValue(keyless({ total: { kind: "exact", value: 2 } }));
    render(<TableView tab={tab} />);
    await screen.findByText("Page 1 of 1");
    expect(screen.queryByText(ORDER_NOTE)).toBeNull();
    expect(screen.queryByRole("note")).toBeNull();
  });

  it("the sort survives a page change", async () => {
    fetchPage.mockResolvedValue(page({ rows: rows(100) }));
    render(<TableView tab={tab} />);
    await screen.findByText("Page 1 of 13");
    await userEvent.click(screen.getByRole("button", { name: "header-status" }));
    await waitFor(() =>
      expect(fetchPage).toHaveBeenLastCalledWith(
        expect.objectContaining({ sort: { column: "status", descending: false }, page: 0 }),
      ),
    );
    await userEvent.click(nextButton());
    await screen.findByText("Page 2 of 13");
    expect(fetchPage).toHaveBeenLastCalledWith({
      table,
      filter: null,
      sort: { column: "status", descending: false },
      page: 1,
      pageSize: 100,
    });
  });
});
