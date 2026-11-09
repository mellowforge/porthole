import { Button } from "../components/Button";
import { counts, type PendingChanges, summaryText } from "./pending";

export interface PendingBarProps {
  pending: PendingChanges;
  error: string | null;
  showSql: boolean;
  saving: boolean;
  onToggleSql: () => void;
  onDiscard: () => void;
  onSave: () => void;
}

/**
 * The amber strip above the footer while a table tab has unsaved changes. A save error replaces the
 * muted summary (`2 edits · …`) until the next edit or save attempt. Save and Discard are disabled
 * while a save runs, including one started from the leave prompt.
 */
export function PendingBar({ pending, error, showSql, saving, onToggleSql, onDiscard, onSave }: PendingBarProps) {
  const c = counts(pending);
  if (c.total === 0) return null;
  const { title, detail } = summaryText(c);
  return (
    <div className="pending-bar">
      <span className="pending-dot" aria-hidden="true" />
      <span className="pending-title">{title}</span>
      {error ? (
        <span className="pending-error" role="alert" title={error}>{error}</span>
      ) : (
        <span className="pending-detail">{detail}</span>
      )}
      <div className="pending-actions">
        <Button variant="ghost" label={showSql ? "Hide SQL" : "Show SQL"} onClick={onToggleSql} />
        <Button variant="secondary" label="Discard" onClick={onDiscard} disabled={saving} />
        <Button variant="primary" label="Save changes" shortcut="⌘S" onClick={onSave} disabled={saving} />
      </div>
    </div>
  );
}
