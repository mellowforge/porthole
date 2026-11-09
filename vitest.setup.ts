import "@testing-library/jest-dom/vitest";
import { cleanup } from "@testing-library/react";
import { afterEach } from "vitest";

afterEach(() => cleanup());

// CodeMirror measures text through layout APIs that jsdom lacks.
const emptyRect = () =>
  ({ x: 0, y: 0, top: 0, left: 0, right: 0, bottom: 0, width: 0, height: 0, toJSON() {} }) as DOMRect;
const emptyRects = () => ({ length: 0, item: () => null, [Symbol.iterator]: [][Symbol.iterator] }) as unknown as DOMRectList;
Range.prototype.getBoundingClientRect = emptyRect;
Range.prototype.getClientRects = emptyRects;
Element.prototype.getClientRects = emptyRects;

// The app targets macOS: CodeMirror's `Mod-` must mean ⌘ (it reads the platform at import time).
Object.defineProperty(navigator, "platform", { value: "MacIntel", configurable: true });
