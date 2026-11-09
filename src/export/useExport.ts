import { useCallback, useRef, useState } from "react";
import { cancelExport, revealExport } from "../api/tauri";
import type { ExportOutcome } from "../api/types";

export type ExportState =
  | { kind: "idle" }
  | { kind: "running"; id: string; rows: number }
  | { kind: "done"; rows: number; path: string; fileName: string }
  | { kind: "canceled" }
  | { kind: "failed"; message: string }
  /** Show in Finder failed; `message` is the whole text (`Couldn't show the file: …`). */
  | { kind: "revealFailed"; message: string };

export type ExportRun = (id: string, onProgress: (rows: number) => void) => Promise<ExportOutcome>;

function messageOf(e: unknown): string {
  if (e && typeof e === "object" && "message" in e && typeof (e as { message: unknown }).message === "string") {
    return (e as { message: string }).message;
  }
  return e instanceof Error ? e.message : String(e);
}

/** One export at a time per component instance; Rust owns the save dialog and the file. */
export function useExport() {
  const [state, setState] = useState<ExportState>({ kind: "idle" });
  const running = useRef<string | null>(null);

  const start = useCallback(async (run: ExportRun) => {
    if (running.current) return;
    const id = crypto.randomUUID();
    running.current = id;
    setState({ kind: "running", id, rows: 0 });
    try {
      const outcome = await run(id, (rows) => {
        if (running.current === id) setState({ kind: "running", id, rows });
      });
      if (outcome.kind === "done") {
        setState({ kind: "done", rows: outcome.rows, path: outcome.path, fileName: outcome.fileName });
      } else if (outcome.kind === "canceled") {
        setState({ kind: "canceled" });
      } else {
        setState({ kind: "idle" });
      }
    } catch (e) {
      setState({ kind: "failed", message: messageOf(e) });
    } finally {
      running.current = null;
    }
  }, []);

  const cancel = useCallback(() => {
    const id = running.current;
    if (!id) return;
    // The export is still running (and still shows Cancel): a failed cancel is only logged.
    cancelExport(id).catch((e) => console.error("cancel_export failed", e));
  }, []);

  /** Show in Finder; a failure replaces the status with its message. */
  const reveal = useCallback((path: string) => {
    revealExport(path).catch((e) => setState({ kind: "revealFailed", message: messageOf(e) }));
  }, []);

  /** Resets a finished state; a running export is left alone. */
  const clear = useCallback(() => {
    if (running.current) return;
    setState((s) => (s.kind === "idle" ? s : { kind: "idle" }));
  }, []);

  return { state, start, cancel, reveal, clear };
}
