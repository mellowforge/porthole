import { useRef, useState } from "react";
import { Button } from "../components/Button";
import { ContextMenu } from "../components/ContextMenu";

export interface ExportMenuProps {
  note: string | null;
  disabled: boolean;
  onPick: (format: "csv" | "json") => void;
  variant: "secondary" | "ghost";
}

export function ExportMenu({ note, disabled, onPick, variant }: ExportMenuProps) {
  const [at, setAt] = useState<{ x: number; y: number } | null>(null);
  const ref = useRef<HTMLSpanElement>(null);
  // The menu closes on any outside mousedown, including on this button; remember that so the click
  // that follows toggles it shut instead of reopening it.
  const wasOpen = useRef(false);

  const toggle = () => {
    if (wasOpen.current) return;
    const r = ref.current?.getBoundingClientRect();
    // Keep the menu (up to ~270px wide with a note) inside the window when the button is at the right edge.
    setAt({ x: Math.max(8, Math.min(r?.left ?? 0, window.innerWidth - 280)), y: (r?.bottom ?? 0) + 4 });
  };

  return (
    <span className="export-menu" ref={ref} onMouseDownCapture={() => (wasOpen.current = at !== null)}>
      <Button variant={variant} label="Export" disabled={disabled} onClick={toggle} />
      {at && (
        <ContextMenu
          x={at.x}
          y={at.y}
          note={note ?? undefined}
          onClose={() => setAt(null)}
          items={[
            { label: "Export as CSV", onSelect: () => onPick("csv") },
            { label: "Export as JSON", onSelect: () => onPick("json") },
          ]}
        />
      )}
    </span>
  );
}
