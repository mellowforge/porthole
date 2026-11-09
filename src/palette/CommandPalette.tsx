import { useEffect, useMemo, useRef, useState } from "react";
import * as api from "../api/tauri";
import { isRegisteredShortcut, useShortcuts } from "../hooks/useShortcuts";
import { errorMessage, newWindowFailed, useApp } from "../state/store";
import { filterItems, KIND_LABELS, paletteItems } from "./paletteItems";

/** The ⌘K overlay: global `mod+k` toggle plus the dialog itself. Mounted once in `App`. */
export function CommandPaletteHost() {
  const open = useApp((s) => s.paletteOpen);
  const leaving = useApp((s) => s.leave !== null);
  // A quit or window-close prompt never sits on top of a focused palette.
  useEffect(() => {
    if (leaving) useApp.getState().setPaletteOpen(false);
  }, [leaving]);
  // The leave prompt owns the keyboard while it is open.
  useShortcuts({
    "mod+k": () => {
      const s = useApp.getState();
      if (s.leave) return false;
      s.setPaletteOpen(!s.paletteOpen);
    },
  });
  return open ? <CommandPalette /> : null;
}

export function CommandPalette() {
  const setPaletteOpen = useApp((s) => s.setPaletteOpen);
  const [query, setQuery] = useState("");
  const [selected, setSelected] = useState(0);
  const listRef = useRef<HTMLDivElement>(null);

  // Built once on open: the palette is modal, so the state behind it does not change meanwhile.
  const items = useMemo(
    () =>
      paletteItems(useApp.getState(), {
        openHistory: () => useApp.getState().setHistoryOpen(true),
        newWindow: () =>
          void api.openWindow().catch((e) => useApp.getState().setNotice(newWindowFailed(errorMessage(e)))),
        exportActive: (format) => {
          const { activeTabId, setExportRequest } = useApp.getState();
          if (activeTabId) setExportRequest({ tabId: activeTabId, format });
        },
      }),
    [],
  );
  const shown = useMemo(() => filterItems(items, query), [items, query]);
  const current = Math.min(selected, shown.length - 1);

  useEffect(() => {
    listRef.current?.querySelector('[aria-selected="true"]')?.scrollIntoView?.({ block: "nearest" });
  }, [current, shown]);

  const close = () => setPaletteOpen(false);
  const runAt = (i: number) => {
    const item = shown[i];
    if (!item) return;
    close();
    item.run();
  };

  const onKeyDown = (e: React.KeyboardEvent) => {
    // Page shortcuts (⌘T, ⌘W...) must not act on the page behind the palette. ⌘K still toggles it.
    // React's root listener runs before the window listeners, which skip prevented events.
    if ((e.metaKey || e.ctrlKey) && !(e.metaKey && !e.ctrlKey && !e.altKey && !e.shiftKey && e.key.toLowerCase() === "k") && isRegisteredShortcut(e.nativeEvent)) {
      e.preventDefault();
    }
    if (e.key === "Escape") {
      e.preventDefault();
      close();
    } else if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      if (shown.length === 0) return;
      setSelected((current + (e.key === "ArrowDown" ? 1 : -1) + shown.length) % shown.length);
    } else if (e.key === "Enter" && !e.nativeEvent.isComposing) {
      e.preventDefault();
      runAt(current);
    }
  };

  return (
    <div className="palette-backdrop" onMouseDown={(e) => e.target === e.currentTarget && close()}>
      <div className="palette" role="dialog" aria-label="Search everything" onKeyDown={onKeyDown}>
        <input
          className="palette-input"
          role="combobox"
          aria-expanded="true"
          aria-controls="palette-list"
          aria-activedescendant={shown[current] ? `palette-opt-${current}` : undefined}
          aria-label="Search everything"
          placeholder="Search tables, queries, connections and actions"
          autoFocus
          spellCheck={false}
          autoComplete="off"
          value={query}
          onChange={(e) => {
            setQuery(e.target.value);
            setSelected(0);
          }}
        />
        {shown.length === 0 ? (
          <p className="palette-empty">Nothing matches “{query.trim()}”.</p>
        ) : (
          <div className="palette-list" id="palette-list" role="listbox" ref={listRef}>
            {shown.map((item, i) => (
              <div
                key={item.id}
                id={`palette-opt-${i}`}
                role="option"
                aria-selected={i === current}
                className="palette-row"
                onMouseMove={() => i !== current && setSelected(i)}
                onClick={() => runAt(i)}
              >
                <span className="palette-label">{item.label}</span>
                {item.detail && <span className="palette-detail">{item.detail}</span>}
                <span className="palette-meta">
                  {item.shortcut && <kbd>{item.shortcut}</kbd>}
                  <span className="palette-kind">{KIND_LABELS[item.kind]}</span>
                </span>
              </div>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}
