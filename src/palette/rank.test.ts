import { describe, expect, it } from "vitest";
import { rank } from "./rank";

describe("rank", () => {
  it("orders prefix > word start > substring", () => {
    const a = rank("ord", "orders")!;
    const b = rank("ord", "customer_orders")!;
    const c = rank("ord", "word_count")!;
    expect(a).toBeGreaterThan(b);
    expect(b).toBeGreaterThan(c);
  });
  it("matches word initials as an acronym, ahead of a plain subsequence", () => {
    expect(rank("oi", "order_items")).toBe(200 * 1000 - "order_items".length);
    expect(rank("oi", "audit_log_ids")).toBe(10 * 1000 - "audit_log_ids".length);
    expect(rank("oi", "order_items")!).toBeGreaterThan(rank("oi", "audit_log_ids")!);
  });
  it("treats a lower→upper change as a word start", () => {
    expect(rank("ord", "customerOrders")).toBe(200 * 1000 - "customerOrders".length);
  });
  it("uses the tier values and prefers shorter text", () => {
    expect(rank("orders", "orders")).toBe(400 * 1000 - 6);
    expect(rank("ord", "orders")).toBe(300 * 1000 - 6);
    expect(rank("rde", "orders")).toBe(100 * 1000 - 6);
    expect(rank("ors", "orders")).toBe(10 * 1000 - 6);
    expect(rank("ord", "orders")!).toBeGreaterThan(rank("ord", "orders_archive")!);
  });
  it("returns null without a match and ignores case", () => {
    expect(rank("xyz", "orders")).toBeNull();
    expect(rank("ORD", "orders")).toBe(rank("ord", "orders"));
  });
});
