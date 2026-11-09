import { useEffect, useId, useRef, useState, type KeyboardEvent } from "react";
import * as api from "../api/tauri";
import { capabilities, hasOtherDatabases, targetName } from "../connections/profile";
import { errorMessage, tabDatabase, useApp } from "../state/store";
import type { QueryTab } from "../state/store";

type Loaded = { items: string[] } | { error: string } | null;

/** Loads a list when `key` changes; the answer of a superseded load is ignored. A `null` key loads nothing. */
function useList(load: () => Promise<string[]>, key: string | null, failure: string): Loaded {
  const [state, setState] = useState<Loaded>(null);
  useEffect(() => {
    let live = true;
    setState(null);
    if (key === null) return;
    load().then(
      (items) => live && setState({ items }),
      (e) => live && setState({ error: `${failure}${errorMessage(e)}` }),
    );
    return () => {
      live = false;
    };
    // `load` closes over `key` only.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key]);
  return state;
}

interface ListProps {
  label: string;
  entries: { value: string | null; text: string }[];
  selected: string | null;
  onPick: (value: string | null) => void;
  autoFocus?: boolean;
  disabled?: boolean;
}

/** A listbox: Tab moves between lists, ↑/↓ within one, Enter picks. */
function PickList({ label, entries, selected, onPick, autoFocus, disabled }: ListProps) {
  const id = useId();
  const [cursor, setCursor] = useState(Math.max(0, entries.findIndex((e) => e.value === selected)));
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (autoFocus) ref.current?.focus();
  }, [autoFocus]);
  const onKey = (e: KeyboardEvent) => {
    if (disabled) return;
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      setCursor((c) => Math.min(entries.length - 1, Math.max(0, c + (e.key === "ArrowDown" ? 1 : -1))));
    } else if (e.key === "Enter") {
      e.preventDefault();
      const entry = entries[cursor];
      if (entry) onPick(entry.value);
    }
  };
  return (
    <div className={`target-list${disabled ? " disabled" : ""}`} role="listbox" aria-label={label} aria-disabled={disabled || undefined} tabIndex={0} ref={ref} onKeyDown={onKey} aria-activedescendant={`${id}-${cursor}`}>
      {entries.map((entry, i) => (
        <div
          key={entry.value ?? ""}
          id={`${id}-${i}`}
          role="option"
          aria-selected={entry.value === selected}
          className={`target-option${i === cursor ? " cursor" : ""}`}
          onClick={() => !disabled && onPick(entry.value)}
        >
          {entry.text}
        </div>
      ))}
    </div>
  );
}

export interface TargetPickerProps {
  tab: QueryTab;
  onClose: () => void;
}

/** The popover under the target chip: which database and schema this query tab runs against. */
export function TargetPicker({ tab, onClose }: TargetPickerProps) {
  const retarget = useApp((s) => s.retargetQueryTab);
  const updateQueryTab = useApp((s) => s.updateQueryTab);
  const own = useApp((s) => (s.active?.profile ? targetName(s.active.profile) : null));
  // A SQLite file is one database: the picker offers its schemas only.
  const pickDatabase = useApp((s) => !s.active?.profile || hasOtherDatabases(s.active.profile));
  const schemaLabel = useApp((s) => (s.active?.profile ? capabilities(s.active.profile.kind).schemaLabel : "Schema"));
  const database = useApp((s) => tabDatabase(tab.database, s.active));
  const root = useRef<HTMLDivElement>(null);
  const current = database ?? own;

  const databases = useList(() => api.listDatabases(), pickDatabase ? "" : null, "Couldn't list databases: ");
  const schemas = useList(() => api.targetSchemas(database), database ?? "", "Couldn't list schemas: ");

  useEffect(() => {
    const onKey = (e: globalThis.KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    // The chip itself (the picker's parent holds both) toggles the popover.
    const onDown = (e: MouseEvent) => {
      if (!root.current?.parentElement?.contains(e.target as Node)) onClose();
    };
    document.addEventListener("keydown", onKey);
    document.addEventListener("mousedown", onDown);
    return () => {
      document.removeEventListener("keydown", onKey);
      document.removeEventListener("mousedown", onDown);
    };
  }, [onClose]);

  return (
    <div className="target-picker" ref={root} role="dialog" aria-label="Target">
      {pickDatabase && (
        <div className="target-section">
          <div className="target-title">Database</div>
          {databases === null ? null : "error" in databases ? (
            <p className="target-error" role="alert">{databases.error}</p>
          ) : (
            <PickList
              label="Database"
              autoFocus
              disabled={tab.run.status !== "idle"}
              entries={databases.items.map((d) => ({ value: d, text: d }))}
              selected={current}
              onPick={(d) => d !== null && retarget(tab.id, d)}
            />
          )}
        </div>
      )}
      <div className="target-section">
        <div className="target-title">{schemaLabel}</div>
        {schemas === null ? null : "error" in schemas ? (
          <p className="target-error" role="alert">{schemas.error}</p>
        ) : (
          <PickList
            label={schemaLabel}
            autoFocus={!pickDatabase}
            entries={[{ value: null, text: `Default ${schemaLabel.toLowerCase()}` }, ...schemas.items.map((s) => ({ value: s, text: s }))]}
            selected={tab.schema}
            onPick={(schema) => {
              updateQueryTab(tab.id, { schema });
              onClose();
            }}
          />
        )}
      </div>
    </div>
  );
}
