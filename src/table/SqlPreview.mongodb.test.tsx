import { render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ChangeSet, ConnectedInfo } from "../api/types";

vi.mock("../api/tauri", () => ({ previewChanges: vi.fn() }));

import * as api from "../api/tauri";
import { useApp } from "../state/store";
import { SqlPreview } from "./SqlPreview";

const set: ChangeSet = { table: { schema: "fx", name: "users" }, changes: [{ kind: "update", key: ["1"], values: { age: "31" } }] };
const MONGO = 'db.getCollection("users").updateOne({ _id: ObjectId("65a1b2c3d4e5f60718293a4b") }, { $set: { age: 31 } })';

function connect(kind: string, oneByOneSaves?: boolean) {
  useApp.setState({
    active: { profile: { name: "Local", kind }, schema: "fx", ...(oneByOneSaves === undefined ? {} : { oneByOneSaves }) } as unknown as ConnectedInfo,
  });
}

beforeEach(() => {
  vi.resetAllMocks();
  useApp.setState(useApp.getInitialState(), true);
});

describe("SqlPreview on MongoDB", () => {
  it("a standalone server says one change at a time, and the mongosh text has no highlighting", async () => {
    connect("mongodb", true);
    vi.mocked(api.previewChanges).mockResolvedValue([MONGO]);
    const { container } = render(<SqlPreview set={set} connectionName="Local MongoDB" />);
    expect(screen.getByText("Will run on Local MongoDB · one change at a time (this server has no transactions)")).toBeTruthy();
    expect(await screen.findByText(MONGO)).toBeTruthy();
    expect(container.querySelectorAll("[data-token]")).toHaveLength(0);
  });

  it("a replica set keeps the one-transaction title", async () => {
    connect("mongodb", false);
    vi.mocked(api.previewChanges).mockResolvedValue([MONGO]);
    render(<SqlPreview set={set} connectionName="RS" />);
    expect(screen.getByText("Will run on RS · in one transaction")).toBeTruthy();
    expect(await screen.findByText(MONGO)).toBeTruthy();
  });

  it("Postgres keeps its title and highlighting", async () => {
    connect("postgres");
    vi.mocked(api.previewChanges).mockResolvedValue(["UPDATE users SET age = 31 WHERE id = 1;"]);
    const { container } = render(<SqlPreview set={set} connectionName="Local Postgres" />);
    expect(screen.getByText("Will run on Local Postgres · in one transaction")).toBeTruthy();
    await screen.findByText("UPDATE");
    expect(container.querySelectorAll('[data-token="keyword"]').length).toBeGreaterThan(0);
  });
});
