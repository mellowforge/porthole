import { useShortcuts } from "../hooks/useShortcuts";
import { Button } from "../components/Button";
import { discardStoredTabs } from "../state/persist";
import { useApp } from "../state/store";
import { LeaveGuard } from "./LeaveGuard";
import { Sidebar } from "./Sidebar";
import { QueryView } from "../query/QueryView";
import { TableView } from "../table/TableView";
import { Tabs } from "./Tabs";
import { Toolbar } from "./Toolbar";

export function Workspace() {
  const environment = useApp((s) => s.active?.profile.environment);
  const kind = useApp((s) => s.active?.profile.kind);
  const tabs = useApp((s) => s.tabs);
  const activeTabId = useApp((s) => s.activeTabId);
  const closeTab = useApp((s) => s.closeTab);
  const tabsNotice = useApp((s) => s.tabsNotice);
  const tabsSaveBlocked = useApp((s) => s.tabsSaveBlocked);
  const dismissTabsNotice = useApp((s) => s.dismissTabsNotice);
  const notice = useApp((s) => s.notice);
  const setNotice = useApp((s) => s.setNotice);

  useShortcuts({ "mod+w": () => activeTabId && closeTab(activeTabId) });

  return (
    <div className="workspace">
      <div className="env-strip" data-testid="env-strip" style={{ background: `var(--env-${environment ?? "local"})` }} />
      <Toolbar />
      <div className="workspace-body">
        <Sidebar />
        <div className="workspace-main">
          <Tabs />
          {tabsNotice && (
            <div className="restore-note" role="alert">
              <span>{tabsNotice}</span>
              {tabsSaveBlocked && <Button variant="secondary" label="Discard saved tabs" onClick={() => void discardStoredTabs()} />}
              <button type="button" className="restore-note-close" aria-label="Dismiss" onClick={dismissTabsNotice}>
                ×
              </button>
            </div>
          )}
          {notice && (
            <div className="restore-note" role="alert">
              <span>{notice}</span>
              <button type="button" className="restore-note-close" aria-label="Dismiss" onClick={() => setNotice(null)}>
                ×
              </button>
            </div>
          )}
          <div className="workspace-content">
            {tabs.length === 0 && (
              <p className="workspace-empty">
                {kind === "mongodb" ? "Double-click or click a collection to open it." : "Double-click or click a table to open it."}
              </p>
            )}
            {tabs.map((t) => (
              <div key={t.id} hidden={t.id !== activeTabId} className="workspace-tab">
                {t.kind === "table" ? <TableView tab={t} /> : <QueryView tab={t} />}
              </div>
            ))}
          </div>
        </div>
      </div>
      <LeaveGuard />
    </div>
  );
}
