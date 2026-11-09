import { useEffect, useRef, type KeyboardEvent } from "react";

export interface ContextMenuItem {
  label: string;
  onSelect: () => void;
  danger?: boolean;
  disabled?: boolean;
}

export interface ContextMenuProps {
  x: number;
  y: number;
  items: ContextMenuItem[];
  /** A muted line above the items; not a menu item. */
  note?: string;
  onClose: () => void;
}

/** A menu at the pointer. Closes on Esc, outside click and item click. */
export function ContextMenu({ x, y, items, note, onClose }: ContextMenuProps) {
  const ref = useRef<HTMLDivElement>(null);
  const close = useRef(onClose);
  close.current = onClose;

  useEffect(() => {
    ref.current?.querySelector<HTMLElement>("[role=menuitem]:not(:disabled)")?.focus();
    const onKey = (e: globalThis.KeyboardEvent) => {
      if (e.key === "Escape") close.current();
    };
    const onDown = (e: MouseEvent) => {
      if (!ref.current?.contains(e.target as Node)) close.current();
    };
    document.addEventListener("keydown", onKey);
    document.addEventListener("mousedown", onDown);
    return () => {
      document.removeEventListener("keydown", onKey);
      document.removeEventListener("mousedown", onDown);
    };
  }, []);

  const move = (e: KeyboardEvent<HTMLDivElement>) => {
    const delta = e.key === "ArrowDown" ? 1 : e.key === "ArrowUp" ? -1 : 0;
    if (!delta) return;
    e.preventDefault();
    const els = Array.from(ref.current?.querySelectorAll<HTMLElement>("[role=menuitem]:not(:disabled)") ?? []);
    const i = els.indexOf(document.activeElement as HTMLElement);
    els[(i + delta + els.length) % els.length]?.focus();
  };

  return (
    <div ref={ref} className="context-menu" role="menu" style={{ left: x, top: y }} onKeyDown={move}>
      {note && <div className="context-menu-note">{note}</div>}
      {items.map((it) => (
        <button
          key={it.label}
          type="button"
          role="menuitem"
          data-danger={it.danger ? "true" : undefined}
          disabled={it.disabled}
          onClick={() => {
            onClose();
            it.onSelect();
          }}
        >
          {it.label}
        </button>
      ))}
    </div>
  );
}
