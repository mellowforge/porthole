import { getCurrentWindow } from "@tauri-apps/api/window";
import { useCallback, useEffect, useMemo, useState } from "react";
import { Button } from "../components/Button";
import { flushTabState } from "../state/persist";
import { quitCancelSettled } from "./useWindowEvents";
import { leaveTabIds, tabsWithEdits, txNeedsAsk, useApp } from "../state/store";
import type { LeaveChoice, QueryTab, TableTab } from "../state/store";
import { counts, type PendingChanges, toChangeSet } from "../table/pending";
import { saveConfirmCopy, SqlStatements, UNCERTAIN_CONFIRM, useSqlPreview } from "../table/SqlPreview";

/**
 * Sheet that asks what to do with unsaved grid edits (Save / Discard) and then with open transactions
 * (Commit / Roll back) before a tab, connection, window or the app is left, or the grid reloads. While
 * a listed tab's save (or Commit / Roll back) is still running it first shows `Waiting for the save to
 * finish…` (or `Waiting for the transaction to end…`) with only Cancel (which keeps it going), then
 * continues with those stages. Also hooks the window-close
 * event into the same prompt (⌘Q is handled in `useWindowEvents`).
 */
export function LeaveGuard() {
  const leave = useApp((s) => s.leave);
  const leaveError = useApp((s) => s.leaveError);
  const tabs = useApp((s) => s.tabs);
  const resolveLeave = useApp((s) => s.resolveLeave);
  const production = useApp((s) => s.active?.profile.environment === "production");
  const [busy, setBusy] = useState(false);
  // Per edited tab, the pending changes whose SQL preview has loaded (when the prompt shows SQL before
  // saving). Compared with the tab's current changes on render, so a change is never confirmed against
  // the statements of an earlier one, not even for the render before the preview's effect runs.
  const [previewed, setPreviewed] = useState<ReadonlyMap<string, PendingChanges>>(new Map());
  const onPreviewed = useCallback(
    (id: string, loaded: PendingChanges | null) =>
      setPreviewed((prev) => {
        if ((prev.get(id) ?? null) === loaded) return prev;
        const next = new Map(prev);
        if (loaded) next.set(id, loaded);
        else next.delete(id);
        return next;
      }),
    [],
  );

  useEffect(() => {
    // Not available in a plain browser (`?mock`) or in tests.
    if (!("__TAURI_INTERNALS__" in window)) return;
    let disposed = false;
    const unlisten: Array<() => void> = [];
    const keep = (u: () => void) => (disposed ? u() : unlisten.push(u));
    const fail = (what: string) => (e: unknown) => console.error(`${what} failed`, e);

    const win = getCurrentWindow();
    win
      .onCloseRequested((e) => {
        // Always intercept: the tab state is flushed before the window goes away.
        e.preventDefault();
        useApp.getState().requestLeave("window", leaveTabIds(useApp.getState().tabs), () =>
          void flushTabState()
            // A quit this request replaced is cancelled in Rust before the window goes away.
            .then(() => quitCancelSettled())
            .then(() => win.destroy())
            .catch(fail("destroy")),
        );
      })
      .then(keep, fail("onCloseRequested"));

    return () => {
      disposed = true;
      unlisten.forEach((u) => u());
    };
  }, []);

  useEffect(() => {
    if (!leave) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !busy) void resolveLeave("cancel");
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [leave, busy, resolveLeave]);

  if (!leave) return null;
  const choose = async (choice: LeaveChoice) => {
    setBusy(true);
    try {
      await resolveLeave(choice);
    } finally {
      setBusy(false);
    }
  };

  if (leave.stage === "waiting") {
    const saving = tabs.some((t) => t.kind === "table" && leave.tabIds.includes(t.id) && t.saving);
    return (
      <div className="leave-scrim">
        <div className="leave-sheet" role="alertdialog" aria-modal="true" aria-label="Saving" aria-describedby="leave-msg">
          <p id="leave-msg">{saving ? "Waiting for the save to finish…" : "Waiting for the transaction to end…"}</p>
          <div className="leave-actions">
            <Button variant="secondary" label="Cancel" onClick={() => void choose("cancel")} />
          </div>
        </div>
      </div>
    );
  }

  if (leave.stage === "edits") {
    const edited = tabsWithEdits(tabs, leave.tabIds);
    // Transient: the store advances the request itself once nothing is left to save.
    if (edited.length === 0) return null;
    const n = edited.length === 1 ? counts(edited[0].pending).total : 0;
    const message =
      edited.length === 1
        ? `“${edited[0].table.name}” has ${n} unsaved ${n === 1 ? "change" : "changes"}.`
        : `${edited.length} tabs have unsaved changes.`;
    // Production, or a tab whose last save may already have been applied: the SQL is shown first, and
    // Save waits until every tab's statements have loaded.
    const uncertain = edited.filter((t) => t.uncertainSave);
    const confirm = saveConfirmCopy(production, uncertain.length > 0);
    const allPreviewed = edited.every((t) => previewed.get(t.id) === t.pending);
    return (
      <div className="leave-scrim">
        <div
          className={confirm ? "leave-sheet leave-sheet-wide" : "leave-sheet"}
          role="alertdialog"
          aria-modal="true"
          aria-label="Unsaved changes"
          aria-describedby="leave-msg"
        >
          <p id="leave-msg">{message}</p>
          <small>
            {production ? "This writes to Production." : "Nothing is written to the database unless you choose Save."}
          </small>
          {uncertain.length > 0 && (
            <small className="leave-warning">
              {edited.length === 1
                ? UNCERTAIN_CONFIRM
                : `${uncertain.map((t) => `“${t.table.name}”`).join(", ")}: ${UNCERTAIN_CONFIRM}`}
            </small>
          )}
          {confirm && (
            <div className="leave-previews" role="region" aria-label="SQL preview">
              {edited.map((t) => (
                <LeavePreview key={t.id} tab={t} titled={edited.length > 1} onReady={onPreviewed} />
              ))}
            </div>
          )}
          {leaveError && (
            <div className="leave-error" role="alert">
              {leaveError}
            </div>
          )}
          <div className="leave-actions">
            <Button variant="secondary" label="Cancel" disabled={busy} onClick={() => void choose("cancel")} />
            <Button variant="secondary" label="Discard" disabled={busy} onClick={() => void choose("discard")} />
            {confirm ? (
              <Button
                variant="secondary"
                danger
                label={confirm.saveLabel}
                disabled={busy || !allPreviewed}
                onClick={() => void choose("save")}
              />
            ) : (
              <Button variant="primary" label="Save" disabled={busy} onClick={() => void choose("save")} />
            )}
          </div>
        </div>
      </div>
    );
  }

  const affected = tabs.filter((t): t is QueryTab => t.kind === "query" && leave.tabIds.includes(t.id) && txNeedsAsk(t.txStatus));
  // Transient: the store advances the request itself once no listed transaction is open.
  if (affected.length === 0) return null;
  // Only when no listed tab is known to be open or failed is the copy hedged.
  const maybe = affected.every((t) => t.txStatus === "unknown");
  const message =
    affected.length === 1
      ? maybe
        ? `“${affected[0].title}” may have an open transaction.`
        : `“${affected[0].title}” has an open transaction.`
      : maybe
        ? `${affected.length} tabs may have open transactions.`
        : `${affected.length} tabs have open transactions.`;

  return (
    <div className="leave-scrim">
      <div className="leave-sheet" role="alertdialog" aria-modal="true" aria-label="Open transaction" aria-describedby="leave-msg">
        <p id="leave-msg">{message}</p>
        <small>Nothing is committed unless you choose Commit.</small>
        {leaveError && (
          <div className="leave-error" role="alert">
            {leaveError}
          </div>
        )}
        <div className="leave-actions">
          <Button variant="secondary" label="Cancel" disabled={busy} onClick={() => void choose("cancel")} />
          <Button variant="secondary" label="Roll back" disabled={busy} onClick={() => void choose("rollback")} />
          <Button variant="secondary" label="Commit" disabled={busy} onClick={() => void choose("commit")} />
        </div>
      </div>
    </div>
  );
}

/** One edited tab's statements in the leave prompt; reports whether those of its current changes loaded. */
function LeavePreview({
  tab,
  titled,
  onReady,
}: {
  tab: TableTab;
  titled: boolean;
  onReady: (id: string, loaded: PendingChanges | null) => void;
}) {
  const set = useMemo(() => toChangeSet(tab.pending, tab.table), [tab.pending, tab.table]);
  const preview = useSqlPreview(set);
  const loaded = preview.ready ? tab.pending : null;
  useEffect(() => {
    onReady(tab.id, loaded);
    // Gone (prompt closed, tab saved or discarded): a later prompt must load it again.
    return () => onReady(tab.id, null);
  }, [onReady, tab.id, loaded]);
  return (
    <div className="leave-preview">
      {titled && <div className="sql-preview-title">{tab.table.name}</div>}
      <SqlStatements preview={preview} />
    </div>
  );
}
