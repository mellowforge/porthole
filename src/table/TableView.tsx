import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import * as api from "../api/tauri";
import type { AppError, Editing, PageRequest, SortSpec, TablePage, TableStructure } from "../api/types";
import { Button } from "../components/Button";
import { canStopPage } from "../connections/profile";
import { useShortcuts } from "../hooks/useShortcuts";
import { ExportMenu } from "../export/ExportMenu";
import { ExportStatus } from "../export/ExportStatus";
import { useExport } from "../export/useExport";
import { DataGrid } from "./DataGrid";
import { FilterBar } from "./FilterBar";
import { formatCount, nextSort, PAGE_SIZE } from "./gridModel";
import { StructureView } from "./StructureView";
import { errorMessage, type TableTab, type TableViewState, useApp } from "../state/store";
import { PendingBar } from "./PendingBar";
import { counts, toChangeSet } from "./pending";
import { saveConfirmCopy, SqlPreview } from "./SqlPreview";
import { useTableEditing } from "./useTableEditing";

const READ_ONLY_NOTES: Record<Exclude<Editing["kind"], "allowed">, string> = {
  noPrimaryKey: "This table has no primary key, so it opens read-only.",
  view: "Views are read-only.",
  floatKey: "Rows can't be matched exactly by a floating-point primary key, so this table opens read-only.",
};
const STOPPED_MESSAGE = "Stopped. The page wasn't loaded.";
const stopNotCancelled = (message: string) =>
  `Stopped here, but the query couldn't be cancelled on the server: ${message}. It may keep running.`;
/** How long a page load runs before Stop shows, in milliseconds. */
const STOP_AFTER_MS = 1000;
const READ_ONLY_CONNECTION_NOTE = "This connection is read-only. Turn read-only off in the connection settings to edit.";

function asAppError(e: unknown): AppError {
  if (e && typeof e === "object" && "kind" in e && "message" in e) return e as AppError;
  return { kind: "query", message: e instanceof Error ? e.message : String(e) };
}

/**
 * Commits Glide's open overlay editor the way Enter does, so ⌘S never drops what was typed. Resolves
 * once the editor has closed (the grid commits on the next tick), or after a short timeout.
 */
async function commitOpenEditor(): Promise<void> {
  const active = document.activeElement;
  if (!(active instanceof HTMLElement) || !active.closest(".gdg-clip-region")) return;
  // Glide schedules a timer on every keydown, including the ⌘S that got us here; it cancels the edit if
  // an Enter has been seen by the time it fires. Let that timer run first, as a no-op.
  await new Promise((r) => setTimeout(r, 0));
  if (!active.isConnected) return;
  active.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }));
  for (let i = 0; i < 20 && document.querySelector(".gdg-clip-region"); i++) {
    await new Promise((r) => setTimeout(r, 10));
  }
  await new Promise((r) => setTimeout(r, 0));
}

export function TableView({ tab }: { tab: TableTab }) {
  const { table } = tab;
  const updateTableView = useApp((s) => s.updateTableView);
  const [mode, setModeState] = useState<TableViewState["mode"]>(tab.view.mode);
  const [filter, setFilter] = useState<string | null>(tab.view.filter);
  const [sort, setSort] = useState<SortSpec | null>(tab.view.sort);
  const tabId = tab.id;
  const setMode = (m: TableViewState["mode"]) => {
    setModeState(m);
    updateTableView(tabId, { mode: m });
  };
  const [pageIndex, setPageIndex] = useState(0);
  const [data, setData] = useState<TablePage | null>(null);
  const [loading, setLoading] = useState(false);
  const [filterError, setFilterError] = useState<AppError | null>(null);
  const [error, setError] = useState<AppError | null>(null);
  const [structure, setStructure] = useState<TableStructure | null>(null);
  const [structureError, setStructureError] = useState<AppError | null>(null);
  const [structureAttempt, setStructureAttempt] = useState(0);

  const readOnlyConnection = useApp((s) => s.active?.profile.readOnly ?? false);
  const kind = useApp((s) => s.active?.profile?.kind);
  const addRow = useApp((s) => s.addRow);
  const editingAllowed = data?.editing.kind === "allowed" && !readOnlyConnection;
  // While a save is in flight the grid is locked (pending state stays visible), so nothing typed
  // meanwhile can be lost to the post-save subtraction.
  const locked = !!tab.saving;
  const readOnlyNote = !data
    ? null
    : data.editing.kind !== "allowed"
      ? READ_ONLY_NOTES[data.editing.kind]
      : readOnlyConnection
        ? READ_ONLY_CONNECTION_NOTE
        : null;
  const { editable, extraRows, menu } = useTableEditing(tabId, data, tab.pending, editingAllowed, locked);

  const activeTabId = useApp((s) => s.activeTabId);
  const connectionName = useApp((s) => s.active?.profile.name ?? "");
  const production = useApp((s) => s.active?.profile.environment === "production");
  const saveTableChanges = useApp((s) => s.saveTableChanges);
  const discardPending = useApp((s) => s.discardPending);
  const setSaveNote = useApp((s) => s.setSaveNote);
  const [showSql, setShowSql] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const [saving, setSaving] = useState(false);
  const pendingCounts = counts(tab.pending);
  const changeSet = useMemo(() => toChangeSet(tab.pending, table), [tab.pending, table]);
  const confirmCopy = saveConfirmCopy(production, !!tab.uncertainSave);

  useEffect(() => {
    if (pendingCounts.total === 0) {
      setShowSql(false);
      setConfirming(false);
    }
  }, [pendingCounts.total]);

  // Set before any await, so a repeated ⌘S (auto-repeat) can't start a second save.
  const busy = useRef(false);
  const exclusive = async (f: () => Promise<void>) => {
    if (busy.current) return;
    busy.current = true;
    setSaving(true);
    try {
      await f();
    } finally {
      busy.current = false;
      setSaving(false);
    }
  };
  const save = () =>
    exclusive(async () => {
      setConfirming(false);
      await saveTableChanges(tabId);
    });
  const requestSave = () =>
    exclusive(async () => {
      await commitOpenEditor();
      const current = useApp.getState().tabs.find((t) => t.id === tabId);
      if (!current || current.kind !== "table" || counts(current.pending).total === 0) return;
      if (saveConfirmCopy(production, !!current.uncertainSave)) {
        setShowSql(true);
        setConfirming(true);
      } else {
        setConfirming(false);
        await saveTableChanges(tabId);
      }
    });
  // Hidden tabs stay mounted: the shortcut is "not handled" (false) unless this is the active tab in
  // Data mode and it has changes or an open cell editor, so other tabs' listeners still see it.
  useShortcuts({
    "mod+s": () => {
      if (activeTabId !== tabId || mode !== "data") return false;
      const editorOpen = !!document.activeElement?.closest(".gdg-clip-region");
      if (pendingCounts.total === 0 && !editorOpen) return false;
      void requestSave();
      return true;
    },
  });

  const exp = useExport();
  const clearExport = exp.clear;
  const runExport = (format: "csv" | "json") =>
    void exp.start((id, onProgress) =>
      api.exportTable(id, { table, filter: tab.view.filter, sort: tab.view.sort }, format, (p) => onProgress(p.rows)),
    );

  // The palette's "Export table as …": one-shot, acted on by this tab only, through the same path as the menu.
  // Asked before the page has loaded, it waits for it. Asked while an export runs, it is dropped: the
  // footer already says `Exporting…`.
  const exportRequest = useApp((s) => (s.exportRequest?.tabId === tabId ? s.exportRequest : null));
  const setExportRequest = useApp((s) => s.setExportRequest);
  const loaded = !!data;
  const exportRunning = exp.state.kind === "running";
  const runExportRef = useRef(runExport);
  runExportRef.current = runExport;
  useEffect(() => {
    if (!exportRequest) return;
    if (exportRunning) {
      setExportRequest(null);
      return;
    }
    if (!loaded) return;
    setExportRequest(null);
    runExportRef.current(exportRequest.format);
  }, [exportRequest, loaded, exportRunning, setExportRequest]);

  const lastReq = useRef<PageRequest | null>(null);
  const seq = useRef(0);
  // Set once the load in flight has run for STOP_AFTER_MS; a new load starts the wait again.
  const [slow, setSlow] = useState(false);
  const [loadId, setLoadId] = useState(0);

  const load = useCallback(
    async (next: { filter: string | null; sort: SortSpec | null; page: number }, afterSave = false) => {
      if (!afterSave) setSaveNote(tabId, null);
      // A finished export's status (done, canceled, failed) gives the footer back; a running one stays.
      clearExport();
      const req: PageRequest = { table, ...next, pageSize: PAGE_SIZE };
      lastReq.current = req;
      const id = ++seq.current;
      setLoading(true);
      setLoadId(id);
      try {
        const result = await api.fetchPage(req);
        if (id !== seq.current) return;
        setData(result);
        setFilter(req.filter ?? null);
        setSort(req.sort ?? null);
        updateTableView(tabId, { filter: req.filter ?? null, sort: req.sort ?? null });
        setPageIndex(req.page);
        setFilterError(null);
        setError(null);
      } catch (e) {
        if (id !== seq.current) return;
        const err = asAppError(e);
        if (afterSave) {
          // The save went through; only the refetch failed (the banner below offers Retry).
          const t = useApp.getState().tabs.find((x) => x.id === tabId);
          const note = t?.kind === "table" ? t.saveNote : null;
          if (note && !note.endsWith("couldn't reload.")) setSaveNote(tabId, `${note}, but the page couldn't reload.`);
        }
        if (err.kind === "filter") {
          setFilterError(err);
          setError(null);
        } else {
          setError(err);
        }
      } finally {
        if (id === seq.current) setLoading(false);
      }
    },
    [table, tabId, updateTableView, setSaveNote, clearExport],
  );

  useEffect(() => {
    if (!loading) return;
    const t = setTimeout(() => setSlow(true), STOP_AFTER_MS);
    return () => {
      clearTimeout(t);
      setSlow(false);
    };
  }, [loading, loadId]);
  const stoppable = loading && slow && !!kind && canStopPage(kind);
  // Stop: the load's result is ignored and the server cancels its query; Retry loads it again.
  // A cancel that fails says so: the query may still be running.
  const stop = () => {
    const id = ++seq.current;
    setLoading(false);
    setError({ kind: "query", message: STOPPED_MESSAGE });
    api.cancelPage(table).catch((e) => {
      console.error("cancel_page failed", e);
      // A Retry meanwhile owns the banner.
      if (id !== seq.current) return;
      setError({ kind: "query", message: stopNotCancelled(errorMessage(e)) });
    });
  };

  // Initial load uses the tab's saved view; later changes come from user actions.
  const initialView = useRef(tab.view);
  useEffect(() => {
    void load({ filter: initialView.current.filter, sort: initialView.current.sort, page: 0 });
    return () => {
      seq.current++;
    };
  }, [load]);

  // After a save, refetch the page the user is on, with its filter and sort.
  const reloadNonce = tab.reloadNonce ?? 0;
  const seenNonce = useRef(reloadNonce);
  useEffect(() => {
    if (seenNonce.current === reloadNonce) return;
    seenNonce.current = reloadNonce;
    const r = lastReq.current;
    if (r) void load({ filter: r.filter ?? null, sort: r.sort ?? null, page: r.page }, true);
  }, [reloadNonce, load]);

  useEffect(() => {
    if (mode !== "structure" || structure) return;
    let cancelled = false;
    setStructureError(null);
    api
      .tableStructure(table)
      .then((s) => !cancelled && setStructure(s))
      .catch((e) => !cancelled && setStructureError(asAppError(e)));
    return () => {
      cancelled = true;
    };
  }, [mode, structure, table, structureAttempt]);

  // Changing page, sort or filter, or refetching, would leave unsaved edits against a page that no
  // longer shows those rows, so ask first (Save / Discard / Cancel).
  const requestLeave = useApp((s) => s.requestLeave);
  const guardedLoad = (next: { filter: string | null; sort: SortSpec | null; page: number }, then?: () => void) => {
    const nonce = reloadNonceNow();
    requestLeave("reload", [tabId], () => {
      then?.();
      // A save made in the prompt keeps its `Saved n changes` note through the requested fetch.
      void load(next, reloadNonceNow() !== nonce);
    });
  };
  const reloadNonceNow = () => {
    const t = useApp.getState().tabs.find((x) => x.id === tabId);
    return t?.kind === "table" ? (t.reloadNonce ?? 0) : 0;
  };
  const retry = () => {
    const r = lastReq.current;
    if (r) guardedLoad({ filter: r.filter ?? null, sort: r.sort ?? null, page: r.page });
  };

  const columns = data?.columns ?? [];
  const count = data ? (data.matches ?? data.total) : null;
  const rowCount = data?.rows.length ?? 0;
  // Only an exact count bounds the pager; with an estimate or unknown count a
  // full page means there may be more.
  const totalPages = count?.kind === "exact" ? Math.max(1, Math.ceil(count.value / PAGE_SIZE)) : null;
  const hasNext = totalPages !== null ? pageIndex + 1 < totalPages : rowCount === PAGE_SIZE;
  const pageLabel = totalPages !== null ? `Page ${pageIndex + 1} of ${totalPages}` : `Page ${pageIndex + 1}`;

  return (
    <div className="table-view">
      <div className="table-header">
        <div className="table-title">
          <h2>{table.name}</h2>
          <span className="table-meta">
            {data ? `${table.schema} · ${formatCount(data.total)} rows · ${columns.length} columns` : table.schema}
          </span>
        </div>
        <div className="segmented" role="group" aria-label="View">
          {(["data", "structure"] as const).map((m) => (
            <button
              key={m}
              type="button"
              aria-pressed={mode === m}
              data-active={mode === m ? "true" : undefined}
              onClick={() => setMode(m)}
            >
              {m === "data" ? "Data" : "Structure"}
            </button>
          ))}
        </div>
        {editingAllowed && !locked && mode === "data" && (
          <Button variant="secondary" label="+ Add row" onClick={() => addRow(tabId)} />
        )}
        {mode === "data" && (
          <ExportMenu
            variant="secondary"
            disabled={!data || exp.state.kind === "running"}
            note={pendingCounts.total > 0 ? "Unsaved changes are not included." : null}
            onPick={runExport}
          />
        )}
      </div>
      {readOnlyNote && <p className="table-note">{readOnlyNote}</p>}

      {mode === "data" ? (
        <>
          <FilterBar
            columns={columns}
            value={filter ?? ""}
            matches={filter ? (data?.matches ?? null) : null}
            error={filterError}
            onApply={(text) => guardedLoad({ filter: text.trim() === "" ? null : text, sort, page: 0 })}
            onClear={() => guardedLoad({ filter: null, sort, page: 0 }, () => setFilterError(null))}
          />
          {data?.unstableOrder && (pageIndex > 0 || hasNext) && (
            <p className="order-note" role="note">
              Rows here have no fixed order, so paging can repeat or skip a row.
            </p>
          )}
          <div className="grid-area" data-loading={loading ? "true" : undefined}>
            {error && (
              <div className="error-banner" role="alert">
                <span>{error.message}</span>
                <Button variant="secondary" label="Retry" onClick={retry} />
              </div>
            )}
            {data && (
              <DataGrid
                columns={columns}
                rows={data.rows}
                rowOffset={pageIndex * PAGE_SIZE}
                sort={sort}
                onHeaderClick={(name) => guardedLoad({ filter, sort: nextSort(sort, name), page: 0 })}
                editable={editable}
                extraRows={extraRows}
              />
            )}
            {menu}
          </div>
          <div className="pending-wrap">
            <PendingBar
              pending={tab.pending}
              error={tab.saveError ?? null}
              showSql={showSql}
              saving={saving || locked}
              onToggleSql={() => {
                setShowSql((v) => !v);
                setConfirming(false);
              }}
              onDiscard={() => discardPending(tabId)}
              onSave={() => void requestSave()}
            />
            {showSql && pendingCounts.total > 0 && (
              <SqlPreview
                set={changeSet}
                connectionName={connectionName}
                confirm={
                  confirming && confirmCopy
                    ? { ...confirmCopy, onSave: () => void save(), onCancel: () => setConfirming(false) }
                    : undefined
                }
              />
            )}
          </div>
          <div className="table-footer">
            <span>
              {filter && data ? `${rowCount} rows · filtered from ${formatCount(data.total)}` : `${rowCount} rows`}
              {extraRows > 0 && ` · ${extraRows} new`}
            </span>
            {exp.state.kind !== "idle" ? (
              <ExportStatus state={exp.state} onCancel={exp.cancel} onReveal={exp.reveal} />
            ) : (
              tab.saveNote && <span className="save-note">{tab.saveNote}</span>
            )}
            <div className="pager">
              <button
                type="button"
                aria-label="Previous page"
                disabled={pageIndex === 0 || loading}
                onClick={() => guardedLoad({ filter, sort, page: pageIndex - 1 })}
              >
                ‹
              </button>
              <span>{pageLabel}</span>
              <button
                type="button"
                aria-label="Next page"
                disabled={!hasNext || loading}
                onClick={() => guardedLoad({ filter, sort, page: pageIndex + 1 })}
              >
                ›
              </button>
            </div>
            {stoppable && <Button variant="secondary" label="Stop" onClick={stop} />}
            <span>{data ? `${data.elapsedMs} ms` : ""}</span>
          </div>
        </>
      ) : structureError ? (
        <div className="error-banner" role="alert">
          <span>{structureError.message}</span>
          <Button variant="secondary" label="Retry" onClick={() => setStructureAttempt((n) => n + 1)} />
        </div>
      ) : structure ? (
        <StructureView structure={structure} />
      ) : (
        <p className="table-loading">Loading…</p>
      )}
    </div>
  );
}
