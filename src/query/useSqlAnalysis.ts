import { useEffect, useState } from "react";
import * as api from "../api/tauri";
import type { SqlAnalysis } from "../api/types";

export const ANALYZE_DEBOUNCE_MS = 150;

/**
 * `analyze_sql` for the editor text, `ANALYZE_DEBOUNCE_MS` after the last edit (also on
 * mount). Returns `null` until the first analysis arrives; empty text is not sent and
 * analyzes to no statements and no params.
 */
export function useSqlAnalysis(sql: string): SqlAnalysis | null {
  const [analysis, setAnalysis] = useState<SqlAnalysis | null>(null);

  useEffect(() => {
    if (sql.trim() === "") {
      setAnalysis({ statements: [], params: [] });
      return;
    }
    let stale = false;
    const timer = setTimeout(() => {
      api
        .analyzeSql(sql)
        .then((a) => {
          if (!stale) setAnalysis(a);
        })
        .catch((e) => console.error("analyze_sql failed", e));
    }, ANALYZE_DEBOUNCE_MS);
    return () => {
      stale = true;
      clearTimeout(timer);
    };
  }, [sql]);

  return analysis;
}
