import * as api from "../api/tauri";
import { flushTabState } from "../state/persist";
import { leaveTabIds, useApp } from "../state/store";

/** Stops this window owning its connection's tab state. Never rejects: a failure must not block leaving. */
async function releaseTabState(): Promise<void> {
  try {
    await api.releaseTabState();
  } catch (e) {
    console.error("release_tab_state failed", e);
  }
}

/**
 * Leaves the workspace for the Connections screen (selecting `selectId` there), after the leave
 * prompt and a tab-state flush. The window then gives up its connection's tab state, so another
 * window on that connection saves its tabs. Shared by the sidebar (Edit / Switch connection) and
 * the palette.
 */
export function switchConnection(selectId?: string, reason: "edit" | "switch" = "switch") {
  const { requestLeave, openConnections, tabs } = useApp.getState();
  requestLeave(reason, leaveTabIds(tabs), () =>
    void flushTabState()
      .then(releaseTabState)
      .then(() => openConnections(selectId)),
  );
}
