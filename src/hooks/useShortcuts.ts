import { useEffect, useRef } from "react";

const MODIFIERS = ["mod", "ctrl", "alt", "shift"] as const;
/**
 * Shortcuts that still fire while focus is in an input or textarea. `mod+w`
 * must always close the tab; otherwise macOS may close the whole window.
 */
const ALLOWED_WHILE_TYPING = new Set([
  "mod+f",
  "mod+k",
  "mod+r",
  "mod+enter",
  "mod+shift+enter",
  "mod+s",
  "mod+t",
  "mod+w",
  "mod+y",
]);

/** Canonical form of a combo such as "shift+mod+Enter" → "mod+shift+enter". */
function normalize(combo: string): string {
  const parts = combo.toLowerCase().split("+");
  const key = parts.pop() ?? "";
  return [...MODIFIERS.filter((m) => parts.includes(m)), key].join("+");
}

function eventCombo(e: KeyboardEvent): string {
  const pressed = { mod: e.metaKey, ctrl: e.ctrlKey, alt: e.altKey, shift: e.shiftKey };
  return [...MODIFIERS.filter((m) => pressed[m]), e.key.toLowerCase()].join("+");
}

/** `<input>` types that take no text, so focus in them is not typing. */
const NON_TEXT_INPUTS = new Set(["checkbox", "radio", "button", "submit", "reset", "file", "color", "range", "image"]);

function isTyping(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  if (target.tagName === "INPUT") return !NON_TEXT_INPUTS.has((target as HTMLInputElement).type);
  return target.tagName === "TEXTAREA" || target.isContentEditable;
}

/** The live maps of every mounted `useShortcuts` call. */
const liveMaps = new Set<{ current: Record<string, () => unknown> }>();

/** True when some mounted `useShortcuts` map has a key whose combo equals the event's. */
export function isRegisteredShortcut(e: KeyboardEvent): boolean {
  const combo = eventCombo(e);
  for (const ref of liveMaps) if (Object.keys(ref.current).some((k) => normalize(k) === combo)) return true;
  return false;
}

/**
 * Keys like 'mod+n', 'mod+enter', 'mod+shift+enter'; `mod` is the Command (meta) key.
 * Modifiers must match exactly. While focus is in an input or textarea only
 * the ALLOWED_WHILE_TYPING combos fire. A handler returning `false` did not handle the event: it is
 * not prevented, so other listeners still see it. Any other return value (including undefined) counts
 * as handled. Events already handled (defaultPrevented) are ignored.
 */
export function useShortcuts(map: Record<string, () => unknown>) {
  const ref = useRef(map);
  ref.current = map;

  useEffect(() => {
    liveMaps.add(ref);
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.defaultPrevented) return;
      const combo = eventCombo(e);
      if (isTyping(e.target) && !ALLOWED_WHILE_TYPING.has(combo)) return;
      const entry = Object.entries(ref.current).find(([k]) => normalize(k) === combo);
      if (entry) {
        const handled = entry[1]();
        if (handled !== false) e.preventDefault();
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => {
      liveMaps.delete(ref);
      window.removeEventListener("keydown", onKeyDown);
    };
  }, []);
}
