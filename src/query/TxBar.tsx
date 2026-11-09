import { useState } from "react";
import { Button } from "../components/Button";
import { errorMessage, useApp } from "../state/store";
import type { QueryTab } from "../state/store";

const LABEL = {
  open: "Transaction open",
  failed: "Transaction failed — roll back to continue",
  unknown: "Transaction state unknown",
} as const;

/** Transaction indicator for the editor header; renders nothing while the session is auto-commit. */
export function TxBar({ tab }: { tab: QueryTab }) {
  const endTransaction = useApp((s) => s.endTransaction);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  if (tab.txStatus === "idle") return null;
  const end = async (commit: boolean) => {
    setBusy(true);
    setError(null);
    try {
      await endTransaction(tab.id, commit);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  const disabled = busy || tab.run.status === "running";

  return (
    <div className="tx-bar" data-status={tab.txStatus}>
      <span className="tx-dot" aria-hidden="true" />
      <span>{LABEL[tab.txStatus]}</span>
      {tab.txStatus !== "failed" && <Button variant="secondary" label="Commit" disabled={disabled} onClick={() => void end(true)} />}
      <Button variant="secondary" label="Rollback" disabled={disabled} onClick={() => void end(false)} />
      {error && (
        <span className="tx-error" role="alert">
          {error}
        </span>
      )}
    </div>
  );
}
