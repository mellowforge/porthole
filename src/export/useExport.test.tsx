import { act, renderHook } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../api/tauri", () => ({ cancelExport: vi.fn().mockResolvedValue(undefined) }));

import * as api from "../api/tauri";
import type { ExportOutcome } from "../api/types";
import { useExport } from "./useExport";

beforeEach(() => vi.clearAllMocks());

function deferred() {
  let resolve!: (o: ExportOutcome) => void;
  let reject!: (e: unknown) => void;
  const promise = new Promise<ExportOutcome>((res, rej) => ((resolve = res), (reject = rej)));
  return { promise, resolve, reject };
}

describe("useExport", () => {
  it("tracks progress, then done with the file name", async () => {
    const d = deferred();
    let report!: (n: number) => void;
    const { result } = renderHook(() => useExport());
    let p!: Promise<void>;
    act(() => {
      p = result.current.start((_id, onProgress) => ((report = onProgress), d.promise));
    });
    expect(result.current.state).toMatchObject({ kind: "running", rows: 0 });
    act(() => report(1000));
    expect(result.current.state).toMatchObject({ kind: "running", rows: 1000 });
    await act(async () => {
      d.resolve({ kind: "done", rows: 17, path: "/tmp/users.csv", fileName: "users.csv" });
      await p;
    });
    expect(result.current.state).toEqual({ kind: "done", rows: 17, path: "/tmp/users.csv", fileName: "users.csv" });
  });

  it("maps canceled, dismissed and failures", async () => {
    const { result } = renderHook(() => useExport());
    await act(() => result.current.start(async () => ({ kind: "canceled" })));
    expect(result.current.state).toEqual({ kind: "canceled" });
    await act(() => result.current.start(async () => ({ kind: "dismissed" })));
    expect(result.current.state).toEqual({ kind: "idle" });
    await act(() => result.current.start(() => Promise.reject({ kind: "io", message: "disk full" })));
    expect(result.current.state).toEqual({ kind: "failed", message: "disk full" });
  });

  it("ignores start while running and cancels by id", async () => {
    const d = deferred();
    const first = vi.fn((id: string) => ((seen = id), d.promise));
    let seen = "";
    const second = vi.fn();
    const { result } = renderHook(() => useExport());
    let p!: Promise<void>;
    act(() => {
      p = result.current.start(first);
    });
    await act(() => result.current.start(second));
    expect(second).not.toHaveBeenCalled();
    act(() => result.current.cancel());
    expect(api.cancelExport).toHaveBeenCalledWith(seen);
    await act(async () => {
      d.resolve({ kind: "canceled" });
      await p;
    });
    expect(result.current.state).toEqual({ kind: "canceled" });
    act(() => result.current.clear());
    expect(result.current.state).toEqual({ kind: "idle" });
  });
});
