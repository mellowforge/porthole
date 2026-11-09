import { act, render } from "@testing-library/react";
import { EditorView } from "@codemirror/view";
import { completionStatus, startCompletion } from "@codemirror/autocomplete";
import { PostgreSQL, SQLite } from "@codemirror/lang-sql";
import { language } from "@codemirror/language";
import { useRef } from "react";
import { describe, expect, it, vi } from "vitest";
import type { RunTarget, SqlAnalysis } from "../api/types";
import { useShortcuts } from "../hooks/useShortcuts";
import { SqlEditor } from "./SqlEditor";

const analysis: SqlAnalysis = {
  statements: [
    { from: 0, to: 8, kind: "read", verb: "SELECT" },
    { from: 11, to: 19, kind: "read", verb: "SELECT" },
  ],
  params: [],
};

function setup(over: { value?: string; analysis?: SqlAnalysis | null } = {}) {
  const fns = {
    onChange: vi.fn(),
    onRun: vi.fn<(t: RunTarget) => void>(),
    onRunAll: vi.fn(),
    onSave: vi.fn(),
    onFormat: vi.fn(),
    window: vi.fn(),
  };
  let view: { current: EditorView | null } = { current: null };
  function Host() {
    view = useRef<EditorView | null>(null);
    useShortcuts({ "mod+enter": fns.window, "mod+f": fns.window });
    return (
      <SqlEditor
        value={over.value ?? "select 1;\n\nselect 2;"}
        onChange={fns.onChange}
        analysis={over.analysis === undefined ? analysis : over.analysis}
        completion={{}}
        defaultSchema="public"
        onRun={fns.onRun}
        onRunAll={fns.onRunAll}
        onSave={fns.onSave}
        onFormat={fns.onFormat}
        errorAt={null}
        editorRef={view}
      />
    );
  }
  const utils = render(<Host />);
  return { ...utils, fns, view: () => view.current! };
}

const press = (v: EditorView, init: KeyboardEventInit) => {
  // A real keydown on the content element: CodeMirror's own handler runs, then it bubbles to window.
  const e = new KeyboardEvent("keydown", { bubbles: true, cancelable: true, ...init });
  v.contentDOM.dispatchEvent(e);
  return { handled: e.defaultPrevented, e };
};

describe("SqlEditor", () => {
  it("typing calls onChange", () => {
    const { fns, view } = setup({ value: "" });
    act(() => view().dispatch({ changes: { from: 0, insert: "x" }, userEvent: "input.type" }));
    expect(fns.onChange).toHaveBeenCalledWith("x");
  });

  it("⌘↵ in editor calls onRun once with cursor target", () => {
    const { fns, view } = setup();
    act(() => view().dispatch({ selection: { anchor: 3 } }));
    const { handled, e } = press(view(), { key: "Enter", metaKey: true });
    expect(handled).toBe(true);
    expect(e.defaultPrevented).toBe(true); // the window shortcut skips it
    expect(fns.window).not.toHaveBeenCalled();
    expect(fns.onRun).toHaveBeenCalledTimes(1);
    expect(fns.onRun).toHaveBeenCalledWith({ kind: "cursor", at: 3 });
  });

  it("⌘↵ with selection sends range", () => {
    const { fns, view } = setup();
    act(() => view().dispatch({ selection: { anchor: 2, head: 6 } }));
    press(view(), { key: "Enter", metaKey: true });
    expect(fns.onRun).toHaveBeenCalledWith({ kind: "range", from: 2, to: 6 });
  });

  it("⇧⌘↵ calls onRunAll", () => {
    const { fns, view } = setup();
    press(view(), { key: "Enter", metaKey: true, shiftKey: true });
    expect(fns.onRunAll).toHaveBeenCalledTimes(1);
    expect(fns.onRun).not.toHaveBeenCalled();
  });

  it("⌘S and ⇧⌘F call onSave and onFormat", () => {
    const { fns, view } = setup();
    press(view(), { key: "s", metaKey: true });
    press(view(), { key: "F", keyCode: 70, metaKey: true, shiftKey: true });
    expect(fns.onSave).toHaveBeenCalledTimes(1);
    expect(fns.onFormat).toHaveBeenCalledTimes(1);
  });

  it("⌘F in editor does not focus sidebar search", () => {
    const { fns, view } = setup();
    const { handled, e } = press(view(), { key: "f", metaKey: true });
    expect(handled).toBe(true);
    expect(e.defaultPrevented).toBe(true);
    expect(fns.window).not.toHaveBeenCalled();
  });

  it("current statement gets highlight class", () => {
    const { container, view } = setup();
    expect(container.querySelectorAll(".cm-current-statement")).toHaveLength(1);
    act(() => view().dispatch({ selection: { anchor: 14 } }));
    const lines = [...container.querySelectorAll(".cm-line")];
    expect(lines.map((l) => l.classList.contains("cm-current-statement"))).toEqual([false, false, true]);
  });

  it("errorAt places the cursor at a UTF-16 offset, again for each new nonce", () => {
    const sql = "select '😀', nope";
    const at = sql.indexOf("nope");
    let view: { current: EditorView | null } = { current: null };
    function Host({ errorAt }: { errorAt: { at: number; nonce: number } | null }) {
      view = useRef<EditorView | null>(null);
      return (
        <SqlEditor
          value={sql}
          onChange={() => {}}
          analysis={null}
          completion={{}}
          defaultSchema="public"
          onRun={() => {}}
          onRunAll={() => {}}
          onSave={() => {}}
          onFormat={() => {}}
          errorAt={errorAt}
          editorRef={view}
        />
      );
    }
    const { rerender } = render(<Host errorAt={{ at, nonce: 1 }} />);
    expect(view.current!.state.selection.main.head).toBe(at);
    expect(view.current!.state.sliceDoc(at, at + 4)).toBe("nope");
    act(() => view.current!.dispatch({ selection: { anchor: 0 } }));
    expect(view.current!.state.selection.main.head).toBe(0);
    rerender(<Host errorAt={{ at, nonce: 2 }} />);
    expect(view.current!.state.selection.main.head).toBe(at);
  });
});

describe("SqlEditor Tab", () => {
  it("Tab indents at the cursor when no completion is open, Shift-Tab outdents", () => {
    const { view } = setup({ value: "select 1", analysis: null });
    act(() => view().dispatch({ selection: { anchor: 0 } }));
    expect(press(view(), { key: "Tab" }).handled).toBe(true);
    expect(view().state.doc.toString()).toBe("  select 1");
    act(() => {
      press(view(), { key: "Tab", shiftKey: true });
    });
    expect(view().state.doc.toString()).toBe("select 1");
  });

  it("Tab accepts the open completion instead of indenting", async () => {
    const { view } = setup({ value: "selec", analysis: null });
    act(() => view().dispatch({ selection: { anchor: 5 } }));
    act(() => {
      startCompletion(view());
    });
    await vi.waitFor(() => expect(completionStatus(view().state)).toBe("active"));
    await new Promise((r) => setTimeout(r, 100)); // CodeMirror ignores accept keys for 75 ms after opening
    act(() => {
      press(view(), { key: "Tab" });
    });
    expect(view().state.doc.toString()).toBe("SELECT");
  });
});

describe("SqlEditor dialect", () => {
  function mountWith(kind: "postgres" | "sqlite" | undefined) {
    const editorRef = { current: null as EditorView | null };
    const props = {
      value: "select 1", onChange: vi.fn(), analysis: null, completion: {}, defaultSchema: "main",
      onRun: vi.fn(), onRunAll: vi.fn(), onSave: vi.fn(), onFormat: vi.fn(), errorAt: null, editorRef,
    };
    const utils = render(<SqlEditor {...props} kind={kind} />);
    return { ...utils, view: () => editorRef.current!, props };
  }

  it("uses SQLite for a SQLite connection and PostgreSQL otherwise, and follows a change", () => {
    const { view, rerender, props } = mountWith("sqlite");
    expect(view().state.facet(language)).toBe(SQLite.language);
    rerender(<SqlEditor {...props} kind="postgres" />);
    expect(view().state.facet(language)).toBe(PostgreSQL.language);
    mountWith(undefined).unmount();
  });
});

