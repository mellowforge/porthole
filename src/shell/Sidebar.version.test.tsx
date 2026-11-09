import { render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../api/tauri", () => ({}));

import { useApp } from "../state/store";
import { shortVersion } from "../connections/profile";
import { Sidebar } from "./Sidebar";

const PG_FULL = "PostgreSQL 17.2 (Debian 17.2-1.pgdg120+1) on aarch64-unknown-linux-gnu, compiled by gcc (Debian 12.2.0-14) 12.2.0, 64-bit";

const connect = (kind: string, serverVersion: string) =>
  useApp.setState({
    active: {
      profile: { id: "c1", name: "Local", database: "fx", kind, transport: "network", host: "localhost", port: 5432 } as never,
      serverVersion,
      schemas: ["public"],
      schema: "public",
    } as never,
    tree: { tables: [], views: [], functions: [] },
  });

beforeEach(() => useApp.setState(useApp.getInitialState(), true));

describe("server version on the sidebar card", () => {
  it("shows Postgres's name and version without the build details, with the full text on hover", () => {
    connect("postgres", PG_FULL);
    render(<Sidebar />);
    const version = screen.getByText("PostgreSQL 17.2");
    expect(version.getAttribute("title")).toBe(PG_FULL);
  });

  it.each([
    ["mysql", "MySQL 8.4.13"],
    ["mysql", "MariaDB 11.4.5"],
    ["sqlite", "SQLite 3.50.4"],
    ["mongodb", "MongoDB 8.0.34"],
  ])("shows %s's version as the server reports it: %s", (kind, v) => {
    connect(kind, v);
    render(<Sidebar />);
    expect(screen.getByText(v)).toBeTruthy();
  });

  it("adds no version line when the server reported no version", () => {
    connect("postgres", "");
    const { container } = render(<Sidebar />);
    expect(container.querySelector(".side-version")).toBeNull();
  });

  it("shortVersion leaves anything it doesn't recognise alone", () => {
    expect(shortVersion(PG_FULL)).toBe("PostgreSQL 17.2");
    expect(shortVersion("PostgreSQL 11.22 on x86_64-pc-linux-musl")).toBe("PostgreSQL 11.22");
    expect(shortVersion("17")).toBe("17");
    expect(shortVersion("")).toBe("");
  });
});
