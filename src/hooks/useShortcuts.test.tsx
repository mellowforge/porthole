import { fireEvent, render } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { isRegisteredShortcut, useShortcuts } from "./useShortcuts";

function Host({ map }: { map: Record<string, () => void> }) {
  useShortcuts(map);
  return (
    <>
      <input aria-label="field" />
      <textarea aria-label="area" />
    </>
  );
}

describe("useShortcuts", () => {
  it("a handler returning false is not handled: no preventDefault and other listeners still fire", () => {
    const second = vi.fn();
    render(<Host map={{ "mod+s": () => false }} />);
    render(<Host map={{ "mod+s": second }} />);
    const ev = new KeyboardEvent("keydown", { metaKey: true, key: "s", cancelable: true });
    window.dispatchEvent(ev);
    expect(second).toHaveBeenCalledTimes(1);
    expect(ev.defaultPrevented).toBe(true);
    // A combo whose only handler returns false stays unprevented.
    render(<Host map={{ "mod+q": () => false }} />);
    const ev2 = new KeyboardEvent("keydown", { metaKey: true, key: "q", cancelable: true });
    window.dispatchEvent(ev2);
    expect(ev2.defaultPrevented).toBe(false);
  });

  it("false from the only handler leaves defaultPrevented false", () => {
    render(<Host map={{ "mod+s": () => false }} />);
    const ev = new KeyboardEvent("keydown", { metaKey: true, key: "s", cancelable: true });
    window.dispatchEvent(ev);
    expect(ev.defaultPrevented).toBe(false);
  });

  it("mod+w calls handler", () => {
    const handler = vi.fn();
    render(<Host map={{ "mod+w": handler }} />);
    const ev = new KeyboardEvent("keydown", { metaKey: true, key: "w", cancelable: true });
    window.dispatchEvent(ev);
    expect(handler).toHaveBeenCalledTimes(1);
    expect(ev.defaultPrevented).toBe(true);
  });

  it("ignores keys without mod and removes listener on unmount", () => {
    const handler = vi.fn();
    const { unmount } = render(<Host map={{ "mod+w": handler }} />);
    fireEvent.keyDown(window, { key: "w" });
    expect(handler).not.toHaveBeenCalled();
    unmount();
    fireEvent.keyDown(window, { key: "w", metaKey: true });
    expect(handler).not.toHaveBeenCalled();
  });

  it("matches modifiers exactly", () => {
    const close = vi.fn();
    render(<Host map={{ "mod+w": close }} />);
    fireEvent.keyDown(window, { key: "W", metaKey: true, shiftKey: true });
    fireEvent.keyDown(window, { key: "w", metaKey: true, ctrlKey: true });
    fireEvent.keyDown(window, { key: "w", metaKey: true, altKey: true });
    expect(close).not.toHaveBeenCalled();
    fireEvent.keyDown(window, { key: "w", metaKey: true });
    expect(close).toHaveBeenCalledTimes(1);
  });

  it("mod+w in an input still closes and prevents the native close", () => {
    const close = vi.fn();
    const { getByLabelText } = render(<Host map={{ "mod+w": close }} />);
    const ev = new KeyboardEvent("keydown", { metaKey: true, key: "w", cancelable: true, bubbles: true });
    getByLabelText("field").dispatchEvent(ev);
    expect(close).toHaveBeenCalledTimes(1);
    expect(ev.defaultPrevented).toBe(true);
  });

  it("mod+shift+enter is expressible and distinct from mod+enter", () => {
    const run = vi.fn();
    const runAll = vi.fn();
    render(<Host map={{ "mod+enter": run, "shift+mod+Enter": runAll }} />);
    fireEvent.keyDown(window, { key: "Enter", metaKey: true, shiftKey: true });
    expect(runAll).toHaveBeenCalledTimes(1);
    expect(run).not.toHaveBeenCalled();
    fireEvent.keyDown(window, { key: "Enter", metaKey: true });
    expect(run).toHaveBeenCalledTimes(1);
    expect(runAll).toHaveBeenCalledTimes(1);
  });

  it("only the allowed combos fire while typing in an input or textarea", () => {
    const close = vi.fn();
    const create = vi.fn();
    const find = vi.fn();
    const submit = vi.fn();
    const { getByLabelText } = render(
      <Host map={{ "mod+w": close, "mod+n": create, "mod+f": find, "mod+enter": submit }} />,
    );
    for (const field of [getByLabelText("field"), getByLabelText("area")]) {
      fireEvent.keyDown(field, { key: "w", metaKey: true });
      fireEvent.keyDown(field, { key: "n", metaKey: true });
      fireEvent.keyDown(field, { key: "f", metaKey: true });
      fireEvent.keyDown(field, { key: "Enter", metaKey: true });
    }
    expect(close).toHaveBeenCalledTimes(2);
    expect(create).not.toHaveBeenCalled();
    expect(find).toHaveBeenCalledTimes(2);
    expect(submit).toHaveBeenCalledTimes(2);
  });

  it("contenteditable counts as typing", () => {
    const create = vi.fn();
    const save = vi.fn();
    const run = vi.fn();
    const { getByTestId } = render(
      <>
        <Host map={{ "mod+n": create, "mod+s": save, "mod+enter": run }} />
        <div data-testid="ce" contentEditable suppressContentEditableWarning />
      </>,
    );
    const el = getByTestId("ce");
    Object.defineProperty(el, "isContentEditable", { value: true });
    fireEvent.keyDown(el, { key: "n", metaKey: true });
    fireEvent.keyDown(el, { key: "s", metaKey: true });
    fireEvent.keyDown(el, { key: "Enter", metaKey: true });
    expect(create).not.toHaveBeenCalled();
    expect(save).toHaveBeenCalledTimes(1);
    expect(run).toHaveBeenCalledTimes(1);
  });

  it("defaultPrevented events are ignored", () => {
    const handler = vi.fn();
    render(<Host map={{ "mod+w": handler }} />);
    const ev = new KeyboardEvent("keydown", { metaKey: true, key: "w", cancelable: true });
    ev.preventDefault();
    window.dispatchEvent(ev);
    expect(handler).not.toHaveBeenCalled();
  });

  it("a checkbox, radio or button input doesn't count as typing", () => {
    const handler = vi.fn();
    render(
      <>
        <Host map={{ "mod+n": handler }} />
        <input type="checkbox" aria-label="cb" />
        <input type="radio" aria-label="rd" />
        <input type="button" aria-label="bt" />
      </>,
    );
    for (const label of ["cb", "rd", "bt"]) {
      const el = document.querySelector(`[aria-label="${label}"]`) as HTMLElement;
      el.focus();
      fireEvent.keyDown(el, { key: "n", metaKey: true });
    }
    expect(handler).toHaveBeenCalledTimes(3);
  });

  it("isRegisteredShortcut knows the mounted combos", () => {
    const ev = (key: string) => new KeyboardEvent("keydown", { key, metaKey: true });
    const { unmount } = render(<Host map={{ "mod+t": () => {} }} />);
    expect(isRegisteredShortcut(ev("t"))).toBe(true);
    expect(isRegisteredShortcut(ev("c"))).toBe(false);
    unmount();
    expect(isRegisteredShortcut(ev("t"))).toBe(false);
  });
});
