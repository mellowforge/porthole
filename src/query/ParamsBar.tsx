import { useApp } from "../state/store";
import type { QueryTab } from "../state/store";

export interface ParamsBarProps {
  tab: QueryTab;
  names: string[];
  saved: boolean;
}

export function ParamsBar({ tab, names, saved }: ParamsBarProps) {
  const updateQueryTab = useApp((s) => s.updateQueryTab);
  const set = (name: string, value: string | null) => updateQueryTab(tab.id, { params: { ...tab.params, [name]: value } });
  return (
    <div className="params-bar">
      <span className="params-label">Parameters</span>
      {names.map((name) => {
        const value = tab.params[name] ?? "";
        const isNull = tab.params[name] === null;
        const id = `param-${tab.id}-${name}`;
        return (
          <div key={name} className="param-field">
            <label htmlFor={id}>:{name}</label>
            <input
              id={id}
              type="text"
              value={isNull ? "" : value}
              placeholder={isNull ? "NULL" : undefined}
              data-null={isNull ? "true" : undefined}
              disabled={isNull}
              spellCheck={false}
              onChange={(e) => set(name, e.target.value)}
            />
            <button
              type="button"
              className="param-null"
              aria-label={`Set :${name} to NULL`}
              aria-pressed={isNull}
              onClick={() => set(name, isNull ? "" : null)}
            >
              NULL
            </button>
          </div>
        );
      })}
      <span className="query-spacer" />
      {saved && <span className="params-hint">Last used values are remembered for this query</span>}
    </div>
  );
}
