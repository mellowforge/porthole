import { useRef, useState } from "react";
import type { ConnectionProfile, SavedQuery } from "../api/types";
import queryIcon from "../assets/icons/query.svg";
import refreshIcon from "../assets/icons/refresh.svg";
import functionIcon from "../assets/icons/function.svg";
import searchIcon from "../assets/icons/search.svg";
import tableIcon from "../assets/icons/table.svg";
import viewIcon from "../assets/icons/view.svg";
import { Button } from "../components/Button";
import { ContextMenu } from "../components/ContextMenu";
import { useShortcuts } from "../hooks/useShortcuts";
import { capabilities, KIND_BADGES, locationLabel, shortVersion, useHomeDir } from "../connections/profile";
import { useApp } from "../state/store";
import { switchConnection } from "./connectionFlow";

const Icon = ({ src }: { src: string }) => <img className="side-icon" src={src} alt="" width={14} height={14} />;

const location = (p: ConnectionProfile) => locationLabel(p, { port: false });

export function Sidebar() {
  useHomeDir((s) => s.home); // re-render when the home folder arrives
  const active = useApp((s) => s.active);
  const tree = useApp((s) => s.tree);
  const setSchema = useApp((s) => s.setSchema);
  const schemaError = useApp((s) => s.schemaError);
  const refreshCatalog = useApp((s) => s.refreshCatalog);
  const refreshing = useApp((s) => s.catalogRefreshing);
  const openTable = useApp((s) => s.openTable);
  const activeTabId = useApp((s) => s.activeTabId);
  const savedQueries = useApp((s) => s.savedQueries);
  const openSavedQuery = useApp((s) => s.openSavedQuery);
  const renameSavedQuery = useApp((s) => s.renameSavedQuery);
  const duplicateSavedQuery = useApp((s) => s.duplicateSavedQuery);
  const deleteSavedQuery = useApp((s) => s.deleteSavedQuery);
  const currentSavedId = useApp((s) => {
    const t = s.tabs.find((x) => x.id === s.activeTabId);
    return t?.kind === "query" ? t.savedQueryId : null;
  });
  const [menu, setMenu] = useState<{ x: number; y: number; q: SavedQuery } | null>(null);
  const [renaming, setRenaming] = useState<{ id: string; name: string } | null>(null);
  const [deleting, setDeleting] = useState<string | null>(null);
  const [savedError, setSavedError] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const searchRef = useRef<HTMLInputElement>(null);

  const refresh = () => {
    if (!refreshing) void refreshCatalog();
  };
  useShortcuts({ "mod+f": () => searchRef.current?.focus(), "mod+r": refresh });

  if (!active) return null;
  const leaveTo = (reason: "edit" | "switch", selectId?: string) => switchConnection(selectId, reason);
  const { profile, schemas, schema } = active;
  const caps = capabilities(profile.kind);
  const searchLabel = caps.searchLabel;
  const q = query.trim().toLowerCase();
  const match = (name: string) => name.toLowerCase().includes(q);
  const tables = (tree?.tables ?? []).filter(match);
  const views = (tree?.views ?? []).filter(match);
  const functions = (tree?.functions ?? []).filter((f) => match(f.name));
  const saved = savedQueries.filter((q) => match(q.name));
  const guarded = async (action: () => Promise<void>) => {
    setSavedError(null);
    try {
      await action();
    } catch (e) {
      setSavedError((e as { message?: string })?.message ?? String(e));
    }
  };
  const isOpen = (name: string) => activeTabId === `${schema}.${name}`;

  return (
    <aside className="sidebar">
      <div className="side-card">
        <div className="side-card-head">
          <span className="conn-badge" data-size="lg">{KIND_BADGES[profile.kind]}</span>
          <span className="side-card-title">
            <strong>{profile.name}</strong>
            <small>{location(profile)}</small>
            {active.serverVersion && (
              <small className="side-version" title={active.serverVersion}>
                {shortVersion(active.serverVersion)}
              </small>
            )}
          </span>
          <Button variant="secondary" label="Edit" onClick={() => leaveTo("edit", profile.id)} />
        </div>
        <div className="side-card-foot">
          <span className="side-connected">
            <span className="side-dot" aria-hidden="true" />
            Connected
          </span>
          <button type="button" className="side-switch" onClick={() => leaveTo("switch")}>
            Switch connection ▾
          </button>
        </div>
      </div>

      <div className="side-search">
        <Icon src={searchIcon} />
        <input
          ref={searchRef}
          type="search"
          placeholder={`${searchLabel}…`}
          aria-label={searchLabel}
          value={query}
          onChange={(e) => setQuery(e.target.value)}
        />
        <kbd>⌘F</kbd>
      </div>

      <div className="side-schema-row">
        <label className="side-schema">
          {caps.schemaLabel}
          <select value={schema} onChange={(e) => void setSchema(e.target.value)}>
            {schemas.map((s) => (
              <option key={s} value={s}>
                {s}
              </option>
            ))}
          </select>
        </label>
        <button
          type="button"
          className="side-refresh"
          aria-label="Refresh"
          title="Refresh (⌘R)"
          disabled={refreshing}
          onClick={refresh}
        >
          <Icon src={refreshIcon} />
        </button>
      </div>
      {schemaError && (
        <div className="side-error" role="alert">
          {schemaError}
        </div>
      )}

      <div className="side-sections">
        {tables.length > 0 && (
          <section>
            <h3>
              {caps.tablesLabel} <span>{tables.length}</span>
            </h3>
            {tables.map((name) => (
              <button
                key={name}
                type="button"
                className="side-item"
                aria-current={isOpen(name) || undefined}
                onClick={() => openTable({ schema, name })}
              >
                <Icon src={tableIcon} />
                <span>{name}</span>
              </button>
            ))}
          </section>
        )}
        {views.length > 0 && (
          <section>
            <h3>
              VIEWS <span>{views.length}</span>
            </h3>
            {views.map((name) => (
              <button
                key={name}
                type="button"
                className="side-item"
                aria-current={isOpen(name) || undefined}
                onClick={() => openTable({ schema, name })}
              >
                <Icon src={viewIcon} />
                <span>{name}</span>
              </button>
            ))}
          </section>
        )}
        {functions.length > 0 && (
          <section>
            <h3>
              FUNCTIONS <span>{functions.length}</span>
            </h3>
            {functions.map((f) => (
              <div key={`${f.name}(${f.signature})`} className="side-item" title={f.kind === "procedure" ? `procedure · ${f.signature}` : f.signature}>
                <Icon src={functionIcon} />
                <span>{f.name}</span>
              </div>
            ))}
          </section>
        )}
        {saved.length > 0 && (
          <section>
            <h3>
              SAVED QUERIES <span>{saved.length}</span>
            </h3>
            {savedError && (
              <div className="side-error" role="alert">
                {savedError}
              </div>
            )}
            {saved.map((q) =>
              renaming?.id === q.id ? (
                <div key={q.id} className="side-item">
                  <Icon src={queryIcon} />
                  <input
                    className="side-rename"
                    aria-label="Rename saved query"
                    value={renaming.name}
                    autoFocus
                    onFocus={(e) => e.currentTarget.select()}
                    onChange={(e) => setRenaming({ id: q.id, name: e.target.value })}
                    onBlur={() => setRenaming(null)}
                    onKeyDown={(e) => {
                      if (e.key === "Escape") setRenaming(null);
                      else if (e.key === "Enter" && renaming.name.trim()) {
                        const name = renaming.name;
                        setRenaming(null);
                        void guarded(() => renameSavedQuery(q.id, name));
                      }
                    }}
                  />
                </div>
              ) : deleting === q.id ? (
                <div key={q.id} className="side-confirm">
                  <span>Delete “{q.name}”?</span>
                  <div>
                    <Button
                      variant="primary"
                      danger
                      label="Delete"
                      onClick={() => {
                        setDeleting(null);
                        void guarded(() => deleteSavedQuery(q.id));
                      }}
                    />
                    <Button variant="secondary" label="Cancel" onClick={() => setDeleting(null)} />
                  </div>
                </div>
              ) : (
                <button
                  key={q.id}
                  type="button"
                  className="side-item"
                  aria-current={currentSavedId === q.id || undefined}
                  onClick={() => openSavedQuery(q)}
                  onContextMenu={(e) => {
                    e.preventDefault();
                    setMenu({ x: e.clientX, y: e.clientY, q });
                  }}
                >
                  <Icon src={queryIcon} />
                  <span>{q.name}</span>
                </button>
              ),
            )}
          </section>
        )}
      </div>
      {menu && (
        <ContextMenu
          x={menu.x}
          y={menu.y}
          onClose={() => setMenu(null)}
          items={[
            { label: "Rename", onSelect: () => setRenaming({ id: menu.q.id, name: menu.q.name }) },
            { label: "Duplicate", onSelect: () => void guarded(() => duplicateSavedQuery(menu.q.id)) },
            { label: "Delete", danger: true, onSelect: () => setDeleting(menu.q.id) },
          ]}
        />
      )}
    </aside>
  );
}
