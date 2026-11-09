import { useState } from "react";
import type { ConnectionProfile, Environment } from "../api/types";
import { Button } from "../components/Button";
import { useShortcuts } from "../hooks/useShortcuts";
import { useApp } from "../state/store";
import { ConnectionForm, newProfile } from "./ConnectionForm";
import { KIND_BADGES, locationLabel, searchText, useHomeDir } from "./profile";

const GROUPS: { env: Environment; title: string }[] = [
  { env: "local", title: "LOCAL" },
  { env: "staging", title: "STAGING" },
  { env: "production", title: "PRODUCTION" },
];

const subtitle = (c: ConnectionProfile) => locationLabel(c, { port: true });

export function ConnectionsScreen() {
  useHomeDir((s) => s.home); // re-render when the home folder arrives
  const connections = useApp((s) => s.connections);
  const loadError = useApp((s) => s.loadError);
  const selectedId = useApp((s) => s.selectedConnectionId);
  const openConnections = useApp((s) => s.openConnections);
  const loadConnections = useApp((s) => s.loadConnections);
  const [query, setQuery] = useState("");
  const [formKey, setFormKey] = useState(0);
  const newConnectionNonce = useApp((s) => s.newConnectionNonce);

  const startNew = () => {
    openConnections();
    setFormKey((n) => n + 1);
  };
  useShortcuts({ "mod+n": startNew });

  const q = query.trim().toLowerCase();
  const visible = connections.filter(
    (c) => !q || searchText(c).includes(q),
  );
  const selected = connections.find((c) => c.id === selectedId);

  return (
    <div className="conn-screen">
      <div className="conn-titlebar" data-tauri-drag-region>
        Connections
      </div>
      <div className="conn-body">
        <aside className="conn-list">
          <div className="conn-list-head">
            <h2>Saved connections</h2>
            <Button variant="primary" label="+ New" shortcut="⌘N" onClick={startNew} />
          </div>
          <input
            className="conn-search"
            type="search"
            placeholder="Search connections…"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
          />
          {loadError && <div className="conn-status" data-kind="error" role="alert">✗ {loadError}</div>}
          {GROUPS.map(({ env, title }) => {
            const rows = visible.filter((c) => c.environment === env);
            if (rows.length === 0) return null;
            return (
              <section key={env}>
                <h3>
                  <span className="conn-dot" data-env={env} aria-hidden="true" />
                  {title}
                </h3>
                {rows.map((c) => (
                  <button
                    key={c.id}
                    type="button"
                    className="conn-row"
                    aria-current={c.id === selectedId}
                    onClick={() => {
                      openConnections(c.id);
                      setFormKey((n) => n + 1);
                    }}
                  >
                    <span className="conn-badge">{KIND_BADGES[c.kind]}</span>
                    <span className="conn-row-text">
                      <strong>{c.name}</strong>
                      <small>{subtitle(c)}</small>
                    </span>
                  </button>
                ))}
              </section>
            );
          })}
        </aside>
        <ConnectionForm
          key={`${formKey}:${newConnectionNonce}`}
          initial={selected ?? newProfile()}
          onSaved={async (saved) => {
            await loadConnections().catch(() => {});
            openConnections(saved.id);
          }}
          onDeleted={async () => {
            await loadConnections().catch(() => {});
            openConnections();
            setFormKey((n) => n + 1);
          }}
        />
      </div>
    </div>
  );
}
