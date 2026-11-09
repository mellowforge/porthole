import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../api/tauri", () => ({ endTransaction: vi.fn(), closeQuerySession: vi.fn() }));

import * as api from "../api/tauri";
import { useApp } from "../state/store";
import type { QueryTab } from "../state/store";
import { TxBar } from "./TxBar";

const tab = (txStatus: QueryTab["txStatus"]): QueryTab => ({
  kind: "query",
  id: "q1",
  title: "Query 1",
  sql: "",
  savedQueryId: null,
  savedSql: null,
  params: {},
  schema: null,
  database: null,
  run: { status: "idle", results: [], selected: 0, startedAt: null, sqlAtRun: null, pending: null, error: null, statementCount: null },
  txStatus,
  note: null,
});

beforeEach(() => {
  vi.resetAllMocks();
});

describe("TxBar", () => {
  it("open shows Commit and Rollback", () => {
    render(<TxBar tab={tab("open")} />);
    expect(screen.getByText("Transaction open")).not.toBeNull();
    expect(screen.getByRole("button", { name: "Commit" })).not.toBeNull();
    expect(screen.getByRole("button", { name: "Rollback" })).not.toBeNull();
  });

  it("failed shows only Rollback", () => {
    render(<TxBar tab={tab("failed")} />);
    expect(screen.getByText("Transaction failed — roll back to continue")).not.toBeNull();
    expect(screen.queryByRole("button", { name: "Commit" })).toBeNull();
    expect(screen.getByRole("button", { name: "Rollback" })).not.toBeNull();
  });

  it("renders nothing when idle", () => {
    const { container } = render(<TxBar tab={tab("idle")} />);
    expect(container.innerHTML).toBe("");
  });

  it("Commit calls endTransaction(tab, true)", async () => {
    vi.mocked(api.endTransaction).mockResolvedValue("idle");
    useApp.setState({ tabs: [tab("open")], activeTabId: "q1" });
    render(<TxBar tab={tab("open")} />);
    await userEvent.click(screen.getByRole("button", { name: "Commit" }));
    expect(api.endTransaction).toHaveBeenCalledWith("q1", true);
    expect((useApp.getState().tabs[0] as QueryTab).txStatus).toBe("idle");
  });

  it("shows the error when ending fails", async () => {
    vi.mocked(api.endTransaction).mockRejectedValue({ message: "connection lost" });
    useApp.setState({ tabs: [tab("open")], activeTabId: "q1" });
    render(<TxBar tab={tab("open")} />);
    await userEvent.click(screen.getByRole("button", { name: "Rollback" }));
    expect((await screen.findByRole("alert")).textContent).toContain("connection lost");
  });

  it("unknown shows a grey state with Commit and Rollback", async () => {
    vi.mocked(api.endTransaction).mockResolvedValue("idle");
    useApp.setState({ tabs: [tab("unknown")], activeTabId: "q1" });
    const { container } = render(<TxBar tab={tab("unknown")} />);
    expect(screen.getByText("Transaction state unknown")).not.toBeNull();
    expect(container.querySelector(".tx-bar")?.getAttribute("data-status")).toBe("unknown");
    for (const name of ["Commit", "Rollback"]) {
      expect(screen.getByRole("button", { name }).getAttribute("data-variant")).toBe("secondary");
    }
    await userEvent.click(screen.getByRole("button", { name: "Commit" }));
    expect(api.endTransaction).toHaveBeenCalledWith("q1", true);
  });
});
