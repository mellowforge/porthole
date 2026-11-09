import type { EditorView } from "@codemirror/view";
import { useCallback, useEffect, useRef, useState, type KeyboardEvent, type PointerEvent } from "react";
import playIcon from "../assets/icons/play.svg";
import { formatMongo } from "../api/tauri";
import type { RunTarget } from "../api/types";
import { useShortcuts } from "../hooks/useShortcuts";
import { Button } from "../components/Button";
import { completionKey, defaultSchemaFor, loadFields, runSchema, tabDatabase, useApp } from "../state/store";
import type { QueryTab } from "../state/store";
import { runTargetFor } from "./editorExtensions";
import { editorStateLabel, formatSql } from "./format";
import { ConfirmBar } from "./ConfirmBar";
import { ParamsBar } from "./ParamsBar";
import { ResultsPanel } from "./ResultsPanel";
import { TargetPicker } from "./TargetPicker";
import { KIND_BADGES, schemaIsDatabase, targetName } from "../connections/profile";
import { TxBar } from "./TxBar";
import { SqlEditor } from "./SqlEditor";
import { useSqlAnalysis } from "./useSqlAnalysis";

const MIN_PANE = 120;
const NO_COMPLETION: Record<string, string[]> = {};

export interface QueryViewProps {
  tab: QueryTab;
}

export function QueryView({ tab }: QueryViewProps) {
  const active = useApp((s) => s.active);
  const ensureCompletion = useApp((s) => s.ensureCompletion);
  const database = tabDatabase(tab.database, active);
  const defaultSchema = tab.schema ?? defaultSchemaFor(database, active);
  const cacheKey = completionKey(database, defaultSchema);
  const completion = useApp((s) => s.completionCache[cacheKey]) ?? NO_COMPLETION;
  const missing = !useApp((s) => s.completionCache[cacheKey]);
  // MongoDB key completion: the fields of a collection in the tab's database (its schema).
  const fields = useCallback((collection: string) => loadFields(defaultSchema, collection), [defaultSchema]);
  const [pickerOpen, setPickerOpen] = useState(false);
  const closePicker = useCallback(() => setPickerOpen(false), []);
  const updateQueryTab = useApp((s) => s.updateQueryTab);
  const runQuery = useApp((s) => s.runQuery);
  const stopQuery = useApp((s) => s.stopQuery);
  const trackQueryExport = useApp((s) => s.trackQueryExport);
  const cancelConfirm = useApp((s) => s.cancelConfirm);
  const saveQueryTab = useApp((s) => s.saveQueryTab);
  const isActive = useApp((s) => s.activeTabId === tab.id);
  const [errorAt, setErrorAt] = useState<{ at: number; nonce: number } | null>(null);
  const lastTarget = useRef<RunTarget>({ kind: "all" });
  const analysis = useSqlAnalysis(tab.sql);
  const editorRef = useRef<EditorView | null>(null);
  const [formatError, setFormatError] = useState<string | null>(null);
  const [naming, setNaming] = useState<string | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);
  const panelRef = useRef<HTMLDivElement>(null);
  const [editorHeight, setEditorHeight] = useState<number | null>(null);

  useEffect(() => {
    if (missing && active) void ensureCompletion(database, defaultSchema);
  }, [missing, active, ensureCompletion, database, defaultSchema]);

  const onChange = useCallback(
    (sql: string) => {
      setFormatError(null);
      updateQueryTab(tab.id, { sql });
    },
    [tab.id, updateQueryTab],
  );

  const format = () => {
    const view = editorRef.current;
    if (!view) return;
    const { from, to, empty } = view.state.selection.main;
    const range = empty ? { from: 0, to: view.state.doc.length } : { from, to };
    const text = view.state.sliceDoc(range.from, range.to);
    // One transaction, so a single ⌘Z undoes it.
    const apply = (insert: string) => view.dispatch({ changes: { ...range, insert }, userEvent: "input.format" });
    if (active?.profile?.kind === "mongodb") {
      const sent = view.state.doc;
      formatMongo(text).then(
        (insert) => {
          // A closed editor or an edit made meanwhile wins: the result is for text that is gone.
          if (editorRef.current === view && view.state.doc.eq(sent) && insert !== text) apply(insert);
        },
        (e) => {
          if (editorRef.current === view) setFormatError((e as { message?: string })?.message ?? String(e));
        },
      );
      return;
    }
    try {
      apply(formatSql(text, active?.profile.kind, active?.serverVersion));
    } catch (e) {
      setFormatError((e as Error)?.message ?? String(e));
    }
  };

  const paramNames = analysis?.params ?? [];
  const startRun = (target: RunTarget) => {
    lastTarget.current = target;
    void runQuery(tab.id, target);
  };

  const persist = async (name?: string) => {
    setSaveError(null);
    try {
      await saveQueryTab(tab.id, name);
      setNaming(null);
    } catch (e) {
      setSaveError((e as { message?: string })?.message ?? String(e));
    }
  };
  const save = () => {
    if (tab.savedQueryId !== null) void persist();
    else setNaming((n) => n ?? tab.title);
  };
  const onNameKey = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "Escape") setNaming(null);
    else if (e.key === "Enter" && naming?.trim()) void persist(naming);
  };
  const run = () => {
    const view = editorRef.current;
    startRun(view ? runTargetFor(view.state) : { kind: "all" });
  };
  const running = tab.run.status === "running";

  // Hidden tabs stay mounted, so the shortcuts only act for the active one, and return `false`
  // otherwise so the active tab's listener still sees the event.
  // Inside the editor the keymap handles these first.
  useShortcuts({
    "mod+enter": () => {
      if (!isActive) return false;
      if (!running) run();
    },
    "mod+shift+enter": () => {
      if (!isActive) return false;
      if (!running) startRun({ kind: "all" });
    },
  });
  const runAnyway = () => {
    // The run that asked, as it was: same text, schema and database, whatever the tab points at now.
    void runQuery(tab.id, lastTarget.current, {
      confirmed: true,
      sql: tab.run.sqlAtRun ?? tab.sql,
      ...(tab.run.schemaAtRun !== undefined ? { schema: tab.run.schemaAtRun } : {}),
      ...(tab.run.databaseAtRun !== undefined ? { database: tab.run.databaseAtRun } : {}),
    });
  };
  const cancel = useCallback(() => cancelConfirm(tab.id), [cancelConfirm, tab.id]);

  const resize = (e: PointerEvent<HTMLDivElement>) => {
    if (!e.currentTarget.hasPointerCapture?.(e.pointerId)) return;
    const panel = panelRef.current;
    if (!panel) return;
    const rect = panel.getBoundingClientRect();
    setEditorHeight(Math.min(Math.max(e.clientY - rect.top, MIN_PANE), Math.max(MIN_PANE, rect.height - MIN_PANE)));
  };
  const resizeByKey = (e: KeyboardEvent<HTMLDivElement>) => {
    const delta = e.key === "ArrowUp" ? -24 : e.key === "ArrowDown" ? 24 : 0;
    const panel = panelRef.current;
    if (!delta || !panel) return;
    e.preventDefault();
    const h = panel.getBoundingClientRect().height;
    const current = editorHeight ?? h * 0.4;
    setEditorHeight(Math.min(Math.max(current + delta, MIN_PANE), Math.max(MIN_PANE, h - MIN_PANE)));
  };

  const profile = active?.profile;
  // A MySQL schema is a database: the chip names the one the tab runs on.
  const target = profile && schemaIsDatabase(profile.kind) ? defaultSchema : `${database ?? (profile ? targetName(profile) : "")}${tab.schema ? ` · ${tab.schema}` : ""}`;
  const chip = profile ? `${KIND_BADGES[profile.kind]} · ${target}` : "";

  return (
    <div className="query-view">
      {tab.note && (
        <div className="restore-note" role="status">
          <span>{tab.note}</span>
          <button type="button" className="restore-note-close" aria-label="Dismiss" onClick={() => updateQueryTab(tab.id, { note: null })}>
            ×
          </button>
        </div>
      )}
      <div className="query-header">
        {naming === null ? (
          <h2 className="query-title">{tab.title}</h2>
        ) : (
          <input
            className="query-name"
            aria-label="Query name"
            placeholder="Query name"
            value={naming}
            autoFocus
            onChange={(e) => setNaming(e.target.value)}
            onKeyDown={onNameKey}
          />
        )}
        <span className="query-target">
          <button
            type="button"
            className="query-chip"
            aria-haspopup="dialog"
            aria-expanded={pickerOpen}
            aria-label={`${chip} ▾`}
            onClick={() => setPickerOpen((o) => !o)}
          >
            {chip} ▾
          </button>
          {pickerOpen && <TargetPicker tab={tab} onClose={closePicker} />}
        </span>
        <span className="query-state">{saveError
            ? `Couldn't save: ${saveError}`
            : formatError
              ? `Couldn't format: ${formatError}`
              : editorStateLabel(tab, profile?.name ?? "")}</span>
        <span className="query-spacer" />
        <TxBar tab={tab} />
        <Button variant="ghost" label="Format" shortcut="⇧⌘F" onClick={format} />
        <Button variant="ghost" label="Save" shortcut="⌘S" onClick={save} />
        {running ? (
          <Button variant="secondary" label="Stop" onClick={() => void stopQuery(tab.id)} />
        ) : (
          <Button variant="primary" label="Run" shortcut="⌘↵" icon={playIcon} onClick={run} />
        )}
      </div>
      {tab.run.status === "confirm" && tab.run.pending && (
        <ConfirmBar statements={tab.run.pending} active={isActive} onRunAnyway={runAnyway} onCancel={cancel} />
      )}
      {paramNames.length > 0 && <ParamsBar tab={tab} names={paramNames} saved={tab.savedQueryId !== null} />}
      <div className="query-panel" ref={panelRef}>
        <div className="query-editor" style={{ flexBasis: editorHeight ?? "40%" }}>
          <SqlEditor
            value={tab.sql}
            onChange={onChange}
            analysis={analysis}
            completion={completion}
            defaultSchema={defaultSchema}
            kind={profile?.kind}
            serverVersion={active?.serverVersion}
            fields={fields}
            onRun={startRun}
            onRunAll={() => startRun({ kind: "all" })}
            onSave={save}
            onFormat={format}
            errorAt={errorAt}
            editorRef={editorRef}
          />
        </div>
        <div
          className="query-splitter"
          role="separator"
          aria-orientation="horizontal"
          aria-label="Resize editor"
          tabIndex={0}
          onPointerDown={(e) => e.currentTarget.setPointerCapture?.(e.pointerId)}
          onPointerMove={resize}
          onKeyDown={resizeByKey}
        >
          <span />
        </div>
        <ResultsPanel
          title={tab.title}
          run={tab.run}
          exportTarget={
            tab.txStatus === "failed"
              ? null
              : {
                  tabId: tab.id,
                  database: tabDatabase(tab.run.databaseAtRun !== undefined ? tab.run.databaseAtRun : tab.database, active),
                  schema: runSchema(tab.run.schemaAtRun !== undefined ? tab.run.schemaAtRun : tab.schema, active),
                }
          }
          trackQueryExport={(work) => trackQueryExport(tab.id, work)}
          kind={profile?.kind}
          onSelect={(selected) => updateQueryTab(tab.id, { run: { ...tab.run, selected } })}
          onFetchAll={(r) =>
            void runQuery(tab.id, { kind: "all" }, {
              fetchAll: true,
              sql: r.sql,
              params: r.params,
              schema: tab.run.schemaAtRun !== undefined ? tab.run.schemaAtRun : tab.schema,
              database: tab.run.databaseAtRun !== undefined ? tab.run.databaseAtRun : tab.database,
            })
          }
          onGoToError={(at) => setErrorAt((e) => ({ at, nonce: (e?.nonce ?? 0) + 1 }))}
        />
      </div>
    </div>
  );
}
