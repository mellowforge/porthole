import { Button } from "../components/Button";
import type { ExportState } from "./useExport";

export interface ExportStatusProps {
  state: ExportState;
  onCancel: () => void;
  onReveal: (path: string) => void;
}

const n = (v: number) => v.toLocaleString("en-US");

export function ExportStatus({ state, onCancel, onReveal }: ExportStatusProps) {
  switch (state.kind) {
    case "idle":
      return null;
    case "running":
      return (
        <span className="export-status" role="status">
          <span>{`Exporting… ${n(state.rows)} rows`}</span>
          <Button variant="ghost" label="Cancel" onClick={onCancel} />
        </span>
      );
    case "done":
      return (
        <span className="export-status" role="status">
          <span>{`Exported ${n(state.rows)} rows to ${state.fileName}`}</span>
          <Button variant="ghost" label="Show in Finder" onClick={() => onReveal(state.path)} />
        </span>
      );
    case "canceled":
      return <span className="export-status" role="status">Export canceled.</span>;
    case "failed":
      return <span className="export-status export-failed" role="alert">{`Export failed: ${state.message}`}</span>;
    case "revealFailed":
      return <span className="export-status export-failed" role="alert">{state.message}</span>;
  }
}
