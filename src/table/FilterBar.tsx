import { useEffect, useLayoutEffect, useRef, useState } from "react";
import type { KeyboardEvent } from "react";
import type { AppError, ColumnInfo, RowCount } from "../api/types";
import filterIcon from "../assets/icons/filter.svg";
import { Button } from "../components/Button";
import { formatCount } from "./gridModel";

export const FILTER_HINT =
  "Type a column, an operator (=, !=, >, <, like, in, is null) and a value. Combine with and / or.";

export interface FilterBarProps {
  columns: ColumnInfo[];
  value: string;
  matches: RowCount | null;
  error: AppError | null;
  onApply: (text: string) => void;
  onClear: () => void;
}

const WORD = /[A-Za-z_][A-Za-z0-9_$]*$/;
const PLAIN_IDENT = /^[A-Za-z_][A-Za-z0-9_$]*$/;
const KEYWORDS = new Set(["and", "or", "in", "is", "not", "null", "like"]);

/** How a column name is written in a filter: backtick-quoted unless it is a plain, non-keyword identifier. */
export function columnToken(name: string): string {
  return PLAIN_IDENT.test(name) && !KEYWORDS.has(name.toLowerCase()) ? name : `\`${name}\``;
}
const IDENT_POSITION = /(^|\(\s*|\b(and|or)\s+)$/i;

function currentWord(text: string, caret: number): { word: string; start: number } | null {
  const head = text.slice(0, caret);
  const m = WORD.exec(head);
  if (!m) return null;
  const start = head.length - m[0].length;
  return IDENT_POSITION.test(head.slice(0, start)) ? { word: m[0], start } : null;
}

export function FilterBar({ columns, value, matches, error, onApply, onClear }: FilterBarProps) {
  const [text, setText] = useState(value);
  const [caret, setCaret] = useState(value.length);
  const [highlight, setHighlight] = useState(0);
  const [dismissed, setDismissed] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);
  const pendingCaret = useRef<number | null>(null);

  useLayoutEffect(() => {
    if (pendingCaret.current !== null && inputRef.current) {
      inputRef.current.setSelectionRange(pendingCaret.current, pendingCaret.current);
      pendingCaret.current = null;
    }
  });

  // The applied filter was cleared: the input follows. (Apply needs nothing; the text is what was applied.)
  useEffect(() => {
    if (value !== "") return;
    setText("");
    setCaret(0);
  }, [value]);

  const target = currentWord(text, caret);
  const lower = target?.word.toLowerCase() ?? "";
  const suggestions =
    target && !dismissed
      ? columns
          .map((c) => c.name)
          .filter((n) => n.toLowerCase().startsWith(lower) && n.toLowerCase() !== lower)
          .slice(0, 8)
      : [];

  const insert = (name: string) => {
    if (!target) return;
    const token = columnToken(name);
    const next = text.slice(0, target.start) + token + text.slice(caret);
    const pos = target.start + token.length;
    pendingCaret.current = pos;
    setText(next);
    setCaret(pos);
    setHighlight(0);
  };

  const onKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    const open = suggestions.length > 0;
    if (open && (e.key === "Tab" || e.key === "Enter")) {
      e.preventDefault();
      insert(suggestions[Math.min(highlight, suggestions.length - 1)]);
    } else if (open && e.key === "ArrowDown") {
      e.preventDefault();
      setHighlight((h) => (h + 1) % suggestions.length);
    } else if (open && e.key === "ArrowUp") {
      e.preventDefault();
      setHighlight((h) => (h - 1 + suggestions.length) % suggestions.length);
    } else if (open && e.key === "Escape") {
      setDismissed(true);
    } else if (e.key === "Enter") {
      onApply(text);
    }
  };

  return (
    <div className="filter-bar">
      <div className="filter-row">
        <img className="filter-icon" src={filterIcon} alt="" width={14} height={14} />
        <label htmlFor="quick-filter" className="filter-label">Filter</label>
        <span className="filter-sep" aria-hidden="true" />
        <div className="filter-input-wrap">
          <input
            id="quick-filter"
            ref={inputRef}
            className="filter-input"
            spellCheck={false}
            autoComplete="off"
            placeholder="id = 1 and status = 'paid'"
            value={text}
            onChange={(e) => {
              setText(e.target.value);
              setCaret(e.target.selectionStart ?? e.target.value.length);
              setHighlight(0);
              setDismissed(false);
            }}
            onSelect={(e) => setCaret(e.currentTarget.selectionStart ?? 0)}
            onKeyDown={onKeyDown}
          />
          {suggestions.length > 0 && (
            <ul className="filter-suggest" role="listbox">
              {suggestions.map((name, i) => (
                <li
                  key={name}
                  role="option"
                  aria-selected={i === highlight}
                  data-active={i === highlight ? "true" : undefined}
                  onMouseDown={(e) => {
                    e.preventDefault();
                    insert(name);
                  }}
                >
                  {name}
                </li>
              ))}
            </ul>
          )}
          {error && error.position !== undefined && (
            <div className="filter-caret-line" aria-hidden="true">
              <span data-testid="filter-caret" style={{ marginLeft: `${error.position}ch` }}>^</span>
            </div>
          )}
        </div>
        {matches && <span className="filter-matches">{`${formatCount(matches)} matches`}</span>}
        <kbd className="filter-apply" aria-hidden="true">↵ apply</kbd>
        <Button
          variant="ghost"
          label="Clear"
          onClick={() => {
            // With a filter applied, the text is cleared once the clear goes through (see the `value`
            // effect above); a cancelled leave prompt must leave the input showing the applied filter.
            if (value === "") {
              setText("");
              setCaret(0);
            }
            onClear();
          }}
        />
      </div>
      {error && (
        <div className="filter-error" role="alert">
          {error.message}
        </div>
      )}
      <div className="filter-hint">{FILTER_HINT}</div>
    </div>
  );
}
