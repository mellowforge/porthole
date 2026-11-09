import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ConnectedInfo, SchemaTree } from "../api/types";

vi.mock("../api/tauri", () => ({
  collectionFields: vi.fn(),
  schemaTree: vi.fn(),
  targetSchemas: vi.fn(),
  targetColumns: vi.fn(),
}));

import * as api from "../api/tauri";
import { loadFields, useApp } from "./store";

const mongo = { profile: { id: "g1", kind: "mongodb", transport: "network", database: "fx" }, serverVersion: "MongoDB 8.0.4", schemas: ["fx"], schema: "fx" } as unknown as ConnectedInfo;
const tree: SchemaTree = { tables: ["users"], views: [], functions: [] };

beforeEach(async () => {
  vi.resetAllMocks();
  vi.mocked(api.targetSchemas).mockResolvedValue(["fx"]);
  vi.mocked(api.schemaTree).mockResolvedValue(tree);
  useApp.setState(useApp.getInitialState(), true);
  useApp.setState({ active: mongo, tree });
  // Each test starts with an empty field cache.
  await useApp.getState().refreshCatalog();
  vi.mocked(api.collectionFields).mockReset();
});

describe("loadFields", () => {
  it("makes one call per collection, and caches the result", async () => {
    vi.mocked(api.collectionFields).mockResolvedValue(["_id", "name"]);
    const [a, b] = await Promise.all([loadFields("fx", "users"), loadFields("fx", "users")]);
    expect(a).toEqual(["_id", "name"]);
    expect(b).toEqual(["_id", "name"]);
    expect(await loadFields("fx", "users")).toEqual(["_id", "name"]);
    expect(api.collectionFields).toHaveBeenCalledTimes(1);
    expect(api.collectionFields).toHaveBeenCalledWith("fx", "users");
  });

  it("keys the cache by database and collection", async () => {
    vi.mocked(api.collectionFields).mockImplementation((schema, coll) => Promise.resolve([`${schema}.${coll}`]));
    expect(await loadFields("fx", "users")).toEqual(["fx.users"]);
    expect(await loadFields("fx", "orders")).toEqual(["fx.orders"]);
    expect(await loadFields("fx_other", "users")).toEqual(["fx_other.users"]);
    expect(api.collectionFields).toHaveBeenCalledTimes(3);
  });

  it("gives [] on a failure, logs it, and asks again next time", async () => {
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    vi.mocked(api.collectionFields).mockRejectedValueOnce({ kind: "query", message: "down" }).mockResolvedValueOnce(["name"]);
    expect(await loadFields("fx", "users")).toEqual([]);
    expect(error).toHaveBeenCalledWith("collection_fields failed", { kind: "query", message: "down" });
    expect(await loadFields("fx", "users")).toEqual(["name"]);
    expect(api.collectionFields).toHaveBeenCalledTimes(2);
    error.mockRestore();
  });

  it("refreshCatalog clears the cache", async () => {
    vi.mocked(api.collectionFields).mockResolvedValueOnce(["name"]).mockResolvedValueOnce(["name", "age"]);
    expect(await loadFields("fx", "users")).toEqual(["name"]);
    await useApp.getState().refreshCatalog();
    expect(await loadFields("fx", "users")).toEqual(["name", "age"]);
    expect(api.collectionFields).toHaveBeenCalledTimes(2);
  });

  it("a load that a refresh overtook isn't cached", async () => {
    let finish: (f: string[]) => void = () => {};
    vi.mocked(api.collectionFields)
      .mockReturnValueOnce(new Promise((resolve) => (finish = resolve)))
      .mockResolvedValueOnce(["new"]);
    const stale = loadFields("fx", "users");
    await useApp.getState().refreshCatalog();
    finish(["old"]);
    expect(await stale).toEqual(["old"]);
    expect(await loadFields("fx", "users")).toEqual(["new"]);
  });
});
