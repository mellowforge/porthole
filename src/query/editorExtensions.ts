import { defaultKeymap, history, historyKeymap, indentLess, indentMore } from "@codemirror/commands";
import { acceptCompletion, autocompletion, closeBrackets, closeBracketsKeymap, completionKeymap, type Completion, type CompletionContext, type CompletionResult, type CompletionSource } from "@codemirror/autocomplete";
import { javascript } from "@codemirror/lang-javascript";
import { HighlightStyle, bracketMatching, indentUnit, syntaxHighlighting, syntaxTree } from "@codemirror/language";
import { MariaSQL, MySQL, PostgreSQL, SQLite, sql, type SQLDialect, type SQLNamespace } from "@codemirror/lang-sql";
import { highlightSelectionMatches, search, searchKeymap } from "@codemirror/search";
import { EditorState, Prec, StateEffect, StateField, type Extension } from "@codemirror/state";
import { Decoration, EditorView, drawSelection, keymap, lineNumbers, type KeyBinding } from "@codemirror/view";
import { tags } from "@lezer/highlight";
import type { DbKind, RunTarget, StatementSpan } from "../api/types";
import { isMariaDb, statementAtCursor } from "./format";

export interface KeyHandlers {
  onRun(target: RunTarget): void;
  onRunAll(): void;
  onSave(): void;
  onFormat(): void;
}

/** A non-empty selection runs as a range; otherwise the statement at the cursor. */
export function runTargetFor(state: EditorState): RunTarget {
  const { from, to, head } = state.selection.main;
  return from !== to ? { kind: "range", from, to } : { kind: "cursor", at: head };
}

/** Each handler returns true so CodeMirror prevents the default and window shortcuts skip the event. */
export function buildKeymap(h: KeyHandlers): KeyBinding[] {
  return [
    { key: "Mod-Enter", preventDefault: true, run: (v) => (h.onRun(runTargetFor(v.state)), true) },
    { key: "Mod-Shift-Enter", preventDefault: true, run: () => (h.onRunAll(), true) },
    { key: "Mod-s", preventDefault: true, run: () => (h.onSave(), true) },
    { key: "Mod-Shift-f", preventDefault: true, run: () => (h.onFormat(), true) },
  ];
}

/** The range of the statement the cursor is in, or null when there are no statements. */
export function currentStatementRange(state: EditorState, spans: StatementSpan[]): { from: number; to: number } | null {
  const s = statementAtCursor(spans, state.selection.main.head);
  if (!s) return null;
  const len = state.doc.length;
  const from = Math.min(s.from, len);
  return { from, to: Math.max(from, Math.min(s.to, len)) };
}

export const setSpans = StateEffect.define<StatementSpan[]>();

const statementLine = Decoration.line({ class: "cm-current-statement" });

/** Spans from the last analysis, mapped through edits made since; the decoration follows the cursor. */
const statementField = StateField.define<{ spans: StatementSpan[]; deco: ReturnType<typeof Decoration.set> }>({
  create: () => ({ spans: [], deco: Decoration.none }),
  update(value, tr) {
    let spans = value.spans;
    for (const e of tr.effects) if (e.is(setSpans)) spans = e.value;
    if (tr.docChanged && spans === value.spans) {
      const max = tr.startState.doc.length; // spans may be stale or longer than the text
      spans = spans.map((s) => ({
        ...s,
        from: tr.changes.mapPos(Math.min(s.from, max), 1),
        to: tr.changes.mapPos(Math.min(s.to, max), -1),
      }));
    }
    if (spans === value.spans && !tr.selection && !tr.docChanged) return value;
    const r = currentStatementRange(tr.state, spans);
    if (!r) return { spans, deco: Decoration.none };
    const doc = tr.state.doc;
    const first = doc.lineAt(r.from).number;
    const last = doc.lineAt(Math.max(r.from, r.to - 1)).number;
    const ranges = [];
    for (let n = first; n <= last; n++) ranges.push(statementLine.range(doc.line(n).from));
    return { spans, deco: Decoration.set(ranges) };
  },
  provide: (f) => EditorView.decorations.from(f, (v) => v.deco),
});

const highlight = HighlightStyle.define([
  { tag: tags.keyword, color: "var(--syntax-keyword)" },
  { tag: [tags.typeName, tags.standard(tags.typeName)], color: "var(--syntax-keyword)" },
  { tag: [tags.string, tags.special(tags.string)], color: "var(--syntax-string)" },
  { tag: [tags.number, tags.bool, tags.null], color: "var(--syntax-number)" },
  { tag: [tags.function(tags.variableName), tags.standard(tags.name)], color: "var(--syntax-function)" },
  { tag: [tags.lineComment, tags.blockComment], color: "var(--muted)" },
]);

const theme = EditorView.theme({
  "&": { height: "100%", fontSize: "13px", backgroundColor: "#fff" },
  "&.cm-focused": { outline: "none" },
  ".cm-scroller": { fontFamily: "var(--font-mono)", lineHeight: "20.8px" },
  ".cm-content": { padding: "12px 0" },
  ".cm-line": { padding: "0 16px 0 14px" },
  ".cm-gutters": { backgroundColor: "#fff", color: "var(--muted)", border: "none", fontSize: "11px" },
  ".cm-lineNumbers .cm-gutterElement": { padding: "0 10px 0 16px" },
  ".cm-current-statement": { backgroundColor: "var(--statement-bg)" },
  ".cm-activeLine": { backgroundColor: "transparent" },
  ".cm-tooltip": { fontFamily: "var(--font-mono)", fontSize: "12px" },
});

/** The editor's SQL dialect: SQLite's, MySQL's or MariaDB's (by the server's version) for those connections, PostgreSQL's otherwise. */
// mongodb's tabs use `javascript()` (see `sqlCompartmentConfig`); its PostgreSQL entry is never reached.
const EDITOR_DIALECTS: Record<DbKind, SQLDialect> = { postgres: PostgreSQL, mysql: MySQL, sqlite: SQLite, sqlserver: PostgreSQL, mongodb: PostgreSQL };
export const sqlDialectFor = (kind: DbKind | undefined, serverVersion?: string): SQLDialect =>
  kind === "mysql" && isMariaDb(serverVersion) ? MariaSQL : kind ? EDITOR_DIALECTS[kind] : PostgreSQL;

// As `METHODS` in `mongoquery/call.rs`, in that order.
const MONGO_METHODS = ["find", "findOne", "aggregate", "countDocuments", "estimatedDocumentCount", "distinct", "insertOne", "insertMany", "updateOne", "updateMany", "replaceOne", "deleteOne", "deleteMany", "findOneAndUpdate", "findOneAndReplace", "findOneAndDelete", "bulkWrite", "drop", "createIndex", "dropIndex", "getIndexes", "renameCollection"];
const MONGO_CHAIN = ["sort", "skip", "limit"];
const MONGO_OPERATORS = (
  "$eq $ne $gt $gte $lt $lte $in $nin $exists $type $regex $and $or $nor $not $elemMatch $size $all $expr " +
  "$set $unset $inc $push $pull $addToSet $rename " +
  "$match $group $project $sort $limit $skip $unwind $lookup $count $addFields $out $merge " +
  "$sum $avg $min $max $first $last"
).split(" ");
const MONGO_CONSTRUCTORS = ["ObjectId", "ISODate", "NumberLong", "NumberInt", "NumberDecimal", "UUID"];

const completions = (labels: string[], type: string): Completion[] => labels.map((label) => ({ label, type }));
// Its groups hold the collection's name: `db.<1>`, `db.getCollection("<2>" or '<3>')`, `db["<4>" or '<5>']`.
const COLLECTION_RE = String.raw`db(?:\.([A-Za-z_]\w*)|\.getCollection\(\s*(?:"([^"]*)"|'([^']*)')\s*\)|\[\s*(?:"([^"]*)"|'([^']*)')\s*\])`;
const AFTER_DB = /(?:^|[^\w$.])db\.$/;
const AFTER_COLLECTION = new RegExp(`(?:^|[^\\w$.])${COLLECTION_RE}\\.$`);
const AFTER_CALL = /\)\s*\.$/;
const FIND_CHAIN = new RegExp(`(?:^|[^\\w$.])${COLLECTION_RE}\\.find\\(`);
/** A statement's collection, followed by its method: `db.users.`, but not `db.getCollection(`. */
const STATEMENT_COLLECTION = new RegExp(`^[;\\s]*${COLLECTION_RE}\\s*\\.`);

/** The text of the statement the cursor is in, roughly: from the last `db` that starts a statement. */
function statementBefore(before: string): string {
  const starts = [...before.matchAll(/(?:^|[;\n])\s*db\b/g)];
  const last = starts[starts.length - 1];
  return last ? before.slice(last.index) : before;
}

type SyntaxNode = ReturnType<ReturnType<typeof syntaxTree>["resolveInner"]>;

/** Syntax nodes whose text is not code: nothing completes inside them. */
const NOT_CODE = new Set(["String", "TemplateString", "LineComment", "BlockComment", "RegExp"]);

/** Whether a string, comment or regex node that ends at the cursor is closed, so the cursor is after it, not in it. */
function closedBefore(ctx: CompletionContext, n: SyntaxNode): boolean {
  const text = ctx.state.sliceDoc(n.from, n.to);
  switch (n.name) {
    case "String":
    case "TemplateString":
      return text.length >= 2 && text.endsWith(text[0]);
    case "BlockComment":
      return text.length >= 4 && text.endsWith("*/");
    case "RegExp":
      return true;
    default: // a line comment runs to the end of its line
      return false;
  }
}

/** Where the cursor is in the JavaScript syntax tree: inside a string, a comment or a regex, and inside a call's arguments. */
function treeContext(ctx: CompletionContext): { notCode: boolean; inArgs: boolean } {
  let notCode = false;
  let inArgs = false;
  for (let n: SyntaxNode | null = syntaxTree(ctx.state).resolveInner(ctx.pos, -1); n; n = n.parent) {
    if (NOT_CODE.has(n.name) && !(n.to === ctx.pos && closedBefore(ctx, n))) notCode = true;
    if (n.name === "ArgList" && !(n.to === ctx.pos && ctx.state.sliceDoc(n.to - 1, n.to) === ")")) inArgs = true;
  }
  return { notCode, inArgs };
}

/** The bracketed syntax nodes, object literals first, for the innermost bracket around a word. Unclosed, `{ na` parses as a
 *  pattern (destructuring), hence the patterns. */
const OBJECT_NODES = new Set(["ObjectExpression", "ObjectPattern"]);
const OTHER_BRACKETS = new Set(["ArrayExpression", "ArrayPattern", "ArgList", "ParamList", "Block", "ParenthesizedExpression", "ClassBody", "TemplateString"]);

/** Whether the word at `from` is a key of an object literal inside a call's arguments (`find({ na`,
 *  `{ name: "Ann", a`), not a value (`{ name: na`) or an array element. */
function atObjectKey(ctx: CompletionContext, from: number, before: string): boolean {
  if (!/[{,]\s*$/.test(before)) return false;
  let n: SyntaxNode | null = syntaxTree(ctx.state).resolveInner(ctx.pos, -1);
  while (n && !OBJECT_NODES.has(n.name)) {
    if (OTHER_BRACKETS.has(n.name)) return false;
    n = n.parent;
  }
  if (!n || n.from >= from) return false;
  for (let p: SyntaxNode | null = n.parent; p; p = p.parent) if (p.name === "ArgList") return true;
  return false;
}

/** The collection of the statement the cursor is in: `db.<coll>`, `db.getCollection("…")` or `db["…"]`. */
function statementCollection(before: string): string | null {
  const m = STATEMENT_COLLECTION.exec(statementBefore(before));
  return m ? (m[1] ?? m[2] ?? m[3] ?? m[4] ?? m[5] ?? null) : null;
}

const IDENTIFIER = /^[A-Za-z_$][\w$]*$/;

/** The collection's fields as options; a path (`address.city`), or any name that isn't an identifier, is written quoted. */
async function fieldOptions(fields: (collection: string) => Promise<string[]>, collection: string, base: { from: number; validFor: RegExp }): Promise<CompletionResult | null> {
  try {
    const names = await fields(collection);
    if (names.length === 0) return null;
    return { ...base, options: names.map((label) => (IDENTIFIER.test(label) ? { label, type: "property" } : { label, type: "property", apply: JSON.stringify(label) })) };
  } catch {
    // The loader logs its own failures; completion just offers nothing.
    return null;
  }
}

/** MongoDB tab completion: collections after `db.`, methods after a collection, `sort`/`skip`/`limit` after a find, `$` operators, the collection's
 *  field names (from `fields`, when given) at a key of a call's object argument, and the shell's constructors. Nothing inside a string, a comment or a regex. */
export function mongoCompletion(collections: string[], fields?: (collection: string) => Promise<string[]>): CompletionSource {
  // Synchronous but for field names, which come as a promise.
  return (ctx: CompletionContext): CompletionResult | null | Promise<CompletionResult | null> => {
    const word = ctx.matchBefore(/[\w$]*/);
    if (!word) return null;
    const tree = treeContext(ctx);
    if (tree.notCode) return null;
    // An empty word completes only on request, or right after a dot.
    if (word.from === word.to && !ctx.explicit && ctx.state.sliceDoc(Math.max(0, word.from - 1), word.from) !== ".") return null;
    const from = word.from;
    const base = { from, validFor: /^[\w$]*$/ };
    const before = ctx.state.sliceDoc(Math.max(0, from - 2000), from);
    if (word.text.startsWith("$")) return { ...base, options: completions(MONGO_OPERATORS, "keyword") };
    if (AFTER_DB.test(before)) return { ...base, options: [...completions(collections, "class"), ...completions(["getCollection"], "method")] };
    if (AFTER_COLLECTION.test(before)) return { ...base, options: completions(MONGO_METHODS, "method") };
    // Inside a call's arguments (`find({ _id: ObjectId("x").`), a `).` doesn't continue the find chain.
    if (AFTER_CALL.test(before)) return !tree.inArgs && FIND_CHAIN.test(statementBefore(before)) ? { ...base, options: completions(MONGO_CHAIN, "method") } : null;
    if (before.endsWith(".")) return null;
    if (fields && atObjectKey(ctx, from, before)) {
      const collection = statementCollection(before);
      return collection === null ? null : fieldOptions(fields, collection, base);
    }
    return { ...base, options: completions(MONGO_CONSTRUCTORS, "function") };
  };
}

/** `fields` gives a MongoDB collection's field paths, for key completion (the caller binds it to the tab's database); SQL ignores it. */
export const sqlCompartmentConfig = (
  completion: Record<string, string[]>,
  defaultSchema: string,
  kind?: DbKind,
  serverVersion?: string,
  fields?: (collection: string) => Promise<string[]>,
): Extension =>
  kind === "mongodb"
    ? // The completion map's keys are the database's collections and views (the store loads them from `schema_tree`).
      [javascript(), autocompletion({ override: [mongoCompletion(Object.keys(completion), fields)] })]
    : sql({ dialect: sqlDialectFor(kind, serverVersion), schema: completion as SQLNamespace, defaultSchema, upperCaseKeywords: true });

/** Inserts one indent unit at the cursor; with a selection, indents the selected lines. */
function indentAtCursor(view: EditorView): boolean {
  if (!view.state.selection.ranges.every((r) => r.empty)) return indentMore(view);
  view.dispatch(view.state.update(view.state.replaceSelection(view.state.facet(indentUnit)), { scrollIntoView: true, userEvent: "input" }));
  return true;
}

/** The static extensions; the SQL language (schema-dependent) and the key handlers are added by the component. */
export const baseExtensions: Extension[] = [
  lineNumbers(),
  history(),
  drawSelection(),
  bracketMatching(),
  closeBrackets(),
  autocompletion(),
  search({ top: true }),
  highlightSelectionMatches(),
  statementField,
  syntaxHighlighting(highlight),
  theme,
  keymap.of([...closeBracketsKeymap, ...defaultKeymap, ...historyKeymap, ...searchKeymap, ...completionKeymap]),
  // Tab accepts an open completion (as Enter does); otherwise it indents at the cursor. Esc then Tab
  // still moves focus out of the editor (CodeMirror's tab-focus mode).
  Prec.high(keymap.of([{ key: "Tab", run: (v) => acceptCompletion(v) || indentAtCursor(v), shift: indentLess }])),
];

export const handlerKeymap = (h: KeyHandlers): Extension => Prec.highest(keymap.of(buildKeymap(h)));
