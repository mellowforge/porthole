import queryIcon from "../assets/icons/query.svg";
import tableIcon from "../assets/icons/table.svg";
import { hasQueryTabs } from "../connections/profile";
import { type Tab, useApp } from "../state/store";

const label = (t: Tab) => (t.kind === "query" ? t.title : t.table.name);

export function Tabs() {
  const tabs = useApp((s) => s.tabs);
  const activeTabId = useApp((s) => s.activeTabId);
  const closeTab = useApp((s) => s.closeTab);
  const setActiveTab = useApp((s) => s.setActiveTab);
  const newQueryTab = useApp((s) => s.newQueryTab);
  const queryTabs = useApp((s) => hasQueryTabs(s.active?.profile?.kind));

  return (
    <div className="tabs" role="tablist">
      {tabs.map((t) => (
        <div
          key={t.id}
          className="tab"
          role="tab"
          tabIndex={0}
          aria-selected={t.id === activeTabId}
          onClick={() => setActiveTab(t.id)}
          onKeyDown={(e) => {
            if (e.target !== e.currentTarget) return;
            if (e.key === "Enter" || e.key === " ") {
              e.preventDefault();
              setActiveTab(t.id);
            }
          }}
        >
          <img className="tab-icon" src={t.kind === "query" ? queryIcon : tableIcon} alt="" width={14} height={14} />
          <span>{label(t)}</span>
          <button type="button" className="tab-close" aria-label={`Close ${label(t)}`} onClick={(e) => {
              e.stopPropagation();
              closeTab(t.id);
            }}>
            ×
          </button>
        </div>
      ))}
      {queryTabs && (
        <button type="button" className="tab-new" aria-label="New query" onClick={() => newQueryTab()}>
          +
        </button>
      )}
    </div>
  );
}
