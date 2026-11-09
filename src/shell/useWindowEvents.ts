import { listen } from "@tauri-apps/api/event";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useEffect } from "react";
import * as api from "../api/tauri";
import { schemaIsDatabase, targetName } from "../connections/profile";
import { flushTabState, onTabStatePendingChange, tabStatePending } from "../state/persist";
import { leaveTabIds, newWindowFailed, quitNeedsAsk, useApp } from "../state/store";

export const DEFAULT_TITLE = "Porthole";

/** The `quitCancel` invoke in flight, if any. */
let cancelling: Promise<void> = Promise.resolve();

/** Tells Rust the quit is over. Never rejects; `quitCancelSettled` waits for it. */
export function cancelQuit(): void {
  cancelling = api.quitCancel().catch((e) => console.error("quit_cancel failed", e));
}

/**
 * Resolves once any `quitCancel` has reached Rust. A window that was asking during a quit must not
 * be destroyed before then: Rust's `Destroyed` handling would otherwise move the quit on.
 */
export const quitCancelSettled = (): Promise<void> => cancelling;

export const windowTitle = (active: ReturnType<typeof useApp.getState>["active"]) =>
  active
    ? `${active.profile.name} · ${schemaIsDatabase(active.profile.kind) ? active.schema : targetName(active.profile)}`
    : DEFAULT_TITLE;

/**
 * Window-level events: this window's turn in a quit (`quit-requested`, sent to this webview only), a
 * failed File ▸ New Window from the menu (`open-window-failed`, sent to the focused window), connection
 * and saved-query changes made in other windows, and the window title. Mount once, in `App`.
 */
export function useWindowEvents() {
  useEffect(() => {
    // Not available in a plain browser (`?mock`) or in tests.
    if (!("__TAURI_INTERNALS__" in window)) return;
    let disposed = false;
    const unlisten: Array<() => void> = [];
    const keep = (u: () => void) => (disposed ? u() : unlisten.push(u));
    const fail = (what: string) => (e: unknown) => console.error(`${what} failed`, e);

    // File ▸ New Window from the menu failed; Rust tells the focused window.
    getCurrentWebviewWindow()
      .listen<string>("open-window-failed", ({ payload }) => useApp.getState().setNotice(newWindowFailed(String(payload))))
      .then(keep, fail("listen open-window-failed"));

    const proceed = () =>
      void flushTabState()
        .then(() => api.quitContinue())
        .catch(fail("quit_continue"));
    getCurrentWebviewWindow()
      .listen("quit-requested", () => {
        const { screen, tabs, leave, requestLeave } = useApp.getState();
        // Asked again (⌘Q during the quit) while this window's quit prompt is up: it already asks.
        // Replacing it would cancel the quit.
        if (leave?.reason === "quit") return;
        if (screen !== "workspace") proceed();
        else requestLeave("quit", leaveTabIds(tabs), proceed, cancelQuit);
      })
      .then(keep, fail("listen quit-requested"));

    listen("connections-changed", () => {
      useApp.getState().loadConnections().catch(fail("loadConnections"));
    }).then(keep, fail("listen connections-changed"));

    listen<{ connectionId: string }>("saved-queries-changed", ({ payload }) => {
      const s = useApp.getState();
      if (payload?.connectionId === s.active?.profile.id) void s.loadSavedQueries();
    }).then(keep, fail("listen saved-queries-changed"));

    // Whether quitting this window would ask anything, so a system quit (Dock ▸ Quit, logout)
    // with nothing to ask can end the app at once. Sent only when it changes.
    let reported: boolean | null = null;
    const reportQuitPrompt = () => {
      const asks = quitNeedsAsk(useApp.getState()) || tabStatePending();
      if (asks === reported) return;
      reported = asks;
      api.reportQuitPrompt(asks).catch((e) => {
        // Rust kept the old value: send it again on the next change.
        reported = null;
        fail("report_quit_prompt")(e);
      });
    };
    reportQuitPrompt();
    const offPending = onTabStatePendingChange(reportQuitPrompt);

    let title = DEFAULT_TITLE;
    const unsubscribe = useApp.subscribe((s) => {
      reportQuitPrompt();
      const next = windowTitle(s.active);
      if (next === title) return;
      title = next;
      getCurrentWindow().setTitle(next).catch(fail("setTitle"));
    });

    return () => {
      disposed = true;
      unlisten.forEach((u) => u());
      unsubscribe();
      offPending();
    };
  }, []);
}
