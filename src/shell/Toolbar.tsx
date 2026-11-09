import { useCallback } from "react";
import { Button } from "../components/Button";
import { useShortcuts } from "../hooks/useShortcuts";
import { hasQueryTabs } from "../connections/profile";
import { useApp } from "../state/store";
import { HistoryPanel } from "./HistoryPanel";

export function Toolbar() {
  const newQueryTab = useApp((s) => s.newQueryTab);
  const historyOpen = useApp((s) => s.historyOpen);
  const setHistoryOpen = useApp((s) => s.setHistoryOpen);
  const setPaletteOpen = useApp((s) => s.setPaletteOpen);
  const queryTabs = useApp((s) => hasQueryTabs(s.active?.profile?.kind));
  const closeHistory = useCallback(() => setHistoryOpen(false), [setHistoryOpen]);
  const toggleHistory = () => setHistoryOpen(!useApp.getState().historyOpen);
  const newQuery = () => {
    if (queryTabs) newQueryTab();
  };
  useShortcuts({ "mod+t": newQuery, "mod+y": toggleHistory });
  return (
    <header className="toolbar" data-tauri-drag-region>
      <Button
        variant="primary"
        label="+ New query"
        shortcut="⌘T"
        onClick={newQuery}
        disabled={!queryTabs}
      />
      <span className="toolbar-history" data-history-toggle>
        <Button variant="ghost" label="History" shortcut="⌘Y" onClick={toggleHistory} />
      </span>
      <span className="toolbar-spacer" data-tauri-drag-region />
      <Button variant="secondary" label="Search everything" shortcut="⌘K" onClick={() => setPaletteOpen(true)} />
      {historyOpen && <HistoryPanel onClose={closeHistory} />}
    </header>
  );
}
