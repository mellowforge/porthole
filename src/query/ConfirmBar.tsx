import { useEffect } from "react";
import type { PendingWrite } from "../api/types";
import { Button } from "../components/Button";

export interface ConfirmBarProps {
  statements: PendingWrite[];
  /** Only the active tab's bar reacts to Esc; hidden tabs stay mounted. */
  active: boolean;
  onRunAnyway(): void;
  onCancel(): void;
}

export function ConfirmBar({ statements, active, onRunAnyway, onCancel }: ConfirmBarProps) {
  useEffect(() => {
    if (!active) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !e.defaultPrevented) onCancel();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [active, onCancel]);

  const verbs = [...new Set(statements.map((s) => s.verb))].join(", ");
  return (
    <div className="confirm-bar" role="alertdialog" aria-label="Confirm Production write">
      <span className="confirm-text">This looks like it changes data on Production:</span>
      <strong className="confirm-verbs">{verbs}</strong>
      <span className="query-spacer" />
      <Button variant="secondary" danger label="Run anyway" onClick={onRunAnyway} />
      <Button variant="secondary" label="Cancel" shortcut="Esc" onClick={onCancel} />
    </div>
  );
}
