import { Compartment, EditorState } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { useEffect, useRef, type MutableRefObject } from "react";
import type { DbKind, RunTarget, SqlAnalysis } from "../api/types";
import { baseExtensions, handlerKeymap, setSpans, sqlCompartmentConfig } from "./editorExtensions";

export interface SqlEditorProps {
  value: string;
  onChange(v: string): void;
  analysis: SqlAnalysis | null;
  completion: Record<string, string[]>;
  defaultSchema: string;
  /** The connection's database kind, for the SQL dialect (PostgreSQL when absent). */
  kind?: DbKind;
  /** The server's version string; tells MariaDB from MySQL. */
  serverVersion?: string;
  /** A MongoDB collection's field paths, for key completion; keep it stable (it reconfigures the language). */
  fields?: (collection: string) => Promise<string[]>;
  onRun(target: RunTarget): void;
  onRunAll(): void;
  onSave(): void;
  onFormat(): void;
  /** `nonce` changes on every request, so the same position can be placed again. */
  errorAt: { at: number; nonce: number } | null;
  editorRef?: MutableRefObject<EditorView | null>;
}

export function SqlEditor(props: SqlEditorProps) {
  const host = useRef<HTMLDivElement>(null);
  const viewRef = useRef<EditorView | null>(null);
  const language = useRef(new Compartment());
  // Handlers are read through a ref so the editor is created once.
  const latest = useRef(props);
  latest.current = props;

  useEffect(() => {
    const view = new EditorView({
      parent: host.current!,
      state: EditorState.create({
        doc: latest.current.value,
        extensions: [
          handlerKeymap({
            onRun: (t) => latest.current.onRun(t),
            onRunAll: () => latest.current.onRunAll(),
            onSave: () => latest.current.onSave(),
            onFormat: () => latest.current.onFormat(),
          }),
          language.current.of(sqlCompartmentConfig(latest.current.completion, latest.current.defaultSchema, latest.current.kind, latest.current.serverVersion, latest.current.fields)),
          baseExtensions,
          EditorView.updateListener.of((u) => {
            if (u.docChanged) latest.current.onChange(u.state.doc.toString());
          }),
        ],
      }),
    });
    viewRef.current = view;
    if (latest.current.editorRef) latest.current.editorRef.current = view;
    if (latest.current.analysis) view.dispatch({ effects: setSpans.of(latest.current.analysis.statements) });
    return () => {
      view.destroy();
      viewRef.current = null;
      if (latest.current.editorRef) latest.current.editorRef.current = null;
    };
  }, []);

  // External changes to the text (not our own edits echoed back).
  useEffect(() => {
    const view = viewRef.current;
    if (view && view.state.doc.toString() !== props.value) {
      view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: props.value } });
    }
  }, [props.value]);

  useEffect(() => {
    viewRef.current?.dispatch({ effects: setSpans.of(props.analysis?.statements ?? []) });
  }, [props.analysis]);

  useEffect(() => {
    viewRef.current?.dispatch({
      effects: language.current.reconfigure(sqlCompartmentConfig(props.completion, props.defaultSchema, props.kind, props.serverVersion, props.fields)),
    });
  }, [props.completion, props.defaultSchema, props.kind, props.serverVersion, props.fields]);

  useEffect(() => {
    const view = viewRef.current;
    if (view && props.errorAt) {
      const at = Math.min(props.errorAt.at, view.state.doc.length);
      view.dispatch({ selection: { anchor: at }, effects: EditorView.scrollIntoView(at, { y: "center" }) });
      view.focus();
    }
  }, [props.errorAt]);

  return <div className="sql-editor" ref={host} />;
}
