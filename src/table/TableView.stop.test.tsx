import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ColumnInfo, ConnectedInfo, DbKind, TablePage } from "../api/types";
import { emptyPending } from "./pending";

vi.mock("../api/tauri", () => ({ fetchPage: vi.fn(), tableStructure: vi.fn(), cancelPage: vi.fn() }));
vi.mock("./DataGrid", () => ({
  DataGrid: (p: { rows: unknown[] }) => <div data-testid="grid">{`${p.rows.length} rows`}</div>,
}));

import * as api from "../api/tauri";
import { type TableTab, useApp } from "../state/store";
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
const page = (): TablePage => ({
  columns: [col("id", true), col("status")],
  primaryKey: ["id"],
  editing: { kind: "allowed" },
  rows: [["1", "a"]],
  total: { kind: "exact", value: 1 },
  matches: null,
  elapsedMs: 12,
});
const table = { schema: "public", name: "orders" };
const req = { table, filter: null, sort: null, page: 0, pageSize: 100 };
const tab: TableTab = { kind: "table", id: "public.orders", table, view: { mode: "data", filter: null, sort: null }, pending: emptyPending };
const fetchPage = vi.mocked(api.fetchPage);
const cancelPage = vi.mocked(api.cancelPage);
const STOPPED = "Stopped. The page wasn't loaded.";

const connectAs = (kind: DbKind) =>
  useApp.setState({ active: { profile: { kind, readOnly: false, environment: "local" }, schema: "public" } as unknown as ConnectedInfo });
const advance = (ms: number) => act(() => vi.advanceTimersByTimeAsync(ms));
const stopButton = () => screen.queryByRole("button", { name: "Stop" });

beforeEach(() => {
  vi.resetAllMocks();
  vi.useFakeTimers();
  cancelPage.mockResolvedValue(1);
});
afterEach(() => {
  vi.useRealTimers();
});

describe("TableView Stop", () => {
  it("Stop shows after a second of loading on Postgres and stops the page", async () => {
    connectAs("postgres");
    let finish: (p: TablePage) => void = () => {};
    fetchPage.mockImplementationOnce(() => new Promise((r) => (finish = r)));
    render(<TableView tab={tab} />);
    await advance(1000);
    fireEvent.click(stopButton()!);
    expect(cancelPage).toHaveBeenCalledWith(table);
    expect(screen.getByText(STOPPED)).not.toBeNull();
    expect(stopButton()).toBeNull();

    // The stopped load's late result is ignored.
    await act(async () => finish(page()));
    await advance(0);
    expect(screen.queryByTestId("grid")).toBeNull();
    expect(screen.getByText(STOPPED)).not.toBeNull();

    fetchPage.mockResolvedValue(page());
    fireEvent.click(screen.getByRole("button", { name: "Retry" }));
    await advance(0);
    expect(screen.getByTestId("grid")).not.toBeNull();
    expect(screen.queryByText(STOPPED)).toBeNull();
    expect(fetchPage).toHaveBeenCalledTimes(2);
    expect(fetchPage).toHaveBeenLastCalledWith(req);
  });

  it("no Stop on other kinds", async () => {
    connectAs("mysql");
    fetchPage.mockImplementation(() => new Promise(() => {}));
    render(<TableView tab={tab} />);
    await advance(2000);
    expect(stopButton()).toBeNull();
  });

  it("no Stop before a second", async () => {
    connectAs("postgres");
    fetchPage.mockImplementation(() => new Promise(() => {}));
    render(<TableView tab={tab} />);
    await advance(900);
    expect(stopButton()).toBeNull();
    await advance(100);
    expect(stopButton()).not.toBeNull();
  });

  it("a failed cancel says the query may keep running", async () => {
    connectAs("postgres");
    const log = vi.spyOn(console, "error").mockImplementation(() => {});
    let reject: (e: unknown) => void = () => {};
    cancelPage.mockImplementation(() => new Promise((_, r) => (reject = r)));
    fetchPage.mockImplementation(() => new Promise(() => {}));
    render(<TableView tab={tab} />);
    await advance(1000);
    fireEvent.click(stopButton()!);
    // Stopped at once, before the cancel answers.
    expect(screen.getByText(STOPPED)).not.toBeNull();
    await act(async () => reject({ kind: "connection", message: "too many clients already" }));
    await advance(0);
    expect(log).toHaveBeenCalledWith("cancel_page failed", { kind: "connection", message: "too many clients already" });
    expect(
      screen.getByText("Stopped here, but the query couldn't be cancelled on the server: too many clients already. It may keep running."),
    ).not.toBeNull();
    expect(screen.queryByText(STOPPED)).toBeNull();
    log.mockRestore();
  });

  it("a cancel answering 0 keeps the plain Stopped text", async () => {
    connectAs("postgres");
    cancelPage.mockResolvedValue(0);
    fetchPage.mockImplementation(() => new Promise(() => {}));
    render(<TableView tab={tab} />);
    await advance(1000);
    fireEvent.click(stopButton()!);
    await advance(0);
    expect(screen.getByText(STOPPED)).not.toBeNull();
  });

  it("a failed cancel after Retry leaves the new load alone", async () => {
    connectAs("postgres");
    vi.spyOn(console, "error").mockImplementation(() => {});
    let reject: (e: unknown) => void = () => {};
    cancelPage.mockImplementation(() => new Promise((_, r) => (reject = r)));
    fetchPage.mockImplementationOnce(() => new Promise(() => {}));
    render(<TableView tab={tab} />);
    await advance(1000);
    fireEvent.click(stopButton()!);
    fetchPage.mockResolvedValue(page());
    fireEvent.click(screen.getByRole("button", { name: "Retry" }));
    await advance(0);
    await act(async () => reject({ kind: "connection", message: "down" }));
    await advance(0);
    expect(screen.getByTestId("grid")).not.toBeNull();
    expect(screen.queryByRole("alert")).toBeNull();
    vi.mocked(console.error).mockRestore();
  });

  it("the stopped load failing late keeps the Stopped banner", async () => {
    connectAs("postgres");
    let fail: (e: unknown) => void = () => {};
    fetchPage.mockImplementationOnce(() => new Promise((_, r) => (fail = r)));
    render(<TableView tab={tab} />);
    await advance(1000);
    fireEvent.click(stopButton()!);
    await advance(0);
    await act(async () => fail({ kind: "query", message: "canceling statement due to user request" }));
    await advance(0);
    expect(screen.getByText(STOPPED)).not.toBeNull();
    expect(screen.queryByText(/canceling statement/)).toBeNull();
  });
});
