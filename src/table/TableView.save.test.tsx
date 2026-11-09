import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ColumnInfo, ConnectedInfo, TablePage } from "../api/types";
import { emptyPending } from "./pending";

vi.mock("../api/tauri", () => ({
  fetchPage: vi.fn(),
  tableStructure: vi.fn(),
  previewChanges: vi.fn(),
  applyChanges: vi.fn(),
}));
vi.mock("./DataGrid", () => ({ DataGrid: () => <div data-testid="grid" /> }));

import * as api from "../api/tauri";
import { type TableTab, useApp } from "../state/store";
import { TableView } from "./TableView";

const col = (name: string, over: Partial<ColumnInfo> = {}): ColumnInfo => ({
  name, dataType: "text", fullType: "text", castType: "text", nullable: true, default: null,
  isPrimaryKey: false, readOnly: false, ...over,
});
const page: TablePage = {
  columns: [col("id", { isPrimaryKey: true, nullable: false }), col("status"), col("name")],
  primaryKey: ["id"],
  editing: { kind: "allowed" },
  rows: [["1", "a", "Ann"], ["2", null, "Bo"], ["3", "c", "Cy"]],
  total: { kind: "exact", value: 3 },
  matches: null,
  elapsedMs: 3,
};
const table = { schema: "public", name: "users" };
const tabId = "public.users";
const tab: TableTab = { kind: "table", id: tabId, table, view: { mode: "data", filter: null, sort: null }, pending: emptyPending };
const fetchPage = vi.mocked(api.fetchPage);
const previewChanges = vi.mocked(api.previewChanges);
const applyChanges = vi.mocked(api.applyChanges);

function connect(environment: "local" | "production") {
  useApp.setState({
    active: { profile: { name: "Local Postgres", readOnly: false, environment }, schema: "public" } as unknown as ConnectedInfo,
  });
}
function Live({ id = tabId }: { id?: string }) {
  const t = useApp((s) => s.tabs.find((x) => x.id === id)) as TableTab;
  return <TableView tab={t} />;
}
const pendingNow = () => (useApp.getState().tabs.find((t) => t.id === tabId) as TableTab).pending;
/** 2 edits + 1 deletion = 3 pending changes. */
function makeThreeChanges() {
  const s = useApp.getState();
  act(() => {
    s.editCell(tabId, ["1"], "status", "x", "a");
    s.editCell(tabId, ["1"], "name", "Zed", "Ann");
    s.toggleDeleteRow(tabId, ["3"]);
  });
}

beforeEach(() => {
  vi.resetAllMocks();
  fetchPage.mockResolvedValue(page);
  previewChanges.mockResolvedValue(["UPDATE users SET status = 'x' WHERE id = 1;"]);
  applyChanges.mockResolvedValue({ applied: 3 });
  useApp.setState({ tabs: [tab], activeTabId: tabId });
  connect("local");
});

describe("pending bar and save", () => {
  it("bar appears with counts copy", async () => {
    render(<Live />);
    await screen.findByTestId("grid");
    expect(screen.queryByText(/pending change/)).toBeNull();
    makeThreeChanges();
    expect(screen.getByText("3 pending changes")).toBeTruthy();
    expect(screen.getByText("2 edits · 1 deletion · nothing is written until you save")).toBeTruthy();
    expect(screen.getByRole("button", { name: /Save changes/ }).textContent).toContain("⌘S");
  });

  it("Show SQL fetches and shows preview with title", async () => {
    render(<Live />);
    await screen.findByTestId("grid");
    makeThreeChanges();
    await userEvent.click(screen.getByRole("button", { name: "Show SQL" }));
    expect(await screen.findByText("Will run on Local Postgres · in one transaction")).toBeTruthy();
    await waitFor(() => expect(screen.getByRole("region", { name: "SQL preview" }).textContent).toContain("UPDATE users"));
    expect(previewChanges).toHaveBeenCalledTimes(1);
    expect(previewChanges.mock.calls[0][0].changes).toHaveLength(2);
    expect(screen.getByRole("button", { name: "Hide SQL" })).toBeTruthy();
  });

  it("Save calls applyChanges with the change set, clears pending, refetches, shows Saved 3 changes", async () => {
    render(<Live />);
    await screen.findByTestId("grid");
    makeThreeChanges();
    await userEvent.click(screen.getByRole("button", { name: /Save changes/ }));
    await waitFor(() => expect(pendingNow().edits).toEqual({}));
    expect(applyChanges).toHaveBeenCalledTimes(1);
    expect(applyChanges.mock.calls[0][0].changes).toHaveLength(2);
    await waitFor(() => expect(fetchPage).toHaveBeenCalledTimes(2));
    expect(fetchPage.mock.calls[1][0]).toMatchObject({ table, filter: null, sort: null, page: 0 });
    expect(await screen.findByText("Saved 3 changes")).toBeTruthy();
    expect(screen.queryByText(/pending change/)).toBeNull();
  });

  it("refetch keeps the filter and sort", async () => {
    useApp.setState({ tabs: [{ ...tab, view: { mode: "data", filter: "id > 0", sort: { column: "id", descending: true } } }] });
    render(<Live />);
    await screen.findByTestId("grid");
    makeThreeChanges();
    await userEvent.click(screen.getByRole("button", { name: /Save changes/ }));
    await waitFor(() => expect(fetchPage).toHaveBeenCalledTimes(2));
    expect(fetchPage.mock.calls[1][0]).toMatchObject({ filter: "id > 0", sort: { column: "id", descending: true }, page: 0 });
  });

  it("save failure keeps pending and shows Nothing was saved", async () => {
    applyChanges.mockRejectedValue({ kind: "query", message: "value too long" });
    render(<Live />);
    await screen.findByTestId("grid");
    makeThreeChanges();
    await userEvent.click(screen.getByRole("button", { name: /Save changes/ }));
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toBe("Nothing was saved: value too long");
    expect(pendingNow().edits).not.toEqual({});
    expect(screen.getByText("3 pending changes")).toBeTruthy();
    expect(fetchPage).toHaveBeenCalledTimes(1);
  });

  it("Discard clears", async () => {
    render(<Live />);
    await screen.findByTestId("grid");
    makeThreeChanges();
    await userEvent.click(screen.getByRole("button", { name: "Show SQL" }));
    await userEvent.click(screen.getByRole("button", { name: "Discard" }));
    expect(pendingNow()).toEqual(emptyPending);
    expect(screen.queryByText(/pending change/)).toBeNull();
    expect(screen.queryByRole("region", { name: "SQL preview" })).toBeNull();
    expect(applyChanges).not.toHaveBeenCalled();
  });

  it("⌘S saves only when the table tab is active and has changes", async () => {
    render(<Live />);
    await screen.findByTestId("grid");
    await userEvent.keyboard("{Meta>}s{/Meta}");
    expect(applyChanges).not.toHaveBeenCalled();
    makeThreeChanges();
    act(() => useApp.setState({ activeTabId: "other" }));
    await userEvent.keyboard("{Meta>}s{/Meta}");
    expect(applyChanges).not.toHaveBeenCalled();
    act(() => useApp.setState({ activeTabId: tabId }));
    await userEvent.keyboard("{Meta>}s{/Meta}");
    await waitFor(() => expect(applyChanges).toHaveBeenCalledTimes(1));
  });

  it("Production: Save opens preview with This writes to Production.; Save to Production applies; Cancel doesn't", async () => {
    connect("production");
    render(<Live />);
    await screen.findByTestId("grid");
    makeThreeChanges();
    await userEvent.click(screen.getByRole("button", { name: /Save changes/ }));
    expect(await screen.findByText("This writes to Production.")).toBeTruthy();
    expect(screen.getByText(/Will run on Local Postgres/)).toBeTruthy();
    expect(applyChanges).not.toHaveBeenCalled();
    await userEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(screen.queryByText("This writes to Production.")).toBeNull();
    expect(applyChanges).not.toHaveBeenCalled();
    expect(pendingNow().edits).not.toEqual({});

    await userEvent.click(screen.getByRole("button", { name: /Save changes/ }));
    await userEvent.keyboard("{Escape}");
    expect(screen.queryByText("This writes to Production.")).toBeNull();

    await userEvent.click(screen.getByRole("button", { name: /Save changes/ }));
    await userEvent.click(await screen.findByRole("button", { name: "Save to Production" }));
    await waitFor(() => expect(applyChanges).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(pendingNow().edits).toEqual({}));
  });

  it("an uncertain commit shows its message without the prefix, keeps the changes and reloads", async () => {
    const uncertain =
      "The connection failed while committing, so the save may or may not have been applied. Reload before saving again.";
    applyChanges.mockRejectedValue({ kind: "uncertain", message: uncertain });
    render(<Live />);
    await screen.findByTestId("grid");
    makeThreeChanges();
    await userEvent.click(screen.getByRole("button", { name: /Save changes/ }));
    expect((await screen.findByRole("alert")).textContent).toBe(uncertain);
    expect(screen.queryByText(/Nothing was saved/)).toBeNull();
    expect(screen.getByText("3 pending changes")).toBeTruthy();
    await waitFor(() => expect(fetchPage).toHaveBeenCalledTimes(2));
    expect(screen.queryByText(/^Saved/)).toBeNull();
  });

  it("after an uncertain commit, the next Save shows the SQL and asks; once saved, saving is direct again", async () => {
    applyChanges.mockRejectedValueOnce({ kind: "uncertain", message: "may or may not" });
    render(<Live />);
    await screen.findByTestId("grid");
    makeThreeChanges();
    await userEvent.click(screen.getByRole("button", { name: /Save changes/ }));
    await screen.findByText("may or may not");
    expect(applyChanges).toHaveBeenCalledTimes(1);

    await userEvent.click(screen.getByRole("button", { name: /Save changes/ }));
    expect(await screen.findByText("The last save may already have been applied. Check the rows before saving again.")).toBeTruthy();
    expect(applyChanges).toHaveBeenCalledTimes(1);
    await userEvent.click(await screen.findByRole("button", { name: "Save anyway" }));
    await waitFor(() => expect(applyChanges).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(pendingNow().edits).toEqual({}));

    act(() => useApp.getState().editCell(tabId, ["2"], "name", "B", "Bo"));
    await userEvent.click(screen.getByRole("button", { name: /Save changes/ }));
    await waitFor(() => expect(applyChanges).toHaveBeenCalledTimes(3));
    expect(screen.queryByRole("button", { name: "Save anyway" })).toBeNull();
  });

  it("after an uncertain commit, discarding the changes stops asking", async () => {
    applyChanges.mockRejectedValueOnce({ kind: "uncertain", message: "may or may not" });
    render(<Live />);
    await screen.findByTestId("grid");
    makeThreeChanges();
    await userEvent.click(screen.getByRole("button", { name: /Save changes/ }));
    await screen.findByText("may or may not");
    await userEvent.click(screen.getByRole("button", { name: "Discard" }));
    expect((useApp.getState().tabs[0] as TableTab).uncertainSave).toBe(false);
    act(() => useApp.getState().editCell(tabId, ["2"], "name", "B", "Bo"));
    await userEvent.click(screen.getByRole("button", { name: /Save changes/ }));
    await waitFor(() => expect(applyChanges).toHaveBeenCalledTimes(2));
  });

  it("Production after an uncertain commit: the warning, with Save to Production", async () => {
    connect("production");
    useApp.setState({ tabs: [{ ...tab, uncertainSave: true }] });
    render(<Live />);
    await screen.findByTestId("grid");
    makeThreeChanges();
    await userEvent.click(screen.getByRole("button", { name: /Save changes/ }));
    expect(await screen.findByText("The last save may already have been applied. Check the rows before saving again.")).toBeTruthy();
    expect(await screen.findByRole("button", { name: "Save to Production" })).toBeTruthy();
  });

  it("Production: Save to Production waits for the preview of the current changes", async () => {
    connect("production");
    let respond!: (s: string[]) => void;
    previewChanges.mockImplementation(() => new Promise((r) => (respond = r)));
    render(<Live />);
    await screen.findByTestId("grid");
    makeThreeChanges();
    await userEvent.click(screen.getByRole("button", { name: /Save changes/ }));
    const button = () => screen.getByRole("button", { name: "Save to Production" }) as HTMLButtonElement;
    expect(await screen.findByText("Preparing SQL…")).toBeTruthy();
    expect(button().disabled).toBe(true);
    await act(async () => respond(["UPDATE users SET status = 'x' WHERE id = 1;"]));
    expect(button().disabled).toBe(false);
    expect(screen.queryByText("Preparing SQL…")).toBeNull();

    // An edit invalidates what was shown, until the new preview arrives.
    act(() => useApp.getState().editCell(tabId, ["2"], "name", "Bob", "Bo"));
    expect(button().disabled).toBe(true);
    expect(screen.getByText("Preparing SQL…")).toBeTruthy();
    expect(screen.queryByText(/UPDATE users/)).toBeNull();
    await act(async () => respond(["UPDATE users SET name = 'Bob' WHERE id = 2;"]));
    expect(button().disabled).toBe(false);
    await userEvent.click(button());
    await waitFor(() => expect(applyChanges).toHaveBeenCalledTimes(1));
  });

  it("Production: a failed preview shows its error and keeps Save to Production disabled", async () => {
    connect("production");
    previewChanges.mockRejectedValue({ kind: "query", message: "Unknown column nope" });
    render(<Live />);
    await screen.findByTestId("grid");
    makeThreeChanges();
    await userEvent.click(screen.getByRole("button", { name: /Save changes/ }));
    expect(await screen.findByText("Unknown column nope")).toBeTruthy();
    expect((screen.getByRole("button", { name: "Save to Production" }) as HTMLButtonElement).disabled).toBe(true);
  });

  it("a reload failing after a successful save says so in the footer and offers Retry", async () => {
    render(<Live />);
    await screen.findByTestId("grid");
    makeThreeChanges();
    fetchPage.mockRejectedValueOnce({ kind: "connection", message: "connection reset" });
    await userEvent.click(screen.getByRole("button", { name: /Save changes/ }));
    expect(await screen.findByText("Saved 3 changes, but the page couldn't reload.")).toBeTruthy();
    expect(screen.getByText("connection reset")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Retry" })).toBeTruthy();
  });

  it("Save and Discard are disabled while a save started elsewhere (the leave prompt) runs", async () => {
    let finish!: () => void;
    applyChanges.mockReturnValue(new Promise((r) => (finish = () => r({ applied: 3 }))));
    render(<Live />);
    await screen.findByTestId("grid");
    makeThreeChanges();
    let saving!: Promise<boolean>;
    act(() => {
      saving = useApp.getState().saveTableChanges(tabId);
    });
    expect((screen.getByRole("button", { name: /Save changes/ }) as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByRole("button", { name: "Discard" }) as HTMLButtonElement).disabled).toBe(true);
    await act(async () => {
      finish();
      await saving;
    });
  });

  it("footer shows new rows after the fetched count", async () => {
    render(<Live />);
    await screen.findByTestId("grid");
    expect(screen.getByText("3 rows")).toBeTruthy();
    act(() => void useApp.getState().addRow(tabId));
    expect(screen.getByText("3 rows · 1 new")).toBeTruthy();
  });

  it("⌘S while the cell editor is open commits it first", async () => {
    render(<Live />);
    await screen.findByTestId("grid");
    // Stand-in for Glide's overlay: every keydown schedules a timer that cancels the edit if an Enter
    // was seen by then; an Enter's own timer commits. Closing the editor ends it either way.
    const clip = document.createElement("div");
    clip.className = "gdg-clip-region";
    const input = document.createElement("textarea");
    clip.append(input);
    document.body.append(clip);
    let motion = false;
    let finished = false;
    clip.addEventListener("keydown", (e) => {
      const save = e.key === "Enter";
      if (save) motion = true;
      setTimeout(() => {
        if (finished || !motion) return;
        finished = true;
        if (save) useApp.getState().editCell(tabId, ["2"], "status", "typed", null);
        clip.remove();
      }, 0);
    });
    input.focus();
    await userEvent.keyboard("{Meta>}s{/Meta}");
    await waitFor(() => expect(applyChanges).toHaveBeenCalledTimes(1));
    expect(applyChanges.mock.calls[0][0].changes).toEqual([
      { kind: "update", key: ["2"], values: { status: "typed" } },
    ]);
  });

  it("⌘S saves in the second, active table tab", async () => {
    const other: TableTab = { ...tab, id: "public.orders", table: { schema: "public", name: "orders" } };
    useApp.setState({ tabs: [other, tab], activeTabId: tabId });
    render(
      <>
        <Live id={other.id} />
        <Live />
      </>,
    );
    await screen.findAllByTestId("grid");
    makeThreeChanges();
    await userEvent.keyboard("{Meta>}s{/Meta}");
    await waitFor(() => expect(applyChanges).toHaveBeenCalledTimes(1));
  });

  it("a repeated ⌘S while saving applies once", async () => {
    render(<Live />);
    await screen.findByTestId("grid");
    makeThreeChanges();
    await userEvent.keyboard("{Meta>}s{/Meta}{Meta>}s{/Meta}");
    await waitFor(() => expect(pendingNow().edits).toEqual({}));
    expect(applyChanges).toHaveBeenCalledTimes(1);
  });
});
